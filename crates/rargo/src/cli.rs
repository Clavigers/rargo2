use crate::Config;
use crate::project::{self, Credential};
use rargo_proto::secure;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

pub const ROUTED: &[&str] =
    &["build", "b", "check", "c", "clippy", "test", "t", "run", "r", "doc", "d", "bench", "rustc", "rustdoc", "nextest"];

pub const TOKEN_COMMANDS: &[&str] = &["invite", "create", "tokens", "revoke", "allow", "disallow"];

pub const AUTH_HELP: &str = "rargo auth [HOST]            get this machine a token for your server (or HOST), confirmed in a browser\n\
     rargo auth --token T [HOST]  store a token an administrator made (rargod token create)\n\
     rargo auth status            which server you build on, what named it, and as whom\n\
     rargo auth remove [HOST]     revoke the token for your server (or HOST) there and forget it here\n\
     \n\
     On your server, for an administrator (anyone may list and revoke their own tokens):\n\
     rargo auth invite [--user NAME] [--admin] [--days N] [--uses N] [--token-days N]\n\
     rargo auth create --user NAME [--admin] [--days N]\n\
     rargo auth tokens\n\
     rargo auth revoke ID-OR-USER\n\
     rargo auth allow GITHUB-LOGIN [--admin]\n\
     rargo auth disallow GITHUB-LOGIN";

pub const STATUS_HELP: &str = "Usage: rargo status\n\nShow your server, account, daemon, mount and versions.";

pub const BANNER: &str = "Usage: rargo [CARGO COMMAND...]\n\n\
     rargo runs cargo, with build, check, clippy, test, run, doc, bench and nextest done on your rargo server.\n\n\
     rargo's own commands:\n    \
     @HOST            Name HOST as your rargo server (e.g. `rargo @rargo.rs`)\n    \
     auth             Get, show or remove this machine's token for your server (`rargo help auth`)\n    \
     status           Show your server, account, daemon, mount and versions\n    \
     --rargo-version  Print rargo's version and protocol\n\n\
     Cargo's help follows.\n";

/// Whether help may carry colour: `CARGO_TERM_COLOR` decides when it says always or never, then `NO_COLOR`, then the terminal.
pub fn colour(vars: &[(String, String)], tty: bool) -> bool {
    let var = |name: &str| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str()).filter(|v| !v.is_empty());
    match var("CARGO_TERM_COLOR") {
        Some("always") => true,
        Some("never") => false,
        _ => var("NO_COLOR").is_none() && tty,
    }
}

/// The banner with its headings, usage and command names in bold yellow.
pub fn paint(text: &str) -> String {
    let bold = |s: &str| format!("\x1b[1;33m{s}\x1b[0m");
    let mut out = String::new();
    for line in text.split_inclusive('\n') {
        let (body, end) = line.strip_suffix('\n').map_or((line, ""), |b| (b, "\n"));
        if let Some(usage) = body.strip_prefix("Usage: ") {
            out += &format!("{} {}", bold("Usage:"), bold(usage));
        } else if let Some(entry) = body.strip_prefix("    ") {
            let (name, rest) = entry.split_at(entry.find("  ").unwrap_or(entry.len()));
            out += &format!("    {}{rest}", bold(name));
        } else if body.ends_with(':') {
            out += &bold(body);
        } else {
            out += body;
        }
        out += end;
    }
    out
}

pub fn version() -> String {
    format!("rargo {} (protocol {})", env!("CARGO_PKG_VERSION"), rargo_proto::PROTO_VERSION)
}

const RARGO_OPTIONS: &[&str] = &[
    "--rargo-version", "--rargo-daemon", "--rargo-status", "--rargo-settle", "--rargo-gc", "--rargo-incr-stats", "--rargo-compact",
    "--rargo-export", "--rargo-import", "--rargo-drift-check", "--rargo-daemon-status", "--rargo-daemon-stop", "--rargo-daemon-sync",
];

pub trait World {
    fn vars(&self) -> &[(String, String)];
    fn config_path(&self) -> PathBuf;
    fn config_text(&self) -> Result<Option<String>, String>;
    fn resolve(&self, host: &str) -> Result<String, String>;
    fn credential(&self, host: &str) -> Option<Credential>;
    fn nextest_version(&self) -> Option<String>;
    fn cwd(&self) -> PathBuf;
    fn is_file(&self, path: &Path) -> bool;
    fn holder(&self, host: &str, token: &str) -> Result<Option<String>, String>;
    /// Whether the server says it has authentication disabled, so `rargo auth` has nothing to do there.
    fn auth_disabled(&self, host: &str) -> bool;
}

fn var<'a>(w: &'a impl World, name: &str) -> Option<&'a str> {
    w.vars().iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str()).filter(|v| !v.is_empty())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostFrom {
    Env,
    File(PathBuf),
}

fn host_named(w: &impl World, daemon: bool) -> Result<(String, HostFrom), String> {
    if let Some(h) = var(w, "RARGO_SERVER") {
        return Ok((h.to_string(), HostFrom::Env));
    }
    if daemon {
        return Err("the daemon was started without a server".into());
    }
    let path = w.config_path();
    match w.config_text()?.map(|text| project::parse_host(&path, &text)).transpose()?.flatten() {
        Some(h) => Ok((h, HostFrom::File(path))),
        None => Err(format!("no rargo server is named: run `rargo @rargo.rs` (or `rargo @HOST` for a server of your own), which writes {}", path.display())),
    }
}

pub fn config(w: &impl World) -> Result<Config, String> {
    let daemon = var(w, "RARGO_DAEMON_CONFIG").is_some();
    let (host, _) = host_named(w, daemon)?;
    let server = if daemon { host.clone() } else { w.resolve(&host)? };
    let (user, secret) = if let Some(token) = var(w, "RARGO_TOKEN") {
        let (id, secret) = project::split_token(token).ok_or_else(|| format!("RARGO_TOKEN is not a token: {}", project::TOKEN_FORM))?;
        (id.to_string(), Some(secret.to_string()))
    } else if let Some(c) = (!daemon).then(|| w.credential(&host)).flatten() {
        (c.id, Some(c.secret))
    } else {
        (var(w, "USER").or(var(w, "LOGNAME")).unwrap_or("user").to_string(), None)
    };
    let psk = secret.as_ref().map_or_else(secure::open_psk, |s| secure::psk_from_secret(s.as_bytes()));
    Ok(Config { host, server, user, secret, psk })
}

pub fn is_loopback(server: &str) -> bool {
    let name = server.rsplit_once(':').map_or(server, |(h, _)| h);
    name == "localhost" || name.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

#[derive(Clone, Debug, PartialEq)]
pub enum Auth {
    Status { host: String, from: Option<HostFrom> },
    Remove { host: String },
    Store { host: String, token: String },
    Login { host: String },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Plan {
    /// Run cargo, first printing `banner` if there is one.
    Cargo { release: bool, banner: Option<String> },
    Print(String),
    Status,
    StopDaemons,
    Fail { text: String, code: i32 },
    Name { host: String, force: bool },
    Auth(Auth),
    Daemon(OsString),
    Remote { config: Config, args: Vec<String>, vars: Vec<(String, String)>, kind: &'static str },
}

fn die(msg: impl std::fmt::Display) -> Plan {
    Plan::Fail { text: format!("error: {msg}"), code: 101 }
}

pub fn unsupported_toolchain(w: &impl World, prefix: Option<&str>) -> Option<String> {
    let source = prefix.filter(|arg| arg.starts_with('+')).map(|arg| format!("`{arg}`"))
        .or_else(|| var(w, "RUSTUP_TOOLCHAIN").map(|_| "`RUSTUP_TOOLCHAIN`".into()))?;
    Some(format!("rargo cannot use {source} for remote builds. Select Rust with `rust-toolchain.toml` when using the default flake, or in your project's `flake.nix`."))
}

pub fn plan(w: &impl World, argv0: Option<&OsStr>, raw: &[OsString]) -> Plan {
    if w.vars().iter().any(|(k, _)| k == "RARGO_BYPASS") {
        return Plan::Cargo { release: false, banner: None };
    }
    let first = raw.first().map(OsString::as_os_str);
    let second = raw.get(1).and_then(|a| a.to_str());
    let cargo_link = argv0.is_some_and(|arg| Path::new(arg).file_name().is_some_and(|name| name == "cargo"));
    if !cargo_link && first.is_some_and(|a| a == "status") {
        return match second {
            Some("--help" | "-h") if raw.len() == 2 => Plan::Print(STATUS_HELP.into()),
            None if raw.len() == 1 => Plan::Status,
            _ => die("usage: rargo status"),
        };
    }
    if !cargo_link && first.is_some_and(|a| a == "help") {
        match (second, raw.len()) {
            (Some("auth"), 2) => return Plan::Print(AUTH_HELP.into()),
            (Some("status"), 2) => return Plan::Print(STATUS_HELP.into()),
            _ => {}
        }
    }
    let alone = |names: &[&str]| !cargo_link && raw.len() == 1 && first.and_then(OsStr::to_str).is_some_and(|a| names.contains(&a));
    let banner = if (raw.is_empty() && !cargo_link) || alone(&["--help", "-h", "help"]) {
        Some(BANNER.to_string())
    } else if alone(&["--version", "-V"]) {
        Some(version())
    } else {
        None
    };
    if first.is_some_and(|a| a == "--rargo-version") {
        return Plan::Print(version());
    }
    if let Some(option) = first.and_then(OsStr::to_str).filter(|a| a.starts_with("--rargo-") && !RARGO_OPTIONS.contains(a)) {
        return Plan::Fail { text: format!("error: unknown option `{option}` (see `rargo --help`)"), code: 2 };
    }
    if let Some(host) = first.and_then(OsStr::to_str).and_then(|a| a.strip_prefix('@')) {
        return match raw[1..].iter().map(|a| a.to_str()).collect::<Vec<_>>().as_slice() {
            [] => Plan::Name { host: host.to_string(), force: false },
            [Some("--force")] => Plan::Name { host: host.to_string(), force: true },
            _ => die("usage: rargo @HOST [--force]"),
        };
    }
    let token_command = first.is_some_and(|a| a == "auth") && second.is_some_and(|a| TOKEN_COMMANDS.contains(&a));
    if token_command && raw[2..].iter().take_while(|a| *a != "--").any(|a| a == "--help" || a == "-h") {
        return Plan::Print(AUTH_HELP.into());
    }
    if first.is_some_and(|a| a == "auth") && !token_command {
        return auth(w, &raw[1..]);
    }
    if first.is_some_and(|a| a == "--rargo-daemon-stop") {
        return Plan::StopDaemons;
    }
    if first.is_some_and(|a| a == "--rargo-daemon") {
        return match raw.get(1) {
            Some(socket) => Plan::Daemon(socket.clone()),
            None => die("usage: rargo --rargo-daemon SOCKET"),
        };
    }
    let (args, kind) = match route(raw, token_command) {
        Route::Local { release } => return Plan::Cargo { release, banner },
        Route::Usage(text) => return Plan::Fail { text, code: 2 },
        Route::Remote { args, kind } => (args, kind),
    };
    if kind == "build" {
        if let Err(e) = find_manifest(w, &args) {
            return die(e);
        }
        if let Some(error) = unsupported_toolchain(w, first.and_then(OsStr::to_str)) {
            return die(error);
        }
    }
    let mut vars = w.vars().to_vec();
    if subcommand(&args) == Some("nextest")
        && !vars.iter().any(|(k, _)| k == "RARGO_NEXTEST_VERSION")
        && let Some(v) = w.nextest_version()
    {
        vars.push(("RARGO_NEXTEST_VERSION".into(), v));
    }
    let config = match config(w) {
        Ok(c) => c,
        Err(e) => return die(e),
    };
    if config.secret.is_none() && !is_loopback(&config.server) && matches!(kind, "admin" | "drift" | "build") {
        if kind == "build" && var(w, "RARGO_FALLBACK") == Some("cargo") {
            return Plan::Cargo { release: false, banner: None };
        }
        return die(format!(
            "this machine is not authenticated to {}: run `rargo auth`, which also says how to get access if the server has not let you in yet",
            config.host
        ));
    }
    Plan::Remote { config, args, vars, kind }
}

/// The manifest the command would use: `--manifest-path` if given, else
/// the first Cargo.toml walking up from the current directory. Cargo's own
/// error when there is none, so a command fails before anything is sent.
pub(crate) fn find_manifest(w: &impl World, args: &[String]) -> Result<PathBuf, String> {
    let cwd = w.cwd();
    if let Some(given) = manifest_path_arg(args) {
        let path = cwd.join(&given);
        return if w.is_file(&path) { Ok(path) } else { Err(format!("manifest path `{given}` does not exist")) };
    }
    cwd.ancestors()
        .map(|dir| dir.join("Cargo.toml"))
        .find(|candidate| w.is_file(candidate))
        .ok_or_else(|| format!("could not find `Cargo.toml` in `{}` or any parent directory", cwd.display()))
}

/// Where a command runs from when `--manifest-path` names a manifest in a
/// worktree the current directory is not in: the manifest's own directory,
/// with the flags cargo reads as paths made absolute against `cwd`, so they
/// still mean what they meant there. `manifest` is the canonical path of the
/// named manifest; `root_of` finds the worktree of a directory. None when
/// the command already runs inside the manifest's worktree, or there is none.
pub(crate) fn rehome(cwd: &Path, args: &[String], manifest: &Path, root_of: impl Fn(&Path) -> Option<PathBuf>) -> Option<(PathBuf, Vec<String>)> {
    manifest_path_arg(args)?;
    let dir = manifest.parent()?;
    let root = root_of(dir)?;
    if root_of(cwd).as_ref() == Some(&root) {
        return None;
    }
    const PATHS: [&str; 5] = ["--manifest-path", "--target-dir", "--artifact-dir", "--out-dir", "--lockfile-path"];
    let absolute = |flag: &str, value: &str| -> String {
        if flag == "--manifest-path" { manifest.to_string_lossy().into_owned() } else { cwd.join(value).to_string_lossy().into_owned() }
    };
    let mut out = Vec::with_capacity(args.len());
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--" {
            out.push(arg.clone());
            out.extend(it.by_ref().cloned());
            break;
        }
        if let Some(flag) = PATHS.iter().find(|f| *f == arg) {
            out.push(arg.clone());
            if let Some(value) = it.next() {
                out.push(absolute(flag, value));
            }
        } else if let Some((flag, value)) = arg.split_once('=').filter(|(f, _)| PATHS.contains(f)) {
            out.push(format!("{flag}={}", absolute(flag, value)));
        } else {
            out.push(arg.clone());
        }
    }
    Some((dir.to_path_buf(), out))
}

/// The value of `--manifest-path`, in either spelling, before any `--`.
pub(crate) fn manifest_path_arg(args: &[String]) -> Option<String> {
    let mut it = args.iter().take_while(|a| *a != "--");
    while let Some(a) = it.next() {
        if a == "--manifest-path" {
            return it.next().cloned();
        }
        if let Some(v) = a.strip_prefix("--manifest-path=") {
            return Some(v.to_string());
        }
    }
    None
}

/// Whether `host` may be named as the server: it must answer as one,
/// unless it is an address (`host:port`) or the user said `--force`.
/// Nothing is written when it may not, so a typo keeps the server set.
pub fn may_name(w: &impl World, host: &str, force: bool) -> Result<(), String> {
    if force || host.contains(':') {
        return Ok(());
    }
    w.resolve(host).map(drop).map_err(|e| {
        let why = e.strip_suffix(project::RENAME).unwrap_or(&e);
        format!("{why}; nothing was changed (`rargo @{host} --force` names it anyway)")
    })
}

pub fn named(w: &impl World, host: &str, path: &Path) -> String {
    let mut text = format!("{} now names `{host}` as your rargo server.", path.display());
    if host.contains(':') || w.resolve(host).is_ok() {
        if w.credential(host).is_none() && var(w, "RARGO_TOKEN").is_none() {
            if w.auth_disabled(host) {
                text.push_str("\nThis server has authentication disabled. Next: `rargo build`.");
            } else {
                text.push_str("\nNext: `rargo auth`, then `rargo build`.");
            }
        }
    } else {
        text.push_str(&format!("\nwarning: no rargo server answers for `{host}` right now (https://{host}/.well-known/rargo.json)"));
    }
    text
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

fn auth(w: &impl World, args: &[OsString]) -> Plan {
    let AuthArgs { action, host, token } = match parse_auth(args) {
        Ok(Some(a)) => a,
        Ok(None) => return Plan::Print(AUTH_HELP.into()),
        Err(e) => return die(e),
    };
    let (host, from) = match host {
        Some(h) => (h, None),
        None => match host_named(w, false) {
            Ok((h, from)) => (h, Some(from)),
            Err(e) => return die(format!("{e}; or say `rargo auth HOST`")),
        },
    };
    if !project::valid_host(&host) {
        return die(format!("`{host}` is not a host"));
    }
    Plan::Auth(match (action, token) {
        ("status", _) => Auth::Status { host, from },
        ("remove", _) if w.credential(&host).is_none() => return die(format!("there is no token for {host} on this machine")),
        ("remove", _) => Auth::Remove { host },
        (_, Some(token)) => Auth::Store { host, token },
        (_, None) => Auth::Login { host },
    })
}

pub fn auth_status(w: &impl World, host: &str, from: Option<&HostFrom>) -> String {
    let mut text = match from {
        Some(HostFrom::Env) => "server named by RARGO_SERVER\n".to_string(),
        Some(HostFrom::File(path)) => format!("server named by {}\n", path.display()),
        None => String::new(),
    };
    let server = w.resolve(host).unwrap_or_else(|e| format!("unknown ({e})"));
    if let Some(token) = var(w, "RARGO_TOKEN") {
        text.push_str(&format!("{host} (server {server}): {}", env_token_status(w, host, token)));
        return text;
    }
    text.push_str(&match w.credential(host) {
        Some(c) if c.name.is_empty() => format!("{host} (server {server}): authenticated (token {})", c.id),
        Some(c) => format!("{host} (server {server}): authenticated as {} (token {})", c.name, c.id),
        None if w.auth_disabled(host) => format!("{host} (server {server}): authentication disabled; nothing needs doing"),
        None => format!("{host} (server {server}): not authenticated; run `rargo auth`"),
    });
    text
}

/// Who RARGO_TOKEN signs in as, since it comes before any token stored for the server.
fn env_token_status(w: &impl World, host: &str, token: &str) -> String {
    let Some((id, _)) = project::split_token(token) else {
        return format!("RARGO_TOKEN is not a token: {}", project::TOKEN_FORM);
    };
    match w.holder(host, token) {
        Ok(Some(user)) => format!("authenticated as {user} by RARGO_TOKEN (token {id})"),
        Ok(None) => format!("the server does not accept RARGO_TOKEN (token {id}): it does not know it, or it was revoked or has expired"),
        Err(e) => format!("RARGO_TOKEN (token {id}) is used; the server could not say whose it is ({e})"),
    }
}

pub fn removed(host: &str, revoked: Result<bool, String>) -> (String, Option<String>) {
    match revoked {
        Ok(true) => (format!("revoked the token for {host} on the server and forgot it here"), None),
        Ok(false) => (format!("forgot the token for {host} (the server did not know it as a token that still works)"), None),
        Err(e) => (
            format!("forgot the token for {host} on this machine"),
            Some(format!("warning: the server could not be told to revoke it ({e}); it keeps working for whoever has a copy until an administrator runs `rargod token revoke`")),
        ),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Route {
    Local { release: bool },
    Usage(String),
    Remote { args: Vec<String>, kind: &'static str },
}

fn route(raw: &[OsString], token_command: bool) -> Route {
    let Ok(mut args) = raw.iter().map(|a| a.clone().into_string()).collect::<Result<Vec<_>, _>>() else {
        return Route::Local { release: false };
    };
    if args.first().is_some_and(|arg| arg.starts_with('+')) {
        args.remove(0);
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
    Route::Remote { args, kind }
}

pub fn subcommand(args: &[String]) -> Option<&str> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const SECRET: &str = "0123456789abcdef";
    const PATH: &str = "/home/u/.config/rargo/config.toml";

    struct Fake {
        vars: Vec<(String, String)>,
        config: Result<Option<String>, String>,
        servers: Vec<(String, Result<String, String>)>,
        creds: Vec<(String, Credential)>,
        nextest: Option<String>,
        cwd: PathBuf,
        files: Vec<PathBuf>,
        holders: Vec<(String, Result<Option<String>, String>)>,
        open: Vec<String>,
        asked: RefCell<Vec<String>>,
    }

    fn cred(id: &str, name: &str) -> Credential {
        Credential { id: id.into(), secret: SECRET.into(), name: name.into() }
    }

    fn fake(vars: &[(&str, &str)]) -> Fake {
        Fake {
            vars: vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            config: Ok(Some("host = \"h.example\"\n".into())),
            servers: vec![("h.example".into(), Ok("srv.example:7459".into())), ("local.example".into(), Ok("127.0.0.1:7459".into()))],
            creds: vec![("h.example".into(), cred("stored", ""))],
            nextest: Some("0.9.100".into()),
            cwd: "/w/app/src".into(),
            files: vec!["/w/app/Cargo.toml".into()],
            holders: Vec::new(),
            open: Vec::new(),
            asked: RefCell::new(Vec::new()),
        }
    }

    impl Fake {
        fn config(mut self, text: Result<Option<&str>, &str>) -> Self {
            self.config = text.map(|t| t.map(str::to_owned)).map_err(str::to_owned);
            self
        }

        fn no_creds(mut self) -> Self {
            self.creds.clear();
            self
        }

        fn files(mut self, files: &[&str]) -> Self {
            self.files = files.iter().map(PathBuf::from).collect();
            self
        }

        fn asked(&self) -> Vec<String> {
            self.asked.borrow().clone()
        }
    }

    impl World for Fake {
        fn vars(&self) -> &[(String, String)] {
            &self.vars
        }

        fn config_path(&self) -> PathBuf {
            PATH.into()
        }

        fn config_text(&self) -> Result<Option<String>, String> {
            self.asked.borrow_mut().push("config".into());
            self.config.clone()
        }

        fn resolve(&self, host: &str) -> Result<String, String> {
            self.asked.borrow_mut().push(format!("resolve {host}"));
            if host.contains(':') {
                return Ok(host.into());
            }
            self.servers.iter().find(|(h, _)| h == host).map(|(_, s)| s.clone()).unwrap_or_else(|| Err(format!("no answer from {host}")))
        }

        fn credential(&self, host: &str) -> Option<Credential> {
            self.asked.borrow_mut().push(format!("credential {host}"));
            self.creds.iter().find(|(h, _)| h == host).map(|(_, c)| c.clone())
        }

        fn holder(&self, host: &str, token: &str) -> Result<Option<String>, String> {
            self.asked.borrow_mut().push(format!("holder {host}"));
            self.holders.iter().find(|(t, _)| t == token).map_or(Ok(None), |(_, h)| h.clone())
        }

        fn auth_disabled(&self, host: &str) -> bool {
            self.asked.borrow_mut().push(format!("auth_disabled {host}"));
            self.open.iter().any(|h| h == host)
        }

        fn nextest_version(&self) -> Option<String> {
            self.asked.borrow_mut().push("nextest".into());
            self.nextest.clone()
        }

        fn cwd(&self) -> PathBuf {
            self.cwd.clone()
        }

        fn is_file(&self, path: &Path) -> bool {
            self.files.iter().any(|f| f == path)
        }
    }

    fn os(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    fn strings(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    fn conf(host: &str, server: &str, user: &str, secret: Option<&str>) -> Config {
        let psk = secret.map_or_else(secure::open_psk, |s| secure::psk_from_secret(s.as_bytes()));
        Config { host: host.into(), server: server.into(), user: user.into(), secret: secret.map(str::to_owned), psk }
    }

    fn stored() -> Config {
        conf("h.example", "srv.example:7459", "stored", Some(SECRET))
    }

    fn remote(args: &[&str], vars: &[(&str, &str)], kind: &'static str) -> Plan {
        Plan::Remote { config: stored(), args: strings(args), vars: vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), kind }
    }

    fn fail(msg: &str) -> Plan {
        Plan::Fail { text: format!("error: {msg}"), code: 101 }
    }

    fn usage(text: &str) -> Plan {
        Plan::Fail { text: text.into(), code: 2 }
    }

    fn toml_error(text: &str) -> String {
        format!("{PATH}: {}", text.parse::<toml::Table>().unwrap_err())
    }

    #[test]
    fn every_command_line_is_planned_without_io() {
        let not_named = format!("no rargo server is named: run `rargo @rargo.rs` (or `rargo @HOST` for a server of your own), which writes {PATH}");
        let cargo = Plan::Cargo { release: false, banner: None };
        let cases: Vec<(Option<&str>, Vec<&str>, Fake, Plan)> = vec![
            (None, vec!["build"], fake(&[("RARGO_BYPASS", "1")]), cargo.clone()),
            (None, vec!["build"], fake(&[("RARGO_BYPASS", "")]), cargo.clone()),
            (None, vec!["status"], fake(&[]), Plan::Status),
            (None, vec!["status", "-h"], fake(&[]), Plan::Print(STATUS_HELP.into())),
            (None, vec!["status", "--help"], fake(&[]), Plan::Print(STATUS_HELP.into())),
            (None, vec!["status", "x"], fake(&[]), fail("usage: rargo status")),
            (None, vec!["status", "-h", "x"], fake(&[]), fail("usage: rargo status")),
            (Some("/usr/bin/cargo"), vec!["status"], fake(&[]), cargo.clone()),
            (Some("rargo"), vec!["-h"], fake(&[]), Plan::Cargo { release: false, banner: Some(BANNER.into()) }),
            (None, vec!["--help"], fake(&[]), Plan::Cargo { release: false, banner: Some(BANNER.into()) }),
            (None, vec![], fake(&[]), Plan::Cargo { release: false, banner: Some(BANNER.into()) }),
            (Some("/bin/cargo"), vec![], fake(&[]), cargo.clone()),
            (Some("/bin/cargo"), vec!["-h"], fake(&[]), cargo.clone()),
            (None, vec!["-h", "build"], fake(&[]), cargo.clone()),
            (None, vec!["--rargo-version"], fake(&[]), Plan::Print(format!("rargo {} (protocol {})", env!("CARGO_PKG_VERSION"), rargo_proto::PROTO_VERSION))),
            (None, vec!["help"], fake(&[]), Plan::Cargo { release: false, banner: Some(BANNER.into()) }),
            (None, vec!["help", "auth"], fake(&[]), Plan::Print(AUTH_HELP.into())),
            (None, vec!["help", "status"], fake(&[]), Plan::Print(STATUS_HELP.into())),
            (None, vec!["help", "build"], fake(&[]), cargo.clone()),
            (Some("cargo"), vec!["help", "auth"], fake(&[]), cargo.clone()),
            (None, vec!["--version"], fake(&[]), Plan::Cargo { release: false, banner: Some(version()) }),
            (None, vec!["-V"], fake(&[]), Plan::Cargo { release: false, banner: Some(version()) }),
            (Some("cargo"), vec!["--version"], fake(&[]), cargo.clone()),
            (None, vec!["--rargo-bogus"], fake(&[]), usage("error: unknown option `--rargo-bogus` (see `rargo --help`)")),
            (None, vec!["auth", "invite", "--help"], fake(&[]), Plan::Print(AUTH_HELP.into())),
            (None, vec!["auth", "tokens", "-h"], fake(&[]), Plan::Print(AUTH_HELP.into())),
            (None, vec!["@b.example"], fake(&[]), Plan::Name { host: "b.example".into(), force: false }),
            (None, vec!["@b.example", "--force"], fake(&[]), Plan::Name { host: "b.example".into(), force: true }),
            (None, vec!["@b.example", "-x"], fake(&[]), fail("usage: rargo @HOST [--force]")),
            (Some("cargo"), vec!["@b.example:1"], fake(&[]), Plan::Name { host: "b.example:1".into(), force: false }),
            (None, vec!["--rargo-daemon", "/run/s"], fake(&[]), Plan::Daemon("/run/s".into())),
            (None, vec!["--rargo-daemon"], fake(&[]), fail("usage: rargo --rargo-daemon SOCKET")),
            (None, vec!["auth", "-h"], fake(&[]), Plan::Print(AUTH_HELP.into())),
            (None, vec!["auth", "status"], fake(&[]), Plan::Auth(Auth::Status { host: "h.example".into(), from: Some(HostFrom::File(PATH.into())) })),
            (None, vec!["auth", "status"], fake(&[("RARGO_SERVER", "e.example:1")]), Plan::Auth(Auth::Status { host: "e.example:1".into(), from: Some(HostFrom::Env) })),
            (None, vec!["auth", "status", "@x.example"], fake(&[]), Plan::Auth(Auth::Status { host: "x.example".into(), from: None })),
            (None, vec!["auth", "--token", "id.secret", "x.example"], fake(&[]), Plan::Auth(Auth::Store { host: "x.example".into(), token: "id.secret".into() })),
            (None, vec!["auth", "x.example", "--token", "T"], fake(&[]), Plan::Auth(Auth::Store { host: "x.example".into(), token: "T".into() })),
            (None, vec!["auth"], fake(&[]), Plan::Auth(Auth::Login { host: "h.example".into() })),
            (None, vec!["auth"], fake(&[("RARGO_DAEMON_CONFIG", "1")]), Plan::Auth(Auth::Login { host: "h.example".into() })),
            (None, vec!["auth", "remove"], fake(&[]), Plan::Auth(Auth::Remove { host: "h.example".into() })),
            (None, vec!["auth", "remove"], fake(&[]).no_creds(), fail("there is no token for h.example on this machine")),
            (None, vec!["auth", "bad host!"], fake(&[]), fail("`bad host!` is not a host")),
            (None, vec!["auth", "x:port"], fake(&[]), fail("`x:port` is not a host")),
            (None, vec!["auth"], fake(&[]).config(Ok(None)), fail(&format!("{not_named}; or say `rargo auth HOST`"))),
            (None, vec!["auth", "status"], fake(&[]).config(Err("cannot read")), fail("cannot read; or say `rargo auth HOST`")),
            (None, vec!["auth", "--token"], fake(&[]), fail("`--token` needs the token")),
            (None, vec!["auth", "-x"], fake(&[]), fail("unknown option `-x` (see `rargo auth --help`)")),
            (None, vec!["auth", "a", "b"], fake(&[]), fail("unexpected `b` (see `rargo auth --help`)")),
            (None, vec!["auth", "tokens"], fake(&[]), remote(&["__token", "list"], &[], "admin")),
            (None, vec!["auth", "revoke", "bob"], fake(&[]), remote(&["__token", "revoke", "bob"], &[], "admin")),
            (None, vec!["auth", "invite", "--admin"], fake(&[]), remote(&["__token", "invite", "--admin"], &[], "admin")),
            (None, vec!["--rargo-status"], fake(&[]), remote(&["__status"], &[], "admin")),
            (None, vec!["--rargo-settle", "x"], fake(&[]), remote(&["__settle"], &[], "admin")),
            (None, vec!["--rargo-gc"], fake(&[]), remote(&["__gc"], &[], "admin")),
            (None, vec!["--rargo-incr-stats"], fake(&[]), remote(&["__incr"], &[], "admin")),
            (None, vec!["--rargo-compact"], fake(&[]), remote(&["__compact"], &[], "admin")),
            (None, vec!["--rargo-export", "/srv/x"], fake(&[]), remote(&["__export", "/srv/x"], &[], "admin")),
            (None, vec!["--rargo-import", "/srv/x"], fake(&[]), remote(&["__import", "/srv/x"], &[], "admin")),
            (None, vec!["--rargo-export"], fake(&[]), usage("usage: rargo --rargo-export DIRECTORY-ON-THE-SERVER")),
            (None, vec!["--rargo-import"], fake(&[]), usage("usage: rargo --rargo-import DIRECTORY-ON-THE-SERVER")),
            (None, vec!["--rargo-daemon-status", "x"], fake(&[]), remote(&[], &[], "status")),
            (None, vec!["--rargo-daemon-stop"], fake(&[]), Plan::StopDaemons),
            (None, vec!["--rargo-daemon-sync"], fake(&[]), remote(&[], &[], "sync")),
            (None, vec!["--rargo-drift-check", "x"], fake(&[]), remote(&["__drift"], &[], "drift")),
            (None, vec!["clean"], fake(&[]), Plan::Cargo { release: true, banner: None }),
            (None, vec!["clean", "-p", "x"], fake(&[]), Plan::Cargo { release: true, banner: None }),
            (None, vec!["clean", "-vn"], fake(&[]), cargo.clone()),
            (None, vec!["clean", "--dry-run"], fake(&[]), cargo.clone()),
            (None, vec!["clean", "--help"], fake(&[]), cargo.clone()),
            (None, vec!["check", "--version"], fake(&[]), cargo.clone()),
            (None, vec!["test", "-h"], fake(&[]), cargo.clone()),
            (None, vec!["metadata"], fake(&[]), cargo.clone()),
            (None, vec!["build", "--unit-graph"], fake(&[]), usage("error: rargo does not support --unit-graph for remote builds; use local Cargo to get a unit graph with local paths")),
            (None, vec!["build", "--", "--unit-graph"], fake(&[]), remote(&["build", "--", "--unit-graph"], &[], "build")),
            (None, vec!["--color", "always", "b"], fake(&[]), remote(&["--color", "always", "b"], &[], "build")),
            (
                None,
                vec!["+nightly", "check", "--", "-h"],
                fake(&[("RUSTUP_TOOLCHAIN", "stable"), ("RUSTFLAGS", "-W")]),
                fail("rargo cannot use `+nightly` for remote builds. Select Rust with `rust-toolchain.toml` when using the default flake, or in your project's `flake.nix`."),
            ),
            (None, vec!["+nosuchtoolchain", "run"], fake(&[]), fail("rargo cannot use `+nosuchtoolchain` for remote builds. Select Rust with `rust-toolchain.toml` when using the default flake, or in your project's `flake.nix`.")),
            (None, vec!["run"], fake(&[("RUSTUP_TOOLCHAIN", "1.70.0")]), fail("rargo cannot use `RUSTUP_TOOLCHAIN` for remote builds. Select Rust with `rust-toolchain.toml` when using the default flake, or in your project's `flake.nix`.")),
            (None, vec!["check"], fake(&[("RUSTUP_TOOLCHAIN", "")]), remote(&["check"], &[("RUSTUP_TOOLCHAIN", "")], "build")),
            (None, vec!["run", "--", "+nightly"], fake(&[]), remote(&["run", "--", "+nightly"], &[], "build")),
            (None, vec!["+nightly", "metadata"], fake(&[("RUSTUP_TOOLCHAIN", "stable")]), cargo.clone()),
            (None, vec!["+nightly", "check"], fake(&[("RARGO_BYPASS", "1")]), cargo.clone()),
            (None, vec!["build"], fake(&[]).no_creds(), fail("this machine is not authenticated to h.example: run `rargo auth`, which also says how to get access if the server has not let you in yet")),
            (None, vec!["--rargo-gc"], fake(&[]).no_creds(), fail("this machine is not authenticated to h.example: run `rargo auth`, which also says how to get access if the server has not let you in yet")),
            (None, vec!["build"], fake(&[("RARGO_FALLBACK", "cargo")]).no_creds(), Plan::Cargo { release: false, banner: None }),
            (None, vec!["--rargo-gc"], fake(&[("RARGO_FALLBACK", "cargo")]).no_creds(), fail("this machine is not authenticated to h.example: run `rargo auth`, which also says how to get access if the server has not let you in yet")),
            (None, vec!["--rargo-drift-check"], fake(&[]).no_creds(), fail("this machine is not authenticated to h.example: run `rargo auth`, which also says how to get access if the server has not let you in yet")),
            (None, vec!["--rargo-daemon-stop"], fake(&[("USER", "u")]).no_creds(), Plan::StopDaemons),
            (None, vec!["build"], fake(&[("RARGO_SERVER", "local.example"), ("USER", "u")]).no_creds(), Plan::Remote { config: conf("local.example", "127.0.0.1:7459", "u", None), args: strings(&["build"]), vars: vec![("RARGO_SERVER".into(), "local.example".into()), ("USER".into(), "u".into())], kind: "build" }),
            (None, vec!["build"], fake(&[]).config(Ok(Some("host ="))), fail(&toml_error("host ="))),
            (None, vec!["build"], fake(&[]).config(Ok(Some("x = 1"))), fail(&not_named)),
            (None, vec!["build"], fake(&[("RARGO_SERVER", "gone.example")]), fail("no answer from gone.example")),
        ];
        for (argv0, words, world, expected) in cases {
            assert_eq!(plan(&world, argv0.map(OsStr::new), &os(&words)), expected, "{argv0:?} {words:?}");
        }
        use std::os::unix::ffi::OsStringExt;
        let not_utf8 = vec![OsString::from("build"), OsString::from_vec(vec![0xff])];
        assert_eq!(plan(&fake(&[]), None, &not_utf8), Plan::Cargo { release: false, banner: None });
    }

    #[test]
    fn local_answers_ask_nothing_and_remote_ones_ask_only_what_they_need() {
        for words in [vec!["status"], vec!["--rargo-version"], vec!["@x"], vec!["metadata"], vec!["auth", "-h"], vec!["auth", "invite", "--help"], vec!["help", "auth"], vec!["--rargo-bogus"], vec!["auth", "x.example"], vec!["--rargo-daemon", "s"]] {
            let w = fake(&[]);
            plan(&w, None, &os(&words));
            assert_eq!(w.asked(), Vec::<String>::new(), "{words:?}");
        }
        let w = fake(&[]);
        plan(&w, None, &os(&["build"]));
        assert_eq!(w.asked(), ["config", "resolve h.example", "credential h.example"]);
        let w = fake(&[]);
        plan(&w, None, &os(&["auth", "remove", "x.example"]));
        assert_eq!(w.asked(), ["credential x.example"]);
    }

    #[test]
    fn a_manifest_named_in_another_worktree_moves_the_command_there_with_its_path_flags_kept_meaning_the_same() {
        let root_of = |dir: &Path| ["/work/demo", "/home/ann/other"].iter().map(PathBuf::from).find(|r| dir.starts_with(r));
        let manifest = Path::new("/work/demo/Cargo.toml");
        let words = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        type Case<'a> = (&'a str, Vec<&'a str>, Option<Vec<&'a str>>);
        let cases: Vec<Case> = vec![
            ("/work", vec!["build", "--manifest-path", "demo/Cargo.toml"], Some(vec!["build", "--manifest-path", "/work/demo/Cargo.toml"])),
            ("/work", vec!["build", "--manifest-path=demo/Cargo.toml"], Some(vec!["build", "--manifest-path=/work/demo/Cargo.toml"])),
            ("/home/ann/other", vec!["test", "--manifest-path", "/work/demo/Cargo.toml"], Some(vec!["test", "--manifest-path", "/work/demo/Cargo.toml"])),
            (
                "/work",
                vec!["build", "--manifest-path", "demo/Cargo.toml", "--target-dir", "t", "--artifact-dir=out", "--", "--target-dir", "x"],
                Some(vec!["build", "--manifest-path", "/work/demo/Cargo.toml", "--target-dir", "/work/t", "--artifact-dir=/work/out", "--", "--target-dir", "x"]),
            ),
            ("/work/demo/src", vec!["build", "--manifest-path", "../Cargo.toml"], None),
            ("/work", vec!["build"], None),
            ("/work", vec!["run", "--", "--manifest-path", "demo/Cargo.toml"], None),
        ];
        for (cwd, args, expected) in cases {
            let got = rehome(Path::new(cwd), &words(&args), manifest, root_of);
            let expected = expected.map(|a| (PathBuf::from("/work/demo"), words(&a)));
            assert_eq!(got, expected, "{cwd} {args:?}");
        }
        assert_eq!(rehome(Path::new("/work"), &words(&["build", "--manifest-path", "x/Cargo.toml"]), Path::new("/work/x/Cargo.toml"), root_of), None);
    }

    #[test]
    fn a_routed_command_with_no_manifest_fails_at_once_without_asking_for_a_server_a_credential_or_a_daemon() {
        let none = |text: &str| Plan::Fail { text: format!("error: {text}"), code: 101 };
        let not_found = none("could not find `Cargo.toml` in `/w/app/src` or any parent directory");
        let cases: Vec<(Vec<&str>, Fake, Option<Plan>)> = vec![
            (vec!["build"], fake(&[]).files(&[]), Some(not_found.clone())),
            (vec!["+nightly", "test", "-p", "x"], fake(&[]).files(&[]), Some(not_found.clone())),
            (vec!["nextest", "run"], fake(&[]).files(&[]), Some(not_found.clone())),
            (vec!["build"], fake(&[]).files(&["/w/Cargo.toml"]), None),
            (vec!["build"], fake(&[]).files(&["/w/app/src/Cargo.toml"]), None),
            (vec!["build", "--manifest-path", "../other/Cargo.toml"], fake(&[]), Some(none("manifest path `../other/Cargo.toml` does not exist"))),
            (vec!["build", "--manifest-path=../other/Cargo.toml"], fake(&[]), Some(none("manifest path `../other/Cargo.toml` does not exist"))),
            (vec!["build", "--manifest-path", "../other/Cargo.toml"], fake(&[]).files(&["/w/app/src/../other/Cargo.toml"]), None),
            (vec!["run", "--", "--manifest-path", "missing"], fake(&[]), None),
            (vec!["--rargo-status"], fake(&[]).files(&[]), None),
            (vec!["--rargo-daemon-status"], fake(&[]).files(&[]), None),
        ];
        for (words, w, failure) in cases {
            let got = plan(&w, None, &os(&words));
            match failure {
                Some(expected) => {
                    assert_eq!(got, expected, "{words:?}");
                    assert_eq!(w.asked(), Vec::<String>::new(), "{words:?} asked the world");
                }
                None => assert!(matches!(got, Plan::Remote { .. }), "{words:?}: {got:?}"),
            }
        }
    }

    #[test]
    fn nextest_gets_rargos_own_version_unless_the_user_set_one() {
        let version = |vars: &[(&str, &str)], words: &[&str], have: Option<&str>| {
            let mut w = fake(vars);
            w.nextest = have.map(str::to_owned);
            let Plan::Remote { vars, .. } = plan(&w, None, &os(words)) else { panic!("{words:?}") };
            let found = vars.iter().find(|(k, _)| k == "RARGO_NEXTEST_VERSION").map(|(_, v)| v.clone());
            (found, w.asked().contains(&"nextest".to_string()))
        };
        assert_eq!(version(&[], &["nextest", "run"], Some("0.9.100")), (Some("0.9.100".into()), true));
        assert_eq!(version(&[("RARGO_NEXTEST_VERSION", "0.9.1")], &["nextest", "run"], Some("0.9.100")), (Some("0.9.1".into()), false));
        assert_eq!(version(&[], &["--color", "never", "nextest", "-h"], Some("0.9.100")), (Some("0.9.100".into()), true));
        assert_eq!(version(&[], &["nextest", "run"], None), (None, true));
        assert_eq!(version(&[], &["test"], Some("0.9.100")), (None, false));
    }

    #[test]
    fn configuration_follows_each_precedence_rule() {
        let token = format!("tid.{SECRET}");
        type Rule<'a> = (&'a str, Fake, Result<Config, String>, Vec<&'a str>);
        let cases: Vec<Rule> = vec![
            ("the environment names the server before the file", fake(&[("RARGO_SERVER", "e.example:1")]).config(Err("unreadable")), Ok(conf("e.example:1", "e.example:1", "user", None)), vec!["resolve e.example:1", "credential e.example:1"]),
            ("an empty RARGO_SERVER is unset", fake(&[("RARGO_SERVER", "")]), Ok(stored()), vec!["config", "resolve h.example", "credential h.example"]),
            ("the daemon is given its server and asks no one", fake(&[("RARGO_SERVER", "h.example"), ("RARGO_DAEMON_CONFIG", "x"), ("USER", "u")]), Ok(conf("h.example", "h.example", "u", None)), vec![]),
            ("the daemon is never given a server by the file", fake(&[("RARGO_DAEMON_CONFIG", "x")]), Err("the daemon was started without a server".into()), vec![]),
            ("RARGO_TOKEN in the daemon", fake(&[("RARGO_SERVER", "h.example"), ("RARGO_DAEMON_CONFIG", "x"), ("RARGO_TOKEN", &token)]), Ok(conf("h.example", "h.example", "tid", Some(SECRET))), vec![]),
            ("RARGO_TOKEN before the stored token", fake(&[("RARGO_TOKEN", &token)]), Ok(conf("h.example", "srv.example:7459", "tid", Some(SECRET))), vec!["config", "resolve h.example"]),
            ("the stored token before USER", fake(&[("USER", "u")]), Ok(stored()), vec!["config", "resolve h.example", "credential h.example"]),
            ("USER before LOGNAME", fake(&[("USER", "u"), ("LOGNAME", "l")]).no_creds(), Ok(conf("h.example", "srv.example:7459", "u", None)), vec!["config", "resolve h.example", "credential h.example"]),
            ("LOGNAME without USER", fake(&[("USER", ""), ("LOGNAME", "l")]).no_creds(), Ok(conf("h.example", "srv.example:7459", "l", None)), vec!["config", "resolve h.example", "credential h.example"]),
            ("`user` without either", fake(&[]).no_creds(), Ok(conf("h.example", "srv.example:7459", "user", None)), vec!["config", "resolve h.example", "credential h.example"]),
            ("a bad RARGO_TOKEN", fake(&[("RARGO_TOKEN", "bad")]), Err(format!("RARGO_TOKEN is not a token: {}", project::TOKEN_FORM)), vec!["config", "resolve h.example"]),
            ("discovery fails before a bad token is read", fake(&[("RARGO_SERVER", "gone.example"), ("RARGO_TOKEN", "bad")]), Err("no answer from gone.example".into()), vec!["resolve gone.example"]),
            ("a file that cannot be read", fake(&[]).config(Err("/x: permission denied")), Err("/x: permission denied".into()), vec!["config"]),
            ("a file that is not TOML", fake(&[]).config(Ok(Some("host = \"a"))), Err(toml_error("host = \"a")), vec!["config"]),
            ("a host that is not a string", fake(&[]).config(Ok(Some("host = 7"))), Err(format!("{PATH}: `host` is not a string")), vec!["config"]),
        ];
        for (rule, world, expected, asked) in cases {
            assert_eq!(config(&world), expected, "{rule}");
            assert_eq!(world.asked(), asked, "{rule}");
        }
    }

    #[test]
    fn what_auth_and_naming_a_server_say() {
        let path = Path::new(PATH);
        let next = format!("{PATH} now names `x.example:1` as your rargo server.\nNext: `rargo auth`, then `rargo build`.");
        assert_eq!(named(&fake(&[]), "x.example:1", path), next);
        assert_eq!(named(&fake(&[("RARGO_TOKEN", "t")]), "x.example:1", path), format!("{PATH} now names `x.example:1` as your rargo server."));
        assert_eq!(named(&fake(&[]), "h.example", path), format!("{PATH} now names `h.example` as your rargo server."));
        assert_eq!(
            named(&fake(&[]), "gone.example", path),
            format!("{PATH} now names `gone.example` as your rargo server.\nwarning: no rargo server answers for `gone.example` right now (https://gone.example/.well-known/rargo.json)")
        );

        let w = fake(&[]);
        assert_eq!(may_name(&w, "h.example", false), Ok(()));
        assert_eq!(may_name(&w, "x.example:1", false), Ok(()));
        assert_eq!(may_name(&w, "gone.example", true), Ok(()));
        assert_eq!(may_name(&w, "gone.example", false), Err("no answer from gone.example; nothing was changed (`rargo @gone.example --force` names it anyway)".into()));
        assert_eq!(w.asked(), ["resolve h.example", "resolve gone.example"]);
        let mut w = fake(&[]);
        w.servers.push(("typo.example".into(), Err(project::not_found_text("typo.example", "no such host"))));
        assert_eq!(
            may_name(&w, "typo.example", false),
            Err("cannot find the rargo server `typo.example`: no machine has that name; nothing was changed (`rargo @typo.example --force` names it anyway)".into())
        );

        let mut w = fake(&[]);
        w.creds.push(("named.example:1".into(), cred("t2", "ann")));
        assert_eq!(auth_status(&w, "h.example", Some(&HostFrom::Env)), "server named by RARGO_SERVER\nh.example (server srv.example:7459): authenticated (token stored)");
        assert_eq!(auth_status(&w, "named.example:1", Some(&HostFrom::File(PATH.into()))), format!("server named by {PATH}\nnamed.example:1 (server named.example:1): authenticated as ann (token t2)"));
        assert_eq!(auth_status(&w, "gone.example", None), "gone.example (server unknown (no answer from gone.example)): not authenticated; run `rargo auth`");

        let mut w = fake(&[]);
        w.open.push("127.0.0.1:27097".into());
        assert_eq!(auth_status(&w, "127.0.0.1:27097", None), "127.0.0.1:27097 (server 127.0.0.1:27097): authentication disabled; nothing needs doing");
        assert_eq!(
            named(&w, "127.0.0.1:27097", path),
            format!("{PATH} now names `127.0.0.1:27097` as your rargo server.\nThis server has authentication disabled. Next: `rargo build`.")
        );

        let ann = format!("t9.{SECRET}");
        let stranger = format!("t8.{SECRET}");
        let offline = format!("t7.{SECRET}");
        for (token, said) in [
            (ann.as_str(), "authenticated as ann by RARGO_TOKEN (token t9)".to_string()),
            (stranger.as_str(), "the server does not accept RARGO_TOKEN (token t8): it does not know it, or it was revoked or has expired".to_string()),
            (offline.as_str(), "RARGO_TOKEN (token t7) is used; the server could not say whose it is (no answer)".to_string()),
            ("bad", format!("RARGO_TOKEN is not a token: {}", project::TOKEN_FORM)),
        ] {
            let mut w = fake(&[("RARGO_TOKEN", token)]);
            w.holders = vec![(ann.clone(), Ok(Some("ann".into()))), (offline.clone(), Err("no answer".into()))];
            assert_eq!(auth_status(&w, "h.example", None), format!("h.example (server srv.example:7459): {said}"), "{token}");
        }

        assert_eq!(removed("h", Ok(true)), ("revoked the token for h on the server and forgot it here".into(), None));
        assert_eq!(removed("h", Ok(false)), ("forgot the token for h (the server did not know it as a token that still works)".into(), None));
        assert_eq!(
            removed("h", Err("offline".into())),
            ("forgot the token for h on this machine".into(), Some("warning: the server could not be told to revoke it (offline); it keeps working for whoever has a copy until an administrator runs `rargod token revoke`".into()))
        );
    }

    #[test]
    fn painting_adds_only_colour_and_only_where_the_terminal_and_environment_allow() {
        let painted = paint(BANNER);
        let plain = painted.replace("\x1b[1;33m", "").replace("\x1b[0m", "");
        assert_eq!(plain, BANNER);
        for bold in ["Usage:", "rargo [CARGO COMMAND...]", "rargo's own commands:", "@HOST", "auth", "status", "--rargo-version"] {
            assert!(painted.contains(&format!("\x1b[1;33m{bold}\x1b[0m")), "{bold}");
        }
        assert_eq!(painted.matches("\x1b[1;33m").count(), 7);
        assert_eq!(paint(&version()), version());
        let vars = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<Vec<_>>();
        for (pairs, tty, expected) in [
            (vec![], true, true),
            (vec![], false, false),
            (vec![("NO_COLOR", "1")], true, false),
            (vec![("NO_COLOR", "")], true, true),
            (vec![("CARGO_TERM_COLOR", "always")], false, true),
            (vec![("CARGO_TERM_COLOR", "always"), ("NO_COLOR", "1")], false, true),
            (vec![("CARGO_TERM_COLOR", "never")], true, false),
            (vec![("CARGO_TERM_COLOR", "auto")], true, true),
            (vec![("CARGO_TERM_COLOR", "auto")], false, false),
        ] {
            assert_eq!(colour(&vars(&pairs), tty), expected, "{pairs:?} {tty}");
        }
    }

    #[test]
    fn no_help_calls_the_server_the_projects_and_the_banner_names_rargos_commands() {
        for text in [AUTH_HELP, STATUS_HELP, BANNER] {
            assert!(!text.contains("project's"), "{text}");
        }
        for command in ["@HOST", "auth", "status", "--rargo-version"] {
            assert!(BANNER.contains(command), "{command}");
        }
    }

    #[test]
    fn every_rargo_option_is_planned_as_rargos_own() {
        for option in RARGO_OPTIONS {
            let got = plan(&fake(&[]), None, &os(&[option, "x"]));
            assert!(!matches!(got, Plan::Cargo { .. }), "{option}: {got:?}");
            assert!(!matches!(&got, Plan::Fail { text, .. } if text.contains("unknown option")), "{option}: {got:?}");
        }
    }

    #[test]
    fn loopback_servers_and_subcommands() {
        for (server, local) in [("localhost:7", true), ("127.0.0.1:7459", true), ("::1:7", true), ("10.0.0.1:7", false), ("srv.example:7", false), ("localhost", true)] {
            assert_eq!(is_loopback(server), local, "{server}");
        }
        assert_eq!(subcommand(&strings(&["--color", "always", "build"])), Some("build"));
        assert_eq!(subcommand(&strings(&["-C", "dir", "-Z", "x", "--config", "k=v", "--explain", "E1", "t"])), Some("t"));
        assert_eq!(subcommand(&strings(&["-q", "test", "--", "x"])), Some("test"));
        assert_eq!(subcommand(&strings(&["--", "build"])), None);
    }
}
