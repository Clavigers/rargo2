use std::path::{Path, PathBuf};
use std::time::Duration;

const DISCOVERY_FRESH: Duration = Duration::from_secs(24 * 3600);

pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| crate::home_dir().join(".config")).join("rargo")
}

pub const CONFIG: &str = "config.rargo";

pub fn workspace_root(cwd: &Path) -> Option<PathBuf> {
    let (manifest, _) = rargo_proto::workspace::manifest_for(cwd, |p| std::fs::read_to_string(p).ok())?;
    manifest.parent().map(Path::to_path_buf)
}

pub fn project_host(cwd: &Path) -> Result<Option<(String, PathBuf)>, String> {
    let Some(path) = workspace_root(cwd).map(|r| r.join(CONFIG)) else { return Ok(None) };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let table: toml::Table = text.parse().map_err(|e| format!("{}: {e}", path.display()))?;
    match table.get("host") {
        None => Ok(None),
        Some(toml::Value::String(h)) => Ok(Some((h.clone(), path))),
        Some(_) => Err(format!("{}: `host` is not a string", path.display())),
    }
}

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

pub fn set_host(cwd: &Path, host: &str) -> Result<PathBuf, String> {
    if !valid_host(host) {
        return Err(format!("`{host}` is not a host: write a name (rargo.rs) or a name and a port (build.example:7459)"));
    }
    let Some(root) = workspace_root(cwd) else {
        return Err("no Cargo.toml here or above: `rargo @host` is run inside a cargo project".into());
    };
    let path = root.join(CONFIG);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("{}: {e}", path.display()))?;
    doc["host"] = toml_edit::value(host);
    let new = doc.to_string();
    if new != text {
        crate::write_atomic(&path, new.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    exclude_from_git(&root).map_err(|e| format!("keeping {} out of git: {e}", path.display()))?;
    Ok(path)
}

fn exclude_from_git(root: &Path) -> Result<(), String> {
    let git = |args: &[&str]| -> Option<String> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim_end_matches('\n').to_string())
    };
    let Some(top) = git(&["rev-parse", "--show-toplevel"]) else { return Ok(()) };
    if git(&["check-ignore", "-q", CONFIG]).is_some() {
        return Ok(());
    }
    let exclude = git(&["rev-parse", "--git-path", "info/exclude"]).ok_or("git does not say where its exclude file is")?;
    let exclude = root.join(exclude);
    let top = std::fs::canonicalize(&top).map_err(|e| format!("{top}: {e}"))?;
    let here = std::fs::canonicalize(root).map_err(|e| format!("{}: {e}", root.display()))?;
    let rel = here.strip_prefix(&top).map_err(|_| format!("{} is not inside {}", here.display(), top.display()))?;
    let mut pattern = String::from("/");
    for c in rel.join(CONFIG).to_string_lossy().chars() {
        if matches!(c, '*' | '?' | '[' | '\\' | ' ') {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    if let Some(dir) = exclude.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let mut text = std::fs::read_to_string(&exclude).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&pattern);
    text.push('\n');
    std::fs::write(&exclude, text).map_err(|e| format!("{}: {e}", exclude.display()))
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(5)).timeout(Duration::from_secs(15)).build()
}

pub fn base_url(host: &str) -> String {
    if let Ok(b) = std::env::var("RARGO_AUTH_URL") {
        return b.trim_end_matches('/').to_string();
    }
    if host.contains(':') { format!("http://{host}") } else { format!("https://{host}") }
}

pub fn server_status(host: &str) -> Result<serde_json::Value, String> {
    let url = format!("{}/.well-known/rargo.json", base_url(host));
    let response = agent().get(&url).call().map_err(|err| err.to_string())?;
    let body = response.into_string().map_err(|err| err.to_string())?;
    let status: serde_json::Value = serde_json::from_str(&body).map_err(|err| err.to_string())?;
    status.get("server").and_then(|s| s.as_str()).ok_or("not a rargo server")?;
    Ok(status)
}

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
        Err(e) => read(false).ok_or_else(|| format!("cannot find the build server of `{host}` ({url}): {e}")),
    }
}

#[derive(Clone)]
pub struct Credential {
    pub id: String,
    pub secret: String,
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
    Some(Credential { id: get("id")?, secret: get("secret")?, name: get("name").unwrap_or_default() })
}

fn save_credentials(all: &toml::Table) -> Result<(), String> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let path = credentials_path();
    let dir = config_dir();
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
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

pub fn split_token(token: &str) -> Option<(&str, &str)> {
    let (id, secret) = token.trim().split_once('.')?;
    (!id.is_empty() && secret.len() >= 16).then_some((id, secret))
}

pub fn store_token(host: &str, token: &str, name: &str) -> Result<(), String> {
    let (id, secret) = split_token(token).ok_or("that is not a token (a token is an id, a dot and a secret)")?;
    let mut all = credentials();
    let mut c = toml::Table::new();
    c.insert("id".into(), id.into());
    c.insert("secret".into(), secret.into());
    c.insert("name".into(), name.into());
    all.insert(host.to_string(), toml::Value::Table(c));
    save_credentials(&all)
}

pub fn revoke_on_server(host: &str) -> Result<bool, String> {
    let Some(c) = credential(host) else { return Ok(false) };
    let url = format!("{}/auth/revoke", base_url(host));
    let body = serde_json::json!({ "token": format!("{}.{}", c.id, c.secret) });
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

pub fn holder(host: &str, token: &str) -> Result<Option<String>, String> {
    split_token(token).ok_or("that is not a token (a token is an id, a dot and a secret)")?;
    let url = format!("{}/auth/whoami", base_url(host));
    let body = serde_json::json!({ "token": token.trim() });
    let text = agent()
        .post(&url)
        .set("Content-Type", "application/json")
        .send_string(&body.to_string())
        .map_err(|e| format!("{url}: {e}"))?
        .into_string()
        .map_err(|e| format!("{url}: {e}"))?;
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("{url}: not a rargo answer: {e}"))?;
    Ok(v.get("user").and_then(|u| u.as_str()).map(String::from))
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

    fn host(cwd: &Path) -> Option<String> {
        project_host(cwd).unwrap().map(|(h, _)| h)
    }

    #[test]
    fn at_host_writes_config_rargo_at_the_workspace_root_and_leaves_the_manifests() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        std::fs::create_dir_all(ws.join("crates/a/src")).unwrap();
        let manifest = "# our workspace\n[workspace]\nmembers = [\"crates/*\"]   # all of them\nresolver = \"2\"\n";
        let member = "[package]\nname = \"a\"\nversion = \"0.1.0\"\n";
        std::fs::write(ws.join("Cargo.toml"), manifest).unwrap();
        std::fs::write(ws.join("crates/a/Cargo.toml"), member).unwrap();
        let written = set_host(&ws.join("crates/a/src"), "rargo.rs").unwrap();
        assert_eq!(written, ws.join(CONFIG));
        assert_eq!(std::fs::read_to_string(ws.join(CONFIG)).unwrap(), "host = \"rargo.rs\"\n");
        assert_eq!(std::fs::read_to_string(ws.join("Cargo.toml")).unwrap(), manifest);
        assert_eq!(std::fs::read_to_string(ws.join("crates/a/Cargo.toml")).unwrap(), member);
        assert!(!ws.join("crates/a").join(CONFIG).exists());
        assert_eq!(host(&ws.join("crates/a")).as_deref(), Some("rargo.rs"));
        assert_eq!(project_host(ws).unwrap().unwrap().1, ws.join(CONFIG));
        std::fs::write(ws.join(CONFIG), "# mine\nhost = \"rargo.rs\"\nother = 1\n").unwrap();
        set_host(ws, "build.example:7459").unwrap();
        assert_eq!(std::fs::read_to_string(ws.join(CONFIG)).unwrap(), "# mine\nhost = \"build.example:7459\"\nother = 1\n");
        assert_eq!(host(ws).as_deref(), Some("build.example:7459"));
        assert!(!ws.join(".git").exists());
    }

    #[test]
    fn a_host_in_cargo_toml_is_not_read() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"solo\"\nversion = \"0.1.0\"\n\n[package.metadata.rargo]\nhost = \"rargo.rs\"\n").unwrap();
        assert_eq!(host(dir.path()), None);
        std::fs::write(dir.path().join(CONFIG), "host = 7\n").unwrap();
        assert!(project_host(dir.path()).is_err());
    }

    #[test]
    fn a_package_without_a_workspace_gets_its_own_file() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = "[package]\nname = \"solo\"\nversion = \"0.1.0\"\n\n[dependencies]\n";
        std::fs::write(dir.path().join("Cargo.toml"), manifest).unwrap();
        set_host(dir.path(), "rargo.rs").unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("Cargo.toml")).unwrap(), manifest);
        assert_eq!(host(dir.path()).as_deref(), Some("rargo.rs"));
        assert!(set_host(dir.path(), "not a host").is_err());
        assert!(set_host(dir.path(), "h:99999").is_err());
        let empty = tempfile::tempdir().unwrap();
        assert!(set_host(empty.path(), "rargo.rs").is_err());
        assert_eq!(host(empty.path()), None);
    }

    #[test]
    fn in_a_git_checkout_the_file_is_excluded_locally_once() {
        let dir = tempfile::tempdir().unwrap();
        let top = dir.path();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git").arg("-C").arg(top).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8(out.stdout).unwrap()
        };
        git(&["init", "-q"]);
        let ws = top.join("rust [1]");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("Cargo.toml"), "[package]\nname = \"p\"\nversion = \"0.1.0\"\n").unwrap();
        set_host(&ws, "rargo.rs").unwrap();
        set_host(&ws, "build.example:7459").unwrap();
        let exclude = std::fs::read_to_string(top.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.matches("/rust\\ \\[1]/config.rargo\n").count(), 1, "{exclude}");
        assert!(!top.join(".gitignore").exists());
        assert_eq!(git(&["status", "--porcelain", "--untracked-files=all"]), "?? \"rust [1]/Cargo.toml\"\n");
        let other = tempfile::tempdir().unwrap();
        let top = other.path();
        std::process::Command::new("git").arg("-C").arg(top).args(["init", "-q"]).status().unwrap();
        std::fs::write(top.join(".gitignore"), "config.rargo\n").unwrap();
        std::fs::write(top.join("Cargo.toml"), "[package]\nname = \"p\"\nversion = \"0.1.0\"\n").unwrap();
        let before = std::fs::read_to_string(top.join(".git/info/exclude")).unwrap_or_default();
        set_host(top, "rargo.rs").unwrap();
        assert_eq!(std::fs::read_to_string(top.join(".git/info/exclude")).unwrap_or_default(), before);
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
