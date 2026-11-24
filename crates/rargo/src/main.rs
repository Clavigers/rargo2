//! rargo: the client. It syncs the worktree and the environment to the
//! build server, where everything is analysed, scheduled and built, and
//! brings back output, the artifacts that were asked for, and files the
//! build wrote into the worktree. Commands that are not builds are handed
//! to the real cargo unchanged.
//!
//! The work is done by a daemon (`daemon`), one per user and server on the
//! machine, which this program starts when none is running and talks to
//! over a Unix socket: the daemon keeps the link to the server and follows
//! the worktrees, so a command itself only passes its arguments and
//! environment along and relays what comes back.
//!
//! When this binary is installed under the name `cargo` (the shim), agents
//! that type `cargo build` get rargo without knowing.

mod daemon;
mod git;
mod known;
mod mux;
mod outfs;
mod place;
mod project;
mod report;
mod scan;
mod tmp;
mod watch;

use rargo_proto::secure;
use rargo_proto::{ExecRequest, LocalReply, LocalRequest};
use std::ffi::{CString, OsString};
use std::io::{IsTerminal, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

/// Subcommands the server handles.
const ROUTED: &[&str] =
    &["build", "b", "check", "c", "clippy", "test", "t", "run", "r", "doc", "d", "bench", "rustc", "rustdoc", "nextest"];

/// Writes a file by renaming a complete temporary file into place.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = path.with_file_name(format!("{}{}-{name}", tmp::BESIDE, std::process::id()));
    let _watched = tmp::watch(&tmp);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/".into()))
}

/// Where the server is and how to prove who we are there.
///
/// The server: `RARGO_SERVER`, else the host the project names in its
/// `Cargo.toml` (`rargo @host`), else a server on this machine. A bare
/// name is asked where its server is.
/// Who we are: `RARGO_TOKEN`, else the token `rargo auth` stored for that
/// host. A server on this machine that issues no tokens asks for none.
pub struct Config {
    /// The host as it is written (`rargo.rs`, `build.example:7459`).
    pub host: String,
    /// The server's address.
    pub server: String,
    pub user: String,
    pub secret: Option<String>,
    pub psk: [u8; 32],
}

pub fn load_config() -> Result<Config, String> {
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    // A daemon is told all of it by the command that starts it: it has no
    // project around it to look into.
    let local_name = || env("USER").or_else(|| env("LOGNAME")).unwrap_or_else(|| "user".into());
    let from_token = |token: &str| -> Result<(String, Option<String>), String> {
        let (id, secret) = project::split_token(token).ok_or("RARGO_TOKEN is not a token (an id, a dot and a secret)")?;
        Ok((id.to_string(), Some(secret.to_string())))
    };
    if env("RARGO_DAEMON_CONFIG").is_some() {
        let server = env("RARGO_SERVER").ok_or("the daemon was started without a server")?;
        let (user, secret) = match env("RARGO_TOKEN") {
            Some(t) => from_token(&t)?,
            None => (local_name(), None),
        };
        let psk = secret.as_ref().map_or_else(secure::open_psk, |s| secure::psk_from_secret(s.as_bytes()));
        return Ok(Config { host: server.clone(), server, user, secret, psk });
    }
    let cwd = std::env::current_dir().ok();
    let host = env("RARGO_SERVER").or_else(|| cwd.as_deref().and_then(project::project_host)).unwrap_or_else(|| "127.0.0.1:7459".into());
    let server = project::resolve(&host)?;
    let (user, secret) = if let Some(token) = env("RARGO_TOKEN") {
        from_token(&token)?
    } else if let Some(c) = project::credential(&host) {
        (c.id, Some(c.secret))
    } else {
        (local_name(), None)
    };
    let psk = secret.as_ref().map_or_else(secure::open_psk, |s| secure::psk_from_secret(s.as_bytes()));
    Ok(Config { host, server, user, secret, psk })
}

/// Whether a server at this address accepts anyone on its machine (one
/// started without keys listens on the loopback interface only).
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
    // SAFETY: the buffer is valid for its length.
    if unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } != 0 {
        return "unknown".into();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// The directory that is synced: the git worktree the current directory is
/// in; without git, the outermost directory with a `Cargo.toml`.
pub fn find_root(cwd: &Path) -> Option<PathBuf> {
    let ancestors: Vec<&Path> = cwd.ancestors().collect();
    if let Some(d) = ancestors.iter().find(|d| d.join(".git").exists()) {
        return Some(d.to_path_buf());
    }
    ancestors.iter().rev().find(|d| d.join("Cargo.toml").is_file()).map(|d| d.to_path_buf())
}

/// The parts of the project's policy the client acts on.
#[derive(Default)]
pub struct Policy {
    pub pass: Vec<String>,
    pub exclude: Vec<String>,
    /// `[sync] git`: what the sandbox gets as `.git`.
    pub git: git::Mode,
    /// `[sync] gitignored = false`: what git ignores is not synced, except
    /// what `include` names.
    pub gitignored: bool,
    pub include: Vec<String>,
}

pub fn load_policy(root: &Path, start: &Path) -> Policy {
    // A malformed table is the server's to report, with the build.
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

/// HEAD and its recent first-parent ancestors: a hint for the server's
/// choice of incremental state. Asked of git once per HEAD and remembered.
pub fn recent_commits(root: &Path, head: &git::Head, wt_cache: &Path) -> Vec<[u8; 20]> {
    let head = &head.commit;
    let memo = wt_cache.join("commits");
    let parse = |text: &str| -> Vec<[u8; 20]> { text.lines().filter_map(parse_hex20).collect() };
    if let Ok(text) = std::fs::read_to_string(&memo) {
        if text.lines().next() == Some(head.as_str()) {
            return parse(&text);
        }
    }
    let listed = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-list", "--first-parent", "-n", "64", head])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_else(|| format!("{head}\n"));
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

/// Where the daemon for this server, user and program listens. The name
/// covers everything a daemon is started with, so a changed configuration
/// or a new version of the program gets a daemon of its own and the old
/// one goes when it has been idle.
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
    // Beside the cache if the path fits a socket address, else in the
    // user's runtime directory.
    let in_cache = cache.join("run").join(&name);
    if in_cache.as_os_str().len() < 100 {
        return in_cache;
    }
    // SAFETY: no arguments, cannot fail.
    let uid = unsafe { libc::getuid() };
    let dir = match std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d).join("rargo"),
        None => PathBuf::from(format!("/tmp/rargo-{uid}")),
    };
    dir.join(name)
}

/// Makes the directory the socket lives in: ours alone.
fn private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    match std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let md = std::fs::symlink_metadata(dir)?;
    // SAFETY: no arguments, cannot fail.
    if !md.is_dir() || md.uid() != unsafe { libc::getuid() } {
        return Err(std::io::Error::other(format!("{} is not a directory of this user", dir.display())));
    }
    if md.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Starts a daemon on `socket`, detached from this command: a process of
/// its own session whose parent is gone, with its output in a log file.
fn start_daemon(socket: &Path, cache: &Path, cfg: &Config) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    std::fs::create_dir_all(cache)?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(cache.join("daemon.log"))?;
    let mut cmd = std::process::Command::new(exe);
    // The daemon gets what this command worked out (it runs in no project).
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
    // SAFETY: only async-signal-safe calls between fork and exec. The
    // child forks once more and exits, so the daemon is nobody's child
    // and this command (which may turn into the user's program) never
    // has to wait for it.
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

/// Connects to the daemon, starting one if there is none.
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

/// `rargo auth` commands that the server answers.
const TOKEN_COMMANDS: &[&str] = &["invite", "create", "tokens", "revoke", "allow", "disallow"];

/// This process's mount namespace, as the kernel names it.
pub fn mount_ns() -> String {
    std::fs::read_link("/proc/self/ns/mnt").map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Commands about the local daemon need no server.
fn kind_needs_server(daemon_cmd: Option<&str>) -> bool {
    daemon_cmd.is_none()
}

/// `rargo @host`.
fn at_host(host: &str) -> ! {
    let cwd = std::env::current_dir().unwrap_or_else(|e| die(&format!("cannot determine the current directory: {e}")));
    match project::set_host(&cwd, host) {
        Ok(path) => {
            eprintln!("{} now names `{host}` as this project's rargo server.", path.display());
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

/// `rargo auth [HOST]`, `rargo auth status`, `rargo auth remove [HOST]`,
/// `rargo auth --token TOKEN [HOST]`.
fn auth_command(args: &[OsString]) -> ! {
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
                token = Some(args.get(i).cloned().unwrap_or_else(|| die("`--token` needs the token")));
            }
            "-h" | "--help" => {
                println!(
                    "rargo auth [HOST]            get this machine a token for the project's server (or HOST), confirmed in a browser\n\
                     rargo auth --token T [HOST]  store a token an administrator made (rargod token create)\n\
                     rargo auth status            which server this project builds on, and as whom\n\
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
            }
            a if a.starts_with('-') => die(&format!("unknown option `{a}` (see `rargo auth --help`)")),
            a if host.is_none() => host = Some(a.trim_start_matches('@').to_string()),
            a => die(&format!("unexpected `{a}` (see `rargo auth --help`)")),
        }
        i += 1;
    }
    let host = host
        .or_else(|| std::env::var("RARGO_SERVER").ok().filter(|v| !v.is_empty()))
        .or_else(|| std::env::current_dir().ok().as_deref().and_then(project::project_host))
        .unwrap_or_else(|| die("which server? This project names none: run `rargo @host` first, or say `rargo auth HOST`"));
    if !project::valid_host(&host) {
        die(&format!("`{host}` is not a host"));
    }
    match what {
        "status" => {
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
            // Ended on the server first, so that a copy of it kept
            // anywhere stops working too; forgotten here either way.
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
        // Asked of the server first: a token it does not admit is not kept.
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

/// Asks a running daemon to give this worktree's output directory back to
/// plain files.
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

/// Acts on one thing the daemon says. Returns only if more is to come.
fn relay(reply: LocalReply, cfg: &Config, raw: &[OsString]) {
    match reply {
        LocalReply::Stdout(b) => {
            let mut o = std::io::stdout().lock();
            if o.write_all(&b).and_then(|_| o.flush()).is_err() {
                // Downstream closed (e.g. `| head`): stop like cargo does.
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
            eprintln!("error: cannot reach the rargo server at {}: {why}", cfg.server);
            eprintln!("       (the server is named by `rargo @host` in the project, or by RARGO_SERVER; RARGO_FALLBACK=cargo uses plain cargo instead)");
            std::process::exit(101);
        }
    }
}

/// Hands the request to the daemon and relays what it says until the
/// command is over.
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
    // The drift check runs stock cargo; if this shim is the `cargo` it
    // finds, step aside.
    if std::env::var_os("RARGO_BYPASS").is_some() {
        passthrough(&raw);
    }
    // The daemon handles signals itself (it has directories to unmount).
    if raw.first().is_some_and(|a| a == "--rargo-version") {
        // rargo's own version (`--version` is cargo's and is passed on).
        println!("rargo {} (protocol {})", env!("CARGO_PKG_VERSION"), rargo_proto::PROTO_VERSION);
        return;
    }
    // `rargo @host` names the project's server; `rargo auth` gets this
    // machine a token for it. Neither is a name cargo uses.
    if let Some(host) = raw.first().and_then(|a| a.to_str()).and_then(|a| a.strip_prefix('@')) {
        at_host(host);
    }
    // The token commands of an administrator (and a user's own tokens) are
    // requests to the server like any other; the rest of `rargo auth` is
    // this machine's.
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
    let mut args: Vec<String> = match raw.iter().map(|a| a.clone().into_string()).collect() {
        Ok(a) => a,
        // cargo itself rejects non-UTF-8 arguments for build commands.
        Err(_) => passthrough(&raw),
    };
    let mut vars: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect();
    // `rargo +nightly build` selects a toolchain like the rustup proxy does.
    if let Some(tc) = args.first().and_then(|a| a.strip_prefix('+')).map(|s| s.to_string()) {
        args.remove(0);
        vars.retain(|(k, _)| k != "RUSTUP_TOOLCHAIN");
        vars.push(("RUSTUP_TOOLCHAIN".into(), tc));
    }
    // Requests about the server itself need no worktree.
    let admin = match args.first().map(|s| s.as_str()) {
        Some("auth") if token_command => Some("__token"),
        Some("--rargo-status") => Some("__status"),
        Some("--rargo-gc") => Some("__gc"),
        Some("--rargo-incr-stats") => Some("__incr"),
        Some("--rargo-compact") => Some("__compact"),
        Some("--rargo-export") => Some("__export"),
        Some("--rargo-import") => Some("__import"),
        _ => None,
    };
    let drift = args.first().map(|s| s.as_str()) == Some("--rargo-drift-check");
    // Requests about the daemon on this machine.
    let daemon_cmd = match args.first().map(|s| s.as_str()) {
        Some("--rargo-daemon-status") => Some("status"),
        Some("--rargo-daemon-stop") => Some("stop"),
        _ => None,
    };
    if daemon_cmd.is_some() {
        args.clear();
    } else if let Some(cmd) = admin {
        let mut a = vec![cmd.to_string()];
        if matches!(cmd, "__export" | "__import") {
            match args.get(1) {
                // A directory on the server.
                Some(dir) => a.push(dir.clone()),
                None => {
                    eprintln!("usage: rargo {} DIRECTORY-ON-THE-SERVER", args[0]);
                    std::process::exit(2);
                }
            }
        }
        if cmd == "__token" {
            // `tokens` is what the server calls `list`.
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
            // `cargo clean` removes the output directory, which it cannot
            // while the daemon shows results in it: the daemon lets go of
            // it first.
            Some("clean") => {
                release_output();
                passthrough(&raw)
            }
            _ => passthrough(&raw),
        }
        // Asking for help or version is cargo's business, except for
        // nextest, which is the server's.
        if subcommand(&args) != Some("nextest")
            && args.iter().take_while(|a| *a != "--").any(|a| a == "--help" || a == "-h" || a == "--version" || a == "-V")
        {
            passthrough(&raw);
        }
    }
    let cfg = load_config().unwrap_or_else(|e| die(&e));
    if cfg.secret.is_none() && !is_loopback(&cfg.server) && kind_needs_server(daemon_cmd) {
        die(&format!("this machine is not authenticated to {}: run `rargo auth`", cfg.host));
    }
    let cwd = match std::env::current_dir().and_then(|d| d.canonicalize()) {
        Ok(d) => d,
        Err(e) => die(&format!("cannot determine the current directory: {e}")),
    };
    let kind = if daemon_cmd.is_some() {
        daemon_cmd.unwrap()
    } else if admin.is_some() {
        "admin"
    } else if drift {
        "drift"
    } else {
        "build"
    };
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
        // Only a daemon that is running can be stopped.
        let socket = daemon_socket(&cfg, &cache);
        let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&socket) else { std::process::exit(0) };
        let bytes = rargo_proto::encode(&req);
        let _ = stream.write_all(&(bytes.len() as u32).to_le_bytes()).and_then(|_| stream.write_all(&bytes));
        let mut rest = Vec::new();
        let _ = stream.read_to_end(&mut rest);
        std::process::exit(0);
    }
    through_daemon(cfg, cache, req, &raw)
}

/// The first argument that is not an option: the subcommand.
fn subcommand(args: &[String]) -> Option<&str> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--" {
            return None;
        }
        // Global options that take a value.
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

/// Finds the real cargo: the first `cargo` on PATH that is not this program.
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
    // SAFETY: all pointers are valid C strings or null.
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

/// Enters a mount namespace in which each `(real, expected)` pair makes the
/// real directory visible at the expected path, without touching the host:
/// a new root is assembled on a tmpfs out of binds of the old root's entries.
fn enter_namespace(binds: &[(String, String)]) -> std::io::Result<()> {
    // SAFETY: plain syscalls; this process is single-threaded here.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let os_err = std::io::Error::last_os_error;
    if unsafe { libc::unshare(libc::CLONE_NEWNS) } != 0 {
        // Not privileged: try a user namespace that maps us to ourselves.
        if unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) } != 0 {
            return Err(os_err());
        }
        std::fs::write("/proc/self/setgroups", "deny")?;
        std::fs::write("/proc/self/uid_map", format!("{uid} {uid} 1"))?;
        std::fs::write("/proc/self/gid_map", format!("{gid} {gid} 1"))?;
    }
    mount(None, Path::new("/"), None, libc::MS_REC | libc::MS_PRIVATE, None)?;
    // The new root is a tmpfs mounted over an existing top-level directory;
    // what that directory held is reached through a descriptor. It must not
    // be one a worktree lies under: what is mounted below the covered
    // directory (a worktree's output directory, served by the daemon)
    // would not come along.
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
        // Paths below the covered directory are only reachable through the
        // descriptor (none is a worktree, by the choice above).
        let real_p = match Path::new(real).strip_prefix(covered) {
            Ok(rest) => PathBuf::from(format!("/proc/self/fd/{}", tmp_fd.as_raw_fd())).join(rest),
            Err(_) => PathBuf::from(real),
        };
        mount(Some(&real_p), &dst, None, libc::MS_BIND | libc::MS_REC, None)?;
    }
    let root_c = cstr(new_root);
    // SAFETY: valid C strings; pivot_root(".", ".") then detach the old root.
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

/// Replaces `expected` path prefixes with real ones, for running without a
/// namespace.
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

/// Becomes the program `cargo run` would have started.
fn exec(mut e: ExecRequest) -> ! {
    // A path the client does not have (no cargo home, say) cannot be shown.
    e.binds.retain(|(real, _)| Path::new(real).exists());
    // The fork must happen in a child so that this process (the one the
    // shell is waiting for) keeps its place: unshare changes only us.
    if e.binds.is_empty() {
        // Nothing to make appear anywhere: a plain process.
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
    // The program is the user's: it gets the user's whole environment plus
    // what cargo adds or removes.
    for (k, v) in &e.env {
        match v {
            Some(v) => cmd.env(k, v),
            None => cmd.env_remove(k),
        };
    }
    let err = cmd.exec();
    eprintln!("error: could not execute process `{}` (never executed)\n\nCaused by:\n  {err}", e.program);
    std::process::exit(101)
}

#[cfg(test)]
mod tests {
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
        // A cache path too long for a socket address: the runtime directory.
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
