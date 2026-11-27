use rargo_machine::StateMachine;
use crate::session::Keys;
use crate::tokens::{self, normal_code};
use anyhow::{Context, Result, anyhow};
use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::sync::Arc;
use crate::io::http::{self as outbound, Failure};
use std::time::{Duration, Instant, UNIX_EPOCH};

const PENDING_FOR_MS: u64 = 600_000;
const MAX_PENDING: usize = 1000;
const MAX_TRIES: u32 = 5;
const DEVICE_ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const _: () = assert!(MAX_PENDING > 0);
const _: () = assert!(MAX_TRIES > 0);
const _: () = assert!(!DEVICE_ALPHABET.is_empty() && !CODE_ALPHABET.is_empty());

struct Pending {
    code: String,
    made: u64,
    tries: u32,
    granted: Option<(String, String)>,
    denied: bool,
    state: Option<String>,
}

#[derive(Default)]
struct AuthState {
    pending: BTreeMap<String, Pending>,
}

impl AuthState {
    fn sweep(&mut self, now: u64) {
        self.pending.retain(|_, p| now.saturating_sub(p.made) < PENDING_FOR_MS);
        assert!(self.pending.values().all(|p| now.saturating_sub(p.made) < PENDING_FOR_MS), "a swept state holds no expired login");
        self.invariants();
    }

    fn begin(&mut self, device: String, code: String, now: u64) -> bool {
        let before = self.pending.len();
        if device.is_empty() || code.is_empty() || self.pending.values().any(|p| p.code == code) || self.pending.contains_key(&device) {
            assert_eq!(self.pending.len(), before, "a refused begin adds no login");
            return false;
        }
        self.pending.insert(device.clone(), Pending { code: code.clone(), made: now, tries: 0, granted: None, denied: false, state: None });
        assert_eq!(self.pending.len(), before + 1, "a begin adds one login");
        assert!(
            self.pending.get(&device).is_some_and(|p| p.code == code && p.tries == 0 && p.granted.is_none() && !p.denied && p.state.is_none()),
            "login {device} begins fresh with its code"
        );
        true
    }

    fn poll(&mut self, device: &str, now: u64) -> serde_json::Value {
        let answer = self.answer(device, now);
        let pending = answer["status"] == "pending";
        assert_eq!(self.pending.contains_key(device), pending, "a poll keeps login {device} only while it is pending");
        answer
    }

    fn answer(&mut self, device: &str, now: u64) -> serde_json::Value {
        let Some(pending) = self.pending.get(device) else {
            return serde_json::json!({ "status": "expired" });
        };
        if now.saturating_sub(pending.made) >= PENDING_FOR_MS {
            self.pending.remove(device);
            return serde_json::json!({ "status": "expired" });
        }
        if pending.denied {
            self.pending.remove(device);
            return serde_json::json!({ "status": "denied" });
        }
        if pending.granted.is_some() {
            let (token, user) = self.pending.remove(device).and_then(|p| p.granted).expect("a granted sign-in holds its token");
            assert!(!token.is_empty() && !user.is_empty(), "an approved poll hands over a token for a user");
            return serde_json::json!({ "status": "approved", "token": token, "user": user });
        }
        serde_json::json!({ "status": "pending" })
    }

    fn waiting(&self, code: &str, now: u64) -> bool {
        self.pending.values().any(|p| p.code == code && p.granted.is_none() && now.saturating_sub(p.made) < PENDING_FOR_MS)
    }

    fn authorize(&mut self, code: &str, now: u64) -> Result<&mut Pending> {
        let found = self.pending.values_mut().find(|p| p.code == code && now.saturating_sub(p.made) < PENDING_FOR_MS);
        let pending = found.context("no terminal is waiting with this code (it is good for ten minutes): run `rargo auth` again")?;
        if pending.granted.is_some() {
            anyhow::bail!("this code has been confirmed already");
        }
        assert!(pending.code == code && pending.granted.is_none(), "authorize hands out only the waiting login {code}");
        Ok(pending)
    }

    fn invite_attempt(&mut self, code: &str, now: u64) -> Result<()> {
        let pending = self.authorize(code, now)?;
        let tries = pending.tries;
        if !pending.denied {
            pending.tries += 1;
            pending.denied = pending.tries > MAX_TRIES;
        }
        assert!(pending.tries == tries || pending.tries == tries + 1, "an invitation counts as one try");
        assert!(pending.tries <= MAX_TRIES + 1, "login {code} stops counting once denied");
        if pending.denied {
            anyhow::bail!("too many wrong invitations for this code: run `rargo auth` again");
        }
        Ok(())
    }

    fn grant(&mut self, code: &str, token: String, user: String) -> bool {
        let granted = match self.pending.values_mut().find(|p| p.code == code) {
            Some(p) if !p.denied && p.granted.is_none() => {
                assert!(!token.is_empty() && !user.is_empty(), "login {code} is granted a token for a user");
                p.granted = Some((token, user));
                true
            }
            _ => false,
        };
        assert!(!granted || self.pending.values().any(|p| p.code == code && p.granted.is_some()), "a grant leaves login {code} granted");
        granted
    }

    fn take_github(&self, state: &str, now: u64) -> Option<String> {
        self.pending.values().find(|p| p.state.as_deref() == Some(state) && now.saturating_sub(p.made) < PENDING_FOR_MS).map(|p| p.code.clone())
    }

    fn grant_github(&mut self, state: &str, token: String, user: String) -> Option<String> {
        let code = match self.pending.values_mut().find(|p| p.state.as_deref() == Some(state)) {
            Some(p) if !p.denied && p.granted.is_none() => {
                assert!(!token.is_empty() && !user.is_empty(), "a GitHub sign-in is granted a token for a user");
                let code = p.code.clone();
                p.granted = Some((token, user));
                p.state = None;
                Some(code)
            }
            _ => None,
        };
        assert!(
            code.as_ref().is_none_or(|c| self.pending.values().any(|p| &p.code == c && p.granted.is_some() && p.state.is_none())),
            "a GitHub grant leaves its login granted and its sign-in spent"
        );
        code
    }
}

impl StateMachine for AuthState {
    fn invariants(&self) {
        assert!(self.pending.len() <= MAX_PENDING, "{} logins pending, over {MAX_PENDING}", self.pending.len());
        for p in self.pending.values() {
            assert!(!p.code.is_empty(), "a pending login has a code");
            assert!(p.tries <= MAX_TRIES + 1, "login {} was tried {} times, over {}", p.code, p.tries, MAX_TRIES + 1);
            assert!(!(p.denied && p.granted.is_some()), "login {} is not both denied and granted", p.code);
            assert!(!p.denied || p.tries > MAX_TRIES, "login {} is denied only after {} wrong invitations", p.code, MAX_TRIES);
            assert!(
                p.granted.as_ref().is_none_or(|(token, user)| !token.is_empty() && !user.is_empty()),
                "login {} is granted a token for a user",
                p.code
            );
        }
        let codes: std::collections::BTreeSet<_> = self.pending.values().map(|p| p.code.as_str()).collect();
        assert_eq!(codes.len(), self.pending.len(), "every pending login has its own code");
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct GithubIn {
    client_id: String,
    client_secret: String,
    url: String,
    authorize: String,
    api: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Issued {
    id: String,
    user: String,
    token: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AllowedIn {
    login: String,
    admin: bool,
}

struct Cfg {
    public: Option<String>,
    port: u16,
    has_tokens: bool,
    access: String,
}

/// How to be let in when an administrator has said nothing more: an allowed GitHub account where GitHub signs people in, an invitation everywhere.
fn default_access(github: bool) -> &'static str {
    if github {
        "ask this server's administrator to allow your GitHub account (`rargod token allow YOUR-LOGIN`) or for an invitation (`rargo auth invite`)"
    } else {
        "ask this server's administrator for an invitation (`rargo auth invite`)"
    }
}

enum ReadItem {
    Chunk(Vec<u8>),
    Ended,
    Socket(ErrorKind, String),
}

#[derive(Debug)]
enum Intake {
    More,
    Silent,
    Reply(Outcome),
    Ready(Request),
}

#[derive(Clone, Debug)]
enum Outcome {
    Redirect(String),
    Body { status: u16, kind: &'static str, body: String },
}

#[derive(Debug)]
enum Effect {
    Random { n: usize, alphabet: &'static [u8] },
    LoadGithub,
    Post { url: String, accept: &'static str, fields: Vec<(&'static str, String)>, timeout_secs: u64 },
    Get { url: String, accept: &'static str, user_agent: &'static str, bearer: String, timeout_secs: u64 },
    Redeem { invite: String, name: Option<String> },
    Create { login: String, admin: bool },
    Revoke { id: String },
    RevokeSelf { token: String },
    Holder { token: String },
    Allowed { login: String },
}

enum Event {
    Request(Request),
    Drawn(Result<String, String>),
    Github(Option<GithubIn>),
    Body(Result<String, String>),
    Redeemed(Result<Issued, String>),
    Created(Result<Issued, String>),
    Revoked,
    RevokedSelf(bool),
    Holder(Option<String>),
    Allowed(Option<AllowedIn>),
}

enum NeedGh {
    Discovery { host: String },
    Page { code: String },
    Redirect { code: String },
    Callback { state: String, code: String },
}

enum DrawNext {
    Device,
    Code { device: String },
    Oauth { code: String, gh: GithubIn },
}

enum Wait {
    Github(NeedGh),
    Draw(DrawNext),
    Exchange { terminal: String, oauth_state: String, gh: GithubIn },
    User { terminal: String, oauth_state: String },
    Allow { terminal: String, oauth_state: String, login: String },
    Create { terminal: String, oauth_state: String },
    Redeem { code: String, shown: String },
    RevokeStop { page_code: String, github: bool },
    RevokeSelf,
    Holder,
    AuthPage { code: String, error: String },
}

#[derive(Default)]
struct Phase {
    wait: Option<Wait>,
}

impl StateMachine for Phase {
    fn invariants(&self) {
        match &self.wait {
            Some(Wait::Draw(DrawNext::Code { device })) => assert!(!device.is_empty(), "drawing a code is for a device"),
            Some(Wait::Redeem { code, .. }) => {
                assert!(!code.is_empty(), "a code to redeem is not empty");
                assert!(code.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()), "code {code:?} is upper-case letters and digits");
            }
            Some(Wait::Exchange { terminal, oauth_state, .. } | Wait::User { terminal, oauth_state } | Wait::Allow { terminal, oauth_state, .. } | Wait::Create { terminal, oauth_state }) => {
                assert!(!terminal.is_empty(), "a login in progress has a terminal");
                assert!(!oauth_state.is_empty(), "a login in progress has an OAuth state");
            }
            _ => {}
        }
    }
}

#[derive(Debug)]
struct Request {
    method: String,
    path: String,
    query: BTreeMap<String, String>,
    host: String,
    body: Vec<u8>,
    json: bool,
}

enum Goal {
    Discovery(String),
    Start,
    Poll(String),
    Revoke(Option<String>),
    Who(Option<String>),
    Page(String),
    Github(String),
    Callback { state: String, code: String },
    Approve { code: String, invite: String, name: String },
    Missing,
}

impl Request {
    fn goal(&self) -> Goal {
        let query = |key: &str| self.query.get(key).map(String::as_str).unwrap_or("").to_string();
        let json_field = |key: &str| {
            serde_json::from_slice::<serde_json::Value>(&self.body).ok().and_then(|v| v.get(key)?.as_str().map(String::from))
        };
        match (self.method.as_str(), self.path.as_str()) {
            ("GET" | "HEAD", "/.well-known/rargo.json") => Goal::Discovery(self.host.clone()),
            ("POST", "/auth/start") => Goal::Start,
            ("POST", "/auth/poll") => {
                let device = if self.json { json_field("device") } else { form(&String::from_utf8_lossy(&self.body)).remove("device") };
                Goal::Poll(device.unwrap_or_default())
            }
            ("POST", "/auth/revoke") => Goal::Revoke(json_field("token")),
            ("POST", "/auth/whoami") => Goal::Who(json_field("token")),
            ("GET", "/auth") => Goal::Page(query("code")),
            ("GET", "/auth/github") => Goal::Github(query("code")),
            ("GET", "/auth/github/callback") => Goal::Callback { state: query("state"), code: query("code") },
            ("POST", "/auth/approve") => {
                let fields = form(&String::from_utf8_lossy(&self.body));
                let field = |key: &str| fields.get(key).map_or("", |v| v.trim()).to_owned();
                Goal::Approve { code: field("code"), invite: field("invite"), name: field("name") }
            }
            _ => Goal::Missing,
        }
    }
}

struct GithubExchange<'a> {
    fields: [(&'static str, &'a str); 4],
}

impl<'a> GithubExchange<'a> {
    fn new(github: &'a GithubIn, code: &'a str, callback: &'a str) -> Self {
        Self { fields: [("client_id", github.client_id.as_str()), ("client_secret", github.client_secret.as_str()), ("code", code), ("redirect_uri", callback)] }
    }

    fn identified(answer: &serde_json::Value) -> Result<&str> {
        if let Some(access) = answer.get("access_token").and_then(serde_json::Value::as_str) {
            return Ok(access);
        }
        anyhow::bail!("GitHub did not confirm the sign-in ({})", answer.get("error_description").and_then(serde_json::Value::as_str).unwrap_or("no reason given"))
    }

    fn login(answer: &serde_json::Value) -> Result<&str> {
        let login = answer.get("login").and_then(serde_json::Value::as_str).context("GitHub did not say who signed in")?;
        if !tokens::valid_login(login) {
            anyhow::bail!("GitHub named an account this server cannot use");
        }
        Ok(login)
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

fn form(s: &str) -> BTreeMap<String, String> {
    s.split('&').filter(|p| !p.is_empty()).map(|p| {
        let (k, v) = p.split_once('=').unwrap_or((p, ""));
        (percent_decode(k), percent_decode(v))
    }).collect()
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

fn invite_form(code: &str, autofocus: bool) -> String {
    format!(
        "<form method=\"post\" action=\"/auth/approve\">\
         <label>Code shown in the terminal<input name=\"code\" value=\"{}\" autocomplete=\"off\" required></label>\
         <label>Invitation (from the server's administrator)<input name=\"invite\" autocomplete=\"off\" required{}></label>\
         <label>Your name, if the invitation does not carry one<input name=\"name\" autocomplete=\"username\"></label>\
         <button type=\"submit\">Give that terminal a token</button></form>",
        escape(&shown(code)),
        if autofocus { " autofocus" } else { "" }
    )
}

fn auth_page(code: &str, error: Option<&str>, cfg: &Cfg, github: bool) -> String {
    if !cfg.has_tokens {
        return page("rargo auth", "<p>This server does not issue tokens.</p>");
    }
    let err = error.map(|e| format!("<p class=\"err\">{}</p>", escape(e))).unwrap_or_default();
    let access = format!("<p>Not let in yet? To build here, {}.</p>", escape(&cfg.access));
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
                 <p>The server learns your GitHub user name and nothing else, and gives a token only to accounts its administrator has allowed.</p>\
                 {access}\
                 <h2>Or with an invitation</h2>{}",
                escape(&shown(code)),
                invite_form(code, false)
            ),
        );
    }
    page(
        "rargo auth",
        &format!(
            "<p>A terminal is asking for a token to build on this server. Go on only if the code below is the one \
             <code>rargo auth</code> is showing you.</p>\
             {err}{}{access}",
            invite_form(code, true)
        ),
    )
}

fn server_for_host(cfg: &Cfg, host: &str) -> String {
    match &cfg.public {
        Some(p) => p.clone(),
        None => {
            let name = host.rsplit_once(':').map_or(host, |(h, _)| h);
            format!("{name}:{}", cfg.port)
        }
    }
}

fn json_ok(v: serde_json::Value) -> Outcome {
    Outcome::Body { status: 200, kind: "application/json", body: v.to_string() }
}

fn html(status: u16, body: String) -> Outcome {
    Outcome::Body { status, kind: "text/html; charset=utf-8", body }
}

fn bad_request(msg: impl std::fmt::Display) -> Outcome {
    Outcome::Body { status: 400, kind: "text/plain", body: format!("bad request: {msg}\n") }
}

fn upgrade_required(msg: &str) -> Outcome {
    Outcome::Body { status: 409, kind: "application/json", body: serde_json::json!({ "error": msg }).to_string() }
}

fn service_error(msg: &str) -> Outcome {
    Outcome::Body { status: 503, kind: "application/json", body: serde_json::json!({ "error": msg }).to_string() }
}

fn callback_error(msg: &str) -> Outcome {
    html(403, page("rargo auth", &format!("<p class=\"err\">{}</p><p>Run <code>rargo auth</code> again to start over.</p>", escape(msg))))
}

fn authenticated(code: &str, user: &str, github: bool) -> Outcome {
    let who = if github {
        format!("a token for the GitHub account <b>{}</b>", escape(user))
    } else {
        format!("a token for <b>{}</b>", escape(user))
    };
    html(
        200,
        page(
            "Authenticated",
            &format!(
                "<p>The terminal showing <code class=\"code\">{}</code> now holds {who}.</p>\
                 <p>You can close this page and go back to it: <code>rargo build</code> will work.</p>",
                escape(&shown(code))
            ),
        ),
    )
}

fn issued_note(user: &str, github: bool) -> String {
    if github {
        format!("rargod: a token for the GitHub account {user} was issued through `rargo auth`")
    } else {
        format!("rargod: a token for {user} was issued through `rargo auth`")
    }
}

fn authorize_url(gh: &GithubIn, state: &str) -> String {
    format!(
        "{}/login/oauth/authorize?client_id={}&redirect_uri={}%2Fauth%2Fgithub%2Fcallback&state={state}&allow_signup=false",
        gh.authorize,
        gh.client_id,
        gh.url.replace(':', "%3A").replace('/', "%2F"),
    )
}

fn parse_github(body: &str) -> Result<serde_json::Value, String> {
    serde_json::from_str(body).map_err(|e| format!("GitHub's answer: {e}"))
}

fn ask(phase: &mut Phase, wait: Wait, effect: Effect) -> Act {
    phase.wait = Some(wait);
    Act::Ask(effect)
}

fn done(phase: &mut Phase, outcome: Outcome, notes: Vec<String>) -> Act {
    phase.wait = None;
    Act::Done { outcome, notes }
}

enum Act {
    Ask(Effect),
    Done { outcome: Outcome, notes: Vec<String> },
}

fn intake(buf: &mut Vec<u8>, item: ReadItem) -> Intake {
    match item {
        ReadItem::Ended => Intake::Reply(bad_request("the request ended early")),
        ReadItem::Socket(kind, _) if matches!(kind, ErrorKind::TimedOut | ErrorKind::WouldBlock) => Intake::Silent,
        ReadItem::Socket(_, msg) => Intake::Reply(bad_request(msg)),
        ReadItem::Chunk(bytes) => {
            buf.extend_from_slice(&bytes);
            match parse_request(buf) {
                Ok(None) => Intake::More,
                Ok(Some(req)) => Intake::Ready(req),
                Err(e) => Intake::Reply(bad_request(e)),
            }
        }
    }
}

/// One step of the sign-in handler. `now` is milliseconds supplied by the shell.
/// Random draws, the GitHub application and token-store results arrive as events.
/// GitHub calls and token-store writes leave as effects.
fn handle(state: &mut AuthState, phase: &mut Phase, event: Event, cfg: &Cfg, now: u64) -> Act {
    let act = step(state, phase, event, cfg, now);
    state.invariants();
    phase.invariants();
    act
}

fn step(state: &mut AuthState, phase: &mut Phase, event: Event, cfg: &Cfg, now: u64) -> Act {
    let wait = phase.wait.take();
    match (wait, event) {
        (None, Event::Request(req)) => begin_request(state, phase, req, cfg, now),
        (Some(Wait::Github(need)), Event::Github(app)) => match need {
            NeedGh::Discovery { host } => done(phase, discovery(cfg, &host, app.is_some()), Vec::new()),
            NeedGh::Page { code } if !code.is_empty() && cfg.has_tokens && !state.waiting(&normal_code(&code), now) => {
                let error = "no terminal is waiting with this code: it is good for ten minutes and once. Run `rargo auth` again and open the address it prints";
                done(phase, html(404, auth_page(&code, Some(error), cfg, app.is_some())), Vec::new())
            }
            NeedGh::Page { code } => done(phase, html(200, auth_page(&code, None, cfg, app.is_some())), Vec::new()),
            NeedGh::Redirect { code } => match app {
                None => done(phase, html(403, auth_page(&code, Some("this server does not sign people in with GitHub"), cfg, false)), Vec::new()),
                Some(gh) => ask(phase, Wait::Draw(DrawNext::Oauth { code, gh }), Effect::Random { n: 32, alphabet: DEVICE_ALPHABET }),
            },
            NeedGh::Callback { state: oauth, code } => callback_open(state, phase, cfg, now, oauth, code, app),
        },
        (Some(Wait::Draw(next)), Event::Drawn(drawn)) => match (next, drawn) {
            (DrawNext::Device, Err(e)) | (DrawNext::Code { .. }, Err(e)) => done(phase, service_error(&e), Vec::new()),
            (DrawNext::Device, Ok(device)) => ask(phase, Wait::Draw(DrawNext::Code { device }), Effect::Random { n: 8, alphabet: CODE_ALPHABET }),
            (DrawNext::Code { device }, Ok(code)) => {
                if state.begin(device.clone(), code.clone(), now) {
                    done(phase, started(&device, &code, &cfg.access), Vec::new())
                } else {
                    ask(phase, Wait::Draw(DrawNext::Device), Effect::Random { n: 40, alphabet: DEVICE_ALPHABET })
                }
            }
            (DrawNext::Oauth { code, .. }, Err(e)) => done(phase, html(403, auth_page(&code, Some(&e), cfg, true)), Vec::new()),
            (DrawNext::Oauth { code, gh }, Ok(oauth)) => match state.authorize(&normal_code(&code), now) {
                Err(e) => done(phase, html(403, auth_page(&code, Some(&format!("{e:#}")), cfg, true)), Vec::new()),
                Ok(p) => {
                    p.state = Some(oauth.clone());
                    done(phase, Outcome::Redirect(authorize_url(&gh, &oauth)), Vec::new())
                }
            },
        },
        (Some(Wait::Exchange { terminal, oauth_state, gh }), Event::Body(body)) => match body.and_then(|t| parse_github(&t)) {
            Err(e) => done(phase, callback_error(&e), Vec::new()),
            Ok(answer) => match GithubExchange::identified(&answer) {
                Err(e) => done(phase, callback_error(&format!("{e:#}")), Vec::new()),
                Ok(access) => ask(
                    phase,
                    Wait::User { terminal, oauth_state },
                    Effect::Get { url: format!("{}/user", gh.api), accept: "application/vnd.github+json", user_agent: "rargod", bearer: access.to_string(), timeout_secs: 15 },
                ),
            },
        },
        (Some(Wait::User { terminal, oauth_state }), Event::Body(body)) => match body.and_then(|t| parse_github(&t)) {
            Err(e) => done(phase, callback_error(&e), Vec::new()),
            Ok(answer) => match GithubExchange::login(&answer) {
                Err(e) => done(phase, callback_error(&format!("{e:#}")), Vec::new()),
                Ok(login) => ask(phase, Wait::Allow { terminal, oauth_state, login: login.to_string() }, Effect::Allowed { login: login.to_string() }),
            },
        },
        (Some(Wait::Allow { terminal, oauth_state, login }), Event::Allowed(allowed)) => match allowed {
            None => done(phase, callback_error(&format!("the GitHub account {login} is not on this server's allow list. Its administrator adds it with: rargod token allow {login}")), Vec::new()),
            Some(allowed) => ask(phase, Wait::Create { terminal, oauth_state }, Effect::Create { login: allowed.login, admin: allowed.admin }),
        },
        (Some(Wait::Create { terminal, oauth_state }), Event::Created(created)) => match created {
            Err(e) => done(phase, callback_error(&e), Vec::new()),
            Ok(issued) => match state.grant_github(&oauth_state, issued.token, issued.user.clone()) {
                Some(code) => done(phase, authenticated(&code, &issued.user, true), vec![issued_note(&issued.user, true)]),
                None => ask(phase, Wait::RevokeStop { page_code: terminal, github: true }, Effect::Revoke { id: issued.id }),
            },
        },
        (Some(Wait::Redeem { code, shown }), Event::Redeemed(redeemed)) => match redeemed {
            Err(e) => ask(phase, Wait::AuthPage { code: shown, error: e }, Effect::LoadGithub),
            Ok(issued) => {
                if state.grant(&code, issued.token, issued.user.clone()) {
                    done(phase, authenticated(&shown, &issued.user, false), vec![issued_note(&issued.user, false)])
                } else {
                    ask(phase, Wait::RevokeStop { page_code: shown, github: false }, Effect::Revoke { id: issued.id })
                }
            }
        },
        (Some(Wait::RevokeStop { page_code, github }), Event::Revoked) => {
            let msg = "the terminal stopped waiting: run `rargo auth` again";
            if github {
                done(phase, callback_error(msg), Vec::new())
            } else {
                ask(phase, Wait::AuthPage { code: page_code, error: msg.to_string() }, Effect::LoadGithub)
            }
        }
        (Some(Wait::AuthPage { code, error }), Event::Github(app)) => done(phase, html(403, auth_page(&code, Some(&error), cfg, app.is_some())), Vec::new()),
        (Some(Wait::RevokeSelf), Event::RevokedSelf(done_it)) => {
            let notes = done_it.then(|| "rargod: a token was revoked by its holder (`rargo auth remove`)".to_string()).into_iter().collect();
            done(phase, json_ok(serde_json::json!({ "revoked": done_it })), notes)
        }
        (Some(Wait::Holder), Event::Holder(user)) => done(phase, json_ok(serde_json::json!({ "user": user })), Vec::new()),
        _ => done(phase, Outcome::Body { status: 500, kind: "text/plain", body: "the handler was given an event it did not ask for\n".into() }, Vec::new()),
    }
}

fn begin_request(state: &mut AuthState, phase: &mut Phase, req: Request, cfg: &Cfg, now: u64) -> Act {
    match req.goal() {
        Goal::Discovery(host) => ask(phase, Wait::Github(NeedGh::Discovery { host }), Effect::LoadGithub),
        Goal::Start => {
            let protocol = serde_json::from_slice::<serde_json::Value>(&req.body)
                .ok().and_then(|body| u32::try_from(body.get("protocol")?.as_u64()?).ok());
            match protocol {
                Some(version) if version == crate::proto::PROTO_VERSION => start(state, phase, cfg, now),
                Some(version) => done(phase, upgrade_required(&crate::proto::protocol_mismatch(version)), Vec::new()),
                None => done(phase, upgrade_required(&crate::proto::protocol_unmarked()), Vec::new()),
            }
        }
        Goal::Poll(device) => done(phase, json_ok(state.poll(&device, now)), Vec::new()),
        Goal::Revoke(token) => match (cfg.has_tokens, token) {
            (true, Some(token)) => ask(phase, Wait::RevokeSelf, Effect::RevokeSelf { token }),
            _ => done(phase, json_ok(serde_json::json!({ "revoked": false })), Vec::new()),
        },
        Goal::Who(token) => match (cfg.has_tokens, token) {
            (true, Some(token)) => ask(phase, Wait::Holder, Effect::Holder { token }),
            _ => done(phase, json_ok(serde_json::json!({ "user": None::<String> })), Vec::new()),
        },
        Goal::Page(code) => {
            let notice = req.query.get("protocol").and_then(|v| v.parse::<u32>().ok()).map_or_else(
                || (!code.is_empty()).then(crate::proto::protocol_unmarked),
                |version| (version != crate::proto::PROTO_VERSION).then(|| crate::proto::protocol_mismatch(version)),
            );
            if let Some(notice) = notice {
                done(phase, html(409, page("Update rargo", &format!("<p>{}</p>", escape(&notice)))), Vec::new())
            } else {
                ask(phase, Wait::Github(NeedGh::Page { code }), Effect::LoadGithub)
            }
        }
        Goal::Github(code) => ask(phase, Wait::Github(NeedGh::Redirect { code }), Effect::LoadGithub),
        Goal::Callback { state: oauth, code } => ask(phase, Wait::Github(NeedGh::Callback { state: oauth, code }), Effect::LoadGithub),
        Goal::Approve { code, invite, name } => approve(state, phase, cfg, now, code, invite, name),
        Goal::Missing => done(phase, Outcome::Body { status: 404, kind: "text/plain", body: "this is a rargo build server; see https://rargo.rs\n".into() }, Vec::new()),
    }
}

fn discovery(cfg: &Cfg, host: &str, github: bool) -> Outcome {
    json_ok(serde_json::json!({
        "server": server_for_host(cfg, host),
        "protocol": crate::proto::PROTO_VERSION,
        "version": env!("CARGO_PKG_VERSION"),
        "auth": cfg.has_tokens,
        "github": github,
    }))
}

fn started(device: &str, code: &str, access: &str) -> Outcome {
    json_ok(serde_json::json!({
        "device": device,
        "access": access,
        "code": shown(code),
        "path": format!("/auth?code={}&protocol={}", shown(code), crate::proto::PROTO_VERSION),
        "interval": 2,
        "expires_in": PENDING_FOR_MS / 1000,
    }))
}

fn start(state: &mut AuthState, phase: &mut Phase, cfg: &Cfg, now: u64) -> Act {
    if !cfg.has_tokens {
        return done(phase, service_error("this server does not issue tokens"), Vec::new());
    }
    state.sweep(now);
    if state.pending.len() >= MAX_PENDING {
        return done(phase, service_error("too many requests are waiting to be confirmed; try again in a few minutes"), Vec::new());
    }
    ask(phase, Wait::Draw(DrawNext::Device), Effect::Random { n: 40, alphabet: DEVICE_ALPHABET })
}

fn approve(state: &mut AuthState, phase: &mut Phase, cfg: &Cfg, now: u64, code: String, invite: String, name: String) -> Act {
    if !cfg.has_tokens {
        return done(phase, html(403, auth_page(&code, None, cfg, false)), Vec::new());
    }
    let normalized = normal_code(&code);
    if let Err(e) = state.invite_attempt(&normalized, now) {
        return ask(phase, Wait::AuthPage { code, error: format!("{e:#}") }, Effect::LoadGithub);
    }
    let name = (!name.is_empty()).then_some(name);
    ask(phase, Wait::Redeem { code: normalized, shown: code }, Effect::Redeem { invite, name })
}

fn callback_open(state: &mut AuthState, phase: &mut Phase, cfg: &Cfg, now: u64, oauth: String, gh_code: String, app: Option<GithubIn>) -> Act {
    let Some(gh) = app else {
        return done(phase, callback_error("this server does not sign people in with GitHub"), Vec::new());
    };
    if !cfg.has_tokens {
        return done(phase, callback_error("this server does not issue tokens"), Vec::new());
    }
    if oauth.is_empty() || gh_code.is_empty() {
        return done(phase, callback_error("GitHub sent the browser back without a sign-in (it was cancelled there)"), Vec::new());
    }
    let Some(terminal) = state.take_github(&oauth, now) else {
        return done(phase, callback_error("this sign-in belongs to no terminal that is still waiting"), Vec::new());
    };
    let callback = format!("{}/auth/github/callback", gh.url);
    let exchange = GithubExchange::new(&gh, &gh_code, &callback);
    let fields = exchange.fields.iter().map(|(k, v)| (*k, (*v).to_string())).collect();
    ask(
        phase,
        Wait::Exchange { terminal, oauth_state: oauth, gh: gh.clone() },
        Effect::Post { url: format!("{}/login/oauth/access_token", gh.authorize), accept: "application/json", fields, timeout_secs: 15 },
    )
}

fn parse_request(buf: &[u8]) -> Result<Option<Request>> {
    let Some(head_end) = buf.windows(4).position(|part| part == b"\r\n\r\n").map(|i| i + 4) else {
        if buf.len() > 16 << 10 {
            anyhow::bail!("the request's head is too long");
        }
        return Ok(None);
    };
    if head_end > 16 << 10 {
        anyhow::bail!("the request's head is too long");
    }
    let head = std::str::from_utf8(&buf[..head_end - 4]).context("invalid request header text")?;
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let (Some(method), Some(target), Some("HTTP/1.0" | "HTTP/1.1"), None) = (first.next(), first.next(), first.next(), first.next()) else {
        anyhow::bail!("malformed request line");
    };
    if method.is_empty() || !target.starts_with('/') {
        anyhow::bail!("malformed request target");
    }
    let mut host = String::new();
    let mut length = None;
    let mut json = false;
    for line in lines {
        let Some((k, v)) = line.split_once(':') else { anyhow::bail!("malformed request header") };
        let v = v.trim();
        match k.to_ascii_lowercase().as_str() {
            "host" => host = v.to_string(),
            "x-forwarded-host" => host = v.to_string(),
            "content-length" if length.is_none() => length = Some(v.parse::<usize>().context("Content-Length")?),
            "content-length" => anyhow::bail!("multiple Content-Length headers"),
            "content-type" => json = v.starts_with("application/json"),
            _ => {}
        }
    }
    let length = length.unwrap_or(0);
    if length > 64 << 10 {
        anyhow::bail!("the request's body is too long");
    }
    let Some(body) = buf.get(head_end..head_end + length) else { return Ok(None) };
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    Ok(Some(Request { method: method.into(), path: path.into(), query: form(query), host, body: body.to_vec(), json }))
}

fn next_item(stream: &mut rargo_net::io::TcpStream, deadline: Instant) -> ReadItem {
    let remain = deadline.saturating_duration_since(crate::io::now());
    if remain.is_zero() {
        return ReadItem::Socket(ErrorKind::TimedOut, "timed out".into());
    }
    let mut chunk = [0u8; 2048];
    match rargo_net::io::timeout(remain, || stream.read(&mut chunk)) {
        Ok(Ok(0)) => ReadItem::Ended,
        Ok(Ok(n)) => ReadItem::Chunk(chunk[..n].to_vec()),
        Ok(Err(e)) => ReadItem::Socket(e.kind(), e.to_string()),
        Err(_) => ReadItem::Socket(ErrorKind::TimedOut, "timed out".into()),
    }
}

fn write_all_within(stream: &mut rargo_net::io::TcpStream, bytes: &[u8]) -> Result<()> {
    rargo_net::io::timeout(Duration::from_secs(20), || stream.write_all(bytes))
        .map_err(|_| anyhow!("timed out"))?
        .map_err(|e| anyhow!("{e}"))
}

fn write_outcome(stream: &mut rargo_net::io::TcpStream, outcome: &Outcome) -> Result<()> {
    match outcome {
        Outcome::Redirect(location) => {
            let head = format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n");
            write_all_within(stream, head.as_bytes())?;
            let _ = stream.shutdown();
            Ok(())
        }
        Outcome::Body { status, kind, body } => {
            let reason = match status {
                200 => "OK",
                400 => "Bad Request",
                403 => "Forbidden",
                404 => "Not Found",
                _ => "Service Unavailable",
            };
            let head = format!("HTTP/1.1 {status} {reason}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n", body.len());
            write_all_within(stream, head.as_bytes())?;
            write_all_within(stream, body.as_bytes())?;
            let _ = stream.shutdown();
            Ok(())
        }
    }
}

fn now_ms() -> u64 {
    crate::io::system_now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub struct Http {
    keys: Arc<Keys>,
    public: Option<String>,
    port: u16,
    state: Option<std::path::PathBuf>,
    pending: std::cell::RefCell<AuthState>,
}

impl Http {
    pub fn new(keys: Arc<Keys>, public: Option<String>, port: u16) -> Http {
        Http { keys, public, port, state: None, pending: std::cell::RefCell::new(AuthState::default()) }
    }

    pub fn with_state(mut self, state: std::path::PathBuf) -> Http {
        self.state = Some(state);
        self
    }

    fn view_cfg(&self) -> Cfg {
        let github = self.github();
        let access = match &github {
            Some(g) if !g.access.is_empty() => g.access.clone(),
            _ => default_access(github.is_some()).to_string(),
        };
        Cfg { public: self.public.clone(), port: self.port, has_tokens: self.keys.tokens().is_some(), access }
    }

    fn github(&self) -> Option<tokens::Github> {
        tokens::Github::load(self.state.as_deref()?)
    }

    #[cfg(test)]
    fn server_for(&self, host: &str) -> String {
        server_for_host(&self.view_cfg(), host)
    }

    pub fn serve(&self, mut stream: rargo_net::io::TcpStream) -> Result<()> {
        let mut buf = Vec::new();
        let deadline = crate::io::now() + Duration::from_secs(20);
        let req = loop {
            match intake(&mut buf, next_item(&mut stream, deadline)) {
                Intake::More => {}
                Intake::Silent => return Ok(()),
                Intake::Reply(outcome) => return write_outcome(&mut stream, &outcome),
                Intake::Ready(req) => break req,
            }
        };
        let now = now_ms();
        let cfg = self.view_cfg();
        let mut phase = Phase::default();
        let mut event = Event::Request(req);
        loop {
            let act = {
                let mut st = self.pending.borrow_mut();
                let act = handle(&mut st, &mut phase, event, &cfg, now);
                st.invariants();
                phase.invariants();
                act
            };
            match act {
                Act::Done { outcome, notes } => {
                    for note in notes {
                        eprintln!("{note}");
                    }
                    return write_outcome(&mut stream, &outcome);
                }
                Act::Ask(effect) => event = self.perform(effect),
            }
        }
    }

    fn perform(&self, effect: Effect) -> Event {
        match effect {
            Effect::Random { n, alphabet } => Event::Drawn(tokens::random_text(n, alphabet).map_err(|e| format!("{e:#}"))),
            Effect::LoadGithub => Event::Github(self.load_github()),
            Effect::Post { url, accept, fields, timeout_secs } => Event::Body(post_form(&url, accept, &fields, timeout_secs)),
            Effect::Get { url, accept, user_agent, bearer, timeout_secs } => Event::Body(get_bearer(&url, accept, user_agent, &bearer, timeout_secs)),
            Effect::Redeem { invite, name } => Event::Redeemed(self.redeem(&invite, name.as_deref())),
            Effect::Create { login, admin } => Event::Created(self.create_token(&login, admin)),
            Effect::Revoke { id } => {
                if let Some(store) = self.keys.tokens() {
                    let _ = store.revoke(&id);
                }
                Event::Revoked
            }
            Effect::RevokeSelf { token } => Event::RevokedSelf(self.keys.tokens().and_then(|store| store.revoke_self(&token).ok()).unwrap_or(false)),
            Effect::Holder { token } => Event::Holder(self.keys.tokens().and_then(|store| store.holder(&token))),
            Effect::Allowed { login } => Event::Allowed(self.keys.tokens().and_then(|store| store.allowed(&login)).map(|a| AllowedIn { login: a.login, admin: a.admin })),
        }
    }

    fn load_github(&self) -> Option<GithubIn> {
        let g = self.github()?;
        Some(GithubIn { client_id: g.client_id, client_secret: g.client_secret, url: g.url, authorize: github_url(), api: github_api() })
    }

    fn redeem(&self, invite: &str, name: Option<&str>) -> Result<Issued, String> {
        let store = self.keys.tokens().context("this server does not issue tokens").map_err(|e| format!("{e:#}"))?;
        let (token, secret) = store.redeem(invite, name).map_err(|e| format!("{e:#}"))?;
        Ok(Issued { id: token.id.clone(), user: token.user.clone(), token: tokens::token_text(&token, &secret) })
    }

    fn create_token(&self, login: &str, admin: bool) -> Result<Issued, String> {
        let store = self.keys.tokens().context("this server does not issue tokens").map_err(|e| format!("{e:#}"))?;
        let (token, secret) = store.create(login, admin, None, "github").map_err(|e| format!("{e:#}"))?;
        Ok(Issued { id: token.id.clone(), user: token.user.clone(), token: tokens::token_text(&token, &secret) })
    }
}

fn github_url() -> String {
    crate::io::env::var("RARGO_GITHUB_URL").unwrap_or_else(|_| "https://github.com".into()).trim_end_matches('/').to_string()
}

fn github_api() -> String {
    crate::io::env::var("RARGO_GITHUB_API").unwrap_or_else(|_| "https://api.github.com".into()).trim_end_matches('/').to_string()
}

/// The most bytes of an answer that are read as its text.
const TEXT_LIMIT: usize = 10 * 1024 * 1024;

/// The text of GitHub's answer to `request`, or what went wrong, which `asking` introduces.
fn answer_text(request: &outbound::Request, asking: &str) -> Result<String, String> {
    let answer = match outbound::fetch(request) {
        Ok(answer) => answer,
        Err(Failure::Send(e)) => return Err(format!("{asking}: {e}")),
        Err(Failure::Body(e)) => return Err(format!("{:#}", anyhow!(e).context("reading GitHub's answer"))),
    };
    if let Some(said) = answer.refused {
        return Err(format!("{asking}: {said}"));
    }
    if answer.body.len() > TEXT_LIMIT {
        return Err("reading GitHub's answer: response too big for into_string".into());
    }
    Ok(String::from_utf8_lossy(&answer.body).to_string())
}

fn post_form(url: &str, accept: &str, fields: &[(&'static str, String)], timeout_secs: u64) -> Result<String, String> {
    let owned: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let request = outbound::Request {
        method: "POST",
        url,
        headers: &[("Accept", accept)],
        form: Some(&owned),
        redirects: None,
        user_agent: None,
        timeout: Duration::from_secs(timeout_secs),
        body_limit: TEXT_LIMIT as u64 + 1,
    };
    answer_text(&request, "asking GitHub")
}

fn get_bearer(url: &str, accept: &str, user_agent: &str, bearer: &str, timeout_secs: u64) -> Result<String, String> {
    let authorization = format!("Bearer {bearer}");
    let request = outbound::Request {
        method: "GET",
        url,
        headers: &[("Authorization", &authorization), ("Accept", accept), ("User-Agent", user_agent)],
        form: None,
        redirects: None,
        user_agent: None,
        timeout: Duration::from_secs(timeout_secs),
        body_limit: TEXT_LIMIT as u64 + 1,
    };
    answer_text(&request, "asking GitHub who signed in")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Transcript;
    use expect_test::expect;

    const NOW: u64 = 1_700_000_000_000;

    fn cfg(tokens: bool) -> Cfg {
        Cfg { public: None, port: 7459, has_tokens: tokens, access: "ask for it.".into() }
    }

    fn gh() -> GithubIn {
        GithubIn { client_id: "Iv1.abc".into(), client_secret: "sec".into(), url: "https://build.example".into(), authorize: "https://github.com".into(), api: "https://api.github.com".into() }
    }

    fn issued(id: &str, user: &str) -> Issued {
        Issued { id: id.into(), user: user.into(), token: format!("{id}.secret") }
    }

    fn parsed(raw: &str) -> Request {
        let mut buf = Vec::new();
        match intake(&mut buf, ReadItem::Chunk(raw.as_bytes().to_vec())) {
            Intake::Ready(req) => req,
            Intake::More => panic!("fixture needs another chunk"),
            Intake::Silent => panic!("fixture was silent"),
            Intake::Reply(outcome) => panic!("fixture was rejected: {}", body_of(&outcome).2),
        }
    }

    fn call(method: &str, target: &str, kind: Option<&str>, body: &str) -> Request {
        let extra = match kind {
            Some(k) => format!("Content-Type: {k}\r\nContent-Length: {}\r\n", body.len()),
            None if !body.is_empty() => format!("Content-Length: {}\r\n", body.len()),
            None => String::new(),
        };
        parsed(&format!("{method} {target} HTTP/1.1\r\nHost: build.example\r\n{extra}\r\n{body}"))
    }

    fn body_of(outcome: &Outcome) -> (u16, &'static str, &str) {
        match outcome {
            Outcome::Body { status, kind, body } => (*status, *kind, body),
            Outcome::Redirect(location) => panic!("redirect {location}"),
        }
    }

    #[test]
    fn outdated_clients_get_an_upgrade_notice_before_starting_sign_in() {
        let mut state = AuthState::default();
        for (method, path, body) in [
            ("POST", "/auth/start", r#"{"machine":"old-host"}"#),
            ("POST", "/auth/start", r#"{"protocol":7}"#),
            ("POST", "/auth/start", r#"{"protocol":8}"#),
            ("POST", "/auth/start", r#"{"protocol":9}"#),
            ("GET", "/auth?code=ABCD-2345", ""),
            ("GET", "/auth?code=ABCD-2345&protocol=9", ""),
        ] {
            let req = call(method, path, Some("application/json"), body);
            let (outcome, _) = drive(&mut state, req, NOW, &cfg(true), &mut Script::default());
            let (status, _, body) = body_of(&outcome);
            assert_eq!(status, 409);
            assert!(body.contains(&format!("cargo install rargo --version {}", env!("CARGO_PKG_VERSION"))));
        }
    }

    struct Script {
        random: Vec<Result<String, String>>,
        github: Vec<Option<GithubIn>>,
        body: Vec<Result<String, String>>,
        redeem: Vec<Result<Issued, String>>,
        created: Vec<Result<Issued, String>>,
        allowed: Vec<Option<AllowedIn>>,
        holder: Vec<Option<String>>,
        revoked_self: Vec<bool>,
        effects: Vec<String>,
    }

    impl Script {
        fn answer(&mut self, effect: Effect) -> Event {
            self.effects.push(match &effect {
                Effect::Random { n, .. } => format!("random {n}"),
                Effect::LoadGithub => "github".into(),
                Effect::Post { url, fields, .. } => format!("post {url} {}", fields.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")),
                Effect::Get { url, bearer, .. } => format!("get {url} {bearer}"),
                Effect::Redeem { invite, name } => format!("redeem {invite} {}", name.as_deref().unwrap_or("-")),
                Effect::Create { login, admin } => format!("create {login} {admin}"),
                Effect::Revoke { id } => format!("revoke {id}"),
                Effect::RevokeSelf { token } => format!("revoke-self {token}"),
                Effect::Holder { token } => format!("holder {token}"),
                Effect::Allowed { login } => format!("allowed {login}"),
            });
            fn take<T>(name: &str, slot: &mut Vec<T>) -> T {
                slot.pop().unwrap_or_else(|| panic!("no scripted answer for {name}"))
            }
            match effect {
                Effect::Random { .. } => Event::Drawn(take("random", &mut self.random)),
                Effect::LoadGithub => Event::Github(take("github", &mut self.github)),
                Effect::Post { .. } | Effect::Get { .. } => Event::Body(take("body", &mut self.body)),
                Effect::Redeem { .. } => Event::Redeemed(take("redeem", &mut self.redeem)),
                Effect::Create { .. } => Event::Created(take("created", &mut self.created)),
                Effect::Revoke { .. } => Event::Revoked,
                Effect::RevokeSelf { .. } => Event::RevokedSelf(take("revoke-self", &mut self.revoked_self)),
                Effect::Holder { .. } => Event::Holder(take("holder", &mut self.holder)),
                Effect::Allowed { .. } => Event::Allowed(take("allowed", &mut self.allowed)),
            }
        }
    }

    impl Default for Script {
        fn default() -> Self {
            Self { random: Vec::new(), github: Vec::new(), body: Vec::new(), redeem: Vec::new(), created: Vec::new(), allowed: Vec::new(), holder: Vec::new(), revoked_self: Vec::new(), effects: Vec::new() }
        }
    }

    fn drive(st: &mut AuthState, req: Request, now: u64, cfg: &Cfg, script: &mut Script) -> (Outcome, Vec<String>) {
        let mut phase = Phase::default();
        let mut event = Event::Request(req);
        for _ in 0..20 {
            let act = handle(st, &mut phase, event, cfg, now);
            st.invariants();
            phase.invariants();
            match act {
                Act::Done { outcome, notes } => return (outcome, notes),
                Act::Ask(effect) => event = script.answer(effect),
            }
        }
        panic!("handler did not settle");
    }

    fn script_rev<T>(items: Vec<T>) -> Vec<T> {
        let mut items = items;
        items.reverse();
        items
    }

    #[test]
    fn malformed_requests_are_400_and_a_timeout_is_silent() {
        use ReadItem::{Chunk, Ended, Socket};
        let raw = |bytes: &[u8]| Chunk(bytes.to_vec());
        let cases = [
            ("timeout", vec![Socket(ErrorKind::TimedOut, "timed out".into())]),
            ("would block", vec![Socket(ErrorKind::WouldBlock, "again".into())]),
            ("early end", vec![Ended]),
            ("short body", vec![raw(b"POST /auth/poll HTTP/1.1\r\nContent-Length: 10\r\n\r\nabc"), Ended]),
            ("reset", vec![Socket(ErrorKind::ConnectionReset, "reset".into())]),
            ("bad version", vec![raw(b"GET /oops HTTP/2.0\r\n\r\n")]),
            ("split body", vec![raw(b"POST / HTTP/1.1\r\nContent-Length: 5\r\n\r\n1234"), raw(b"5")]),
            ("size limit", vec![Chunk(vec![b'A'; (16 << 10) + 1])]),
        ];
        let mut outcomes = Transcript::default();
        for (case, items) in cases {
            let mut buf = Vec::new();
            for item in items {
                outcomes.record(case, intake(&mut buf, item));
            }
        }
        expect![[r#"
            timeout: Silent
            would block: Silent
            early end: Reply(Body { status: 400, kind: "text/plain", body: "bad request: the request ended early\n" })
            short body: More
            short body: Reply(Body { status: 400, kind: "text/plain", body: "bad request: the request ended early\n" })
            reset: Reply(Body { status: 400, kind: "text/plain", body: "bad request: reset\n" })
            bad version: Reply(Body { status: 400, kind: "text/plain", body: "bad request: malformed request line\n" })
            split body: More
            split body: Ready(Request { method: "POST", path: "/", query: {}, host: "", body: [49, 50, 51, 52, 53], json: false })
            size limit: Reply(Body { status: 400, kind: "text/plain", body: "bad request: the request's head is too long\n" })
        "#]].assert_eq(&outcomes.0);
    }

    #[test]
    fn routes_decode_input_before_any_endpoint_effect() {
        fn get(path: &str) -> Request {
            parsed(&format!("GET {path} HTTP/1.1\r\nHost: build.example\r\n\r\n"))
        }
        assert!(matches!(get("/.well-known/rargo.json").goal(), Goal::Discovery(host) if host == "build.example"));
        assert!(matches!(get("/auth/github?code=AB%2DCD").goal(), Goal::Github(code) if code == "AB-CD"));
        assert!(matches!(get("/auth/github/callback?state=abc&code=x").goal(), Goal::Callback { state, code } if state == "abc" && code == "x"));
        assert!(matches!(get("/missing").goal(), Goal::Missing));
        let post = |path: &str, body: &str, kind: &str| parsed(&format!("POST {path} HTTP/1.1\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n\r\n{body}", body.len()));
        assert!(matches!(post("/auth/start", "", "text/plain").goal(), Goal::Start));
        assert!(matches!(post("/auth/poll", "device=abc", "application/x-www-form-urlencoded").goal(), Goal::Poll(device) if device == "abc"));
        assert!(matches!(post("/auth/approve", "code=abc&name=Ann+Lee", "application/x-www-form-urlencoded").goal(), Goal::Approve { code, name, .. } if code == "abc" && name == "Ann Lee"));
        for route in ["/auth/revoke", "/auth/whoami"] {
            let request = post(route, r#"{"token":"fixture"}"#, "application/json");
            assert!(matches!(request.goal(), Goal::Revoke(Some(token)) | Goal::Who(Some(token)) if token == "fixture"));
        }
        assert!(auth_page("abc", None, &cfg(false), false).contains("does not issue tokens"));
        assert!(auth_page("abc", Some("<error>"), &cfg(true), false).contains("&lt;error&gt;"));
        assert!(auth_page("abc", None, &cfg(true), true).contains("Sign in with GitHub"));
        let mut outcomes = Transcript::default();
        for path in ["/auth?code=AB%2DCD", "/auth?code=100%"] {
            let Goal::Page(code) = get(path).goal() else { panic!("expected an auth page") };
            outcomes.record(path, code);
        }
        expect![[r#"
            /auth?code=AB%2DCD: "AB-CD"
            /auth?code=100%: "100%"
        "#]].assert_eq(&outcomes.0);
        assert_eq!(server_for_host(&Cfg { public: None, port: 7459, has_tokens: false, access: String::new() }, "build.example:8080"), "build.example:7459");
        assert_eq!(server_for_host(&Cfg { public: Some("b.example:1".into()), port: 7459, has_tokens: false, access: String::new() }, "rargo.rs"), "b.example:1");
        assert_eq!(Http::new(Arc::new(Keys::open()), None, 7459).server_for("build.example"), "build.example:7459");
    }

    #[test]
    fn discovery_start_poll_revoke_whoami_pages_and_the_missing_route() {
        let mut st = AuthState::default();
        let c = cfg(true);
        let mut script = Script { github: script_rev(vec![Some(gh())]), ..Script::default() };
        let (outcome, notes) = drive(&mut st, parsed("GET /.well-known/rargo.json HTTP/1.1\r\nHost: build.example:9\r\n\r\n"), NOW, &c, &mut script);
        assert!(notes.is_empty());
        let body = serde_json::from_str::<serde_json::Value>(body_of(&outcome).2).unwrap();
        assert_eq!(body["server"], "build.example:7459");
        assert_eq!(body["protocol"], crate::proto::PROTO_VERSION);
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(body["auth"], true);
        assert_eq!(body["github"], true);
        let (outcome, _) = drive(&mut AuthState::default(), parsed("GET /.well-known/rargo.json HTTP/1.1\r\nHost: build.example\r\n\r\n"), NOW, &cfg(false), &mut Script { github: script_rev(vec![None]), ..Script::default() });
        let body = serde_json::from_str::<serde_json::Value>(body_of(&outcome).2).unwrap();
        assert_eq!(body["auth"], false);
        assert_eq!(body["github"], false);

        let (outcome, _) = drive(&mut st, call("POST", "/auth/start", Some("application/json"), &format!(r#"{{"protocol":{}}}"#, crate::proto::PROTO_VERSION)), NOW, &cfg(false), &mut Script::default());
        assert_eq!(body_of(&outcome).0, 503);
        assert!(body_of(&outcome).2.contains("does not issue tokens"));
        let mut script = Script { random: script_rev(vec![Ok("deviceone".into()), Ok("ABCD2345".into())]), ..Script::default() };
        let (outcome, _) = drive(&mut st, call("POST", "/auth/start", Some("application/json"), &format!(r#"{{"protocol":{}}}"#, crate::proto::PROTO_VERSION)), NOW, &c, &mut script);
        let body = serde_json::from_str::<serde_json::Value>(body_of(&outcome).2).unwrap();
        assert_eq!(body["device"], "deviceone");
        assert_eq!(body["code"], "ABCD-2345");
        assert_eq!(body["path"], format!("/auth?code=ABCD-2345&protocol={}", crate::proto::PROTO_VERSION));
        assert_eq!(body["interval"], 2);
        assert_eq!(body["expires_in"], 600);
        assert_eq!(body["access"], "ask for it.");
        assert!(st.begin("other".into(), "ABCD2345".into(), NOW) == false);
        let (outcome, _) = drive(&mut st, call("POST", "/auth/poll", Some("application/json"), r#"{"device":"deviceone"}"#), NOW, &c, &mut Script::default());
        assert_eq!(serde_json::from_str::<serde_json::Value>(body_of(&outcome).2).unwrap()["status"], "pending");
        let (outcome, _) = drive(&mut st, call("POST", "/auth/poll", Some("application/x-www-form-urlencoded"), "device=someone-else"), NOW, &c, &mut Script::default());
        assert_eq!(serde_json::from_str::<serde_json::Value>(body_of(&outcome).2).unwrap()["status"], "expired");
        let (outcome, _) = drive(&mut st, call("POST", "/auth/poll", Some("application/json"), r#"{"device":"deviceone"}"#), NOW + PENDING_FOR_MS - 1, &c, &mut Script::default());
        assert_eq!(serde_json::from_str::<serde_json::Value>(body_of(&outcome).2).unwrap()["status"], "pending");

        let (outcome, notes) = drive(&mut st, call("POST", "/auth/revoke", Some("application/json"), r#"{"token":"t.s"}"#), NOW, &c, &mut Script { revoked_self: script_rev(vec![true]), ..Script::default() });
        assert_eq!(serde_json::from_str::<serde_json::Value>(body_of(&outcome).2).unwrap()["revoked"], true);
        assert_eq!(notes, vec!["rargod: a token was revoked by its holder (`rargo auth remove`)".to_string()]);
        let (outcome, notes) = drive(&mut st, call("POST", "/auth/revoke", None, ""), NOW, &c, &mut Script::default());
        assert_eq!(serde_json::from_str::<serde_json::Value>(body_of(&outcome).2).unwrap()["revoked"], false);
        assert!(notes.is_empty());
        let (outcome, _) = drive(&mut st, call("POST", "/auth/whoami", Some("application/json"), r#"{"token":"t.s"}"#), NOW, &c, &mut Script { holder: script_rev(vec![Some("ann".into())]), ..Script::default() });
        assert_eq!(serde_json::from_str::<serde_json::Value>(body_of(&outcome).2).unwrap()["user"], "ann");
        let (outcome, _) = drive(&mut st, call("POST", "/auth/whoami", Some("application/json"), r#"{"token":"no"}"#), NOW, &cfg(false), &mut Script::default());
        assert!(serde_json::from_str::<serde_json::Value>(body_of(&outcome).2).unwrap()["user"].is_null());

        let (outcome, _) = drive(&mut st, call("GET", &format!("/auth?code=abcd2345&protocol={}", crate::proto::PROTO_VERSION), None, ""), NOW, &c, &mut Script { github: script_rev(vec![Some(gh())]), ..Script::default() });
        let (status, _, page) = body_of(&outcome);
        assert_eq!(status, 200);
        assert!(page.contains("Sign in with GitHub") && page.contains("ABCD-2345"));
        let (outcome, _) = drive(&mut st, call("GET", &format!("/auth?code=abcd2345&protocol={}", crate::proto::PROTO_VERSION), None, ""), NOW, &c, &mut Script { github: script_rev(vec![None]), ..Script::default() });
        assert!(body_of(&outcome).2.contains("Invitation") && !body_of(&outcome).2.contains("Sign in with GitHub"));
        let (outcome, _) = drive(&mut st, parsed("GET /auth HTTP/1.1\r\nHost: build.example\r\n\r\n"), NOW, &cfg(false), &mut Script { github: script_rev(vec![None]), ..Script::default() });
        assert!(body_of(&outcome).2.contains("does not issue tokens"));
        let (outcome, _) = drive(&mut st, parsed("GET /nope HTTP/1.1\r\nHost: build.example\r\n\r\n"), NOW, &c, &mut Script::default());
        assert_eq!(body_of(&outcome), (404, "text/plain", "this is a rargo build server; see https://rargo.rs\n"));
    }

    #[test]
    fn the_page_says_how_to_get_access_takes_an_invitation_beside_github_and_names_a_code_no_terminal_waits_with() {
        let mut st = AuthState::default();
        let c = Cfg { access: "open a request at https://example.invalid/<new>".into(), ..cfg(true) };
        assert!(st.begin("deviceone".into(), "ABCD2345".into(), NOW));
        let page_for = |st: &mut AuthState, code: &str, github: Option<GithubIn>, now: u64| {
            let (outcome, _) = drive(st, parsed(&format!("GET /auth?code={code}&protocol={} HTTP/1.1\r\nHost: build.example\r\n\r\n", crate::proto::PROTO_VERSION)), now, &c, &mut Script { github: script_rev(vec![github]), ..Script::default() });
            let (status, _, page) = body_of(&outcome);
            (status, page.to_string())
        };
        for github in [Some(gh()), None] {
            let (status, page) = page_for(&mut st, "ABCD-2345", github.clone(), NOW);
            assert_eq!(status, 200);
            assert!(page.contains("Not let in yet? To build here, open a request at https://example.invalid/&lt;new&gt;."), "{page}");
            assert!(page.contains("action=\"/auth/approve\"") && page.contains("name=\"invite\""), "{page}");
            assert_eq!(page.contains("Sign in with GitHub"), github.is_some());
            assert!(!page.contains("class=\"err\""), "{page}");
        }
        for (code, now) in [("ZZZZ-9999", NOW), ("zzz", NOW), ("ABCD-2345", NOW + PENDING_FOR_MS)] {
            let (status, page) = page_for(&mut st, code, Some(gh()), now);
            assert_eq!(status, 404, "{code}");
            assert!(page.contains("no terminal is waiting with this code") && page.contains("Run `rargo auth` again"), "{page}");
        }
        let (status, page) = page_for(&mut st, "", None, NOW);
        assert_eq!(status, 200);
        assert!(!page.contains("class=\"err\""), "{page}");
        assert!(default_access(true).contains("rargod token allow") && default_access(true).contains("rargo auth invite"));
        assert!(default_access(false).contains("rargo auth invite") && !default_access(false).contains("GitHub"));
    }

    #[test]
    fn a_terminal_gets_its_token_only_after_the_code_is_confirmed() {
        let mut st = AuthState::default();
        let c = cfg(true);
        let mut script = Script { random: script_rev(vec![Ok("d1".into()), Ok("SAMECODE".into()), Ok("d2".into()), Ok("ABCD2345".into())]), ..Script::default() };
        assert!(st.begin("taken".into(), "SAMECODE".into(), NOW));
        let (outcome, _) = drive(&mut st, call("POST", "/auth/start", Some("application/json"), &format!(r#"{{"protocol":{}}}"#, crate::proto::PROTO_VERSION)), NOW, &c, &mut script);
        assert_eq!(serde_json::from_str::<serde_json::Value>(body_of(&outcome).2).unwrap()["code"], "ABCD-2345");
        assert!(script.effects.iter().filter(|e| e.starts_with("random")).count() >= 3, "a colliding code draws again");

        let (outcome, _) = drive(&mut st, call("POST", "/auth/approve", Some("application/x-www-form-urlencoded"), "code=abcd-2345&invite=NOPE&name="), NOW, &c, &mut Script { redeem: script_rev(vec![Err("this invitation is not known, has been used or has expired".into())]), github: script_rev(vec![None]), ..Script::default() });
        assert_eq!(body_of(&outcome).0, 403);
        assert!(body_of(&outcome).2.contains("not known"));
        let (outcome, _) = drive(&mut st, call("POST", "/auth/poll", Some("application/json"), r#"{"device":"d2"}"#), NOW, &c, &mut Script::default());
        assert_eq!(serde_json::from_str::<serde_json::Value>(body_of(&outcome).2).unwrap()["status"], "pending");
        let (outcome, _) = drive(&mut st, call("POST", "/auth/approve", Some("application/x-www-form-urlencoded"), "code=ZZZZZZZZ&invite=INVITE&name="), NOW, &c, &mut Script { github: script_rev(vec![None]), ..Script::default() });
        assert!(body_of(&outcome).2.contains("no terminal is waiting"));
        let mut script = Script { redeem: script_rev(vec![Ok(issued("t-ann", "ann"))]), ..Script::default() };
        let (outcome, notes) = drive(&mut st, call("POST", "/auth/approve", Some("application/x-www-form-urlencoded"), "code=ABCD-2345&invite=INVITE&name=Ann"), NOW, &c, &mut script);
        assert_eq!(body_of(&outcome).0, 200);
        assert!(body_of(&outcome).2.contains("Authenticated") && body_of(&outcome).2.contains("ABCD-2345") && body_of(&outcome).2.contains(">ann<"));
        assert_eq!(notes, vec!["rargod: a token for ann was issued through `rargo auth`".to_string()]);
        assert_eq!(script.effects[0], "redeem INVITE Ann");
        let (outcome, _) = drive(&mut st, call("POST", "/auth/approve", Some("application/x-www-form-urlencoded"), "code=ABCD-2345&invite=INVITE&name="), NOW, &c, &mut Script { github: script_rev(vec![None]), ..Script::default() });
        assert!(body_of(&outcome).2.contains("confirmed already"));
        let (outcome, _) = drive(&mut st, call("POST", "/auth/poll", Some("application/json"), r#"{"device":"d2"}"#), NOW, &c, &mut Script::default());
        let got = serde_json::from_str::<serde_json::Value>(body_of(&outcome).2).unwrap();
        assert_eq!(got["status"], "approved");
        assert_eq!(got["user"], "ann");
        assert_eq!(got["token"], "t-ann.secret");
        let (outcome, _) = drive(&mut st, call("POST", "/auth/poll", Some("application/json"), r#"{"device":"d2"}"#), NOW, &c, &mut Script::default());
        assert_eq!(serde_json::from_str::<serde_json::Value>(body_of(&outcome).2).unwrap()["status"], "expired");
    }

    #[test]
    fn wrong_invitations_are_counted_per_code_and_a_stopped_terminal_revokes() {
        let mut st = AuthState::default();
        let c = cfg(true);
        assert!(st.begin("device".into(), "ABCD2345".into(), NOW));
        for _ in 0..MAX_TRIES {
            let (outcome, _) = drive(&mut st, call("POST", "/auth/approve", Some("application/x-www-form-urlencoded"), "code=ABCD2345&invite=BAD-INVITE&name="), NOW, &c, &mut Script { redeem: script_rev(vec![Err("no".into())]), github: script_rev(vec![None]), ..Script::default() });
            assert_eq!(body_of(&outcome).0, 403);
            st.invariants();
        }
        let (outcome, _) = drive(&mut st, call("POST", "/auth/approve", Some("application/x-www-form-urlencoded"), "code=ABCD2345&invite=GOOD-INVITE&name="), NOW, &c, &mut Script { github: script_rev(vec![None]), ..Script::default() });
        assert!(body_of(&outcome).2.contains("too many wrong invitations"));
        let (outcome, _) = drive(&mut st, call("POST", "/auth/poll", Some("application/json"), r#"{"device":"device"}"#), NOW, &c, &mut Script::default());
        assert_eq!(serde_json::from_str::<serde_json::Value>(body_of(&outcome).2).unwrap()["status"], "denied");

        assert!(st.begin("later".into(), "BBBBBBBB".into(), NOW));
        let mut phase = Phase::default();
        let act = handle(&mut st, &mut phase, Event::Request(call("POST", "/auth/approve", Some("application/x-www-form-urlencoded"), "code=BBBBBBBB&invite=INV&name=")), &c, NOW);
        st.invariants();
        phase.invariants();
        assert!(matches!(act, Act::Ask(Effect::Redeem { .. })));
        assert_eq!(serde_json::from_str::<serde_json::Value>(body_of(&drive(&mut st, call("POST", "/auth/poll", Some("application/json"), r#"{"device":"later"}"#), NOW + PENDING_FOR_MS, &c, &mut Script::default()).0).2).unwrap()["status"], "expired");
        let act = handle(&mut st, &mut phase, Event::Redeemed(Ok(issued("t-x", "ann"))), &c, NOW);
        st.invariants();
        phase.invariants();
        let Act::Ask(Effect::Revoke { id }) = act else { panic!("the issued token is revoked") };
        assert_eq!(id, "t-x");
        let act = handle(&mut st, &mut phase, Event::Revoked, &c, NOW);
        st.invariants();
        phase.invariants();
        let Act::Ask(Effect::LoadGithub) = act else { panic!("the error page asks which form to show") };
        let act = handle(&mut st, &mut phase, Event::Github(None), &c, NOW);
        let Act::Done { outcome, notes } = act else { panic!("done") };
        assert!(notes.is_empty());
        assert!(body_of(&outcome).2.contains("stopped waiting"));
    }

    #[test]
    fn too_many_waiting_terminals_are_refused_and_expired_ones_do_not_count() {
        let mut st = AuthState::default();
        let c = cfg(true);
        for i in 0..MAX_PENDING {
            assert!(st.begin(format!("d{i}"), format!("C{i:07}"), NOW));
        }
        st.invariants();
        let (outcome, _) = drive(&mut st, call("POST", "/auth/start", Some("application/json"), &format!(r#"{{"protocol":{}}}"#, crate::proto::PROTO_VERSION)), NOW, &c, &mut Script::default());
        assert_eq!(body_of(&outcome).0, 503);
        assert!(body_of(&outcome).2.contains("too many"));
        let (outcome, _) = drive(&mut st, call("POST", "/auth/start", Some("application/json"), &format!(r#"{{"protocol":{}}}"#, crate::proto::PROTO_VERSION)), NOW + PENDING_FOR_MS, &c, &mut Script { random: script_rev(vec![Ok("fresh-device".into()), Ok("FFFF2222".into())]), ..Script::default() });
        assert_eq!(serde_json::from_str::<serde_json::Value>(body_of(&outcome).2).unwrap()["code"], "FFFF-2222");
        st.invariants();
    }

    #[test]
    fn github_sign_in_covers_every_outcome_with_recorded_responses() {
        let mut st = AuthState::default();
        let c = cfg(true);
        assert!(st.begin("device".into(), "ABCD2345".into(), NOW));
        let req = parsed("GET /auth/github?code=abcd2345 HTTP/1.1\r\nHost: build.example\r\n\r\n");
        let mut script = Script { github: script_rev(vec![Some(gh())]), random: script_rev(vec![Ok("oauthstateoauthstateoauthstate01".into())]), ..Script::default() };
        let (outcome, _) = drive(&mut st, req, NOW, &c, &mut script);
        let Outcome::Redirect(location) = outcome else { panic!("302") };
        assert_eq!(location, "https://github.com/login/oauth/authorize?client_id=Iv1.abc&redirect_uri=https%3A%2F%2Fbuild.example%2Fauth%2Fgithub%2Fcallback&state=oauthstateoauthstateoauthstate01&allow_signup=false");
        let (outcome, _) = drive(&mut AuthState::default(), parsed("GET /auth/github?code=NOPE HTTP/1.1\r\nHost: h\r\n\r\n"), NOW, &c, &mut Script { github: script_rev(vec![Some(gh())]), random: script_rev(vec![Ok("s".repeat(32))]), ..Script::default() });
        assert_eq!(body_of(&outcome).0, 403);
        assert!(body_of(&outcome).2.contains("no terminal is waiting"));
        let (outcome, _) = drive(&mut st, parsed("GET /auth/github?code=ABCD2345 HTTP/1.1\r\nHost: h\r\n\r\n"), NOW, &c, &mut Script { github: script_rev(vec![None]), ..Script::default() });
        assert!(body_of(&outcome).2.contains("does not sign people in with GitHub"));

        let callback = |query: &str| parsed(&format!("GET /auth/github/callback?{query} HTTP/1.1\r\nHost: h\r\n\r\n"));
        let (outcome, _) = drive(&mut st, callback("state=&code="), NOW, &c, &mut Script { github: script_rev(vec![Some(gh())]), ..Script::default() });
        assert!(body_of(&outcome).2.contains("cancelled"));
        let (outcome, _) = drive(&mut st, callback("state=nope&code=gh"), NOW, &c, &mut Script { github: script_rev(vec![Some(gh())]), ..Script::default() });
        assert!(body_of(&outcome).2.contains("no terminal that is still waiting"));
        let (outcome, _) = drive(&mut st, callback("state=oauthstateoauthstateoauthstate01&code=gh"), NOW, &cfg(false), &mut Script { github: script_rev(vec![Some(gh())]), ..Script::default() });
        assert!(body_of(&outcome).2.contains("does not issue tokens"));
        let refused = r#"{"error":"bad_verification_code","error_description":"The code passed is incorrect or expired."}"#;
        let mut script = Script { github: script_rev(vec![Some(gh())]), body: script_rev(vec![Ok(refused.into())]), ..Script::default() };
        let (outcome, _) = drive(&mut st, callback("state=oauthstateoauthstateoauthstate01&code=gh"), NOW, &c, &mut script);
        assert!(body_of(&outcome).2.contains("incorrect or expired"));
        assert!(script.effects.iter().any(|e| e.starts_with("post https://github.com/login/oauth/access_token ") && e.contains("code=gh") && e.contains("redirect_uri=https://build.example/auth/github/callback")));
        let (outcome, _) = drive(&mut st, callback("state=oauthstateoauthstateoauthstate01&code=gh"), NOW, &c, &mut Script { github: script_rev(vec![Some(gh())]), body: script_rev(vec![Err("asking GitHub: refused".into())]), ..Script::default() });
        assert!(body_of(&outcome).2.contains("asking GitHub: refused"));
        let mut outcomes = Transcript::default();
        for (account, response) in [
            ("missing login", r#"{"id":1}"#),
            ("invalid login", r#"{"login":"-nope"}"#),
        ] {
            let mut script = Script {
                github: script_rev(vec![Some(gh())]),
                body: script_rev(vec![Ok(r#"{"access_token":"tok"}"#.into()), Ok(response.into())]),
                ..Script::default()
            };
            let (outcome, _) = drive(&mut st, callback("state=oauthstateoauthstateoauthstate01&code=gh"), NOW, &c, &mut script);
            let (status, _, page) = body_of(&outcome);
            let error = page.split_once("class=\"err\">").unwrap().1.split_once('<').unwrap().0;
            outcomes.record(account, (status, error));
        }
        expect![[r#"
            missing login: (403, "GitHub did not say who signed in")
            invalid login: (403, "GitHub named an account this server cannot use")
        "#]].assert_eq(&outcomes.0);
        let (outcome, _) = drive(&mut st, callback("state=oauthstateoauthstateoauthstate01&code=gh"), NOW, &c, &mut Script { github: script_rev(vec![Some(gh())]), body: script_rev(vec![Ok(r#"{"access_token":"tok"}"#.into()), Ok(r#"{"login":"octocat"}"#.into())]), allowed: script_rev(vec![None]), ..Script::default() });
        assert!(body_of(&outcome).2.contains("not on this server's allow list") && body_of(&outcome).2.contains("rargod token allow octocat"));

        let mut script = Script {
            github: script_rev(vec![Some(gh())]),
            body: script_rev(vec![Ok(r#"{"access_token":"tok"}"#.into()), Ok(r#"{"login":"OctoCat"}"#.into())]),
            allowed: script_rev(vec![Some(AllowedIn { login: "octocat".into(), admin: true })]),
            created: script_rev(vec![Ok(issued("t-gh", "octocat"))]),
            ..Script::default()
        };
        let (outcome, notes) = drive(&mut st, callback("state=oauthstateoauthstateoauthstate01&code=gh"), NOW, &c, &mut script);
        assert_eq!(body_of(&outcome).0, 200);
        assert!(body_of(&outcome).2.contains("Authenticated") && body_of(&outcome).2.contains("GitHub account") && body_of(&outcome).2.contains(">octocat<") && body_of(&outcome).2.contains("ABCD-2345"));
        assert_eq!(notes, vec!["rargod: a token for the GitHub account octocat was issued through `rargo auth`".to_string()]);
        assert!(script.effects.iter().any(|e| e == "get https://api.github.com/user tok"));
        assert!(script.effects.iter().any(|e| e == "create octocat true"));
        let (outcome, _) = drive(&mut st, call("POST", "/auth/poll", Some("application/json"), r#"{"device":"device"}"#), NOW, &c, &mut Script::default());
        let got = serde_json::from_str::<serde_json::Value>(body_of(&outcome).2).unwrap();
        assert_eq!(got["token"], "t-gh.secret");
        assert_eq!(got["user"], "octocat");
    }

    #[test]
    fn a_terminal_that_stops_while_the_browser_is_at_github_loses_the_token() {
        let mut st = AuthState::default();
        let c = cfg(true);
        assert!(st.begin("device".into(), "ABCD2345".into(), NOW));
        let mut phase = Phase::default();
        st.pending.get_mut("device").unwrap().state = Some("st".into());
        let mut event = Event::Request(parsed("GET /auth/github/callback?state=st&code=gh HTTP/1.1\r\nHost: h\r\n\r\n"));
        let mut saw_revoke = false;
        for _ in 0..8 {
            let act = handle(&mut st, &mut phase, event, &c, NOW);
            st.invariants();
            phase.invariants();
            match act {
                Act::Done { outcome, .. } => {
                    assert!(saw_revoke);
                    assert!(body_of(&outcome).2.contains("stopped waiting"));
                    return;
                }
                Act::Ask(Effect::LoadGithub) => event = Event::Github(Some(gh())),
                Act::Ask(Effect::Post { .. }) => {
                    st.pending.remove("device");
                    event = Event::Body(Ok(r#"{"access_token":"tok"}"#.into()));
                }
                Act::Ask(Effect::Get { .. }) => event = Event::Body(Ok(r#"{"login":"octocat"}"#.into())),
                Act::Ask(Effect::Allowed { .. }) => event = Event::Allowed(Some(AllowedIn { login: "octocat".into(), admin: false })),
                Act::Ask(Effect::Create { .. }) => event = Event::Created(Ok(issued("t-gh", "octocat"))),
                Act::Ask(Effect::Revoke { id }) => {
                    assert_eq!(id, "t-gh");
                    saw_revoke = true;
                    event = Event::Revoked;
                }
                Act::Ask(other) => panic!("unexpected {other:?}"),
            }
        }
        panic!("did not finish");
    }
}
