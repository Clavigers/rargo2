//! The little HTTP a rargo server speaks on its TCP port, for what has to
//! happen before a client holds a key: finding the server's real address
//! from a name (`/.well-known/rargo.json`) and `rargo auth`, where a person
//! confirms in a browser that a terminal may have a token (D67).
//!
//! Requests are small and each connection carries one. A public server
//! puts something that speaks HTTPS in front (rargo.rs forwards these paths
//! to it); on a private network the port is asked directly.
//!
//! The flow of `rargo auth`:
//!   1. the client POSTs `/auth/start` and gets a device secret (which it
//!      keeps) and a short code (which it shows);
//!   2. the person opens `/auth?code=...`, sees the same code, and enters
//!      an invitation an administrator gave them (`rargod token invite`);
//!   3. the client, polling `/auth/poll` with its device secret, is given a
//!      token made for that person.
//! The code only names the request; it grants nothing without the
//! invitation, and the token goes only to whoever holds the device secret.

use crate::session::Keys;
use crate::tokens::{self, normal_code};
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const PENDING_FOR: Duration = Duration::from_secs(600);
const MAX_PENDING: usize = 1000;
const MAX_TRIES: u32 = 5;

struct Pending {
    code: String,
    made: Instant,
    tries: u32,
    /// The token's text and the user's name, once approved.
    granted: Option<(String, String)>,
    denied: bool,
}

pub struct Http {
    keys: Arc<Keys>,
    /// The address clients are told to connect to (`--public`); none: the
    /// name the request was sent to, with the port the server listens on.
    public: Option<String>,
    port: u16,
    /// By device secret.
    pending: parking_lot::Mutex<HashMap<String, Pending>>,
}

struct Request {
    method: String,
    path: String,
    query: HashMap<String, String>,
    host: String,
    body: Vec<u8>,
    json: bool,
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

/// A code as it is shown: `ABCD-EFGH`.
fn shown(code: &str) -> String {
    let c = normal_code(code);
    if c.len() == 8 { format!("{}-{}", &c[..4], &c[4..]) } else { c }
}

fn page(title: &str, body: &str) -> String {
    // On rargo.rs the site's own style sheet is found at /index.css and the
    // page looks like the rest of the site; elsewhere the few rules here
    // are all there is.
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
        Http { keys, public, port, pending: parking_lot::Mutex::new(HashMap::new()) }
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

    pub async fn serve(&self, mut stream: TcpStream) -> Result<()> {
        let req = match tokio::time::timeout(Duration::from_secs(20), read_request(&mut stream)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                let _ = respond(&mut stream, 400, "text/plain", format!("bad request: {e}\n").as_bytes()).await;
                return Ok(());
            }
            Err(_) => return Ok(()),
        };
        let (status, kind, body) = self.answer(&req);
        respond(&mut stream, status, kind, body.as_bytes()).await
    }

    fn answer(&self, req: &Request) -> (u16, &'static str, String) {
        let json = |v: serde_json::Value| (200, "application/json", v.to_string());
        match (req.method.as_str(), req.path.as_str()) {
            ("GET" | "HEAD", "/.well-known/rargo.json") => json(serde_json::json!({
                "server": self.server_for(&req.host),
                "protocol": crate::proto::PROTO_VERSION,
                "version": env!("CARGO_PKG_VERSION"),
                "auth": self.keys.tokens().is_some(),
            })),
            ("POST", "/auth/start") => match self.start() {
                Ok((device, code)) => json(serde_json::json!({
                    "device": device,
                    "code": shown(&code),
                    "path": format!("/auth?code={}", shown(&code)),
                    "interval": 2,
                    "expires_in": PENDING_FOR.as_secs(),
                })),
                Err(e) => (503, "application/json", serde_json::json!({ "error": format!("{e:#}") }).to_string()),
            },
            ("POST", "/auth/poll") => {
                let device = if req.json {
                    serde_json::from_slice::<serde_json::Value>(&req.body).ok().and_then(|v| v.get("device")?.as_str().map(String::from))
                } else {
                    form(&String::from_utf8_lossy(&req.body)).remove("device")
                };
                json(self.poll(device.as_deref().unwrap_or("")))
            }
            ("GET", "/auth") => (200, "text/html; charset=utf-8", self.auth_page(req.query.get("code").map(String::as_str).unwrap_or(""), None)),
            ("POST", "/auth/approve") => {
                let f = form(&String::from_utf8_lossy(&req.body));
                let get = |k: &str| f.get(k).map(String::as_str).unwrap_or("").trim().to_string();
                let (code, invite, name) = (get("code"), get("invite"), get("name"));
                match self.approve(&code, &invite, (!name.is_empty()).then_some(name.as_str())) {
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
                    Err(e) => (403, "text/html; charset=utf-8", self.auth_page(&code, Some(&format!("{e:#}")))),
                }
            }
            _ => (404, "text/plain", "this is a rargo build server; see https://rargo.rs\n".to_string()),
        }
    }

    fn auth_page(&self, code: &str, error: Option<&str>) -> String {
        if self.keys.tokens().is_none() {
            return page("rargo auth", "<p>This server does not issue tokens.</p>");
        }
        let err = error.map(|e| format!("<p class=\"err\">{}</p>", escape(e))).unwrap_or_default();
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

    fn start(&self) -> Result<(String, String)> {
        if self.keys.tokens().is_none() {
            bail!("this server does not issue tokens");
        }
        let mut pending = self.pending.lock();
        pending.retain(|_, p| p.made.elapsed() < PENDING_FOR);
        if pending.len() >= MAX_PENDING {
            bail!("too many requests are waiting to be confirmed; try again in a few minutes");
        }
        let device = tokens::random_text(40, b"abcdefghijklmnopqrstuvwxyz0123456789")?;
        let code = loop {
            let c = tokens::random_text(8, b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789")?;
            if !pending.values().any(|p| p.code == c) {
                break c;
            }
        };
        pending.insert(device.clone(), Pending { code: code.clone(), made: Instant::now(), tries: 0, granted: None, denied: false });
        Ok((device, code))
    }

    fn approve(&self, code: &str, invite: &str, name: Option<&str>) -> Result<String> {
        let store = self.keys.tokens().context("this server does not issue tokens")?;
        let code = normal_code(code);
        // Found and counted under the lock; the token is made outside it.
        {
            let mut pending = self.pending.lock();
            let Some(p) = pending.values_mut().find(|p| p.code == code && p.made.elapsed() < PENDING_FOR) else {
                bail!("no terminal is waiting with this code (it is good for ten minutes): run `rargo auth` again");
            };
            if p.granted.is_some() {
                bail!("this code has been confirmed already");
            }
            if p.denied {
                bail!("too many wrong invitations for this code: run `rargo auth` again");
            }
            p.tries += 1;
            if p.tries > MAX_TRIES {
                p.denied = true;
                bail!("too many wrong invitations for this code: run `rargo auth` again");
            }
        }
        let (token, secret) = store.redeem(invite, name)?;
        let mut pending = self.pending.lock();
        match pending.values_mut().find(|p| p.code == code) {
            Some(p) => p.granted = Some((tokens::token_text(&token, &secret), token.user.clone())),
            None => {
                // The request went away meanwhile: nobody can pick the
                // token up, so it must not stay valid.
                let _ = store.revoke(&token.id);
                bail!("the terminal stopped waiting: run `rargo auth` again");
            }
        }
        eprintln!("rargod: a token for {} was issued through `rargo auth`", token.user);
        Ok(token.user)
    }

    fn poll(&self, device: &str) -> serde_json::Value {
        let mut pending = self.pending.lock();
        let Some(p) = pending.get(device) else {
            return serde_json::json!({ "status": "expired" });
        };
        if p.made.elapsed() >= PENDING_FOR {
            pending.remove(device);
            return serde_json::json!({ "status": "expired" });
        }
        if p.denied {
            pending.remove(device);
            return serde_json::json!({ "status": "denied" });
        }
        if p.granted.is_some() {
            let (token, user) = pending.remove(device).and_then(|p| p.granted).unwrap_or_default();
            return serde_json::json!({ "status": "approved", "token": token, "user": user });
        }
        serde_json::json!({ "status": "pending" })
    }
}

async fn read_request(stream: &mut TcpStream) -> Result<Request> {
    let mut buf: Vec<u8> = Vec::new();
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > 16 << 10 {
            bail!("the request's head is too long");
        }
        let mut chunk = [0u8; 2048];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            bail!("the request ended early");
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let (method, target) = (first.next().unwrap_or("").to_string(), first.next().unwrap_or("").to_string());
    let mut host = String::new();
    let mut length = 0usize;
    let mut json = false;
    for line in lines {
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim();
        match k.to_ascii_lowercase().as_str() {
            "host" => host = v.to_string(),
            // What the name was before something forwarded the request.
            "x-forwarded-host" => host = v.to_string(),
            "content-length" => length = v.parse().context("Content-Length")?,
            "content-type" => json = v.starts_with("application/json"),
            _ => {}
        }
    }
    if length > 64 << 10 {
        bail!("the request's body is too long");
    }
    let mut body = buf[head_end..].to_vec();
    while body.len() < length {
        let mut chunk = [0u8; 2048];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            bail!("the request's body ended early");
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(length);
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    Ok(Request { method, path: path.to_string(), query: form(query), host, body, json })
}

async fn respond(stream: &mut TcpStream, status: u16, kind: &str, body: &[u8]) -> Result<()> {
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
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    let _ = stream.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http() -> (tempfile::TempDir, Http) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(tokens::TokenStore::new(dir.path().join("tokens.json")));
        let keys = Arc::new(Keys::tokens_only().with_tokens(store));
        (dir, Http::new(keys, None, 7459))
    }

    #[test]
    fn a_terminal_gets_its_token_only_after_the_code_is_confirmed_with_an_invitation() {
        let (_dir, h) = http();
        let invite = h.keys.tokens().unwrap().invite(Some("ann"), false, None, 1, None).unwrap();
        let (device, code) = h.start().unwrap();
        assert_eq!(h.poll(&device)["status"], "pending");
        assert_eq!(h.poll("someone-else")["status"], "expired");
        assert!(h.approve(&code, "WRONG-WRONG-WRONG-WRON", None).is_err());
        assert_eq!(h.poll(&device)["status"], "pending");
        assert!(h.approve("ZZZZ-ZZZZ", &invite, None).is_err(), "an unknown code uses up nothing");
        assert_eq!(h.approve(&shown(&code).to_lowercase(), &invite, None).unwrap(), "ann");
        assert!(h.approve(&code, &invite, None).is_err(), "confirmed once");
        let got = h.poll(&device);
        assert_eq!(got["status"], "approved");
        assert_eq!(got["user"], "ann");
        let token = got["token"].as_str().unwrap();
        let (id, _secret) = token.split_once('.').unwrap();
        assert!(h.keys.still_valid(id));
        assert_eq!(h.keys.name_of(id), "ann");
        assert_eq!(h.poll(&device)["status"], "expired", "handed out once");
    }

    #[test]
    fn wrong_invitations_are_counted_per_code() {
        let (_dir, h) = http();
        let (device, code) = h.start().unwrap();
        for _ in 0..MAX_TRIES {
            assert!(h.approve(&code, "AAAA-AAAA-AAAA-AAAA", None).is_err());
        }
        let invite = h.keys.tokens().unwrap().invite(Some("ann"), false, None, 1, None).unwrap();
        assert!(h.approve(&code, &invite, None).is_err(), "the code is spent");
        assert_eq!(h.poll(&device)["status"], "denied");
        assert!(h.keys.tokens().unwrap().invited(&invite).is_some(), "the invitation is not");
    }

    #[test]
    fn the_address_clients_are_told_is_the_public_one_or_the_name_asked() {
        let (_dir, h) = http();
        assert_eq!(h.server_for("build.example"), "build.example:7459");
        assert_eq!(h.server_for("build.example:8080"), "build.example:7459");
        let dir = tempfile::tempdir().unwrap();
        let keys = Arc::new(Keys::tokens_only().with_tokens(Arc::new(tokens::TokenStore::new(dir.path().join("t.json")))));
        assert_eq!(Http::new(keys, Some("b.example:1".into()), 7459).server_for("rargo.rs"), "b.example:1");
        assert_eq!(form("code=AB%2DCD&name=a+b")["code"], "AB-CD");
        assert_eq!(form("code=AB%2DCD&name=a+b")["name"], "a b");
        assert_eq!(percent_decode("100%"), "100%");
    }
}
