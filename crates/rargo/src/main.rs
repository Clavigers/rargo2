//! rargo: the client. It syncs the worktree and the environment to the
//! build server, where everything is analysed, scheduled and built, and
//! brings back output, the artifacts that were asked for, and files the
//! build wrote into the worktree. Commands that are not builds are handed
//! to the real cargo unchanged.
//!
//! When this binary is installed under the name `cargo` (the shim), agents
//! that type `cargo build` get rargo without knowing.

mod place;
mod scan;

use rargo_proto::secure;
use rargo_proto::{Blob, ClientMsg, Digest, ExecRequest, PROTO_VERSION, RunRequest, ServerMsg};
use rayon::prelude::*;
use std::ffi::{CString, OsString};
use std::io::{BufReader, IsTerminal, Read, Write};
use std::net::TcpStream;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

/// Subcommands the server handles.
const ROUTED: &[&str] = &["build", "b", "check", "c", "clippy", "test", "t", "run", "r", "doc", "d", "bench", "rustc", "rustdoc"];

/// Writes a file by renaming a complete temporary file into place.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = path.with_file_name(format!(".rargo-tmp-{}-{name}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/".into()))
}

/// Where to find the server and how to prove who we are: the environment
/// first, then `rargo/config.toml` in the user's configuration directory.
struct Config {
    server: String,
    user: String,
    psk: [u8; 32],
}

fn load_config() -> Result<Config, String> {
    let dir = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home_dir().join(".config"));
    let path = dir.join("rargo/config.toml");
    let file: toml::Table = match std::fs::read_to_string(&path) {
        Ok(text) => text.parse().map_err(|e| format!("{}: {e}", path.display()))?,
        Err(_) => toml::Table::new(),
    };
    let get = |env: &str, key: &str| -> Option<String> {
        std::env::var(env).ok().filter(|v| !v.is_empty()).or_else(|| file.get(key).and_then(|v| v.as_str()).map(String::from))
    };
    let server = get("RARGO_SERVER", "server").unwrap_or_else(|| "127.0.0.1:7459".into());
    let user = get("RARGO_USER", "user")
        .or_else(|| std::env::var("USER").ok())
        .or_else(|| std::env::var("LOGNAME").ok())
        .unwrap_or_else(|| "user".into());
    let secret = match (get("RARGO_KEY", "key"), get("RARGO_KEY_FILE", "key-file")) {
        (Some(k), _) => Some(k.into_bytes()),
        (None, Some(f)) => Some(std::fs::read(&f).map_err(|e| format!("cannot read the key file {f}: {e}"))?),
        (None, None) => None,
    };
    let psk = match secret {
        Some(s) => secure::psk_from_secret(&s),
        None => secure::open_psk(),
    };
    Ok(Config { server, user, psk })
}

fn cache_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("RARGO_CACHE_DIR") {
        return PathBuf::from(d);
    }
    std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from).unwrap_or_else(|| home_dir().join(".cache")).join("rargo")
}

fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for its length.
    if unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } != 0 {
        return "unknown".into();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// The directory that is synced: the git worktree the current directory is
/// in; without git, the nearest directory with a policy file, or else the
/// outermost one with a `Cargo.toml`.
fn find_root(cwd: &Path) -> Option<PathBuf> {
    let ancestors: Vec<&Path> = cwd.ancestors().collect();
    for marker in [".git", ".rargo.toml"] {
        if let Some(d) = ancestors.iter().find(|d| d.join(marker).exists()) {
            return Some(d.to_path_buf());
        }
    }
    ancestors.iter().rev().find(|d| d.join("Cargo.toml").is_file()).map(|d| d.to_path_buf())
}

/// The parts of the repository's policy file the client acts on.
#[derive(Default)]
struct Policy {
    pass: Vec<String>,
    exclude: Vec<String>,
}

fn load_policy(root: &Path) -> Policy {
    let Ok(text) = std::fs::read_to_string(root.join(".rargo.toml")) else { return Policy::default() };
    // A malformed file is the server's to report, with the build.
    let Ok(t) = text.parse::<toml::Table>() else { return Policy::default() };
    let list = |section: &str, key: &str| -> Vec<String> {
        t.get(section)
            .and_then(|s| s.get(key))
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default()
    };
    Policy { pass: list("env", "pass"), exclude: list("sync", "exclude") }
}

fn pattern_matches(pattern: &str, name: &str) -> bool {
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

/// HEAD's commit id, read straight from the git directory.
fn head_commit(root: &Path) -> Option<String> {
    let dot_git = root.join(".git");
    let git_dir = if dot_git.is_file() {
        let text = std::fs::read_to_string(&dot_git).ok()?;
        let p = PathBuf::from(text.strip_prefix("gitdir:")?.trim());
        if p.is_absolute() { p } else { root.join(p) }
    } else {
        dot_git
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    let Some(name) = head.strip_prefix("ref: ") else { return parse_hex20(head).map(|_| head.to_string()) };
    let common = match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(c) => {
            let c = PathBuf::from(c.trim());
            if c.is_absolute() { c } else { git_dir.join(c) }
        }
        Err(_) => git_dir.clone(),
    };
    for dir in [&git_dir, &common] {
        if let Ok(id) = std::fs::read_to_string(dir.join(name)) {
            return parse_hex20(&id).map(|_| id.trim().to_string());
        }
    }
    let packed = std::fs::read_to_string(common.join("packed-refs")).ok()?;
    packed.lines().find_map(|l| l.strip_suffix(name).map(|id| id.trim().to_string())).filter(|id| parse_hex20(id).is_some())
}

/// HEAD and its recent first-parent ancestors: a hint for the server's
/// choice of incremental state. Asked of git once per HEAD and remembered.
fn recent_commits(root: &Path, wt_cache: &Path) -> Vec<[u8; 20]> {
    let Some(head) = head_commit(root) else { return Vec::new() };
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
        .args(["rev-list", "--first-parent", "-n", "64", &head])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_else(|| format!("{head}\n"));
    let _ = write_atomic(&memo, listed.as_bytes());
    parse(&listed)
}

fn die(msg: &str) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(101)
}

/// What one attempt at a command came to.
enum Outcome {
    Exit(i32),
    Exec(ExecRequest),
    /// The worktree changed while it was being sent: scan again.
    Changed,
}

struct Wire {
    rd: BufReader<TcpStream>,
    wr: TcpStream,
    sealer: secure::Sealer,
    opener: secure::Opener,
}

impl Wire {
    fn send(&mut self, m: &ClientMsg) -> std::io::Result<()> {
        secure::write_msg(&mut self.wr, &mut self.sealer, &rargo_proto::encode(m))
    }
    fn recv(&mut self) -> std::io::Result<Option<ServerMsg>> {
        match secure::read_msg(&mut self.rd, &mut self.opener)? {
            Some(b) => Ok(Some(rargo_proto::decode(&b)?)),
            None => Ok(None),
        }
    }
}

fn connect(cfg: &Config, req: &RunRequest) -> Result<Wire, std::io::Error> {
    let stream = TcpStream::connect(&cfg.server)?;
    let _ = stream.set_nodelay(true);
    // The request rides in the first handshake message when it fits.
    let run = ClientMsg::Run(Box::new(req.clone()));
    let packed = zstd::stream::encode_all(&rargo_proto::encode(&run)[..], 1)?;
    let early: &[u8] = if packed.len() <= secure::EARLY_MAX { &packed } else { &[] };
    let (hs, first) = secure::client_start(&cfg.user, &cfg.psk, early)?;
    let mut wr = stream.try_clone()?;
    wr.write_all(&first)?;
    let mut rd = BufReader::with_capacity(256 << 10, stream);
    let mut head = [0u8; 3];
    rd.read_exact(&mut head)?;
    let mut body = vec![0u8; u16::from_le_bytes([head[1], head[2]]) as usize];
    rd.read_exact(&mut body)?;
    if head[0] != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("the server refused the connection: {}", String::from_utf8_lossy(&body).trim()),
        ));
    }
    let (sealer, opener) = secure::client_finish(hs, &body)?;
    let mut wire = Wire { rd, wr, sealer, opener };
    if early.is_empty() {
        wire.send(&run)?;
    }
    Ok(wire)
}

/// Sends the contents the server asked for. Returns `false` if a file no
/// longer has the content the scan saw.
fn upload(wire: &mut Wire, root: &Path, scan: &scan::Scan, want: Vec<Digest>) -> std::io::Result<bool> {
    let mut small: Vec<(Digest, PathBuf, u64)> = Vec::new();
    let mut large: Vec<(Digest, PathBuf)> = Vec::new();
    for h in want {
        let (Some(rel), Some(path)) = (scan.files.get(&h), scan::path_of(scan, root, &h)) else { return Ok(false) };
        let size = scan.index.files.get(rel).map(|e| e.size).unwrap_or(0);
        if size as usize > rargo_proto::PART_SIZE { large.push((h, path)) } else { small.push((h, path, size)) }
    }
    // Small files travel in batches, read and compressed in parallel.
    let mut batches: Vec<Vec<(Digest, PathBuf)>> = vec![Vec::new()];
    let mut bytes = 0usize;
    for (h, p, size) in small {
        if bytes >= rargo_proto::BATCH_SIZE {
            batches.push(Vec::new());
            bytes = 0;
        }
        bytes += size as usize + 64;
        batches.last_mut().unwrap().push((h, p));
    }
    for window in batches.chunks(16) {
        let packed: Vec<Option<Vec<Blob>>> = window
            .par_iter()
            .map(|batch| {
                batch
                    .par_iter()
                    .map(|(h, p)| {
                        let bytes = std::fs::read(p).ok()?;
                        (*blake3::hash(&bytes).as_bytes() == *h).then(|| Blob::pack(*h, &bytes))
                    })
                    .collect::<Option<Vec<Blob>>>()
            })
            .collect();
        for blobs in packed {
            let Some(blobs) = blobs else { return Ok(false) };
            if !blobs.is_empty() {
                wire.send(&ClientMsg::Blobs(blobs))?;
            }
        }
    }
    for (h, path) in large {
        let Ok(mut f) = std::fs::File::open(&path) else { return Ok(false) };
        let mut hasher = blake3::Hasher::new();
        // One part is held back so that the last one is only sent once the
        // whole file is known to be what the scan saw.
        let mut held: Option<Vec<u8>> = None;
        loop {
            let mut buf = vec![0u8; rargo_proto::PART_SIZE];
            let mut n = 0;
            while n < buf.len() {
                let got = f.read(&mut buf[n..])?;
                if got == 0 {
                    break;
                }
                n += got;
            }
            buf.truncate(n);
            hasher.update(&buf);
            if n == 0 {
                break;
            }
            if let Some(prev) = held.replace(buf) {
                wire.send(&ClientMsg::BlobPart { hash: h, data: prev, last: false })?;
            }
        }
        if *hasher.finalize().as_bytes() != h {
            return Ok(false);
        }
        wire.send(&ClientMsg::BlobPart { hash: h, data: held.unwrap_or_default(), last: true })?;
    }
    Ok(true)
}

struct Local {
    cache: PathBuf,
    wt_cache: PathBuf,
    root: PathBuf,
    /// Variable names servers have asked about.
    asked: Vec<String>,
}

fn load_asked(cache: &Path) -> Vec<String> {
    std::fs::read_to_string(cache.join("env-names")).map(|t| t.lines().map(String::from).collect()).unwrap_or_default()
}

/// One conversation with the server.
fn converse(cfg: &Config, req: &RunRequest, local: &mut Local, scan: Option<&scan::Scan>) -> std::io::Result<Outcome> {
    let mut wire = connect(cfg, req)?;
    let placed_path = local.wt_cache.join("placed");
    let mut placer = place::Placer::new(&local.cache, place::Placed::load(&placed_path));
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let finish = |placer: &place::Placer| {
        if placer.changed {
            let _ = placer.placed.save(&placed_path);
            placer.prune();
        }
    };
    loop {
        let Some(msg) = wire.recv()? else {
            return Err(std::io::Error::other("the server closed the connection"));
        };
        match msg {
            ServerMsg::Stdout(b) => {
                let mut o = stdout.lock();
                if o.write_all(&b).and_then(|_| o.flush()).is_err() {
                    // Downstream closed (e.g. `| head`): stop like cargo does.
                    std::process::exit(141);
                }
            }
            ServerMsg::Stderr(b) => {
                let _ = stderr.lock().write_all(&b);
            }
            ServerMsg::WantTrees(want) => {
                let trees: Vec<Vec<u8>> = want.iter().filter_map(|d| scan.and_then(|s| s.trees.get(d)).cloned()).collect();
                wire.send(&ClientMsg::Trees(trees))?;
            }
            ServerMsg::WantBlobs(want) => {
                let ok = match scan {
                    Some(s) => upload(&mut wire, &local.root, s, want)?,
                    None => false,
                };
                if !ok {
                    let _ = wire.send(&ClientMsg::Changed);
                    return Ok(Outcome::Changed);
                }
            }
            ServerMsg::WantEnv(names) => {
                let values: Vec<(String, Option<String>)> =
                    names.iter().map(|n| (n.clone(), std::env::var(n).ok())).collect();
                wire.send(&ClientMsg::Env(values))?;
                // Sent unasked from now on.
                let before = local.asked.len();
                for n in names {
                    if !local.asked.contains(&n) {
                        local.asked.push(n);
                    }
                }
                if local.asked.len() != before {
                    let _ = write_atomic(&local.cache.join("env-names"), (local.asked.join("\n") + "\n").as_bytes());
                }
            }
            ServerMsg::Deliver { id, files, writes } => {
                let hashes = placer.deliver(files, writes)?;
                wire.send(&ClientMsg::Need { id, hashes })?;
            }
            ServerMsg::Blobs(blobs) => {
                for b in blobs {
                    let hash = b.hash;
                    placer.blob(hash, &b.unpack()?)?;
                }
            }
            ServerMsg::BlobPart { hash, data, last } => placer.part(hash, &data, last)?,
            ServerMsg::Delivered { .. } => {
                if !placer.settled() {
                    return Err(std::io::Error::other("the server did not send every file it announced"));
                }
            }
            ServerMsg::Exit(code) => {
                finish(&placer);
                return Ok(Outcome::Exit(code));
            }
            ServerMsg::Exec(e) => {
                finish(&placer);
                return Ok(Outcome::Exec(e));
            }
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
        Some("--rargo-status") => Some("__status"),
        Some("--rargo-gc") => Some("__gc"),
        Some("--rargo-incr-stats") => Some("__incr"),
        Some("--rargo-compact") => Some("__compact"),
        Some("--rargo-export") => Some("__export"),
        Some("--rargo-import") => Some("__import"),
        _ => None,
    };
    let drift = args.first().map(|s| s.as_str()) == Some("--rargo-drift-check");
    if let Some(cmd) = admin {
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
        args = a;
    } else if drift {
        args = vec!["__drift".into()];
    } else {
        match subcommand(&args) {
            Some(cmd) if ROUTED.contains(&cmd) => {}
            _ => passthrough(&raw),
        }
        // Asking for help or version is cargo's business.
        if args.iter().take_while(|a| *a != "--").any(|a| a == "--help" || a == "-h" || a == "--version" || a == "-V") {
            passthrough(&raw);
        }
    }
    let cfg = load_config().unwrap_or_else(|e| die(&e));
    let cwd = match std::env::current_dir().and_then(|d| d.canonicalize()) {
        Ok(d) => d,
        Err(e) => die(&format!("cannot determine the current directory: {e}")),
    };
    let cache = cache_dir();
    let host = hostname();
    let root = if admin.is_some() {
        cwd.clone()
    } else {
        find_root(&cwd).unwrap_or_else(|| {
            die(&format!("`{}` is not inside a git worktree or a cargo package; rargo needs to know what to sync", cwd.display()))
        })
    };
    let wt_id = {
        let mut h = blake3::Hasher::new();
        h.update(host.as_bytes());
        h.update(&[0]);
        h.update(root.as_os_str().as_bytes());
        h.finalize().to_hex()[..24].to_string()
    };
    let mut local = Local { wt_cache: cache.join("wt").join(&wt_id), asked: load_asked(&cache), cache, root: root.clone() };
    let policy = if admin.is_some() { Policy::default() } else { load_policy(&root) };
    let cargo_home = std::env::var_os("CARGO_HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".cargo"));
    let mut home_files = Vec::new();
    if admin.is_none() {
        for name in ["config.toml", "config", "credentials.toml", "credentials"] {
            match std::fs::read(cargo_home.join(name)) {
                Ok(b) if b.len() <= 1 << 20 => home_files.push((name.to_string(), b)),
                _ => {}
            }
        }
    }
    let stats = std::env::var_os("RARGO_SYNC_STATS").is_some();
    let index_path = local.wt_cache.join("index");
    let mut index = if admin.is_some() { scan::Index::default() } else { scan::Index::load(&index_path) };
    for attempt in 0..5 {
        let started = std::time::Instant::now();
        let scanned = if admin.is_some() {
            None
        } else {
            match scan::scan(&root, &index, &policy.exclude) {
                Ok(s) => Some(s),
                Err(e) => die(&format!("scanning {}: {e}", root.display())),
            }
        };
        if let Some(s) = &scanned {
            if s.index_changed {
                let _ = s.index.save(&index_path);
            }
            if stats {
                eprintln!(
                    "rargo: scanned {} files in {:.1} ms ({} read, {:.1} MiB)",
                    s.file_count,
                    started.elapsed().as_secs_f64() * 1e3,
                    s.hashed_files,
                    s.hashed_bytes as f64 / 1048576.0
                );
            }
        }
        let env: Vec<(String, String)> = vars
            .iter()
            .filter(|(k, _)| {
                rargo_proto::env_allowed(k)
                    || policy.pass.iter().any(|p| pattern_matches(p, k))
                    || local.asked.contains(k)
            })
            .cloned()
            .collect();
        let req = RunRequest {
            version: PROTO_VERSION,
            args: args.clone(),
            root: root.to_string_lossy().into_owned(),
            cwd: cwd.to_string_lossy().into_owned(),
            cargo_home: cargo_home.to_string_lossy().into_owned(),
            env,
            env_checked: local.asked.clone(),
            stdout_tty: std::io::stdout().is_terminal(),
            stderr_tty: std::io::stderr().is_terminal(),
            worktree: wt_id.clone(),
            commits: if admin.is_some() { Vec::new() } else { recent_commits(&root, &local.wt_cache) },
            tree: scanned.as_ref().map(|s| s.root).unwrap_or([0; 32]),
            home_files: home_files.clone(),
            who: format!("{}@{host}", cfg.user),
        };
        match converse(&cfg, &req, &mut local, scanned.as_ref()) {
            Ok(Outcome::Exit(code)) => std::process::exit(code),
            Ok(Outcome::Exec(e)) => exec(e),
            Ok(Outcome::Changed) => {
                if attempt == 4 {
                    die("the worktree kept changing while it was being synced");
                }
                if let Some(s) = scanned {
                    index = s.index;
                }
            }
            Err(e) if attempt == 0 && matches!(e.kind(), std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::TimedOut | std::io::ErrorKind::NotFound | std::io::ErrorKind::HostUnreachable | std::io::ErrorKind::NetworkUnreachable) => {
                if std::env::var_os("RARGO_FALLBACK").is_some_and(|v| v == "cargo") {
                    passthrough(&raw);
                }
                eprintln!("error: cannot reach the rargo server at {}: {e}", cfg.server);
                eprintln!("       (set RARGO_SERVER or `server` in ~/.config/rargo/config.toml, or set RARGO_FALLBACK=cargo to use plain cargo)");
                std::process::exit(101);
            }
            Err(e) => die(&format!("talking to the rargo server at {}: {e}", cfg.server)),
        }
    }
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
    // The new root is a tmpfs mounted over /tmp (any existing directory
    // would do); the real /tmp is reached through a descriptor.
    let entries: Vec<std::fs::DirEntry> = std::fs::read_dir("/")?.filter_map(|e| e.ok()).collect();
    let tmp_fd = std::fs::File::open("/tmp")?;
    let new_root = Path::new("/tmp");
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
            let from = if name == "tmp" { PathBuf::from(format!("/proc/self/fd/{}", tmp_fd.as_raw_fd())) } else { src };
            mount(Some(&from), &dst, None, libc::MS_BIND | libc::MS_REC, None)?;
        } else if ft.is_file() {
            std::fs::File::create(&dst)?;
            mount(Some(&src), &dst, None, libc::MS_BIND, None)?;
        }
    }
    for (real, at) in binds {
        let dst = new_root.join(at.trim_start_matches('/'));
        std::fs::create_dir_all(&dst)?;
        // Paths below the old /tmp are only reachable through the descriptor.
        let real_p = match Path::new(real).strip_prefix("/tmp") {
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
