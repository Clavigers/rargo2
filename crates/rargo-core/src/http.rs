use crate::session::Keys;
use crate::tokens::{self, normal_code};
use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::io::{Read, Write};
use std::net::TcpStream;

const PENDING_FOR: Duration = Duration::from_secs(600);
const MAX_PENDING: usize = 1000;
const MAX_TRIES: u32 = 5;
const _: () = assert!(MAX_PENDING > 0);
const _: () = assert!(MAX_TRIES > 0);

struct Pending {
    code: String,
    made: Instant,
    tries: u32,
    granted: Option<(String, String)>,
    denied: bool,
    state: Option<String>,
}

#[derive(Default)]
struct PendingState(BTreeMap<String, Pending>);

impl PendingState {
    fn begin(&mut self, device: String, code: String, now: Instant) -> bool {
        if self.0.values().any(|p| p.code == code) || self.0.contains_key(&device) {
            return false;
        }
        self.0.insert(device, Pending { code, made: now, tries: 0, granted: None, denied: false, state: None });
        true
    }

    fn poll(&mut self, device: &str, now: Instant) -> serde_json::Value {
        let Some(pending) = self.0.get(device) else {
            return serde_json::json!({ "status": "expired" });
        };
        let status = if now.saturating_duration_since(pending.made) >= PENDING_FOR {
            "expired"
        } else if pending.denied {
            "denied"
        } else if pending.granted.is_some() {
            let (token, user) = self.0.remove(device).and_then(|p| p.granted).unwrap();
            return serde_json::json!({ "status": "approved", "token": token, "user": user });
        } else {
            return serde_json::json!({ "status": "pending" });
        };
        self.0.remove(device);
        serde_json::json!({ "status": status })
    }

    fn authorize(&mut self, code: &str, now: Instant) -> Result<&mut Pending> {
        let found = self.0.values_mut().find(|p| p.code == code && now.saturating_duration_since(p.made) < PENDING_FOR);
        let pending = found.context("no terminal is waiting with this code (it is good for ten minutes): run `rargo auth` again")?;
        if pending.granted.is_some() {
            bail!("this code has been confirmed already");
        }
        Ok(pending)
    }

    fn invite_attempt(&mut self, code: &str, now: Instant) -> Result<()> {
        let pending = self.authorize(code, now)?;
        if !pending.denied {
            pending.tries += 1;
            pending.denied = pending.tries > MAX_TRIES;
        }
        if pending.denied {
            bail!("too many wrong invitations for this code: run `rargo auth` again");
        }
        Ok(())
    }

    #[cfg(test)]
    fn invariants(&self) -> bool {
        self.0.len() <= MAX_PENDING
            && self.0.values().all(|p| p.tries <= MAX_TRIES + 1 && !(p.denied && p.granted.is_some()))
            && self.0.values().map(|p| &p.code).collect::<std::collections::BTreeSet<_>>().len() == self.0.len()
    }
}

struct GithubExchange<'a> {
    fields: [(&'static str, &'a str); 4],
}

impl<'a> GithubExchange<'a> {
    fn new(github: &'a tokens::Github, code: &'a str, callback: &'a str) -> Self {
        Self { fields: [
            ("client_id", github.client_id.as_str()),
            ("client_secret", github.client_secret.as_str()),
            ("code", code),
            ("redirect_uri", callback),
        ] }
    }

    fn identified(answer: &serde_json::Value) -> Result<&str> {
        if let Some(access) = answer.get("access_token").and_then(serde_json::Value::as_str) {
            return Ok(access);
        }
        bail!("GitHub did not confirm the sign-in ({})", answer.get("error_description").and_then(serde_json::Value::as_str).unwrap_or("no reason given"))
    }

    fn login(answer: &serde_json::Value) -> Result<&str> {
        let login = answer.get("login").and_then(serde_json::Value::as_str).context("GitHub did not say who signed in")?;
        if !tokens::valid_login(login) {
            bail!("GitHub named an account this server cannot use");
        }
        Ok(login)
    }
}

pub struct Http {
    keys: Arc<Keys>,
    public: Option<String>,
    port: u16,
    state: Option<std::path::PathBuf>,
    pending: parking_lot::Mutex<PendingState>,
}

struct Request {
    method: String,
    path: String,
    query: HashMap<String, String>,
    host: String,
    body: Vec<u8>,
    json: bool,
}

enum Endpoint<'a> {
    Discovery(&'a str),
    Start,
    Poll(String),
    Revoke(Option<String>),
    Who(Option<String>),
    Page(&'a str),
    Github(&'a str),
    Callback { state: &'a str, code: &'a str },
    Approve { code: String, invite: String, name: String },
    Missing,
}

impl Request {
    fn endpoint(&self) -> Endpoint<'_> {
        let query = |key: &str| self.query.get(key).map(String::as_str).unwrap_or("");
        let json_field = |key: &str| {
            serde_json::from_slice::<serde_json::Value>(&self.body).ok()
                .and_then(|v| v.get(key)?.as_str().map(String::from))
        };
        match (self.method.as_str(), self.path.as_str()) {
            ("GET" | "HEAD", "/.well-known/rargo.json") => Endpoint::Discovery(&self.host),
            ("POST", "/auth/start") => Endpoint::Start,
            ("POST", "/auth/poll") => {
                let device = if self.json { json_field("device") } else { form(&String::from_utf8_lossy(&self.body)).remove("device") };
                Endpoint::Poll(device.unwrap_or_default())
            }
            ("POST", "/auth/revoke") => Endpoint::Revoke(json_field("token")),
            ("POST", "/auth/whoami") => Endpoint::Who(json_field("token")),
            ("GET", "/auth") => Endpoint::Page(query("code")),
            ("GET", "/auth/github") => Endpoint::Github(query("code")),
            ("GET", "/auth/github/callback") => Endpoint::Callback { state: query("state"), code: query("code") },
            ("POST", "/auth/approve") => {
                let fields = form(&String::from_utf8_lossy(&self.body));
                let field = |key: &str| fields.get(key).map_or("", |v| v.trim()).to_owned();
                Endpoint::Approve { code: field("code"), invite: field("invite"), name: field("name") }
            }
            _ => Endpoint::Missing,
        }
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn form(s: &str) -> HashMap<String, String> {
    s.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn shown(code: &str) -> String {
    let c = normal_code(code);
    if c.len() == 8 {
        let s = format!("{}-{}", &c[..4], &c[4..]);
        assert_eq!(s.len(), 9);
        s
    } else {
        c
    }
}

fn page(title: &str, body: &str) -> String {
    format!(
        "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{title}</title>\
         <style>body{{max-width:36rem;margin:3rem auto;padding:0 1rem;font-family:Georgia,serif;line-height:1.4}}\
         code,input{{font-family:ui-monospace,monospace;font-size:1.1rem}}\
         .code{{font-size:1.6rem;letter-spacing:.1em}}\
         input{{padding:.3rem;width:100%;box-sizing:border-box;margin:.2rem 0 .8rem}}\
         button{{font-size:1rem;padding:.4rem 1rem}}.err{{color:#a00}}</style>\
         <link rel=\"stylesheet\" href=\"/index.css\"></head><body><header><h1 class=\"title\">{title}</h1></header>{body}</body></html>"
    )
}

impl Http {
    pub fn new(keys: Arc<Keys>, public: Option<String>, port: u16) -> Http {
        Http { keys, public, port, state: None, pending: parking_lot::Mutex::new(PendingState::default()) }
    }

    pub fn with_state(mut self, state: std::path::PathBuf) -> Http {
        self.state = Some(state);
        self
    }

    fn github(&self) -> Option<tokens::Github> {
        tokens::Github::load(self.state.as_deref()?)
    }

    fn server_for(&self, host: &str) -> String {
        match &self.public {
            Some(p) => p.clone(),
            None => {
                let name = host.rsplit_once(':').map_or(host, |(h, _)| h);
                format!("{name}:{}", self.port)
            }
        }
    }

    pub fn serve(&self, mut stream: TcpStream) -> Result<()> {
        stream.set_nonblocking(false)?;
        stream.set_write_timeout(Some(Duration::from_secs(20)))?;
        let req = match read_request(&mut stream) {
            Ok(req) => req,
            Err(e) => {
                if e.downcast_ref::<std::io::Error>().is_some_and(|e| matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock)) {
                    return Ok(());
                }
                return respond(&mut stream, 400, "text/plain", format!("bad request: {e}\n").as_bytes());
            }
        };
        let (status, kind, body) = self.answer(req.endpoint(), Instant::now());
        respond(&mut stream, status, kind, body.as_bytes())
    }

    fn answer(&self, endpoint: Endpoint<'_>, now: Instant) -> (u16, &'static str, String) {
        let json = |v: serde_json::Value| (200, "application/json", v.to_string());
        let render_auth = |code: &str, error: Option<&str>| auth_page(code, error, self.keys.tokens().is_some(), self.github().is_some());
        match endpoint {
            Endpoint::Discovery(host) => json(serde_json::json!({
                "server": self.server_for(host),
                "protocol": crate::proto::PROTO_VERSION,
                "version": env!("CARGO_PKG_VERSION"),
                "auth": self.keys.tokens().is_some(),
                "github": self.github().is_some(),
            })),
            Endpoint::Start => match self.start_at(now) {
                Ok((device, code)) => json(serde_json::json!({
                    "device": device,
                    "code": shown(&code),
                    "path": format!("/auth?code={}", shown(&code)),
                    "interval": 2,
                    "expires_in": PENDING_FOR.as_secs(),
                })),
                Err(e) => (503, "application/json", serde_json::json!({ "error": format!("{e:#}") }).to_string()),
            },
            Endpoint::Poll(device) => json(self.poll_at(&device, now)),
            Endpoint::Revoke(token) => {
                let done = match (self.keys.tokens(), token) {
                    (Some(store), Some(t)) => store.revoke_self(&t).unwrap_or(false),
                    _ => false,
                };
                if done {
                    eprintln!("rargod: a token was revoked by its holder (`rargo auth remove`)");
                }
                json(serde_json::json!({ "revoked": done }))
            }
            Endpoint::Who(token) => {
                let user = match (self.keys.tokens(), token) {
                    (Some(store), Some(t)) => store.holder(&t),
                    _ => None,
                };
                json(serde_json::json!({ "user": user }))
            }
            Endpoint::Page(code) => (200, "text/html; charset=utf-8", render_auth(code, None)),
            Endpoint::Github(code) => match self.to_github(code, now) {
                Ok(location) => (302, "location", location),
                Err(e) => (403, "text/html; charset=utf-8", render_auth(code, Some(&format!("{e:#}")))),
            },
            Endpoint::Callback { state, code } => {
                match self.from_github(state, code, now) {
                    Ok((code, user)) => (
                        200,
                        "text/html; charset=utf-8",
                        page(
                            "Authenticated",
                            &format!(
                                "<p>The terminal showing <code class=\"code\">{}</code> now holds a token for the GitHub account <b>{}</b>.</p>\
                                 <p>You can close this page and go back to it: <code>rargo build</code> will work.</p>",
                                escape(&shown(&code)),
                                escape(&user)
                            ),
                        ),
                    ),
                    Err(e) => (403, "text/html; charset=utf-8", page("rargo auth", &format!("<p class=\"err\">{}</p><p>Run <code>rargo auth</code> again to start over.</p>", escape(&format!("{e:#}"))))),
                }
            }
            Endpoint::Approve { code, invite, name } => {
                match self.approve_at(&code, &invite, (!name.is_empty()).then_some(name.as_str()), now) {
                    Ok(user) => (
                        200,
                        "text/html; charset=utf-8",
                        page(
                            "Authenticated",
                            &format!(
                                "<p>The terminal showing <code class=\"code\">{}</code> now holds a token for <b>{}</b>.</p>\
                                 <p>You can close this page and go back to it: <code>rargo build</code> will work.</p>",
                                escape(&shown(&code)),
                                escape(&user)
                            ),
                        ),
                    ),
                    Err(e) => (403, "text/html; charset=utf-8", render_auth(&code, Some(&format!("{e:#}")))),
                }
            }
            Endpoint::Missing => (404, "text/plain", "this is a rargo build server; see https://rargo.rs\n".to_string()),
        }
    }

    fn start_at(&self, now: Instant) -> Result<(String, String)> {
        if self.keys.tokens().is_none() {
            bail!("this server does not issue tokens");
        }
        let mut pending = self.pending.lock();
        pending.0.retain(|_, p| now.saturating_duration_since(p.made) < PENDING_FOR);
        if pending.0.len() >= MAX_PENDING {
            bail!("too many requests are waiting to be confirmed; try again in a few minutes");
        }
        loop {
            let device = tokens::random_text(40, b"abcdefghijklmnopqrstuvwxyz0123456789")?;
            let code = tokens::random_text(8, b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789")?;
            if pending.begin(device.clone(), code.clone(), now) {
                return Ok((device, code));
            }
        }
    }

    fn approve_at(&self, code: &str, invite: &str, name: Option<&str>, now: Instant) -> Result<String> {
        let store = self.keys.tokens().context("this server does not issue tokens")?;
        let code = normal_code(code);
        self.pending.lock().invite_attempt(&code, now)?;
        let (token, secret) = store.redeem(invite, name)?;
        let mut pending = self.pending.lock();
        match pending.0.values_mut().find(|p| p.code == code) {
            Some(p) if !p.denied && p.granted.is_none() => {
                p.granted = Some((tokens::token_text(&token, &secret), token.user.clone()));
            }
            _ => {
                let _ = store.revoke(&token.id);
                bail!("the terminal stopped waiting: run `rargo auth` again");
            }
        }
        eprintln!("rargod: a token for {} was issued through `rargo auth`", token.user);
        Ok(token.user)
    }

    fn to_github(&self, code: &str, now: Instant) -> Result<String> {
        let gh = self.github().context("this server does not sign people in with GitHub")?;
        let code = normal_code(code);
        let state = tokens::random_text(32, b"abcdefghijklmnopqrstuvwxyz0123456789")?;
        let mut pending = self.pending.lock();
        let p = pending.authorize(&code, now)?;
        p.state = Some(state.clone());
        Ok(format!(
            "{}/login/oauth/authorize?client_id={}&redirect_uri={}%2Fauth%2Fgithub%2Fcallback&state={state}&allow_signup=false",
            github_url(),
            gh.client_id,
            gh.url.replace(':', "%3A").replace('/', "%2F"),
        ))
    }

    fn from_github(&self, state: &str, gh_code: &str, now: Instant) -> Result<(String, String)> {
        let gh = self.github().context("this server does not sign people in with GitHub")?;
        let store = self.keys.tokens().context("this server does not issue tokens")?;
        if state.is_empty() || gh_code.is_empty() {
            bail!("GitHub sent the browser back without a sign-in (it was cancelled there)");
        }
        let code = {
            let pending = self.pending.lock();
            let Some(p) = pending.0.values().find(|p| p.state.as_deref() == Some(state) && now.saturating_duration_since(p.made) < PENDING_FOR) else {
                bail!("this sign-in belongs to no terminal that is still waiting");
            };
            p.code.clone()
        };
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(15)).build();
        let callback = format!("{}/auth/github/callback", gh.url);
        let exchange = GithubExchange::new(&gh, gh_code, &callback);
        let answer: serde_json::Value = agent
            .post(&format!("{}/login/oauth/access_token", github_url()))
            .set("Accept", "application/json")
            .send_form(&exchange.fields)
            .map_err(|e| anyhow::anyhow!("asking GitHub: {e}"))
            .and_then(|r| r.into_string().context("reading GitHub's answer"))
            .and_then(|t| serde_json::from_str(&t).context("GitHub's answer"))?;
        let access = GithubExchange::identified(&answer)?;
        let who: serde_json::Value = agent
            .get(&format!("{}/user", github_api()))
            .set("Authorization", &format!("Bearer {access}"))
            .set("Accept", "application/vnd.github+json")
            .set("User-Agent", "rargod")
            .call()
            .map_err(|e| anyhow::anyhow!("asking GitHub who signed in: {e}"))
            .and_then(|r| r.into_string().context("reading GitHub's answer"))
            .and_then(|t| serde_json::from_str(&t).context("GitHub's answer"))?;
        let login = GithubExchange::login(&who)?;
        let Some(allowed) = store.allowed(login) else {
            bail!(
                "the GitHub account {login} is not on this server's allow list. Its administrator adds it with: rargod token allow {login}"
            );
        };
        let (token, secret) = store.create(&allowed.login, allowed.admin, None, "github")?;
        let mut pending = self.pending.lock();
        match pending.0.values_mut().find(|p| p.state.as_deref() == Some(state)) {
            Some(p) if !p.denied && p.granted.is_none() => {
                p.granted = Some((tokens::token_text(&token, &secret), token.user.clone()));
                p.state = None;
            }
            _ => {
                let _ = store.revoke(&token.id);
                bail!("the terminal stopped waiting: run `rargo auth` again");
            }
        }
        eprintln!("rargod: a token for the GitHub account {} was issued through `rargo auth`", token.user);
        Ok((code, token.user))
    }

    fn poll_at(&self, device: &str, now: Instant) -> serde_json::Value {
        self.pending.lock().poll(device, now)
    }
}

fn auth_page(code: &str, error: Option<&str>, auth: bool, github: bool) -> String {
    if !auth {
        return page("rargo auth", "<p>This server does not issue tokens.</p>");
    }
    let err = error.map(|e| format!("<p class=\"err\">{}</p>", escape(e))).unwrap_or_default();
    if github {
        return page(
            "rargo auth",
            &format!(
                "<p>A terminal is asking for a token to build on this server. Go on only if the code below is the one \
                 <code>rargo auth</code> is showing you.</p>\
                 {err}\
                 <form method=\"get\" action=\"/auth/github\">\
                 <label>Code shown in the terminal<input name=\"code\" value=\"{}\" autocomplete=\"off\" required></label>\
                 <button type=\"submit\">Sign in with GitHub and give that terminal a token</button></form>\
                 <p>The server learns your GitHub user name and nothing else, and gives a token only to accounts its administrator has allowed.</p>",
                escape(&shown(code))
            ),
        );
    }
    page(
        "rargo auth",
        &format!(
            "<p>A terminal is asking for a token to build on this server. Go on only if the code below is the one \
             <code>rargo auth</code> is showing you.</p>\
             {err}\
             <form method=\"post\" action=\"/auth/approve\">\
             <label>Code shown in the terminal<input name=\"code\" value=\"{}\" autocomplete=\"off\" required></label>\
             <label>Invitation (from the server's administrator)<input name=\"invite\" autocomplete=\"off\" required autofocus></label>\
             <label>Your name, if the invitation does not carry one<input name=\"name\" autocomplete=\"username\"></label>\
             <button type=\"submit\">Give that terminal a token</button></form>",
            escape(&shown(code))
        ),
    )
}

fn github_url() -> String {
    std::env::var("RARGO_GITHUB_URL").unwrap_or_else(|_| "https://github.com".into()).trim_end_matches('/').to_string()
}

fn github_api() -> String {
    std::env::var("RARGO_GITHUB_API").unwrap_or_else(|_| "https://api.github.com".into()).trim_end_matches('/').to_string()
}

fn read_request(stream: &mut TcpStream) -> Result<Request> {
    let mut bytes = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(request) = parse_request(&bytes)? {
            return Ok(request);
        }
        let remain = deadline.saturating_duration_since(Instant::now());
        if remain.is_zero() {
            return Err(std::io::Error::from(std::io::ErrorKind::TimedOut).into());
        }
        stream.set_read_timeout(Some(remain))?;
        let mut chunk = [0u8; 2048];
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            bail!("the request ended early");
        }
        bytes.extend_from_slice(&chunk[..n]);
    }
}

fn parse_request(buf: &[u8]) -> Result<Option<Request>> {
    let Some(head_end) = buf.windows(4).position(|part| part == b"\r\n\r\n").map(|i| i + 4) else {
        if buf.len() > 16 << 10 {
            bail!("the request's head is too long");
        }
        return Ok(None);
    };
    if head_end > 16 << 10 {
        bail!("the request's head is too long");
    }
    let head = std::str::from_utf8(&buf[..head_end - 4]).context("invalid request header text")?;
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let (Some(method), Some(target), Some("HTTP/1.0" | "HTTP/1.1"), None) = (first.next(), first.next(), first.next(), first.next()) else {
        bail!("malformed request line");
    };
    if method.is_empty() || !target.starts_with('/') {
        bail!("malformed request target");
    }
    let mut host = String::new();
    let mut length = None;
    let mut json = false;
    for line in lines {
        let Some((k, v)) = line.split_once(':') else { bail!("malformed request header") };
        let v = v.trim();
        match k.to_ascii_lowercase().as_str() {
            "host" => host = v.to_string(),
            "x-forwarded-host" => host = v.to_string(),
            "content-length" if length.is_none() => length = Some(v.parse::<usize>().context("Content-Length")?),
            "content-length" => bail!("multiple Content-Length headers"),
            "content-type" => json = v.starts_with("application/json"),
            _ => {}
        }
    }
    let length = length.unwrap_or(0);
    if length > 64 << 10 {
        bail!("the request's body is too long");
    }
    let Some(body) = buf.get(head_end..head_end + length) else { return Ok(None) };
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    Ok(Some(Request { method: method.into(), path: path.into(), query: form(query), host, body: body.to_vec(), json }))
}

fn respond(stream: &mut TcpStream, status: u16, kind: &str, body: &[u8]) -> Result<()> {
    if status == 302 {
        let head = format!(
            "HTTP/1.1 302 Found\r\nLocation: {}\r\nContent-Length: 0\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
            String::from_utf8_lossy(body)
        );
        stream.write_all(head.as_bytes())?;
        let _ = stream.shutdown(std::net::Shutdown::Write);
        return Ok(());
    }
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Service Unavailable",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_decode_input_before_any_endpoint_effect() {
        fn get(path: &str) -> Request {
            parse_request(format!("GET {path} HTTP/1.1\r\nHost: build.example\r\n\r\n").as_bytes()).unwrap().unwrap()
        }
        assert!(matches!(get("/.well-known/rargo.json").endpoint(), Endpoint::Discovery("build.example")));
        assert!(matches!(get("/auth?code=AB%2DCD").endpoint(), Endpoint::Page("AB-CD")));
        assert!(matches!(get("/auth/github?code=AB%2DCD").endpoint(), Endpoint::Github("AB-CD")));
        assert!(matches!(get("/auth/github/callback?state=abc&code=x").endpoint(), Endpoint::Callback { state: "abc", code: "x" }));
        assert!(matches!(get("/missing").endpoint(), Endpoint::Missing));
        let post = |path: &str, body: &str, kind: &str| {
            parse_request(format!("POST {path} HTTP/1.1\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes())
                .unwrap().unwrap()
        };
        assert!(matches!(post("/auth/start", "", "text/plain").endpoint(), Endpoint::Start));
        assert!(matches!(post("/auth/poll", "device=abc", "application/x-www-form-urlencoded").endpoint(), Endpoint::Poll(device) if device == "abc"));
        assert!(matches!(post("/auth/approve", "code=abc&name=Ann+Lee", "application/x-www-form-urlencoded").endpoint(),
            Endpoint::Approve { code, name, .. } if code == "abc" && name == "Ann Lee"));
        for route in ["/auth/revoke", "/auth/whoami"] {
            let request = post(route, r#"{"token":"fixture"}"#, "application/json");
            assert!(matches!(request.endpoint(), Endpoint::Revoke(Some(token)) | Endpoint::Who(Some(token)) if token == "fixture"));
        }
        assert!(auth_page("abc", None, false, false).contains("does not issue tokens"));
        assert!(auth_page("abc", Some("<error>"), true, false).contains("&lt;error&gt;"));
        assert!(auth_page("abc", None, true, true).contains("Sign in with GitHub"));
        assert!(parse_request(b"GET / HTTP/1.1\r\nBroken header\r\n\r\n").is_err());
        assert!(parse_request(b"POST / HTTP/1.1\r\nContent-Length: x\r\n\r\n").is_err());
        assert!(parse_request(b"POST / HTTP/1.1\r\nContent-Length: 5\r\n\r\n1234").unwrap().is_none());
    }

    #[test]
    fn github_responses_and_pending_terminal_transitions_are_pure() {
        let success = serde_json::json!({"access_token": "fixture"});
        let refused = serde_json::json!({"error": "bad_verification_code", "error_description": "The code passed is incorrect or expired."});
        assert_eq!(GithubExchange::identified(&success).unwrap(), "fixture");
        assert!(GithubExchange::identified(&refused).unwrap_err().to_string().contains("incorrect or expired"));
        assert_eq!(GithubExchange::login(&serde_json::json!({"login":"octocat","id":583231})).unwrap(), "octocat");
        assert!(GithubExchange::login(&serde_json::json!({"message":"Bad credentials"})).is_err());
        let now = Instant::now();
        let mut pending = PendingState::default();
        assert!(pending.begin("fixture".into(), "ABCD2345".into(), now));
        assert!(pending.invariants());
        assert!(!pending.begin("other".into(), "ABCD2345".into(), now));
        assert!(pending.invariants());
        assert_eq!(pending.poll("fixture", now)["status"], "pending");
        assert!(pending.invariants());
        for _ in 0..MAX_TRIES {
            assert!(pending.invite_attempt("ABCD2345", now).is_ok());
            assert!(pending.invariants());
        }
        assert!(pending.invite_attempt("ABCD2345", now).is_err());
        assert!(pending.invariants());
        assert_eq!(pending.poll("fixture", now)["status"], "denied");
        assert!(pending.invariants());
        assert!(pending.begin("fixture".into(), "ABCD2345".into(), now));
        assert!(pending.invariants());
        assert_eq!(pending.poll("fixture", now + PENDING_FOR)["status"], "expired");
        assert!(pending.invariants());
    }

    fn http() -> (tempfile::TempDir, Http) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(tokens::TokenStore::new(dir.path().join("tokens.json")));
        let keys = Arc::new(Keys::with_tokens(store));
        (dir, Http::new(keys, None, 7459))
    }

    #[test]
    fn a_terminal_gets_its_token_only_after_the_code_is_confirmed_with_an_invitation() {
        let (_dir, h) = http();
        let now = Instant::now();
        let invite = h.keys.tokens().unwrap().invite(Some("ann"), false, None, 1, None).unwrap();
        let (device, code) = h.start_at(now).unwrap();
        assert_eq!(h.poll_at(&device, now)["status"], "pending");
        assert_eq!(h.poll_at("someone-else", now)["status"], "expired");
        assert!(h.approve_at(&code, "WRONG-WRONG-WRONG-WRON", None, now).is_err());
        assert_eq!(h.poll_at(&device, now)["status"], "pending");
        assert!(h.approve_at("ZZZZ-ZZZZ", &invite, None, now).is_err(), "an unknown code uses up nothing");
        assert_eq!(h.approve_at(&shown(&code).to_lowercase(), &invite, None, now).unwrap(), "ann");
        assert!(h.approve_at(&code, &invite, None, now).is_err(), "confirmed once");
        let got = h.poll_at(&device, now);
        assert_eq!(got["status"], "approved");
        assert_eq!(got["user"], "ann");
        let token = got["token"].as_str().unwrap();
        let (id, _secret) = token.split_once('.').unwrap();
        assert!(h.keys.still_valid(id));
        assert_eq!(h.keys.name_of(id), "ann");
        assert_eq!(h.poll_at(&device, now)["status"], "expired", "handed out once");
    }

    #[test]
    fn wrong_invitations_are_counted_per_code() {
        let (_dir, h) = http();
        let now = Instant::now();
        let (device, code) = h.start_at(now).unwrap();
        for _ in 0..MAX_TRIES {
            assert!(h.approve_at(&code, "AAAA-AAAA-AAAA-AAAA", None, now).is_err());
        }
        let invite = h.keys.tokens().unwrap().invite(Some("ann"), false, None, 1, None).unwrap();
        assert!(h.approve_at(&code, &invite, None, now).is_err(), "the code is spent");
        assert_eq!(h.poll_at(&device, now)["status"], "denied");
        assert!(h.keys.tokens().unwrap().invited(&invite).is_some(), "the invitation is not");
    }

    #[test]
    fn the_address_clients_are_told_is_the_public_one_or_the_name_asked() {
        let (_dir, h) = http();
        assert_eq!(h.server_for("build.example"), "build.example:7459");
        assert_eq!(h.server_for("build.example:8080"), "build.example:7459");
        let dir = tempfile::tempdir().unwrap();
        let keys = Arc::new(Keys::with_tokens(Arc::new(tokens::TokenStore::new(dir.path().join("t.json")))));
        assert_eq!(Http::new(keys, Some("b.example:1".into()), 7459).server_for("rargo.rs"), "b.example:1");
        assert_eq!(form("code=AB%2DCD&name=a+b")["code"], "AB-CD");
        assert_eq!(form("code=AB%2DCD&name=a+b")["name"], "a b");
        assert_eq!(percent_decode("100%"), "100%");
    }
}
