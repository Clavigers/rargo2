mod daemon;
mod daemon_state;
mod git;
mod known;
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

use rargo_proto::secure;
use rargo_proto::{ExecRequest, LocalReply, LocalRequest};
use std::ffi::{CString, OsString};
use std::io::{IsTerminal, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

const ROUTED: &[&str] =
    &["build", "b", "check", "c", "clippy", "test", "t", "run", "r", "doc", "d", "bench", "rustc", "rustdoc", "nextest"];

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

pub struct Config {
    pub host: String,
    pub server: String,
    pub user: String,
    pub secret: Option<String>,
    pub psk: [u8; 32],
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

#[derive(Default)]
struct ConfigEnv {
    daemon: Option<String>,
    server: Option<String>,
    token: Option<String>,
    user: Option<String>,
    logname: Option<String>,
}

impl ConfigEnv {
    fn read() -> Self {
        Self {
            daemon: env_value("RARGO_DAEMON_CONFIG"),
            server: env_value("RARGO_SERVER"),
            token: env_value("RARGO_TOKEN"),
            user: env_value("USER"),
            logname: env_value("LOGNAME"),
        }
    }

    fn host(&self, path: &Path, text: Result<Option<&str>, String>) -> Result<(String, HostFrom), String> {
        if let Some(h) = &self.server {
            return Ok((h.clone(), HostFrom::Env));
        }
        if self.daemon.is_some() {
            return Err("the daemon was started without a server".into());
        }
        match text?.map(|text| project::parse_host(path, text)).transpose()?.flatten() {
            Some(h) => Ok((h, HostFrom::File(path.to_path_buf()))),
            None => Err(format!("no rargo server is named: run `rargo @rargo.rs` (or `rargo @HOST` for a server of your own), which writes {}", path.display())),
        }
    }

    fn finish(self, host: String, server: Result<String, String>, stored: Option<project::Credential>) -> Result<Config, String> {
        let server = server?;
        let (user, secret) = if let Some(token) = &self.token {
            let (id, secret) = project::split_token(token).ok_or("RARGO_TOKEN is not a token (an id, a dot and a secret)")?;
            (id.to_string(), Some(secret.to_string()))
        } else if let Some(c) = stored.filter(|_| self.daemon.is_none()) {
            (c.id, Some(c.secret))
        } else {
            (self.user.or(self.logname).unwrap_or_else(|| "user".into()), None)
        };
        let psk = secret.as_ref().map_or_else(secure::open_psk, |s| secure::psk_from_secret(s.as_bytes()));
        Ok(Config { host, server, user, secret, psk })
    }
}

pub fn load_config() -> Result<Config, String> {
    let input = ConfigEnv::read();
    let (host, _) = host_from(&input)?;
    let server = if input.daemon.is_some() { Ok(host.clone()) } else { project::resolve(&host) };
    let credential = if server.is_ok() && input.daemon.is_none() && input.token.is_none() { project::credential(&host) } else { None };
    input.finish(host, server, credential)
}

#[derive(Debug, PartialEq, Eq)]
enum HostFrom {
    Env,
    File(PathBuf),
}

fn host_from(input: &ConfigEnv) -> Result<(String, HostFrom), String> {
    let path = project::config_path();
    let text = if input.server.is_some() || input.daemon.is_some() { Ok(None) } else { project::read_config(&path) };
    input.host(&path, text.as_ref().map(|text| text.as_deref()).map_err(Clone::clone))
}

fn server_host() -> Result<(String, HostFrom), String> {
    host_from(&ConfigEnv { server: env_value("RARGO_SERVER"), ..ConfigEnv::default() })
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

fn is_loopback(server: &str) -> bool {
    let name = server.rsplit_once(':').map_or(server, |(h, _)| h);
    name == "localhost" || name.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
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

pub fn recent_commits(root: &Path, head: &git::Head, wt_cache: &Path) -> Vec<[u8; 20]> {
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
    let uid = unsafe { libc::getuid() };
    let dir = match std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d).join("rargo"),
        None => PathBuf::from(format!("/tmp/rargo-{uid}")),
    };
    dir.join(name)
}

fn stop_daemons(cfg: &Config, cache: &Path, req: &LocalRequest) {
    use std::os::unix::net::UnixStream;
    let mut sockets = Vec::new();
    if let Ok(rd) = std::fs::read_dir(cache.join("run")) {
        for ent in rd.flatten() {
            let path = ent.path();
            if path.extension().is_some_and(|ext| ext == "sock") {
                sockets.push(path);
            }
        }
    }
    let primary = daemon_socket(cfg, cache);
    if !sockets.iter().any(|path| path == &primary) {
        sockets.push(primary);
    }
    let bytes = rargo_proto::encode(req);
    for socket in sockets {
        let Ok(mut stream) = UnixStream::connect(&socket) else { continue };
        let _ = stream.write_all(&(bytes.len() as u32).to_le_bytes()).and_then(|_| stream.write_all(&bytes));
        let mut rest = Vec::new();
        let _ = stream.read_to_end(&mut rest);
    }
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

const TOKEN_COMMANDS: &[&str] = &["invite", "create", "tokens", "revoke", "allow", "disallow"];

pub fn mount_ns() -> String {
    std::fs::read_link("/proc/self/ns/mnt").map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()
}

fn at_host(host: &str) -> ! {
    match project::set_host(host) {
        Ok(path) => {
            eprintln!("{} now names `{host}` as your rargo server.", path.display());
            if host.contains(':') || project::resolve(host).is_ok() {
                if project::credential(host).is_none() && std::env::var_os("RARGO_TOKEN").is_none() {
                    eprintln!("Next: `rargo auth`, then `rargo build`.");
                }
            } else {
                eprintln!("warning: no rargo server answers for `{host}` right now (https://{host}/.well-known/rargo.json)");
            }
            std::process::exit(0)
        }
        Err(e) => die(&e),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct AuthArgs {
    action: &'static str,
    host: Option<String>,
    token: Option<String>,
}

fn parse_auth(args: &[OsString]) -> Result<Option<AuthArgs>, String> {
    let args: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    let mut what = "auth";
    let mut host: Option<String> = None;
    let mut token: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "status" if i == 0 => what = "status",
            "remove" if i == 0 => what = "remove",
            "--token" => {
                i += 1;
                token = Some(args.get(i).cloned().ok_or("`--token` needs the token")?);
            }
            "-h" | "--help" => {
                return Ok(None);
            }
            a if a.starts_with('-') => return Err(format!("unknown option `{a}` (see `rargo auth --help`)")),
            a if host.is_none() => host = Some(a.trim_start_matches('@').to_string()),
            a => return Err(format!("unexpected `{a}` (see `rargo auth --help`)")),
        }
        i += 1;
    }
    Ok(Some(AuthArgs { action: what, host, token }))
}

fn auth_command(args: &[OsString]) -> ! {
    let Some(AuthArgs { action: what, host, token }) = parse_auth(args).unwrap_or_else(|e| die(&e)) else {
        println!(
            "rargo auth [HOST]            get this machine a token for the project's server (or HOST), confirmed in a browser\n\
             rargo auth --token T [HOST]  store a token an administrator made (rargod token create)\n\
             rargo auth status            which server you build on, what named it, and as whom\n\
             rargo auth remove [HOST]     revoke the token for the project's server (or HOST) there and forget it here\n\
             \n\
             On the project's server, for an administrator (anyone may list and revoke their own tokens):\n\
             rargo auth invite [--user NAME] [--admin] [--days N] [--uses N] [--token-days N]\n\
             rargo auth create --user NAME [--admin] [--days N]\n\
             rargo auth tokens\n\
             rargo auth revoke ID-OR-USER\n\
             rargo auth allow GITHUB-LOGIN [--admin]\n\
             rargo auth disallow GITHUB-LOGIN"
        );
        std::process::exit(0)
    };
    let (host, from) = match host {
        Some(h) => (h, None),
        None => match server_host() {
            Ok((h, from)) => (h, Some(from)),
            Err(e) => die(&format!("{e}; or say `rargo auth HOST`")),
        },
    };
    if !project::valid_host(&host) {
        die(&format!("`{host}` is not a host"));
    }
    match what {
        "status" => {
            match &from {
                Some(HostFrom::Env) => println!("server named by RARGO_SERVER"),
                Some(HostFrom::File(path)) => println!("server named by {}", path.display()),
                None => {}
            }
            let server = project::resolve(&host).unwrap_or_else(|e| format!("unknown ({e})"));
            match project::credential(&host) {
                Some(c) if c.name.is_empty() => println!("{host} (server {server}): authenticated (token {})", c.id),
                Some(c) => println!("{host} (server {server}): authenticated as {} (token {})", c.name, c.id),
                None => println!("{host} (server {server}): not authenticated; run `rargo auth`"),
            }
            std::process::exit(0)
        }
        "remove" => {
            if project::credential(&host).is_none() {
                die(&format!("there is no token for {host} on this machine"));
            }
            let revoked = project::revoke_on_server(&host);
            if let Err(e) = project::forget(&host) {
                die(&e);
            }
            match revoked {
                Ok(true) => println!("revoked the token for {host} on the server and forgot it here"),
                Ok(false) => println!("forgot the token for {host} (the server did not know it as a token that still works)"),
                Err(e) => {
                    println!("forgot the token for {host} on this machine");
                    eprintln!("warning: the server could not be told to revoke it ({e}); it keeps working for whoever has a copy until an administrator runs `rargod token revoke`");
                }
            }
            std::process::exit(0)
        }
        _ => {}
    }
    if let Some(token) = token {
        let stored = project::holder(&host, &token).and_then(|name| match name {
            Some(name) => project::store_token(&host, &token, &name).map(|()| name),
            None => Err(format!("{host}: {}", rargo_proto::REFUSED)),
        });
        match stored {
            Ok(name) => {
                println!("stored the token for {host}, held by {name}");
                std::process::exit(0)
            }
            Err(e) => die(&format!("rargo auth: {e}")),
        }
    }
    match project::auth(&host) {
        Ok(name) => {
            println!("Authenticated to {host} as {name}. `rargo build` now builds there.");
            std::process::exit(0)
        }
        Err(e) => die(&format!("rargo auth: {e}")),
    }
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
            let cfg2 = Config { host: cfg.host.clone(), server: cfg.server.clone(), user: cfg.user.clone(), secret: cfg.secret.clone(), psk: cfg.psk };
            daemon::inline(cfg2, cache, req, |reply| relay(reply, &cfg, raw));
            die("the command ended without a result");
        }
    };
    let bytes = rargo_proto::encode(&req);
    if stream.write_all(&(bytes.len() as u32).to_le_bytes()).and_then(|_| stream.write_all(&bytes)).is_err() {
        die("the rargo daemon closed the connection");
    }
    loop {
        let mut len = [0u8; 4];
        if stream.read_exact(&mut len).is_err() {
            die("the rargo daemon went away while the command was running");
        }
        let mut buf = vec![0u8; u32::from_le_bytes(len) as usize];
        if stream.read_exact(&mut buf).is_err() {
            die("the rargo daemon went away while the command was running");
        }
        match rargo_proto::decode::<LocalReply>(&buf) {
            Ok(reply) => relay(reply, &cfg, raw),
            Err(_) => die("the rargo daemon said something this program does not understand"),
        }
    }
}

fn main() {
    let raw: Vec<OsString> = std::env::args_os().skip(1).collect();
    if std::env::var_os("RARGO_BYPASS").is_some() {
        passthrough(&raw);
    }
    let cargo_link = std::env::args_os().next().is_some_and(|arg| Path::new(&arg).file_name().is_some_and(|name| name == "cargo"));
    if !cargo_link && raw.first().is_some_and(|a| a == "status") {
        match raw.get(1).and_then(|arg| arg.to_str()) {
            Some("--help" | "-h") if raw.len() == 2 => println!("Usage: rargo status\n\nShow your server, account, daemon, mount and versions."),
            None if raw.len() == 1 => status::run(),
            _ => die("usage: rargo status"),
        }
        return;
    }
    if !cargo_link && raw.len() == 1 && raw.first().is_some_and(|a| a == "--help" || a == "-h") {
        println!("rargo status    Show your server, account, daemon, mount and versions.\n");
    }
    if raw.first().is_some_and(|a| a == "--rargo-version") {
        println!("rargo {} (protocol {})", env!("CARGO_PKG_VERSION"), rargo_proto::PROTO_VERSION);
        return;
    }
    if let Some(host) = raw.first().and_then(|a| a.to_str()).and_then(|a| a.strip_prefix('@')) {
        at_host(host);
    }
    let token_command = raw.first().is_some_and(|a| a == "auth")
        && raw.get(1).and_then(|a| a.to_str()).is_some_and(|a| TOKEN_COMMANDS.contains(&a));
    if raw.first().is_some_and(|a| a == "auth") && !token_command {
        auth_command(&raw[1..]);
    }
    if raw.first().is_some_and(|a| a == "--rargo-daemon") {
        match raw.get(1) {
            Some(socket) => daemon::serve(Path::new(socket)),
            None => die("usage: rargo --rargo-daemon SOCKET"),
        }
    }
    tmp::install_handlers();
    let vars = std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?))).collect();
    let route = prepare_route(&raw, vars, token_command);
    let (args, mut vars, kind) = match route {
        Route::Local { release } => {
            if release {
                release_output();
            }
            passthrough(&raw)
        }
        Route::Usage(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
        Route::Remote { args, vars, kind } => (args, vars, kind),
    };
    if subcommand(&args) == Some("nextest")
        && !vars.iter().any(|(k, _)| k == "RARGO_NEXTEST_VERSION")
        && let Some(v) = own_nextest_version()
    {
        vars.push(("RARGO_NEXTEST_VERSION".into(), v));
    }
    let cfg = load_config().unwrap_or_else(|e| die(&e));
    if cfg.secret.is_none() && !is_loopback(&cfg.server) && matches!(kind, "admin" | "drift" | "build") {
        die(&format!("this machine is not authenticated to {}: run `rargo auth`", cfg.host));
    }
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
    if kind == "stop" {
        stop_daemons(&cfg, &cache, &req);
        std::process::exit(0);
    }
    through_daemon(cfg, cache, req, &raw)
}

#[derive(Debug, PartialEq, Eq)]
enum Route {
    Local { release: bool },
    Usage(String),
    Remote { args: Vec<String>, vars: Vec<(String, String)>, kind: &'static str },
}

fn prepare_route(raw: &[OsString], mut vars: Vec<(String, String)>, token_command: bool) -> Route {
    let Ok(mut args) = raw.iter().map(|a| a.clone().into_string()).collect::<Result<Vec<_>, _>>() else {
        return Route::Local { release: false };
    };
    if let Some(tc) = args.first().and_then(|a| a.strip_prefix('+')).map(str::to_owned) {
        args.remove(0);
        vars.retain(|(k, _)| k != "RUSTUP_TOOLCHAIN");
        vars.push(("RUSTUP_TOOLCHAIN".into(), tc));
    }
    let admin = match args.first().map(|s| s.as_str()) {
        Some("auth") if token_command => Some("__token"),
        Some("--rargo-status") => Some("__status"),
        Some("--rargo-settle") => Some("__settle"),
        Some("--rargo-gc") => Some("__gc"),
        Some("--rargo-incr-stats") => Some("__incr"),
        Some("--rargo-compact") => Some("__compact"),
        Some("--rargo-export") => Some("__export"),
        Some("--rargo-import") => Some("__import"),
        _ => None,
    };
    let drift = args.first().map(|s| s.as_str()) == Some("--rargo-drift-check");
    let daemon_cmd = match args.first().map(|s| s.as_str()) {
        Some("--rargo-daemon-status") => Some("status"),
        Some("--rargo-daemon-stop") => Some("stop"),
        Some("--rargo-daemon-sync") => Some("sync"),
        _ => None,
    };
    if daemon_cmd.is_some() {
        args.clear();
    } else if let Some(cmd) = admin {
        let mut a = vec![cmd.to_string()];
        if matches!(cmd, "__export" | "__import") {
            match args.get(1) {
                Some(dir) => a.push(dir.clone()),
                None => {
                    return Route::Usage(format!("usage: rargo {} DIRECTORY-ON-THE-SERVER", args[0]));
                }
            }
        }
        if cmd == "__token" {
            let what = if args[1] == "tokens" { "list".to_string() } else { args[1].clone() };
            a.push(what);
            a.extend(args[2..].iter().cloned());
        }
        args = a;
    } else if drift {
        args = vec!["__drift".into()];
    } else {
        match subcommand(&args) {
            Some(cmd) if ROUTED.contains(&cmd) => {}
            Some("clean") => {
                let read_only = args.iter().take_while(|arg| *arg != "--").any(|arg| {
                    matches!(arg.as_str(), "--dry-run" | "--help")
                        || arg.strip_prefix('-').is_some_and(|short| {
                            short.chars().all(|flag| matches!(flag, 'h' | 'n' | 'q' | 'v'))
                                && short.contains(['h', 'n'])
                        })
                });
                return Route::Local { release: !read_only };
            }
            _ => return Route::Local { release: false },
        }
        if subcommand(&args) != Some("nextest")
            && args.iter().take_while(|a| *a != "--").any(|a| a == "--help" || a == "-h" || a == "--version" || a == "-V")
        {
            return Route::Local { release: false };
        }
        if args.iter().take_while(|arg| *arg != "--").any(|arg| arg == "--unit-graph") {
            return Route::Usage("error: rargo does not support --unit-graph for remote builds; use local Cargo to get a unit graph with local paths".into());
        }
    }
    let kind = if let Some(daemon) = daemon_cmd {
        daemon
    } else if admin.is_some() {
        "admin"
    } else if drift {
        "drift"
    } else {
        "build"
    };
    Route::Remote { args, vars, kind }
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

fn subcommand(args: &[String]) -> Option<&str> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--" {
            return None;
        }
        if matches!(a.as_str(), "--color" | "--config" | "-C" | "-Z" | "--explain") {
            it.next();
            continue;
        }
        if a.starts_with('-') {
            continue;
        }
        return Some(a);
    }
    None
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
    let loader = std::fs::File::open(&file).ok().and_then(|f| {
        let mut head = Vec::new();
        std::io::Read::read_to_end(&mut std::io::Read::take(f, 64 << 10), &mut head).ok()?;
        elf_interpreter(&head).map(|i| String::from_utf8_lossy(i).into_owned())
    });
    match loader.filter(|l| !Path::new(l).exists()) {
        Some(loader) => eprint!("{}", missing_loader_text(&e.program, &loader)),
        None => eprintln!("error: could not execute process `{}` (never executed)\n\nCaused by:\n  {err}", e.program),
    }
    std::process::exit(101)
}

fn elf_interpreter(head: &[u8]) -> Option<&[u8]> {
    let bytes = |at: usize, n: usize| head.get(at..at.checked_add(n)?);
    let u16_at = |at: usize| Some(u16::from_le_bytes(bytes(at, 2)?.try_into().ok()?));
    let u32_at = |at: usize| Some(u32::from_le_bytes(bytes(at, 4)?.try_into().ok()?));
    let u64_at = |at: usize| Some(u64::from_le_bytes(bytes(at, 8)?.try_into().ok()?));
    if head.get(..6)? != b"\x7fELF\x02\x01" {
        return None;
    }
    let (phoff, phentsize, phnum) = (u64_at(0x20)? as usize, u16_at(0x36)? as usize, u16_at(0x38)? as usize);
    (0..phnum).find_map(|i| {
        let ph = phoff.checked_add(i.checked_mul(phentsize)?)?;
        if u32_at(ph)? != 3 {
            return None;
        }
        let (offset, size) = (u64_at(ph.checked_add(8)?)? as usize, u64_at(ph.checked_add(32)?)? as usize);
        let interp = head.get(offset..offset.checked_add(size)?)?;
        Some(interp.strip_suffix(b"\0").unwrap_or(interp))
    })
}

fn missing_loader_text(program: &str, loader: &str) -> String {
    let store = loader.strip_prefix("/nix/store/").and_then(|rest| rest.split('/').next()).map(|name| format!("/nix/store/{name}"));
    let fetch = match store {
        Some(path) => format!(
            "       The build server links with Nix's C library. Install Nix (https://nixos.org/download), then fetch it once with:\n       \
             nix-store --realise {path} --add-root ~/.cache/rargo/nix-libc\n"
        ),
        None => String::new(),
    };
    format!("error: `{program}` cannot start here: it needs the program loader {loader}, which this machine does not have.\n{fetch}")
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
    fn a_program_whose_loader_is_missing_is_explained_with_the_command_that_fetches_it() {
        let elf = |interp: &[u8], phnum: u16| {
            let mut f = vec![0u8; 0x200];
            f[..6].copy_from_slice(b"\x7fELF\x02\x01");
            f[0x20..0x28].copy_from_slice(&0x40u64.to_le_bytes());
            f[0x36..0x38].copy_from_slice(&56u16.to_le_bytes());
            f[0x38..0x3a].copy_from_slice(&phnum.to_le_bytes());
            let second = 0x40 + 56;
            f[second..second + 4].copy_from_slice(&3u32.to_le_bytes());
            f[second + 8..second + 16].copy_from_slice(&0x100u64.to_le_bytes());
            f[second + 32..second + 40].copy_from_slice(&(interp.len() as u64 + 1).to_le_bytes());
            f[0x100..0x100 + interp.len()].copy_from_slice(interp);
            f
        };
        let nix = b"/nix/store/r3hvhzl5fxhrgn0rfy5g7rda17702pj4-glibc-2.44-57/lib/ld-linux-x86-64.so.2";
        assert_eq!(super::elf_interpreter(&elf(nix, 2)), Some(&nix[..]));
        assert_eq!(super::elf_interpreter(&elf(b"/lib64/ld-linux-x86-64.so.2", 2)), Some(&b"/lib64/ld-linux-x86-64.so.2"[..]));
        assert_eq!(super::elf_interpreter(&elf(nix, 1)), None, "a program without PT_INTERP is static");
        let mut far = elf(nix, 2);
        far[0x20..0x28].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(super::elf_interpreter(&far), None, "headers past what was read are not guessed");
        assert_eq!(super::elf_interpreter(b"#!/bin/sh\n"), None);
        assert_eq!(super::elf_interpreter(&elf(nix, 2)[..0x30]), None);

        let text = super::missing_loader_text("target/debug/app", std::str::from_utf8(nix).unwrap());
        assert_eq!(
            text,
            "error: `target/debug/app` cannot start here: it needs the program loader /nix/store/r3hvhzl5fxhrgn0rfy5g7rda17702pj4-glibc-2.44-57/lib/ld-linux-x86-64.so.2, which this machine does not have.\n       \
             The build server links with Nix's C library. Install Nix (https://nixos.org/download), then fetch it once with:\n       \
             nix-store --realise /nix/store/r3hvhzl5fxhrgn0rfy5g7rda17702pj4-glibc-2.44-57 --add-root ~/.cache/rargo/nix-libc\n"
        );
        assert!(!super::missing_loader_text("app", "/opt/ld.so").contains("nix-store"));
    }

    #[test]
    fn configuration_precedence_and_errors_are_deterministic() {
        let path = Path::new("/config/rargo/config.toml");
        let env = ConfigEnv { server: Some("env:1".into()), ..ConfigEnv::default() };
        assert_eq!(env.host(path, Err("bad file".into())).unwrap(), ("env:1".into(), HostFrom::Env));
        let env = ConfigEnv::default();
        assert_eq!(env.host(path, Ok(Some("host = \"file:1\""))).unwrap(), ("file:1".into(), HostFrom::File(path.into())));
        assert_eq!(env.host(path, Ok(Some("host = 7"))).unwrap_err(), "/config/rargo/config.toml: `host` is not a string");
        assert_eq!(env.host(path, Err("bad file".into())).unwrap_err(), "bad file");
        for text in [None, Some("other = 7")] {
            assert_eq!(env.host(path, Ok(text)).unwrap_err(), "no rargo server is named: run `rargo @rargo.rs` (or `rargo @HOST` for a server of your own), which writes /config/rargo/config.toml");
        }
        let daemon = ConfigEnv { daemon: Some("1".into()), ..ConfigEnv::default() };
        assert_eq!(daemon.host(path, Ok(Some("host = \"file:1\""))).unwrap_err(), "the daemon was started without a server");

        let stored = project::Credential { id: "stored".into(), secret: "0123456789abcdef".into(), name: String::new() };
        for (daemon, token, user, logname, has_stored, expected) in [
            (None, Some("explicit.0123456789abcdef"), Some("u"), Some("l"), true, "explicit"),
            (None, None, Some("u"), Some("l"), true, "stored"),
            (Some("1"), None, Some("u"), Some("l"), true, "u"),
            (None, None, None, Some("l"), false, "l"),
            (None, None, None, None, false, "user"),
        ] {
            let env = ConfigEnv { daemon: daemon.map(str::to_owned), token: token.map(str::to_owned), user: user.map(str::to_owned),
                logname: logname.map(str::to_owned), server: None };
            let cfg = env.finish("host".into(), Ok("server:1".into()), has_stored.then(|| stored.clone())).unwrap();
            assert_eq!(cfg.user, expected);
            assert_eq!(cfg.secret.is_some(), matches!(expected, "explicit" | "stored"));
            assert_eq!(cfg.server, "server:1");
        }
        let env = ConfigEnv { token: Some("bad".into()), ..ConfigEnv::default() };
        assert_eq!(env.finish("host".into(), Ok("server:1".into()), Some(stored)).err().unwrap(),
            "RARGO_TOKEN is not a token (an id, a dot and a secret)");
        assert_eq!(ConfigEnv::default().finish("host".into(), Err("discovery failed".into()), None).err().unwrap(),
            "discovery failed");
    }

    #[test]
    fn routing_and_auth_parse_without_io() {
        let args = |words: &[&str]| words.iter().copied().map(OsString::from).collect::<Vec<_>>();
        for (words, release) in [
            (&["clean"][..], true),
            (&["clean", "-vn"], false),
            (&["clean", "--help"], false),
            (&["check", "--version"], false),
            (&["metadata"], false),
        ] {
            assert_eq!(prepare_route(&args(words), vec![], false), Route::Local { release });
        }
        let vars = vec![("RUSTUP_TOOLCHAIN".into(), "stable".into()), ("RUSTFLAGS".into(), "-Wwarn".into())];
        assert_eq!(prepare_route(&args(&["+nightly", "check", "--", "-h"]), vars, false),
            Route::Remote { args: vec!["check".into(), "--".into(), "-h".into()],
                vars: vec![("RUSTFLAGS".into(), "-Wwarn".into()), ("RUSTUP_TOOLCHAIN".into(), "nightly".into())], kind: "build" });
        assert_eq!(prepare_route(&args(&["--rargo-export"]), vec![], false),
            Route::Usage("usage: rargo --rargo-export DIRECTORY-ON-THE-SERVER".into()));
        assert_eq!(prepare_route(&args(&["auth", "tokens"]), vec![], true),
            Route::Remote { args: vec!["__token".into(), "list".into()], vars: vec![], kind: "admin" });
        for (words, error) in [
            (&["--token"][..], "`--token` needs the token"),
            (&["-x"], "unknown option `-x` (see `rargo auth --help`)"),
            (&["a", "b"], "unexpected `b` (see `rargo auth --help`)"),
        ] {
            assert_eq!(parse_auth(&args(words)).unwrap_err(), error);
        }
        assert_eq!(parse_auth(&args(&["-h"])).unwrap(), None);
        assert_eq!(parse_auth(&args(&["remove", "@build"])).unwrap(),
            Some(AuthArgs { action: "remove", host: Some("build".into()), token: None }));
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

    #[test]
    fn subcommands_are_found_behind_global_options() {
        let a = |v: &[&str]| -> Vec<String> { v.iter().map(|s| s.to_string()).collect() };
        assert_eq!(subcommand(&a(&["--color", "always", "build"])), Some("build"));
        assert_eq!(subcommand(&a(&["-q", "test", "--", "x"])), Some("test"));
        assert_eq!(subcommand(&a(&["--", "build"])), None);
    }
}
