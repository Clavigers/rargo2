mod atomic_write;
mod cli;
mod daemon;
#[path = "io/daemon_control.rs"]
mod daemon_control;
mod git;
#[path = "io/host.rs"]
mod host;
#[path = "io/http.rs"]
mod http;
#[path = "io/inotify.rs"]
mod inotify;
#[path = "io/install.rs"]
mod install;
#[path = "io/kernel.rs"]
mod kernel;
mod known;
mod loader;
mod mux;
#[path = "io/namespace.rs"]
mod namespace;
mod outfs;
mod partial;
mod place;
mod project;
mod report;
#[path = "io/signals.rs"]
mod signals;
mod status;
mod tmp;
mod watch;

use rargo::scan;
use rargo_net::io;
use rargo_net::io::file::PathRing as _;
use rargo_proto::{ExecRequest, LocalReply, LocalRequest};
use std::ffi::OsString;
use std::io::{IsTerminal, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Writes `bytes` to `path` through a temporary beside it that `names` numbers.
pub fn write_atomic(path: &Path, bytes: &[u8], names: &mut tmp::Names) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        io::file::create_dir_all(parent)?;
    }
    let write = atomic_write::Plan::new(path, io::process::id(), names.next());
    let _watched = tmp::watch(write.temporary());
    for effect in write.effects(bytes) {
        match effect {
            atomic_write::Effect::Write { path, bytes } => io::file::write(path, bytes)?,
            atomic_write::Effect::Rename { from, to } => io::file::rename(from, to).inspect_err(|_| {
                let _ = io::file::remove_file(from);
            })?,
        }
    }
    Ok(())
}

fn home_dir() -> PathBuf {
    PathBuf::from(io::env::var_os("HOME").unwrap_or_else(|| "/".into()))
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
    handlers: std::cell::Cell<bool>,
    names: std::cell::RefCell<tmp::Names>,
}

impl Real {
    fn read() -> Self {
        Real { vars: io::env::vars(), handlers: std::cell::Cell::new(false), names: std::cell::RefCell::new(tmp::Names::all()) }
    }

    fn handle_signals(&self) {
        if !self.handlers.replace(true) {
            tmp::install_handlers();
        }
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
        project::resolve(host, &mut self.names.borrow_mut())
    }

    fn holder(&self, host: &str, token: &str) -> Result<Option<String>, String> {
        self.handle_signals();
        project::holder(host, token)
    }

    fn credential(&self, host: &str) -> Option<project::Credential> {
        project::credential(host)
    }

    fn auth_disabled(&self, host: &str) -> bool {
        self.handle_signals();
        project::server_status(host).is_ok_and(|info| info.get("auth").and_then(|a| a.as_bool()) == Some(false))
    }

    fn nextest_version(&self) -> Option<String> {
        self.handle_signals();
        own_nextest_version()
    }

    fn cwd(&self) -> PathBuf {
        std::env::current_dir().unwrap_or_default()
    }

    fn is_file(&self, path: &Path) -> bool {
        path.ring_is_file()
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

/// The one line a command ends with when the kernel gives rargo no io_uring, and what still works.
fn ring_refused_text(why: &str) -> String {
    format!("error: rargo cannot run on this machine: {why}\n       RARGO_FALLBACK=cargo builds with plain cargo meanwhile\n")
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
    if let Some(d) = io::env::var_os("RARGO_CACHE_DIR") {
        return PathBuf::from(d);
    }
    io::env::var_os("XDG_CACHE_HOME").map(PathBuf::from).unwrap_or_else(|| home_dir().join(".cache")).join("rargo")
}

pub fn hostname() -> String {
    host::hostname().unwrap_or_else(|| "unknown".into())
}

pub fn find_root(cwd: &Path) -> Option<PathBuf> {
    let ancestors: Vec<&Path> = cwd.ancestors().collect();
    if let Some(d) = ancestors.iter().find(|d| d.join(".git").ring_exists()) {
        return Some(d.to_path_buf());
    }
    ancestors.iter().rev().find(|d| d.join("Cargo.toml").ring_is_file()).map(|d| d.to_path_buf())
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
    let read = |path: &Path| io::file::read_to_string(root.join(path)).ok();
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

pub fn recent_commits(root: &Path, head: &rargo::daemon_state::Head, wt_cache: &Path, names: &mut tmp::Names) -> Vec<[u8; 20]> {
    let head = &head.commit;
    let memo = wt_cache.join("commits");
    let parse = |text: &str| -> Vec<[u8; 20]> { text.lines().filter_map(parse_hex20).collect() };
    if let Ok(text) = io::file::read_to_string(&memo) {
        if text.lines().next() == Some(head.as_str()) {
            return parse(&text);
        }
    }
    let listed = git::first_parent_list(root, head, 64).unwrap_or_else(|| format!("{head}\n"));
    let _ = write_atomic(&memo, listed.as_bytes(), names);
    parse(&listed)
}

pub fn die(msg: &str) -> ! {
    eprintln!("error: {msg}");
    io::process::exit(101)
}

pub fn load_asked(cache: &Path) -> Vec<String> {
    io::file::read_to_string(cache.join("env-names")).map(|t| t.lines().map(String::from).collect()).unwrap_or_default()
}

fn daemon_socket(cfg: &Config, cache: &Path) -> PathBuf {
    let mut h = blake3::Hasher::new();
    for part in [cfg.server.as_bytes(), cfg.user.as_bytes(), &cfg.psk[..], cache.as_os_str().as_bytes()] {
        h.update(&(part.len() as u64).to_le_bytes());
        h.update(part);
    }
    for name in ["RARGO_TRANSPORT", "RARGO_DAEMON_IDLE_SECS"] {
        h.update(io::env::var(name).unwrap_or_default().as_bytes());
        h.update(&[0]);
    }
    if let Ok(exe) = std::env::current_exe() {
        h.update(exe.as_os_str().as_bytes());
        if let Ok(md) = io::file::metadata(&exe) {
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
    match io::file::create_dir_all_mode(dir, 0o700) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let md = io::file::symlink_metadata(dir)?;
    if !md.is_dir() || md.uid() != io::process::uid() {
        return Err(std::io::Error::other(format!("{} is not a directory of this user", dir.display())));
    }
    if md.mode() & 0o077 != 0 {
        io::file::set_mode(dir, 0o700)?;
    }
    Ok(())
}

fn start_daemon(socket: &Path, cache: &Path, cfg: &Config) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    io::file::create_dir_all(cache)?;
    let log = io::file::OpenOptions::new().create(true).append(true).open(cache.join("daemon.log"))?;
    let mut cmd = io::process::Command::new(exe);
    cmd.env("RARGO_DAEMON_CONFIG", "1").env("RARGO_SERVER", &cfg.server);
    match &cfg.secret {
        Some(s) => cmd.env("RARGO_TOKEN", format!("{}.{s}", cfg.user)),
        None => cmd.env_remove("RARGO_TOKEN"),
    };
    cmd.arg("--rargo-daemon")
        .arg(socket)
        .stdin(io::process::Stdio::null())
        .stdout(std::os::fd::OwnedFd::from(log.try_clone()?))
        .stderr(std::os::fd::OwnedFd::from(log))
        .current_dir("/");
    host::detach(cmd.std_mut());
    cmd.status().map(|_| ())
}

fn connect_daemon(cfg: &Config, cache: &Path) -> std::io::Result<io::UnixStream> {
    use io::UnixStream;
    let socket = daemon_socket(cfg, cache);
    if let Ok(s) = UnixStream::connect(&socket) {
        return Ok(s);
    }
    daemon_control::retire(&socket, cache);
    if let Some(dir) = socket.parent() {
        private_dir(dir)?;
    }
    start_daemon(&socket, cache, cfg)?;
    let started = io::now();
    loop {
        match UnixStream::connect(&socket) {
            Ok(s) => return Ok(s),
            Err(e) if io::now().saturating_duration_since(started) > std::time::Duration::from_secs(10) => {
                return Err(std::io::Error::new(e.kind(), format!("the rargo daemon did not start (see {}): {e}", cache.join("daemon.log").display())));
            }
            Err(_) => io::sleep(std::time::Duration::from_millis(5)),
        }
    }
}


pub fn mount_ns() -> String {
    io::file::read_link("/proc/self/ns/mnt").map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()
}

fn name_server(world: &Real, host: &str, force: bool) -> ! {
    if let Err(e) = cli::may_name(world, host, force) {
        die(&e);
    }
    match project::set_host(host) {
        Ok(path) => {
            eprintln!("{}", cli::named(world, host, &path));
            io::process::exit(0)
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
    io::process::exit(0)
}

fn release_output() {
    let (Ok(cfg), Ok(cwd)) = (load_config(), std::env::current_dir().and_then(io::file::canonicalize)) else { return };
    let socket = daemon_socket(&cfg, &cache_dir());
    let Ok(mut stream) = io::UnixStream::connect(&socket) else { return };
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
                io::process::exit(141);
            }
        }
        LocalReply::Stderr(b) => {
            let _ = std::io::stderr().lock().write_all(&b);
        }
        LocalReply::Exit(code) => io::process::exit(code),
        LocalReply::Exec(e) => exec(e),
        LocalReply::Unreachable(why) => fall_back_or_end(&unreachable_text(&cfg.server, &why), raw),
        LocalReply::Refused(text) => fall_back_or_end(&text, raw),
    }
}

/// When rargo cannot build here, plain cargo builds instead under RARGO_FALLBACK=cargo; otherwise the command ends with `text`.
fn fall_back_or_end(text: &str, raw: &[OsString]) -> ! {
    if io::env::var_os("RARGO_FALLBACK").is_some_and(|v| v == "cargo") {
        passthrough(raw);
    }
    eprint!("{text}");
    io::process::exit(101);
}

fn through_daemon(cfg: Config, cache: PathBuf, req: LocalRequest, raw: &[OsString]) -> ! {
    let no_daemon = io::env::var_os("RARGO_NO_DAEMON").is_some_and(|v| v == "1");
    let stream = if no_daemon { Err(std::io::Error::other("RARGO_NO_DAEMON")) } else { connect_daemon(&cfg, &cache) };
    let mut stream = match stream {
        Ok(s) => s,
        Err(e) => {
            if !no_daemon && io::env::var_os("RARGO_QUIET_DAEMON").is_none() {
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
        let last = matches!(reply, LocalReply::Exit(_) | LocalReply::Exec(_) | LocalReply::Unreachable(_) | LocalReply::Refused(_));
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
                let banner = if cli::colour(&world.vars, std::io::stdout().is_terminal()) { cli::paint(&banner) } else { banner };
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
            io::process::exit(code)
        }
        cli::Plan::Name { host, force } => name_server(&world, &host, force),
        cli::Plan::Auth(auth) => auth_command(&world, auth),
        cli::Plan::Daemon(socket) => daemon::serve(Path::new(&socket)),
        cli::Plan::Remote { config, args, vars, kind } => (config, args, vars, kind),
    };
    world.handle_signals();
    let cwd = match std::env::current_dir().and_then(io::file::canonicalize) {
        Ok(d) => d,
        Err(e) => die(&format!("cannot determine the current directory: {e}")),
    };
    let manifest = cli::manifest_path_arg(&args).and_then(|given| io::file::canonicalize(cwd.join(given)).ok());
    let (cwd, args) = match manifest.and_then(|m| cli::rehome(&cwd, &args, &m, find_root)) {
        Some(moved) => moved,
        None => (cwd, args),
    };
    if kind == "build" {
        let root = find_root(&cwd);
        let configs = cargo_configs(&args, &vars, &cwd, root.as_deref().unwrap_or(&cwd));
        if let Some(root) = &root
            && let Some((name, path, source)) = outside_config_dependency(root, &configs)
        {
            fall_back_or_end(
                &format!(
                    "error: path dependency `{name}` in {source} points to `{}`, outside the synced worktree `{}`\n         move the dependency inside the worktree and update its path, or run with RARGO_FALLBACK=cargo to build locally\n",
                    path.display(),
                    root.display(),
                ),
                &raw,
            );
        }
        if let (Some(root), Ok(manifest)) = (root, cli::find_manifest(&world, &args))
            && let Some((name, path)) = outside_path_dependency(&root, &manifest)
        {
            fall_back_or_end(
                &format!(
                    "error: path dependency `{name}` points to `{path}`, outside the synced worktree `{}`\n         move the dependency inside the worktree and update its path, or run with RARGO_FALLBACK=cargo to build locally\n",
                    root.display(),
                ),
                &raw,
            );
        }
        for source in ignored_compiler_wrappers(&vars, &configs) {
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
    if let Some(why) = io::unavailable() {
        fall_back_or_end(&ring_refused_text(&why), &raw);
    }
    through_daemon(cfg, cache, req, &raw)
}

fn outside_path_dependency(root: &Path, manifest: &Path) -> Option<(String, String)> {
    let (entry_dir, entry_table) = read_dependency_manifest(manifest)?;
    let selected_workspace = dependency_workspace(root, &entry_dir, &entry_table)
        .filter(|(dir, table)| !workspace_excludes(dir, table, &entry_dir));
    let mut pending = vec![(manifest.to_owned(), true)];
    if let Some((dir, table)) = &selected_workspace {
        pending.push((dir.join("Cargo.toml"), true));
        pending.extend(workspace_patterns(table, "members")
            .flat_map(|pattern| workspace_member_paths(dir, pattern))
            .filter(|member| !workspace_excludes(dir, table, member))
            .map(|member| (member.join("Cargo.toml"), true)));
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut entry = true;
    while let Some((manifest, member)) = pending.pop() {
        let Some((dir, table)) = read_dependency_manifest(&manifest) else { continue };
        if !dir.starts_with(root) || !seen.insert((dir.clone(), member)) {
            continue;
        }

        let workspace = dependency_workspace(root, &dir, &table);
        let inherited = workspace.as_ref().and_then(|(_, table)| {
            table.get("workspace")?.get("dependencies")?.as_table()
        });
        let targets = table.get("target").and_then(toml::Value::as_table).into_iter()
            .flat_map(|targets| targets.values().filter_map(toml::Value::as_table));
        let dependencies = std::iter::once(&table).chain(targets)
            .flat_map(|section| {
                ["dependencies", "build-dependencies", "dev-dependencies", "build_dependencies", "dev_dependencies"]
                    .into_iter()
                    .filter(|kind| member || !matches!(*kind, "dev-dependencies" | "dev_dependencies"))
                    .filter_map(|kind| section.get(kind)?.as_table())
            })
            .flatten()
            .filter_map(|(name, dependency)| {
                if dependency.get("workspace").and_then(toml::Value::as_bool) == Some(true) {
                    Some((&workspace.as_ref()?.0, name, inherited?.get(name)?))
                } else {
                    Some((&dir, name, dependency))
                }
            });
        let (patch_dir, patch_table) = selected_workspace.as_ref().map_or((&entry_dir, &entry_table), |(dir, table)| (dir, table));
        // Cargo applies patches from the selected workspace, regardless of
        // which manifests in the dependency graph declare their own patches.
        let patches = entry.then(|| patch_table.get("patch")?.as_table()).flatten().into_iter()
            .flat_map(|sources| sources.values().filter_map(toml::Value::as_table))
            .flatten()
            .map(|(name, dependency)| ((patch_dir, name, dependency), false));
        entry = false;
        for ((base, name, dependency), from_member) in dependencies.map(|dependency| (dependency, member)).chain(patches) {
            let Some(path) = dependency.get("path").and_then(toml::Value::as_str) else { continue };
            let Ok(resolved) = io::file::canonicalize(base.join(path)) else { continue };
            if !resolved.starts_with(root) {
                return Some((name.clone(), path.to_owned()));
            }
            // Implicit membership only follows path dependencies of members.
            let member = from_member && selected_workspace.as_ref().is_some_and(|(dir, table)| {
                resolved.starts_with(dir) && !workspace_excludes(dir, table, &resolved)
            });
            pending.push((resolved.join("Cargo.toml"), member));
        }
    }
    None
}

fn workspace_patterns<'a>(table: &'a toml::Table, key: &str) -> impl Iterator<Item = &'a str> {
    table.get("workspace").and_then(|workspace| workspace.get(key))
        .and_then(toml::Value::as_array).into_iter().flatten().filter_map(toml::Value::as_str)
}

fn workspace_excludes(dir: &Path, table: &toml::Table, candidate: &Path) -> bool {
    // Cargo treats exclusions as path prefixes, with literal member paths taking precedence.
    workspace_patterns(table, "exclude").any(|path| candidate.starts_with(dir.join(path)))
        && !workspace_patterns(table, "members").any(|path| candidate.starts_with(dir.join(path)))
}

// Expand member globs by reading only the directories at the wildcard positions.
fn workspace_member_paths(base: &Path, pattern: &str) -> Vec<PathBuf> {
    let mut paths = vec![base.to_owned()];
    for component in Path::new(pattern).components() {
        let name = component.as_os_str();
        if !name.as_bytes().iter().any(|byte| matches!(byte, b'*' | b'?' | b'[')) {
            for path in &mut paths {
                path.push(name);
            }
            continue;
        }
        paths = paths.into_iter()
            .filter_map(|path| io::file::read_dir(path).ok())
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.file_name().is_some_and(|candidate| {
                git::ignore::wildmatch(name.as_bytes(), candidate.as_bytes(), true)
            }))
            .collect();
    }
    paths.sort_unstable();
    paths
}

fn read_dependency_manifest(manifest: &Path) -> Option<(PathBuf, toml::Table)> {
    let dir = io::file::canonicalize(manifest.parent()?).ok()?;
    let table = io::file::read_to_string(manifest).ok()?.parse().ok()?;
    Some((dir, table))
}

fn dependency_workspace(root: &Path, dir: &Path, table: &toml::Table) -> Option<(PathBuf, toml::Table)> {
    if table.contains_key("workspace") {
        return Some((dir.to_owned(), table.clone()));
    }
    if let Some(workspace) = table.get("package").and_then(|package| package.get("workspace")).and_then(toml::Value::as_str) {
        return read_dependency_manifest(&dir.join(workspace).join("Cargo.toml"))
            .filter(|(dir, _)| dir.starts_with(root));
    }
    dir.ancestors().skip(1).take_while(|dir| dir.starts_with(root))
        .filter_map(|dir| read_dependency_manifest(&dir.join("Cargo.toml")))
        .find(|(_, table)| table.contains_key("workspace"))
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

struct CargoConfig {
    source: String,
    base: PathBuf,
    table: toml::Table,
}

fn cargo_configs(args: &[String], vars: &[(String, String)], cwd: &Path, root: &Path) -> Vec<CargoConfig> {
    let var = |name: &str| vars.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str()).filter(|value| !value.is_empty());
    let cargo_home = var("CARGO_HOME").map(|path| cwd.join(path))
        .unwrap_or_else(|| Path::new(var("HOME").unwrap_or("/")).join(".cargo"));
    let project_configs = cwd.ancestors().take_while(|path| path.starts_with(root)).map(|path| path.join(".cargo"));
    let mut read = std::collections::BTreeSet::new();
    let mut read_config = |file: PathBuf| {
        if !read.insert(file.clone()) || !io::file::metadata(&file).is_ok_and(|md| md.is_file() && md.len() <= 1 << 20) {
            return None;
        }
        let table = io::file::read_to_string(&file).ok()?.parse().ok()?;
        let base = file.parent()?.parent().unwrap_or(Path::new("/")).to_owned();
        Some(CargoConfig { source: format!("`{}`", file.display()), base, table })
    };

    // Read higher-precedence values first, including later --config arguments.
    let mut configs: Vec<_> = cargo_config_args(args).collect();
    configs.reverse();
    let mut configs: Vec<_> = configs.into_iter().filter_map(|config| {
        match config.parse::<toml::Table>() {
            Ok(table) => Some(CargoConfig { source: "`--config` CLI option".into(), base: cwd.to_owned(), table }),
            Err(_) => read_config(cwd.join(config)),
        }
    }).collect();
    configs.extend(project_configs.chain(std::iter::once(cargo_home)).filter_map(|dir| {
        let legacy = dir.join("config");
        read_config(if legacy.ring_is_file() { legacy } else { dir.join("config.toml") })
    }));
    configs
}

fn outside_config_dependency<'a>(root: &Path, configs: &'a [CargoConfig]) -> Option<(String, PathBuf, &'a str)> {
    let mut patches_seen = std::collections::BTreeSet::new();
    for config in configs {
        let outside = |path| io::file::canonicalize(config.base.join(path)).ok().filter(|path| !path.starts_with(root));
        let patches = config.table.get("patch").and_then(toml::Value::as_table).into_iter().flatten();
        for (registry, entries) in patches {
            for (name, entry) in entries.as_table().into_iter().flatten() {
                if let Some(path) = entry.get("path").and_then(toml::Value::as_str)
                    && patches_seen.insert((registry, name))
                    && let Some(resolved) = outside(path)
                {
                    return Some((name.clone(), resolved, &config.source));
                }
            }
        }
        let paths = config.table.get("paths").and_then(toml::Value::as_array).into_iter().flatten();
        if let Some(path) = paths.filter_map(toml::Value::as_str).find_map(outside) {
            let name = read_dependency_manifest(&path.join("Cargo.toml"))
                .and_then(|(_, table)| Some(table.get("package")?.get("name")?.as_str()?.to_owned()))
                .unwrap_or_else(|| path.file_name().unwrap_or(path.as_os_str()).to_string_lossy().into_owned());
            return Some((name, path, &config.source));
        }
    }
    None
}

fn ignored_compiler_wrappers(vars: &[(String, String)], configs: &[CargoConfig]) -> Vec<String> {
    let mut sources: Vec<_> = ["RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER"]
        .into_iter().filter(|name| vars.iter().any(|(key, value)| key == name && !value.is_empty())).map(str::to_owned).collect();
    for config in configs {
        sources.extend(wrapper_config_keys(&config.table).map(|key| format!("build.{key} in {}", config.source)));
    }
    sources
}

fn own_nextest_version() -> Option<String> {
    let out = io::process::Command::new("cargo-nextest")
        .args(["nextest", "--version"])
        .stdin(io::process::Stdio::null())
        .stderr(io::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    let mut words = text.lines().next()?.split_whitespace();
    (words.next()? == "cargo-nextest").then(|| words.next().map(String::from)).flatten()
}

fn real_cargo() -> Option<PathBuf> {
    if let Some(p) = io::env::var_os("RARGO_REAL_CARGO") {
        return Some(PathBuf::from(p));
    }
    let me = std::env::current_exe().ok().and_then(|p| io::file::canonicalize(p).ok());
    for dir in std::env::split_paths(&io::env::var_os("PATH")?) {
        let cand = dir.join("cargo");
        if !cand.ring_is_file() {
            continue;
        }
        if io::file::canonicalize(&cand).ok() == me {
            continue;
        }
        return Some(cand);
    }
    None
}

fn passthrough(args: &[OsString]) -> ! {
    match real_cargo() {
        Some(cargo) => {
            let err = io::process::Command::new(&cargo).args(args).exec();
            eprintln!("error: cannot run {}: {err}", cargo.display());
        }
        None => eprintln!("error: no `cargo` found on PATH to hand this command to"),
    }
    io::process::exit(101)
}

fn enter_namespace(binds: &[(String, String)]) -> std::io::Result<()> {
    use namespace::mount;
    let (uid, gid) = (io::process::uid(), io::process::gid());
    if namespace::unshare(libc::CLONE_NEWNS).is_err() {
        namespace::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS)?;
        io::file::write("/proc/self/setgroups", "deny")?;
        io::file::write("/proc/self/uid_map", format!("{uid} {uid} 1"))?;
        io::file::write("/proc/self/gid_map", format!("{gid} {gid} 1"))?;
    }
    mount(None, Path::new("/"), None, libc::MS_REC | libc::MS_PRIVATE, None)?;
    let entries: Vec<io::file::DirEntry> = io::file::read_dir("/")?.filter_map(|e| e.ok()).collect();
    let covered = ["/tmp", "/mnt", "/media", "/srv", "/opt", "/home", "/var"]
        .into_iter()
        .find(|d| Path::new(d).ring_is_dir() && !binds.iter().any(|(real, _)| Path::new(real).starts_with(d)))
        .ok_or_else(|| std::io::Error::other("no directory to build the namespace on"))?;
    let tmp_fd = io::file::File::open(covered)?;
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
            if let Ok(t) = io::file::read_link(&src) {
                let _ = io::file::symlink(t, &dst);
            }
        } else if ft.is_dir() {
            io::file::create_dir(&dst)?;
            let from = if src == new_root { PathBuf::from(format!("/proc/self/fd/{}", tmp_fd.as_raw_fd())) } else { src };
            mount(Some(&from), &dst, None, libc::MS_BIND | libc::MS_REC, None)?;
        } else if ft.is_file() {
            io::file::File::create(&dst)?;
            mount(Some(&src), &dst, None, libc::MS_BIND, None)?;
        }
    }
    for (real, at) in binds {
        let dst = new_root.join(at.trim_start_matches('/'));
        io::file::create_dir_all(&dst)?;
        let real_p = match Path::new(real).strip_prefix(covered) {
            Ok(rest) => PathBuf::from(format!("/proc/self/fd/{}", tmp_fd.as_raw_fd())).join(rest),
            Err(_) => PathBuf::from(real),
        };
        mount(Some(&real_p), &dst, None, libc::MS_BIND | libc::MS_REC, None)?;
    }
    namespace::chdir(new_root)?;
    namespace::pivot_root(Path::new("."), Path::new("."))?;
    namespace::umount(Path::new("."), libc::MNT_DETACH)
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

/// Replaces this process with the program the server chose. Entering the namespace changes this thread alone, so every step is this thread's own system call.
fn exec(e: ExecRequest) -> ! {
    io::on_this_thread(|| exec_here(e))
}

fn exec_here(mut e: ExecRequest) -> ! {
    e.binds.retain(|(real, _)| Path::new(real).ring_exists());
    if e.binds.is_empty() {
    } else if let Err(err) = enter_namespace(&e.binds) {
        if io::env::var_os("RARGO_QUIET_NS").is_none() {
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
    let mut cmd = io::process::Command::new(&e.program);
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
    if let Ok(f) = io::file::File::open(&file) {
        let _ = std::io::Read::read_to_end(&mut std::io::Read::take(f, 16 << 20), &mut head);
    }
    let Some(loader) = loader::interpreter(&head).map(|i| String::from_utf8_lossy(i).into_owned()).filter(|l| !Path::new(l).ring_exists()) else {
        eprintln!("error: could not execute process `{}` (never executed)\n\nCaused by:\n  {err}", e.program);
        io::process::exit(101)
    };
    let missing: Vec<String> = loader::needed(&head).into_iter().filter(|p| !Path::new(p).ring_exists()).collect();
    let home = io::env::var("HOME").ok();
    let nix_store = loader::nix_store_candidates(io::env::var("PATH").ok().as_deref(), home.as_deref()).into_iter().find(|p| p.ring_is_file());
    let failed = match (nix_store, &home) {
        (Some(nix_store), Some(home)) if !missing.is_empty() => {
            let roots = format!("{home}/.cache/rargo/nix");
            eprintln!("rargo: `{}` needs Nix's C library, which this machine does not have yet; fetching it once", e.program);
            let essential = loader::store_path(loader.as_bytes());
            let fetched = io::file::create_dir_all(&roots).map_err(|err| format!("creating {roots}: {err}")).and_then(|()| {
                loader::fetches(&missing, &roots).into_iter().zip(&missing).try_for_each(|(args, path)| {
                    let out = io::process::Command::new(&nix_store).args(&args).stdin(io::process::Stdio::null()).output();
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
    io::process::exit(101)
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
    fn a_kernel_that_refuses_io_uring_ends_the_command_with_one_error_line_naming_the_fix_and_the_fallback() {
        let why = io::refusal(&std::io::Error::from_raw_os_error(libc::EPERM)).expect("EPERM is a refusal");
        let text = super::ring_refused_text(why);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert!(lines[0].starts_with("error: rargo cannot run on this machine: io_uring is unavailable"), "{text}");
        assert!(lines[0].contains("seccomp=unconfined"), "{text}");
        assert!(lines[1].trim_start().starts_with("RARGO_FALLBACK=cargo"), "{text}");
        assert!(!text.contains("No such file") && !text.contains("without its daemon"), "{text}");
    }

    use super::*;

    #[test]
    fn path_dependencies_must_stay_inside_the_synced_worktree() {
        io::sim::test(|| {
            let temp = io::sim::tempdir();
            let root = temp.join("app");
            io::file::create_dir(&root).unwrap();
            io::file::create_dir(temp.join("dep")).unwrap();
            let inner = root.join("inner");
            io::file::create_dir(&inner).unwrap();
            let manifest = root.join("Cargo.toml");
            let inner_manifest = inner.join("Cargo.toml");
            let external = |path: &str| Some(("renamed".into(), path.into()));
            for section in [
                "dependencies",
                "dev-dependencies",
                "target.'cfg(unix)'.build-dependencies",
                "patch.crates-io",
            ] {
                io::file::write(&manifest, format!("[{section}]\nrenamed = {{ package = 'dep', path = '../dep' }}")).unwrap();
                assert_eq!(outside_path_dependency(&root, &manifest), external("../dep"));
            }

            io::file::write(&manifest, "[dependencies]\ninner = { path = 'inner' }\n[workspace.dependencies]\nrenamed = { path = '../dep' }").unwrap();
            io::file::write(&inner_manifest, "[dependencies]\nrenamed.workspace = true").unwrap();
            assert_eq!(outside_path_dependency(&root, &inner_manifest), external("../dep"));
            assert_eq!(outside_path_dependency(&root, &manifest), external("../dep"));
            io::file::write(&manifest, "[workspace]\nmembers = ['in*']\n[workspace.dependencies]\nrenamed = { path = '../dep' }").unwrap();
            assert_eq!(outside_path_dependency(&root, &manifest), external("../dep"));

            io::file::write(&inner_manifest, "[dependencies]\nrenamed = { path = '../../dep' }").unwrap();
            assert_eq!(outside_path_dependency(&root, &manifest), external("../../dep"));
            io::file::write(&inner_manifest, "[dependencies]\napp = { path = '..' }\nserde = '1'").unwrap();
            assert_eq!(outside_path_dependency(&root, &manifest), None);
        });
    }

    #[test]
    fn cargo_config_path_overrides_use_the_client_paths_and_sources() {
        io::sim::test(|| {
            let temp = io::sim::tempdir();
            let root = temp.join("app");
            let cwd = root.join("member");
            let home = temp.join("cargo-home");
            let dep = temp.join("checkout");
            for dir in [root.join(".cargo"), cwd.clone(), home.clone(), dep.clone()] {
                io::file::create_dir_all(dir).unwrap();
            }
            io::file::write(dep.join("Cargo.toml"), "[package]\nname = 'itoa'").unwrap();
            let vars = vec![("CARGO_HOME".into(), home.to_string_lossy().into_owned())];
            let project = root.join(".cargo/config.toml");
            let home_config = home.join("config");
            let cli_config = root.join(".cargo/extra.toml");
            io::file::write(&project, "paths = ['../checkout']").unwrap();

            let check = |args: &[String], source: &str| {
                let configs = cargo_configs(args, &vars, &cwd, &root);
                assert_eq!(outside_config_dependency(&root, &configs), Some(("itoa".into(), dep.clone(), source)));
            };
            check(&[], &format!("`{}`", project.display()));

            io::file::write(&project, "paths = ['member']").unwrap();
            let configs = cargo_configs(&[], &vars, &cwd, &root);
            assert_eq!(outside_config_dependency(&root, &configs), None);

            io::file::write(&home_config, format!("[patch.crates-io]\nitoa.path = '{}'", dep.display())).unwrap();
            check(&[], &format!("`{}`", home_config.display()));
            let args = ["--config".into(), "patch.crates-io.itoa.path = '.'".into()];
            let configs = cargo_configs(&args, &vars, &cwd, &root);
            assert_eq!(outside_config_dependency(&root, &configs), None);

            let args = ["--config=patch.crates-io.itoa.path = '../../checkout'".into()];
            check(&args, "`--config` CLI option");

            io::file::write(&cli_config, "[patch.crates-io]\nitoa.path = '../checkout'").unwrap();
            let args = ["--config".into(), "../.cargo/extra.toml".into()];
            check(&args, &format!("`{}`", cwd.join("../.cargo/extra.toml").display()));
        });
    }

    #[test]
    fn only_workspace_members_have_their_dev_paths_checked() {
        io::sim::test(|| {
            let temp = io::sim::tempdir();
            let root = temp.join("app");
            let vendor = root.join("vendor/foo");
            io::file::create_dir_all(&vendor).unwrap();
            io::file::create_dir(temp.join("testutil")).unwrap();
            let manifest = root.join("Cargo.toml");
            let vendor_manifest = vendor.join("Cargo.toml");
            let deps = "[dependencies]\nfoo = { path = 'vendor/foo' }";
            let excluded = format!("{deps}\n[workspace]\nexclude = ['vendor']");
            for section in ["dev-dependencies", "target.'cfg(unix)'.dev_dependencies"] {
                io::file::write(&vendor_manifest, format!("[{section}]\ntestutil = {{ path = '../../../testutil' }}")).unwrap();
                for app in [deps, &excluded] {
                    io::file::write(&manifest, app).unwrap();
                    assert_eq!(outside_path_dependency(&root, &manifest), None);
                }

                let external = Some(("testutil".into(), "../../../testutil".into()));
                for app in [
                    format!("{deps}\n[workspace]"),
                    "[workspace]\nmembers = ['vendor/*']".to_owned(),
                ] {
                    io::file::write(&manifest, app).unwrap();
                    assert_eq!(outside_path_dependency(&root, &manifest), external);
                }
            }

            io::file::write(&manifest, excluded).unwrap();
            io::file::write(&vendor_manifest, "[build-dependencies]\ntestutil = { path = '../../../testutil' }").unwrap();
            assert_eq!(
                outside_path_dependency(&root, &manifest),
                Some(("testutil".into(), "../../../testutil".into())),
            );
        });
    }

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
