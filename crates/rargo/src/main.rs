mod cli;
mod daemon;
#[path = "io/daemon_control.rs"]
mod daemon_control;
mod git;
#[path = "io/install.rs"]
mod install;
mod known;
mod loader;
mod mux;
mod outfs;
mod partial;
mod place;
mod project;
mod report;
mod scan;
mod status;
mod tmp;
mod watch;

use rargo_proto::{ExecRequest, LocalReply, LocalRequest};
use std::ffi::{CString, OsString};
use std::io::{IsTerminal, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = path.with_file_name(format!("{}{}-{}-{name}", tmp::BESIDE, std::process::id(), tmp::seq()));
    let _watched = tmp::watch(&tmp);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/".into()))
}

#[derive(Clone, PartialEq)]
pub struct Config {
    pub host: String,
    pub server: String,
    pub user: String,
    pub secret: Option<String>,
    pub psk: [u8; 32],
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config").field("host", &self.host).field("server", &self.server).field("user", &self.user).field("secret", &self.secret.as_ref().map(|_| "…")).finish_non_exhaustive()
    }
}

struct Real {
    vars: Vec<(String, String)>,
    handlers: std::sync::Once,
}

impl Real {
    fn read() -> Self {
        let vars = std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?))).collect();
        Real { vars, handlers: std::sync::Once::new() }
    }

    fn handle_signals(&self) {
        self.handlers.call_once(tmp::install_handlers);
    }
}

impl cli::World for Real {
    fn vars(&self) -> &[(String, String)] {
        &self.vars
    }

    fn config_path(&self) -> PathBuf {
        project::config_path()
    }

    fn config_text(&self) -> Result<Option<String>, String> {
        project::read_config(&project::config_path())
    }

    fn resolve(&self, host: &str) -> Result<String, String> {
        self.handle_signals();
        project::resolve(host)
    }

    fn credential(&self, host: &str) -> Option<project::Credential> {
        project::credential(host)
    }

    fn nextest_version(&self) -> Option<String> {
        self.handle_signals();
        own_nextest_version()
    }

    fn cwd(&self) -> PathBuf {
        std::env::current_dir().unwrap_or_default()
    }

    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }
}

pub fn load_config() -> Result<Config, String> {
    cli::config(&Real::read())
}

pub fn plain_cause(why: &str) -> String {
    let why = match why.rfind(" (os error ") {
        Some(at) if why.ends_with(')') => &why[..at],
        _ => why,
    };
    let mut chars = why.chars();
    chars.next().map(|c| c.to_lowercase().chain(chars).collect()).unwrap_or_default()
}

fn unreachable_text(server: &str, why: &str) -> String {
    format!(
        "error: cannot reach the rargo server at {server}: {}\n       \
         Check that the server runs and that this machine can reach it, or name another with `rargo @HOST`; \
         RARGO_FALLBACK=cargo builds with plain cargo meanwhile\n",
        plain_cause(why)
    )
}

pub fn cache_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("RARGO_CACHE_DIR") {
        return PathBuf::from(d);
    }
    std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from).unwrap_or_else(|| home_dir().join(".cache")).join("rargo")
}

pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    if unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } != 0 {
        return "unknown".into();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

pub fn find_root(cwd: &Path) -> Option<PathBuf> {
    let ancestors: Vec<&Path> = cwd.ancestors().collect();
    if let Some(d) = ancestors.iter().find(|d| d.join(".git").exists()) {
        return Some(d.to_path_buf());
    }
    ancestors.iter().rev().find(|d| d.join("Cargo.toml").is_file()).map(|d| d.to_path_buf())
}

#[derive(Default)]
pub struct Policy {
    pub pass: Vec<String>,
    pub exclude: Vec<String>,
    pub git: git::Mode,
    pub gitignored: bool,
    pub include: Vec<String>,
}

pub fn load_policy(root: &Path, start: &Path) -> Policy {
    let read = |path: &Path| std::fs::read_to_string(root.join(path)).ok();
    let policy = rargo_proto::workspace::manifest_for(start, read)
        .and_then(|(path, _)| read(&path)?.parse::<toml::Table>().ok())
        .and_then(|t| ["workspace", "package"].iter().find_map(|kind| t.get(*kind)?.get("metadata")?.get("rargo")?.as_table().cloned()))
        .unwrap_or_default();
    let list = |name: &str, key: &str| -> Vec<String> {
        policy
            .get(name)
            .and_then(|s| s.get(key))
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default()
    };
    let flag = |key: &str| policy.get("sync").and_then(|s| s.get(key)).and_then(|v| v.as_bool()).unwrap_or(true);
    Policy {
        pass: list("env", "pass"),
        exclude: list("sync", "exclude"),
        git: match policy.get("sync").and_then(|s| s.get("git")) {
            Some(toml::Value::Boolean(false)) => git::Mode::Off,
            Some(toml::Value::String(s)) if s == "head" => git::Mode::Head,
            Some(toml::Value::String(s)) if s == "history" => git::Mode::History,
            _ => git::Mode::Detect,
        },
        gitignored: flag("gitignored"),
        include: list("sync", "include"),
    }
}

pub fn pattern_matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => pattern == name,
    }
}

fn parse_hex20(s: &str) -> Option<[u8; 20]> {
    let s = s.trim();
    if s.len() != 40 {
        return None;
    }
    let mut out = [0u8; 20];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

pub fn recent_commits(root: &Path, head: &rargo::daemon_state::Head, wt_cache: &Path) -> Vec<[u8; 20]> {
    let head = &head.commit;
    let memo = wt_cache.join("commits");
    let parse = |text: &str| -> Vec<[u8; 20]> { text.lines().filter_map(parse_hex20).collect() };
    if let Ok(text) = std::fs::read_to_string(&memo) {
        if text.lines().next() == Some(head.as_str()) {
            return parse(&text);
        }
    }
    let listed = git::first_parent_list(root, head, 64).unwrap_or_else(|| format!("{head}\n"));
    let _ = write_atomic(&memo, listed.as_bytes());
    parse(&listed)
}

pub fn die(msg: &str) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(101)
}

pub fn load_asked(cache: &Path) -> Vec<String> {
    std::fs::read_to_string(cache.join("env-names")).map(|t| t.lines().map(String::from).collect()).unwrap_or_default()
}

fn daemon_socket(cfg: &Config, cache: &Path) -> PathBuf {
    let mut h = blake3::Hasher::new();
    for part in [cfg.server.as_bytes(), cfg.user.as_bytes(), &cfg.psk[..], cache.as_os_str().as_bytes()] {
        h.update(&(part.len() as u64).to_le_bytes());
        h.update(part);
    }
    for name in ["RARGO_TRANSPORT", "RARGO_DAEMON_IDLE_SECS"] {
        h.update(std::env::var(name).unwrap_or_default().as_bytes());
        h.update(&[0]);
    }
    if let Ok(exe) = std::env::current_exe() {
        h.update(exe.as_os_str().as_bytes());
        if let Ok(md) = std::fs::metadata(&exe) {
            use std::os::unix::fs::MetadataExt;
            h.update(&md.len().to_le_bytes());
            h.update(&md.mtime().to_le_bytes());
            h.update(&md.mtime_nsec().to_le_bytes());
        }
    }
    let name = format!("{}.sock", &h.finalize().to_hex()[..20]);
    let in_cache = cache.join("run").join(&name);
    if in_cache.as_os_str().len() < 100 {
        return in_cache;
    }
    daemon_control::runtime_dir().join(name)
}

fn private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    match std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let md = std::fs::symlink_metadata(dir)?;
    if !md.is_dir() || md.uid() != unsafe { libc::getuid() } {
        return Err(std::io::Error::other(format!("{} is not a directory of this user", dir.display())));
    }
    if md.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn start_daemon(socket: &Path, cache: &Path, cfg: &Config) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    std::fs::create_dir_all(cache)?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(cache.join("daemon.log"))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.env("RARGO_DAEMON_CONFIG", "1").env("RARGO_SERVER", &cfg.server);
    match &cfg.secret {
        Some(s) => cmd.env("RARGO_TOKEN", format!("{}.{s}", cfg.user)),
        None => cmd.env_remove("RARGO_TOKEN"),
    };
    cmd.arg("--rargo-daemon")
        .arg(socket)
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .current_dir("/");
    unsafe {
        cmd.pre_exec(|| {
            match libc::fork() {
                -1 => return Err(std::io::Error::last_os_error()),
                0 => {}
                _ => libc::_exit(0),
            }
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    let _ = child.wait();
    Ok(())
}

fn connect_daemon(cfg: &Config, cache: &Path) -> std::io::Result<std::os::unix::net::UnixStream> {
    use std::os::unix::net::UnixStream;
    let socket = daemon_socket(cfg, cache);
    if let Ok(s) = UnixStream::connect(&socket) {
        return Ok(s);
    }
    daemon_control::retire(&socket, cache);
    if let Some(dir) = socket.parent() {
        private_dir(dir)?;
    }
    start_daemon(&socket, cache, cfg)?;
    let started = std::time::Instant::now();
    loop {
        match UnixStream::connect(&socket) {
            Ok(s) => return Ok(s),
            Err(e) if started.elapsed() > std::time::Duration::from_secs(10) => {
                return Err(std::io::Error::new(e.kind(), format!("the rargo daemon did not start (see {}): {e}", cache.join("daemon.log").display())));
            }
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(5)),
        }
    }
}


pub fn mount_ns() -> String {
    std::fs::read_link("/proc/self/ns/mnt").map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()
}

fn name_server(world: &Real, host: &str) -> ! {
    match project::set_host(host) {
        Ok(path) => {
            eprintln!("{}", cli::named(world, host, &path));
            std::process::exit(0)
        }
        Err(e) => die(&e),
    }
}

fn auth_command(world: &Real, auth: cli::Auth) -> ! {
    match auth {
        cli::Auth::Status { host, from } => println!("{}", cli::auth_status(world, &host, from.as_ref())),
        cli::Auth::Remove { host } => {
            let revoked = project::revoke_on_server(&host);
            if let Err(e) = project::forget(&host) {
                die(&e);
            }
            let (said, warned) = cli::removed(&host, revoked);
            println!("{said}");
            if let Some(w) = warned {
                eprintln!("{w}");
            }
        }
        cli::Auth::Store { host, token } => {
            let stored = project::holder(&host, &token).and_then(|name| match name {
                Some(name) => project::store_token(&host, &token, &name).map(|()| name),
                None => Err(format!("{host}: {}", rargo_proto::REFUSED)),
            });
            match stored {
                Ok(name) => println!("stored the token for {host}, held by {name}"),
                Err(e) => die(&format!("rargo auth: {e}")),
            }
        }
        cli::Auth::Login { host } => match project::auth(&host) {
            Ok(name) => println!("Authenticated to {host} as {name}. `rargo build` now builds there."),
            Err(e) => die(&format!("rargo auth: {e}")),
        },
    }
    std::process::exit(0)
}

fn release_output() {
    let (Ok(cfg), Ok(cwd)) = (load_config(), std::env::current_dir().and_then(|d| d.canonicalize())) else { return };
    let socket = daemon_socket(&cfg, &cache_dir());
    let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&socket) else { return };
    let req = LocalRequest {
        args: Vec::new(),
        kind: "release".into(),
        cwd: cwd.to_string_lossy().into_owned(),
        vars: Vec::new(),
        stdout_tty: false,
        stderr_tty: false,
        mount_ns: mount_ns(),
    };
    let bytes = rargo_proto::encode(&req);
    let _ = stream.write_all(&(bytes.len() as u32).to_le_bytes()).and_then(|_| stream.write_all(&bytes));
    let mut rest = Vec::new();
    let _ = stream.read_to_end(&mut rest);
}

fn relay(reply: LocalReply, cfg: &Config, raw: &[OsString]) {
    match reply {
        LocalReply::Stdout(b) => {
            let mut o = std::io::stdout().lock();
            if o.write_all(&b).and_then(|_| o.flush()).is_err() {
                std::process::exit(141);
            }
        }
        LocalReply::Stderr(b) => {
            let _ = std::io::stderr().lock().write_all(&b);
        }
        LocalReply::Exit(code) => std::process::exit(code),
        LocalReply::Exec(e) => exec(e),
        LocalReply::Unreachable(why) => {
            if std::env::var_os("RARGO_FALLBACK").is_some_and(|v| v == "cargo") {
                passthrough(raw);
            }
            eprint!("{}", unreachable_text(&cfg.server, &why));
            std::process::exit(101);
        }
    }
}

fn through_daemon(cfg: Config, cache: PathBuf, req: LocalRequest, raw: &[OsString]) -> ! {
    let no_daemon = std::env::var_os("RARGO_NO_DAEMON").is_some_and(|v| v == "1");
    let stream = if no_daemon { Err(std::io::Error::other("RARGO_NO_DAEMON")) } else { connect_daemon(&cfg, &cache) };
    let mut stream = match stream {
        Ok(s) => s,
        Err(e) => {
            if !no_daemon && std::env::var_os("RARGO_QUIET_DAEMON").is_none() {
                eprintln!("warning: rargo runs without its daemon ({e}); every command connects to the server anew");
            }
            daemon::inline(cfg.clone(), cache, req, |reply| relay(reply, &cfg, raw));
            die("the command ended without a result");
        }
    };
    match daemon_replies(&mut stream, &req, |reply| relay(reply, &cfg, raw)) {
        Ok(()) => die("the command ended without a result"),
        Err(error) => die(error),
    }
}

/// Send a command to the daemon and relay its messages up to the terminal reply.
fn daemon_replies(stream: &mut (impl Read + Write), req: &LocalRequest, mut each: impl FnMut(LocalReply)) -> Result<(), &'static str> {
    let bytes = rargo_proto::encode(req);
    if stream.write_all(&(bytes.len() as u32).to_le_bytes()).and_then(|_| stream.write_all(&bytes)).is_err() {
        return Err("the rargo daemon closed the connection");
    }
    loop {
        let mut len = [0u8; 4];
        if stream.read_exact(&mut len).is_err() {
            return Err("the rargo daemon went away while the command was running");
        }
        let mut buf = vec![0u8; u32::from_le_bytes(len) as usize];
        if stream.read_exact(&mut buf).is_err() {
            return Err("the rargo daemon went away while the command was running");
        }
        let reply = rargo_proto::decode::<LocalReply>(&buf).map_err(|_| "the rargo daemon said something this program does not understand")?;
        let last = matches!(reply, LocalReply::Exit(_) | LocalReply::Exec(_) | LocalReply::Unreachable(_));
        each(reply);
        if last {
            return Ok(());
        }
    }
}

fn main() {
    let argv0 = std::env::args_os().next();
    let raw: Vec<OsString> = std::env::args_os().skip(1).collect();
    let world = Real::read();
    if raw.first().is_some_and(|arg| arg == "install") && !world.vars.iter().any(|(name, _)| name == "RARGO_BYPASS") {
        install::run(&world, &raw);
        return;
    }
    let (cfg, args, vars, kind) = match cli::plan(&world, argv0.as_deref(), &raw) {
        cli::Plan::Cargo { release, banner } => {
            if let Some(banner) = banner {
                println!("{banner}");
            }
            if release {
                world.handle_signals();
                release_output();
            }
            passthrough(&raw)
        }
        cli::Plan::Print(text) => {
            println!("{text}");
            return;
        }
        cli::Plan::Status => return status::run(),
        cli::Plan::StopDaemons => return daemon_control::stop_all(&cache_dir()),
        cli::Plan::Fail { text, code } => {
            eprintln!("{text}");
            std::process::exit(code)
        }
        cli::Plan::Name(host) => name_server(&world, &host),
        cli::Plan::Auth(auth) => auth_command(&world, auth),
        cli::Plan::Daemon(socket) => daemon::serve(Path::new(&socket)),
        cli::Plan::Remote { config, args, vars, kind } => (config, args, vars, kind),
    };
    world.handle_signals();
    let cwd = match std::env::current_dir().and_then(|d| d.canonicalize()) {
        Ok(d) => d,
        Err(e) => die(&format!("cannot determine the current directory: {e}")),
    };
    if kind == "build" {
        for source in ignored_compiler_wrappers(&args, &vars, &cwd) {
            eprintln!("warning: {source} is ignored for remote builds; rargo runs the server's compiler directly");
        }
    }
    let req = LocalRequest {
        args,
        kind: kind.to_string(),
        cwd: cwd.to_string_lossy().into_owned(),
        vars,
        stdout_tty: std::io::stdout().is_terminal(),
        stderr_tty: std::io::stderr().is_terminal(),
        mount_ns: mount_ns(),
    };
    let cache = cache_dir();
    through_daemon(cfg, cache, req, &raw)
}

fn wrapper_config_keys(table: &toml::Table) -> impl Iterator<Item = &'static str> + '_ {
    ["rustc-wrapper", "rustc-workspace-wrapper"].into_iter().filter(|name| {
        table.get("build").and_then(|build| build.get(name)).and_then(toml::Value::as_str).is_some_and(|wrapper| !wrapper.is_empty())
    })
}

fn cargo_config_args(args: &[String]) -> impl Iterator<Item = &str> {
    let mut args = args.iter().map(String::as_str).take_while(|arg| *arg != "--");
    std::iter::from_fn(move || {
        for arg in args.by_ref() {
            if let Some(config) = arg.strip_prefix("--config=") {
                return Some(config);
            }
            if arg == "--config" {
                return args.next();
            }
        }
        None
    })
}

fn ignored_compiler_wrappers(args: &[String], vars: &[(String, String)], cwd: &Path) -> Vec<String> {
    let var = |name: &str| vars.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str()).filter(|value| !value.is_empty());
    let mut sources: Vec<_> = ["RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER"]
        .into_iter().filter(|name| var(name).is_some()).map(str::to_owned).collect();
    let mut read = std::collections::BTreeSet::new();
    let mut files = Vec::new();

    for config in cargo_config_args(args) {
        match config.parse::<toml::Table>() {
            Ok(table) => sources.extend(wrapper_config_keys(&table).map(|key| format!("--config build.{key}"))),
            Err(_) => files.push(cwd.join(config)),
        }
    }
    let cargo_home = var("CARGO_HOME").map(|path| cwd.join(path))
        .unwrap_or_else(|| Path::new(var("HOME").unwrap_or("/")).join(".cargo"));
    // Only worktree files and the selected Cargo home are sent for the remote build.
    let root = find_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    let project_configs = cwd.ancestors().take_while(|path| path.starts_with(&root)).map(|path| path.join(".cargo"));
    for dir in project_configs.chain(std::iter::once(cargo_home)) {
        let legacy = dir.join("config");
        files.push(if legacy.is_file() { legacy } else { dir.join("config.toml") });
    }
    for file in files {
        if !read.insert(file.clone()) || !file.metadata().is_ok_and(|md| md.is_file() && md.len() <= 1 << 20) {
            continue;
        }
        if let Some(table) = std::fs::read_to_string(&file).ok().and_then(|text| text.parse::<toml::Table>().ok()) {
            sources.extend(wrapper_config_keys(&table).map(|key| format!("build.{key} in {}", file.display())));
        }
    }
    sources
}

fn own_nextest_version() -> Option<String> {
    let out = std::process::Command::new("cargo-nextest")
        .args(["nextest", "--version"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    let mut words = text.lines().next()?.split_whitespace();
    (words.next()? == "cargo-nextest").then(|| words.next().map(String::from)).flatten()
}

fn real_cargo() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("RARGO_REAL_CARGO") {
        return Some(PathBuf::from(p));
    }
    let me = std::env::current_exe().ok().and_then(|p| p.canonicalize().ok());
    for dir in std::env::split_paths(&std::env::var_os("PATH")?) {
        let cand = dir.join("cargo");
        if !cand.is_file() {
            continue;
        }
        if cand.canonicalize().ok() == me {
            continue;
        }
        return Some(cand);
    }
    None
}

fn passthrough(args: &[OsString]) -> ! {
    match real_cargo() {
        Some(cargo) => {
            let err = std::process::Command::new(&cargo).args(args).exec();
            eprintln!("error: cannot run {}: {err}", cargo.display());
        }
        None => eprintln!("error: no `cargo` found on PATH to hand this command to"),
    }
    std::process::exit(101)
}

fn cstr(p: &Path) -> CString {
    CString::new(p.as_os_str().as_bytes()).expect("path without NUL")
}

fn mount(src: Option<&Path>, tgt: &Path, fstype: Option<&str>, flags: libc::c_ulong, data: Option<&str>) -> std::io::Result<()> {
    let src_c = src.map(cstr);
    let tgt_c = cstr(tgt);
    let fs_c = fstype.map(|s| CString::new(s).unwrap());
    let data_c = data.map(|s| CString::new(s).unwrap());
    let r = unsafe {
        libc::mount(
            src_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            tgt_c.as_ptr(),
            fs_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            flags,
            data_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr() as *const libc::c_void),
        )
    };
    if r != 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

fn enter_namespace(binds: &[(String, String)]) -> std::io::Result<()> {
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let os_err = std::io::Error::last_os_error;
    if unsafe { libc::unshare(libc::CLONE_NEWNS) } != 0 {
        if unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) } != 0 {
            return Err(os_err());
        }
        std::fs::write("/proc/self/setgroups", "deny")?;
        std::fs::write("/proc/self/uid_map", format!("{uid} {uid} 1"))?;
        std::fs::write("/proc/self/gid_map", format!("{gid} {gid} 1"))?;
    }
    mount(None, Path::new("/"), None, libc::MS_REC | libc::MS_PRIVATE, None)?;
    let entries: Vec<std::fs::DirEntry> = std::fs::read_dir("/")?.filter_map(|e| e.ok()).collect();
    let covered = ["/tmp", "/mnt", "/media", "/srv", "/opt", "/home", "/var"]
        .into_iter()
        .find(|d| Path::new(d).is_dir() && !binds.iter().any(|(real, _)| Path::new(real).starts_with(d)))
        .ok_or_else(|| std::io::Error::other("no directory to build the namespace on"))?;
    let tmp_fd = std::fs::File::open(covered)?;
    let new_root = Path::new(covered);
    mount(Some(Path::new("tmpfs")), new_root, Some("tmpfs"), 0, Some("mode=755"))?;
    use std::os::fd::AsRawFd;
    for e in entries {
        let name = e.file_name();
        let src = Path::new("/").join(&name);
        let dst = new_root.join(&name);
        let Ok(ft) = e.file_type() else { continue };
        if binds.iter().any(|(_, at)| Path::new(at) == src) {
            continue;
        }
        if ft.is_symlink() {
            if let Ok(t) = std::fs::read_link(&src) {
                let _ = std::os::unix::fs::symlink(t, &dst);
            }
        } else if ft.is_dir() {
            std::fs::create_dir(&dst)?;
            let from = if src == new_root { PathBuf::from(format!("/proc/self/fd/{}", tmp_fd.as_raw_fd())) } else { src };
            mount(Some(&from), &dst, None, libc::MS_BIND | libc::MS_REC, None)?;
        } else if ft.is_file() {
            std::fs::File::create(&dst)?;
            mount(Some(&src), &dst, None, libc::MS_BIND, None)?;
        }
    }
    for (real, at) in binds {
        let dst = new_root.join(at.trim_start_matches('/'));
        std::fs::create_dir_all(&dst)?;
        let real_p = match Path::new(real).strip_prefix(covered) {
            Ok(rest) => PathBuf::from(format!("/proc/self/fd/{}", tmp_fd.as_raw_fd())).join(rest),
            Err(_) => PathBuf::from(real),
        };
        mount(Some(&real_p), &dst, None, libc::MS_BIND | libc::MS_REC, None)?;
    }
    let root_c = cstr(new_root);
    unsafe {
        if libc::chdir(root_c.as_ptr()) != 0
            || libc::syscall(libc::SYS_pivot_root, c".".as_ptr(), c".".as_ptr()) != 0
            || libc::umount2(c".".as_ptr(), libc::MNT_DETACH) != 0
        {
            return Err(os_err());
        }
    }
    Ok(())
}

fn to_real(s: &str, binds: &[(String, String)]) -> String {
    let mut out = String::new();
    for (i, part) in s.split(':').enumerate() {
        if i > 0 {
            out.push(':');
        }
        let mut done = false;
        for (real, at) in binds {
            if part == at {
                out.push_str(real);
                done = true;
                break;
            }
            if let Some(rest) = part.strip_prefix(at.as_str()) {
                if rest.starts_with('/') {
                    out.push_str(real);
                    out.push_str(rest);
                    done = true;
                    break;
                }
            }
        }
        if !done {
            out.push_str(part);
        }
    }
    out
}

fn exec(mut e: ExecRequest) -> ! {
    e.binds.retain(|(real, _)| Path::new(real).exists());
    if e.binds.is_empty() {
    } else if let Err(err) = enter_namespace(&e.binds) {
        if std::env::var_os("RARGO_QUIET_NS").is_none() {
            eprintln!(
                "warning: cannot create a mount namespace ({err}); running without it. \
                 Paths compiled into the program (such as CARGO_MANIFEST_DIR) will point to `{}`.",
                e.binds.first().map(|b| b.1.as_str()).unwrap_or("/w")
            );
        }
        e.program = to_real(&e.program, &e.binds);
        for (_, v) in e.env.iter_mut() {
            if let Some(v) = v {
                *v = to_real(v, &e.binds);
            }
        }
    }
    let mut cmd = std::process::Command::new(&e.program);
    cmd.args(&e.args).current_dir(&e.cwd);
    for (k, v) in &e.env {
        match v {
            Some(v) => cmd.env(k, v),
            None => cmd.env_remove(k),
        };
    }
    let err = cmd.exec();
    let file = Path::new(&e.cwd).join(&e.program);
    let mut head = Vec::new();
    if let Ok(f) = std::fs::File::open(&file) {
        let _ = std::io::Read::read_to_end(&mut std::io::Read::take(f, 16 << 20), &mut head);
    }
    let Some(loader) = loader::interpreter(&head).map(|i| String::from_utf8_lossy(i).into_owned()).filter(|l| !Path::new(l).exists()) else {
        eprintln!("error: could not execute process `{}` (never executed)\n\nCaused by:\n  {err}", e.program);
        std::process::exit(101)
    };
    let missing: Vec<String> = loader::needed(&head).into_iter().filter(|p| !Path::new(p).exists()).collect();
    let home = std::env::var("HOME").ok();
    let nix_store = loader::nix_store_candidates(std::env::var("PATH").ok().as_deref(), home.as_deref()).into_iter().find(|p| p.is_file());
    let failed = match (nix_store, &home) {
        (Some(nix_store), Some(home)) if !missing.is_empty() => {
            let roots = format!("{home}/.cache/rargo/nix");
            eprintln!("rargo: `{}` needs Nix's C library, which this machine does not have yet; fetching it once", e.program);
            let essential = loader::store_path(loader.as_bytes());
            let fetched = std::fs::create_dir_all(&roots).map_err(|err| format!("creating {roots}: {err}")).and_then(|()| {
                loader::fetches(&missing, &roots).into_iter().zip(&missing).try_for_each(|(args, path)| {
                    let out = std::process::Command::new(&nix_store).args(&args).stdin(std::process::Stdio::null()).output();
                    let why = match out {
                        Ok(out) if out.status.success() => return Ok(()),
                        Ok(out) => String::from_utf8_lossy(&out.stderr).trim().to_string(),
                        Err(err) => format!("running {}: {err}", nix_store.display()),
                    };
                    if essential.as_ref() == Some(path) { Err(why) } else { Ok(()) }
                })
            });
            match fetched {
                Ok(()) => {
                    let err = cmd.exec();
                    Some(format!("it was fetched, but the program still cannot start: {err}"))
                }
                Err(why) => Some(why),
            }
        }
        _ => None,
    };
    eprint!("{}", loader::text(&e.program, &loader, failed.as_deref()));
    std::process::exit(101)
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_unreachable_server_is_named_with_a_plain_cause_and_what_to_do() {
        for (why, cause) in [
            ("Connection refused (os error 111)", "connection refused"),
            ("connection timed out", "connection timed out"),
            ("No route to host (os error 113)", "no route to host"),
            ("odd (os error) inside", "odd (os error) inside"),
            ("", ""),
        ] {
            assert_eq!(super::plain_cause(why), cause, "{why}");
        }
        let text = super::unreachable_text("build.example:7459", "Connection refused (os error 111)");
        assert!(text.starts_with("error: cannot reach the rargo server at build.example:7459: connection refused\n"), "{text}");
        assert!(text.contains("`rargo @HOST`") && text.contains("RARGO_FALLBACK=cargo") && !text.contains("os error"), "{text}");
    }

    #[test]
    fn writes_of_one_path_at_once_all_succeed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shared");
        let threads: Vec<_> = (0..16)
            .map(|t| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for i in 0..200 {
                        crate::write_atomic(&path, format!("{t} {i}").as_bytes()).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert!(std::fs::read_to_string(&path).unwrap().ends_with(" 199"));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    use super::*;

    #[test]
    fn socket_name_follows_the_configuration() {
        let cfg = |server: &str| Config { host: server.into(), server: server.into(), user: "u".into(), secret: None, psk: [7; 32] };
        let cache = Path::new("/c");
        let a = daemon_socket(&cfg("a:1"), cache);
        assert_eq!(a, daemon_socket(&cfg("a:1"), cache));
        assert_ne!(a, daemon_socket(&cfg("b:1"), cache));
        assert_ne!(a, daemon_socket(&cfg("a:1"), Path::new("/d")));
        assert!(a.starts_with("/c/run"));
        let long = PathBuf::from(format!("/{}", "x".repeat(120)));
        assert!(!daemon_socket(&cfg("a:1"), &long).starts_with(&long));
    }
}
