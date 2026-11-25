use std::path::{Path, PathBuf};
use std::time::Duration;

const DISCOVERY_FRESH: Duration = Duration::from_secs(24 * 3600);

pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| crate::home_dir().join(".config")).join("rargo")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn read_config(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

pub fn parse_host(path: &Path, text: &str) -> Result<Option<String>, String> {
    let table: toml::Table = text.parse().map_err(|e| format!("{}: {e}", path.display()))?;
    match table.get("host") {
        None => Ok(None),
        Some(toml::Value::String(h)) => Ok(Some(h.clone())),
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

pub fn set_host(host: &str) -> Result<PathBuf, String> {
    set_host_in(&config_path(), host)
}

fn set_host_in(path: &Path, host: &str) -> Result<PathBuf, String> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    if !valid_host(host) {
        return Err(format!("`{host}` is not a host: write a name (rargo.rs) or a name and a port (build.example:7459)"));
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let new = set_host_text(path, host, &text)?;
    if new == text {
        return Ok(path.to_path_buf());
    }
    let dir = path.parent().ok_or_else(|| format!("{} has no directory", path.display()))?;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = dir.join(format!(".config.{}", std::process::id()));
    let write = || -> std::io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().create(true).truncate(true).write(true).mode(0o600).open(&tmp)?;
        f.write_all(new.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })?;
    Ok(path.to_path_buf())
}

fn set_host_text(path: &Path, host: &str, text: &str) -> Result<String, String> {
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("{}: {e}", path.display()))?;
    doc["host"] = toml_edit::value(host);
    Ok(doc.to_string())
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
    let asked = agent().get(&url).call().map_err(|e| unfound(&e)).and_then(|r| r.into_string().map_err(|e| e.to_string()));
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
        Err(e) => read(false).ok_or_else(|| not_found_text(host, &e)),
    }
}

fn unfound(e: &ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, _) => format!("{}{code}", NOT_RARGO),
        ureq::Error::Transport(t) => match t.kind() {
            ureq::ErrorKind::Dns => NO_SUCH_HOST.into(),
            ureq::ErrorKind::ConnectionFailed | ureq::ErrorKind::Io => NO_ANSWER.into(),
            _ => t.to_string(),
        },
    }
}

const NO_SUCH_HOST: &str = "no such host";
const NO_ANSWER: &str = "no answer";
const NOT_RARGO: &str = "not rargo: ";

pub fn not_found_text(host: &str, why: &str) -> String {
    let what = if why == NO_SUCH_HOST {
        "no machine has that name".to_string()
    } else if why == NO_ANSWER {
        "nothing answers there".to_string()
    } else if let Some(code) = why.strip_prefix(NOT_RARGO) {
        format!("it answers, but not as a rargo server (HTTP {code})")
    } else {
        why.to_string()
    };
    format!("cannot find the rargo server `{host}`: {what}. Check the name, then name the server again with `rargo @HOST`")
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
    credential_in(&credentials(), host)
}

fn credential_in(all: &toml::Table, host: &str) -> Option<Credential> {
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

    #[test]
    fn a_server_that_cannot_be_found_says_why_in_plain_words_and_what_to_do() {
        for (why, says) in [
            (NO_SUCH_HOST.to_string(), "no machine has that name"),
            (NO_ANSWER.to_string(), "nothing answers there"),
            (format!("{NOT_RARGO}404"), "it answers, but not as a rargo server (HTTP 404)"),
            ("the answer names no server".to_string(), "the answer names no server"),
        ] {
            let text = not_found_text("nowhere.invalid", &why);
            assert_eq!(
                text,
                format!("cannot find the rargo server `nowhere.invalid`: {says}. Check the name, then name the server again with `rargo @HOST`")
            );
            assert!(!text.contains("os error") && !text.contains(".well-known") && !text.contains("Dns"), "{text}");
        }
    }

    #[test]
    fn at_host_writes_the_users_config_and_keeps_the_rest_of_it() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new/rargo/config.toml");
        let host_in = |path: &Path| -> Result<Option<(String, PathBuf)>, String> {
            let Some(text) = read_config(path)? else { return Ok(None) };
            Ok(parse_host(path, &text)?.map(|host| (host, path.to_path_buf())))
        };
        assert_eq!(host_in(&path).unwrap(), None);
        assert_eq!(set_host_in(&path, "rargo.rs").unwrap(), path);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "host = \"rargo.rs\"\n");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(host_in(&path).unwrap(), Some(("rargo.rs".to_string(), path.clone())));
        std::fs::write(&path, "# mine\nhost = \"rargo.rs\"\nother = 1\n").unwrap();
        set_host_in(&path, "build.example:7459").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# mine\nhost = \"build.example:7459\"\nother = 1\n");
        assert_eq!(host_in(&path).unwrap().unwrap().0, "build.example:7459");
        assert_eq!(std::fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        assert!(set_host_in(&path, "not a host").is_err());
        assert!(set_host_in(&path, "h:99999").is_err());
        assert_eq!(host_in(&path).unwrap().unwrap().0, "build.example:7459");
        std::fs::write(&path, "host = 7\n").unwrap();
        assert!(host_in(&path).is_err());
        std::fs::write(&path, "other = 1\n").unwrap();
        assert_eq!(host_in(&path).unwrap(), None);
    }

    #[test]
    fn stored_credentials_and_the_host_line_are_read_and_written_as_text() {
        let all: toml::Table = "[\"a.example\"]\nid = \"t1\"\nsecret = \"s1\"\nname = \"ann\"\n[\"b.example:7\"]\nid = \"t2\"\nsecret = \"s2\"\n[\"c.example\"]\nid = \"t3\"\n[\"d.example\"]\nid = 4\nsecret = \"s4\"\nflat = 1\n"
            .parse()
            .unwrap();
        let got = |host: &str| credential_in(&all, host).map(|c| (c.id, c.secret, c.name));
        assert_eq!(got("a.example"), Some(("t1".into(), "s1".into(), "ann".into())));
        assert_eq!(got("b.example:7"), Some(("t2".into(), "s2".into(), String::new())), "the name is optional");
        assert_eq!(got("c.example"), None, "a token without its secret is no credential");
        assert_eq!(got("d.example"), None, "nor one whose id is not text");
        assert_eq!(got("flat"), None);
        assert_eq!(got("e.example"), None);

        let path = Path::new("/c/config.toml");
        for (before, after) in [
            ("", "host = \"h.example\"\n"),
            ("host = \"old\"\n", "host = \"h.example\"\n"),
            ("# mine\nother = 1 # kept\n", "# mine\nother = 1 # kept\nhost = \"h.example\"\n"),
            ("host = 7\n[t]\nx = 1\n", "host = \"h.example\"\n[t]\nx = 1\n"),
        ] {
            assert_eq!(set_host_text(path, "h.example", before).unwrap(), after, "{before:?}");
        }
        let bad = "host = \"";
        let expected = format!("/c/config.toml: {}", bad.parse::<toml_edit::DocumentMut>().unwrap_err());
        assert_eq!(set_host_text(path, "h.example", bad).unwrap_err(), expected);
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
