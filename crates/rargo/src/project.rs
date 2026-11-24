//! Which server a project builds on and how this machine proves who it is
//! there: `rargo @host`, `rargo auth`, and what a build reads of both (D67).
//!
//! The server is a property of the project: `host` under
//! `[workspace.metadata.rargo]` (or `[package.metadata.rargo]`) in its
//! `Cargo.toml`, a table cargo ignores. What is personal stays on the
//! machine: one token per server in `credentials.toml` beside the old
//! `config.toml`, readable by its owner only.
//!
//! A host is written either as `name:port`, which is the server's address,
//! or as a bare name, which is asked over HTTPS where its server is
//! (`https://name/.well-known/rargo.json`): a name like `rargo.rs` may be
//! served by something that carries web traffic and nothing else.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long an answer to "where is the server of this name" is used
/// without asking again.
const DISCOVERY_FRESH: Duration = Duration::from_secs(24 * 3600);

pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| crate::home_dir().join(".config")).join("rargo")
}

fn host_in(table: &toml::Table, kind: &str) -> Option<String> {
    table.get(kind)?.get("metadata")?.get("rargo")?.get("host")?.as_str().map(String::from)
}

/// The host the project around `cwd` names: the workspace's if a manifest
/// on the way up has one, else the nearest package's.
pub fn project_host(cwd: &Path) -> Option<String> {
    let mut package: Option<String> = None;
    for dir in cwd.ancestors() {
        let Ok(text) = std::fs::read_to_string(dir.join("Cargo.toml")) else { continue };
        let Ok(table) = text.parse::<toml::Table>() else { continue };
        if let Some(h) = host_in(&table, "workspace") {
            return Some(h);
        }
        if package.is_none() {
            package = host_in(&table, "package");
        }
    }
    package
}

/// A host as a person may type it: a name or an address, an optional port.
pub fn valid_host(host: &str) -> bool {
    let (name, port) = match host.rsplit_once(':') {
        Some((n, p)) => (n, Some(p)),
        None => (host, None),
    };
    !name.is_empty()
        && name.len() <= 253
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        && port.is_none_or(|p| p.parse::<u16>().is_ok())
}

/// `rargo @host`: names the server in the project's manifest, leaving the
/// rest of the file as it is.
pub fn set_host(cwd: &Path, host: &str) -> Result<PathBuf, String> {
    if !valid_host(host) {
        return Err(format!("`{host}` is not a host: write a name (rargo.rs) or a name and a port (build.example:7459)"));
    }
    let Some((path, kind)) = rargo_proto::workspace::manifest_for(cwd, |p| std::fs::read_to_string(p).ok()) else {
        return Err("no Cargo.toml here or above: `rargo @host` is run inside a cargo project".into());
    };
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("{}: {e}", path.display()))?;
    let section = doc[kind].or_insert(toml_edit::table());
    let metadata = section["metadata"].or_insert(toml_edit::table());
    if let Some(t) = metadata.as_table_mut() {
        // `[workspace.metadata.rargo]`, not a `[workspace.metadata]` header
        // with nothing in it.
        t.set_implicit(true);
    }
    let rargo = metadata["rargo"].or_insert(toml_edit::table());
    rargo["host"] = toml_edit::value(host);
    let new = doc.to_string();
    if new != text {
        crate::write_atomic(&path, new.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok(path)
}

/// `rargo --rargo-migrate`: moves what the older files hold into the
/// project's manifest and this machine's credentials. Returns what it did.
///   - `server` of `~/.config/rargo/config.toml` becomes the project's
///     `host`, if the project names none;
///   - its `user` and `key` become the credential for that host, if there
///     is none (the file itself is left: other projects may still use it);
///   - the tables of `.rargo.toml` at the top of the repository move under
///     `[workspace.metadata.rargo]` of its manifest, and the file is removed.
pub fn migrate(cwd: &Path) -> Result<Vec<String>, String> {
    let mut done = Vec::new();
    let config_path = config_dir().join("config.toml");
    let config: toml::Table = std::fs::read_to_string(&config_path).ok().and_then(|t| t.parse().ok()).unwrap_or_default();
    let old = |k: &str| config.get(k).and_then(|v| v.as_str()).map(String::from);
    let mut host = project_host(cwd);
    if host.is_none() {
        if let Some(server) = old("server") {
            let path = set_host(cwd, &server)?;
            done.push(format!("{}: host = \"{server}\" (from {})", path.display(), config_path.display()));
            host = Some(server);
        }
    }
    if let Some(host) = &host {
        let key = match (old("key"), old("key-file")) {
            (Some(k), _) => Some(k),
            (None, Some(f)) => std::fs::read_to_string(&f).ok().map(|s| s.trim().to_string()),
            (None, None) => None,
        };
        if let (None, Some(user), Some(key)) = (credential(host), old("user"), key) {
            // Only if the old file was about this server.
            if old("server").as_deref() == Some(host.as_str()) || old("server").is_none() {
                let mut all = credentials();
                let mut c = toml::Table::new();
                c.insert("user".into(), user.clone().into());
                c.insert("key".into(), key.into());
                c.insert("name".into(), user.clone().into());
                all.insert(host.clone(), toml::Value::Table(c));
                save_credentials(&all)?;
                done.push(format!("{}: the key of {user} for {host} (from {})", credentials_path().display(), config_path.display()));
            }
        }
    }
    // The policy file lies at the top of what is synced.
    let Some(root) = crate::find_root(cwd) else { return Ok(done) };
    let policy_path = root.join(".rargo.toml");
    let Ok(policy_text) = std::fs::read_to_string(&policy_path) else { return Ok(done) };
    let policy: toml_edit::DocumentMut = policy_text.parse().map_err(|e| format!("{}: {e}", policy_path.display()))?;
    let manifest_path = root.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest_path)
        .map_err(|_| format!("{} has a policy file but no Cargo.toml to move it into", root.display()))?;
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("{}: {e}", manifest_path.display()))?;
    let kind = if doc.contains_key("workspace") || !doc.contains_key("package") { "workspace" } else { "package" };
    {
        let section = doc[kind].or_insert(toml_edit::table());
        let metadata = section["metadata"].or_insert(toml_edit::table());
        if let Some(t) = metadata.as_table_mut() {
            t.set_implicit(true);
        }
        let rargo = metadata["rargo"].or_insert(toml_edit::table());
        if let Some(t) = rargo.as_table_mut() {
            t.set_implicit(true);
        }
        let mut moved = Vec::new();
        for (name, item) in policy.iter() {
            if !matches!(name, "env" | "network" | "flake" | "sync") {
                return Err(format!("{}: `{name}` is not a table of the policy; nothing was changed", policy_path.display()));
            }
            if rargo.get(name).is_some() {
                // The manifest's table already wins; the file's goes.
                continue;
            }
            // After everything the manifest has, in the file's order (a
            // table brought from another document keeps the place it had
            // there, which would put it among the manifest's own).
            let mut item = item.clone();
            if let Some(t) = item.as_table_mut() {
                t.set_position(100_000 + moved.len());
                let before = t.decor().prefix().and_then(|p| p.as_str()).unwrap_or("").to_string();
                if !before.starts_with('\n') {
                    t.decor_mut().set_prefix(format!("\n{before}"));
                }
            }
            rargo[name] = item;
            moved.push(format!("[{name}]"));
        }
        if !moved.is_empty() {
            done.push(format!("{}: {} under [{kind}.metadata.rargo] (from {})", manifest_path.display(), moved.join(", "), policy_path.display()));
        }
    }
    let new = doc.to_string();
    if new != text {
        crate::write_atomic(&manifest_path, new.as_bytes()).map_err(|e| format!("{}: {e}", manifest_path.display()))?;
    }
    std::fs::remove_file(&policy_path).map_err(|e| format!("{}: {e}", policy_path.display()))?;
    done.push(format!("removed {}", policy_path.display()));
    Ok(done)
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(5)).timeout(Duration::from_secs(15)).build()
}

/// Where the web side of a host is asked: HTTPS for a name, the server's
/// own port for an address with one.
pub fn base_url(host: &str) -> String {
    if let Ok(b) = std::env::var("RARGO_AUTH_URL") {
        return b.trim_end_matches('/').to_string();
    }
    if host.contains(':') { format!("http://{host}") } else { format!("https://{host}") }
}

/// The address of the server a host stands for.
pub fn resolve(host: &str) -> Result<String, String> {
    if host.contains(':') {
        return Ok(host.to_string());
    }
    let cached = crate::cache_dir().join("servers").join(format!("{host}.json"));
    let read = |fresh_only: bool| -> Option<String> {
        let md = std::fs::metadata(&cached).ok()?;
        if fresh_only && md.modified().ok()?.elapsed().ok()? > DISCOVERY_FRESH {
            return None;
        }
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&cached).ok()?).ok()?;
        v.get("server")?.as_str().map(String::from)
    };
    if let Some(server) = read(true) {
        return Ok(server);
    }
    let url = format!("{}/.well-known/rargo.json", base_url(host));
    let asked = agent().get(&url).call().map_err(|e| e.to_string()).and_then(|r| r.into_string().map_err(|e| e.to_string()));
    let found = asked.and_then(|text| {
        let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("not a rargo answer: {e}"))?;
        let server = v.get("server").and_then(|s| s.as_str()).ok_or("the answer names no server")?.to_string();
        if !valid_host(&server) || !server.contains(':') {
            return Err(format!("the answer names `{server}`, which is not an address"));
        }
        Ok((server, text))
    });
    match found {
        Ok((server, text)) => {
            let _ = crate::write_atomic(&cached, text.as_bytes());
            Ok(server)
        }
        // The answer of another day is better than none.
        Err(e) => read(false).ok_or_else(|| format!("cannot find the build server of `{host}` ({url}): {e}")),
    }
}

#[derive(Clone)]
pub struct Credential {
    /// What is presented as the user: a token's id.
    pub user: String,
    pub key: String,
    /// Who the server says this is.
    pub name: String,
}

fn credentials_path() -> PathBuf {
    config_dir().join("credentials.toml")
}

fn credentials() -> toml::Table {
    std::fs::read_to_string(credentials_path()).ok().and_then(|t| t.parse().ok()).unwrap_or_default()
}

pub fn credential(host: &str) -> Option<Credential> {
    let all = credentials();
    let c = all.get(host)?.as_table()?;
    let get = |k: &str| c.get(k).and_then(|v| v.as_str()).map(String::from);
    Some(Credential { user: get("user")?, key: get("key")?, name: get("name").unwrap_or_default() })
}

fn save_credentials(all: &toml::Table) -> Result<(), String> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let path = credentials_path();
    let dir = config_dir();
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    // Never readable by others, not even for the moment of writing.
    let tmp = dir.join(format!(".credentials.{}", std::process::id()));
    let text = format!("# Tokens for rargo servers, one per host. Written by `rargo auth`; keep it private.\n{all}");
    let write = || -> std::io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().create(true).truncate(true).write(true).mode(0o600).open(&tmp)?;
        f.write_all(text.as_bytes())?;
        std::fs::rename(&tmp, &path)
    };
    write().map_err(|e| format!("{}: {e}", path.display()))
}

/// Splits a token's text (`id.secret`) into what is presented and the key.
pub fn split_token(token: &str) -> Option<(&str, &str)> {
    let (id, secret) = token.trim().split_once('.')?;
    (!id.is_empty() && secret.len() >= 16).then_some((id, secret))
}

pub fn store_token(host: &str, token: &str, name: &str) -> Result<(), String> {
    let (id, secret) = split_token(token).ok_or("that is not a token (a token is an id, a dot and a secret)")?;
    let mut all = credentials();
    let mut c = toml::Table::new();
    c.insert("user".into(), id.into());
    c.insert("key".into(), secret.into());
    c.insert("name".into(), name.into());
    all.insert(host.to_string(), toml::Value::Table(c));
    save_credentials(&all)
}

/// Tells the host's server to revoke the token this machine holds for it.
/// `Ok(true)`: revoked there; `Ok(false)`: the server did not know it as a
/// token that works (already revoked, expired, or a user of a key file).
pub fn revoke_on_server(host: &str) -> Result<bool, String> {
    let Some(c) = credential(host) else { return Ok(false) };
    let url = format!("{}/auth/revoke", base_url(host));
    let body = serde_json::json!({ "token": format!("{}.{}", c.user, c.key) });
    let text = agent()
        .post(&url)
        .set("Content-Type", "application/json")
        .send_string(&body.to_string())
        .map_err(|e| format!("{url}: {e}"))?
        .into_string()
        .map_err(|e| format!("{url}: {e}"))?;
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("{url}: not a rargo answer: {e}"))?;
    Ok(v.get("revoked").and_then(|r| r.as_bool()).unwrap_or(false))
}

pub fn forget(host: &str) -> Result<bool, String> {
    let mut all = credentials();
    let had = all.remove(host).is_some();
    if had {
        save_credentials(&all)?;
    }
    Ok(had)
}

fn open_in_browser(url: &str) {
    if std::env::var_os("RARGO_NO_BROWSER").is_some() {
        return;
    }
    // Only where there is a display to open it on; a failure is nothing
    // (the address is printed either way).
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return;
    }
    let _ = std::process::Command::new("xdg-open")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// `rargo auth`: asks the host's server for a token for this machine, which
/// a person grants on a page in their browser.
pub fn auth(host: &str) -> Result<String, String> {
    let base = base_url(host);
    let post = |path: &str, body: serde_json::Value| -> Result<serde_json::Value, String> {
        let r = agent().post(&format!("{base}{path}")).set("Content-Type", "application/json").send_string(&body.to_string());
        let text = match r {
            Ok(r) => r.into_string().map_err(|e| e.to_string())?,
            Err(ureq::Error::Status(code, r)) => {
                let text = r.into_string().unwrap_or_default();
                let said = serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|v| v.get("error")?.as_str().map(String::from));
                return Err(said.unwrap_or_else(|| format!("{base}{path} answered {code}")));
            }
            Err(e) => return Err(format!("{base}{path}: {e}")),
        };
        serde_json::from_str(&text).map_err(|e| format!("{base}{path}: not a rargo answer: {e}"))
    };
    let started = post("/auth/start", serde_json::json!({ "machine": crate::hostname() }))?;
    let field = |k: &str| started.get(k).and_then(|v| v.as_str()).map(String::from);
    let (Some(device), Some(code), Some(path)) = (field("device"), field("code"), field("path")) else {
        return Err(format!("{base}/auth/start: an answer without a code"));
    };
    let url = format!("{base}{path}");
    eprintln!("To let this machine build on {host}, open\n\n    {url}\n\nand confirm there that it shows this code:\n\n    {code}\n");
    open_in_browser(&url);
    eprintln!("Waiting for that (ten minutes at most; any machine's browser will do) ...");
    let interval = Duration::from_secs(started.get("interval").and_then(|v| v.as_u64()).unwrap_or(2).clamp(1, 30));
    let deadline = std::time::Instant::now() + Duration::from_secs(started.get("expires_in").and_then(|v| v.as_u64()).unwrap_or(600));
    let mut failures = 0;
    loop {
        std::thread::sleep(interval);
        if std::time::Instant::now() > deadline {
            return Err("nobody confirmed the code in time; run `rargo auth` again".into());
        }
        let answer = match post("/auth/poll", serde_json::json!({ "device": device })) {
            Ok(a) => a,
            // The network may hiccup while a person reads a page.
            Err(e) => {
                failures += 1;
                if failures > 10 {
                    return Err(e);
                }
                continue;
            }
        };
        failures = 0;
        match answer.get("status").and_then(|s| s.as_str()) {
            Some("pending") => {}
            Some("approved") => {
                let token = answer.get("token").and_then(|t| t.as_str()).ok_or("approved, but no token came")?;
                let name = answer.get("user").and_then(|t| t.as_str()).unwrap_or("");
                store_token(host, token, name)?;
                return Ok(name.to_string());
            }
            Some("denied") => return Err("the code was refused (too many wrong invitations); run `rargo auth` again".into()),
            _ => return Err("the code is no longer valid; run `rargo auth` again".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_host_writes_the_table_and_keeps_the_rest_of_the_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        std::fs::create_dir_all(ws.join("crates/a/src")).unwrap();
        let manifest = "# our workspace\n[workspace]\nmembers = [\"crates/*\"]   # all of them\nresolver = \"2\"\n\n[workspace.dependencies]\nserde = \"1\"\n";
        std::fs::write(ws.join("Cargo.toml"), manifest).unwrap();
        std::fs::write(ws.join("crates/a/Cargo.toml"), "[package]\nname = \"a\"\nversion = \"0.1.0\"\n").unwrap();
        // From inside a member: the workspace's manifest is the one.
        let written = set_host(&ws.join("crates/a/src"), "rargo.rs").unwrap();
        assert_eq!(written, ws.join("Cargo.toml"));
        let text = std::fs::read_to_string(ws.join("Cargo.toml")).unwrap();
        assert!(text.starts_with(manifest), "what was there is untouched:\n{text}");
        assert!(text.contains("[workspace.metadata.rargo]\nhost = \"rargo.rs\"\n"), "{text}");
        assert!(!text.contains("[workspace.metadata]\n"), "{text}");
        assert_eq!(project_host(&ws.join("crates/a")).as_deref(), Some("rargo.rs"));
        // Again with another host: one entry, changed in place.
        set_host(ws, "build.example:7459").unwrap();
        let text = std::fs::read_to_string(ws.join("Cargo.toml")).unwrap();
        assert_eq!(text.matches("host =").count(), 1);
        assert_eq!(project_host(ws).as_deref(), Some("build.example:7459"));
        assert!(std::fs::read_to_string(ws.join("crates/a/Cargo.toml")).unwrap().find("metadata").is_none());
    }

    #[test]
    fn the_policy_file_moves_into_the_manifest_and_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        std::fs::create_dir(ws.join(".git")).unwrap();
        let manifest = "# ours\n[workspace]\nmembers = [\"crates/*\"]\n\n[workspace.metadata.rargo]\nhost = \"build.example:7459\"\n";
        std::fs::write(ws.join("Cargo.toml"), manifest).unwrap();
        std::fs::write(ws.join(".rargo.toml"), "# what tests may see\n[env]\npass = [\"DATABASE_URL\"]   # the test database\n\n[sync]\nexclude = [\"node_modules\"]\n").unwrap();
        let done = migrate(ws).unwrap();
        assert!(done.iter().any(|l| l.contains("[env], [sync]")), "{done:?}");
        assert!(!ws.join(".rargo.toml").exists());
        let text = std::fs::read_to_string(ws.join("Cargo.toml")).unwrap();
        assert!(text.starts_with(manifest), "what was there is untouched:\n{text}");
        assert!(text.contains("[workspace.metadata.rargo.env]\npass = [\"DATABASE_URL\"]   # the test database"), "{text}");
        assert!(text.contains("[workspace.metadata.rargo.sync]\nexclude = [\"node_modules\"]"), "{text}");
        // What it wrote is what the policy reader takes.
        let t: toml::Table = text.parse().unwrap();
        assert_eq!(t["workspace"]["metadata"]["rargo"]["env"]["pass"][0].as_str(), Some("DATABASE_URL"));
        // Nothing left to do.
        assert!(migrate(ws).unwrap().is_empty());
        // A file with something that is no policy table changes nothing.
        std::fs::write(ws.join(".rargo.toml"), "[bogus]\nx = 1\n").unwrap();
        assert!(migrate(ws).is_err());
        assert!(ws.join(".rargo.toml").exists());
    }

    #[test]
    fn a_package_without_a_workspace_gets_package_metadata() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"solo\"\nversion = \"0.1.0\"\n\n[dependencies]\n").unwrap();
        set_host(dir.path(), "rargo.rs").unwrap();
        let text = std::fs::read_to_string(dir.path().join("Cargo.toml")).unwrap();
        assert!(text.contains("[package.metadata.rargo]\nhost = \"rargo.rs\"\n"), "{text}");
        assert_eq!(project_host(dir.path()).as_deref(), Some("rargo.rs"));
        assert!(set_host(dir.path(), "not a host").is_err());
        assert!(set_host(dir.path(), "h:99999").is_err());
        let empty = tempfile::tempdir().unwrap();
        assert!(set_host(empty.path(), "rargo.rs").is_err());
    }

    #[test]
    fn hosts_and_tokens_are_told_from_what_they_are_not() {
        assert!(valid_host("rargo.rs") && valid_host("build.example:7459") && valid_host("10.0.0.1:7459"));
        assert!(!valid_host("") && !valid_host("a b") && !valid_host("http://x") && !valid_host(":7459"));
        assert_eq!(base_url("build.example:7459"), "http://build.example:7459");
        assert_eq!(base_url("rargo.rs"), "https://rargo.rs");
        assert_eq!(resolve("build.example:7459").unwrap(), "build.example:7459");
        assert_eq!(split_token("t-abc.0123456789abcdef0123"), Some(("t-abc", "0123456789abcdef0123")));
        assert!(split_token("no-dot").is_none() && split_token("t-abc.short").is_none());
    }
}
