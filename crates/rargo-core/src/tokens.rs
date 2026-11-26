#[cfg(test)]
use rargo_machine::StateMachine;
use anyhow::{Context, Result, bail};
use rargo_proto::secure;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Token {
    pub id: String,
    pub user: String,
    #[serde(default)]
    pub admin: bool,
    pub key: String,
    pub created: u64,
    #[serde(default)]
    pub expires: Option<u64>,
    #[serde(default)]
    pub revoked: bool,
    #[serde(default)]
    pub note: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Invite {
    pub code: String,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub admin: bool,
    #[serde(default)]
    pub expires: Option<u64>,
    pub uses_left: u32,
    #[serde(default)]
    pub token_days: Option<u32>,
    pub created: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Allowed {
    pub login: String,
    #[serde(default)]
    pub admin: bool,
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Tokens {
    #[serde(default)]
    tokens: Vec<Token>,
    #[serde(default)]
    invites: Vec<Invite>,
    #[serde(default)]
    github: Vec<Allowed>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Github {
    pub client_id: String,
    pub client_secret: String,
    pub url: String,
    /// How someone the server has not let in yet gets access, completing "To build here, …"; empty for the default.
    #[serde(default)]
    pub access: String,
}

impl Github {
    pub fn path(state: &Path) -> PathBuf {
        state.join("github.json")
    }

    pub fn load(state: &Path) -> Option<Github> {
        let g: Github = serde_json::from_slice(&crate::io::file::read(Self::path(state)).ok()?).ok()?;
        (!g.client_id.is_empty() && !g.client_secret.is_empty()).then_some(g)
    }

    pub fn save(&self, state: &Path) -> Result<()> {
        use std::io::Write;
        let path = Self::path(state);
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        let mut w = crate::io::file::OpenOptions::new().create(true).truncate(true).write(true).mode(0o600).open(&tmp)?;
        w.write_all(&serde_json::to_vec_pretty(self)?)?;
        crate::io::file::rename(&tmp, &path)?;
        Ok(())
    }
}

pub fn valid_login(login: &str) -> bool {
    !login.is_empty() && login.len() <= 39 && login.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') && !login.starts_with('-')
}

pub struct TokenStore {
    path: PathBuf,
    cached: parking_lot::Mutex<(Option<(SystemTime, u64)>, Arc<Tokens>)>,
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn random(n: usize) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut b = vec![0u8; n];
    crate::io::file::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).context("reading /dev/urandom")?;
    Ok(b)
}

/// A seed from the OS, for the generators the shell hands the server.
pub fn os_seed() -> Result<u64> {
    let b = random(8)?;
    Ok(u64::from_le_bytes(b.try_into().expect("eight bytes")))
}

const CLEAR: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const _: () = assert!(CLEAR.len() == 32);

pub fn random_text(n: usize, alphabet: &[u8]) -> Result<String> {
    assert!(!alphabet.is_empty());
    assert!(alphabet.len() <= 256);
    assert!(alphabet.is_ascii(), "one byte per character");
    let limit = 256 - 256 % alphabet.len();
    let mut out = String::new();
    while out.len() < n {
        for b in random(n)? {
            if (b as usize) < limit && out.len() < n {
                out.push(alphabet[b as usize % alphabet.len()] as char);
            }
        }
    }
    assert_eq!(out.len(), n);
    Ok(out)
}

fn hex(b: &[u8]) -> String {
    let s: String = b.iter().map(|x| format!("{x:02x}")).collect();
    assert_eq!(s.len(), 2 * b.len());
    s
}

fn unhex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

pub fn normal_code(code: &str) -> String {
    code.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect()
}

fn code_hash(code: &str) -> String {
    hex(blake3::Hasher::new_derive_key("rargo invitation v1").update(normal_code(code).as_bytes()).finalize().as_bytes())
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

impl TokenStore {
    pub fn new(path: PathBuf) -> TokenStore {
        TokenStore { path, cached: parking_lot::Mutex::new((None, Arc::new(Tokens::default()))) }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn file(&self) -> Arc<Tokens> {
        let stamp = crate::io::file::metadata(&self.path).ok().and_then(|m| Some((m.modified().ok()?, m.len())));
        let mut c = self.cached.lock();
        if c.0 != stamp || stamp.is_none() {
            let (file, warning) = Tokens::read(crate::io::file::read(&self.path).ok().as_deref());
            if let Some(w) = warning {
                eprintln!("rargod: {}: {w}", self.path.display());
            }
            *c = (stamp, Arc::new(file));
        }
        c.1.clone()
    }

    fn change<T>(&self, f: impl FnOnce(&mut Tokens) -> Result<T>) -> Result<T> {
        use std::os::fd::AsRawFd;
        if let Some(dir) = self.path.parent() {
            crate::io::file::create_dir_all(dir)?;
        }
        let lock = crate::io::file::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(self.path.with_extension("lock"))
            .context("opening the token lock file")?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            bail!("locking the token file: {}", std::io::Error::last_os_error());
        }
        let mut file: Tokens = match crate::io::file::read(&self.path) {
            Ok(bytes) => Tokens::decode(&bytes).with_context(|| format!("{}", self.path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Tokens::default(),
            Err(e) => return Err(e).with_context(|| format!("{}", self.path.display())),
        };
        let out = f(&mut file)?;
        let tmp = self.path.with_extension(format!("tmp{}", std::process::id()));
        {
            use std::io::Write;
            let mut w = crate::io::file::OpenOptions::new().create(true).truncate(true).write(true).mode(0o600).open(&tmp)?;
            w.write_all(&file.encode()?)?;
            w.sync_all()?;
        }
        crate::io::file::rename(&tmp, &self.path)?;
        *self.cached.lock() = (None, Arc::new(Tokens::default()));
        Ok(out)
    }

    pub fn lookup(&self, id: &str) -> Option<([u8; 32], bool, String)> {
        self.file().lookup(id, now())
    }

    pub fn create(&self, user: &str, admin: bool, days: Option<u32>, note: &str) -> Result<(Token, String)> {
        let (id, secret) = token_entropy()?;
        let token = self.change(|tokens| tokens.create(user, admin, days, note, now(), (&id, &secret)))?;
        Ok((token, secret))
    }

    pub fn revoke(&self, id: &str) -> Result<usize> {
        self.change(|tokens| Ok(tokens.revoke(id, None)))
    }

    pub fn allowed(&self, login: &str) -> Option<Allowed> {
        self.file().allowed(login)
    }

    pub fn revoke_self(&self, text: &str) -> Result<bool> {
        match SelfRevocation::of(text) {
            Some(r) => self.change(|tokens| Ok(tokens.revoke_self(&r))),
            None => Ok(false),
        }
    }

    pub fn holder(&self, text: &str) -> Option<String> {
        self.file().holder(text, now())
    }

    pub fn invite(&self, user: Option<&str>, admin: bool, days: Option<u32>, uses: u32, token_days: Option<u32>) -> Result<String> {
        let raw = random_text(16, CLEAR)?;
        self.change(|tokens| tokens.invite(Invitation { user, admin, days, uses, token_days }, now(), &raw))
    }

    pub fn invited(&self, code: &str) -> Option<Invite> {
        self.file().invited(code, now()).cloned()
    }

    pub fn redeem(&self, code: &str, name: Option<&str>) -> Result<(Token, String)> {
        let (id, secret) = token_entropy()?;
        let token = self.change(|tokens| tokens.redeem(code, name, now(), (&id, &secret)))??;
        Ok((token, secret))
    }

    fn command(&self, cmd: &Command, entropy: Option<&Entropy>) -> Result<Said> {
        let github = cmd.asks_github() && self.path.parent().and_then(Github::load).is_some();
        if cmd.writes() {
            self.change(|tokens| cmd.apply(tokens, now(), entropy, github))
        } else {
            cmd.apply(&mut (*self.file()).clone(), now(), entropy, github)
        }
    }
}

fn validate_name(user: &str) -> Result<()> {
    if !valid_name(user) {
        bail!("a user name is letters, digits, `-`, `_` and `.`, at most 64 of them");
    }
    Ok(())
}

fn token_entropy() -> Result<(String, String)> {
    let secret = random_text(40, b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789")?;
    let id = format!("t-{}", random_text(12, b"abcdefghijklmnopqrstuvwxyz0123456789")?);
    Ok((id, secret))
}

pub struct Entropy {
    id: String,
    secret: String,
    invite: String,
}

impl Entropy {
    fn draw() -> Result<Entropy> {
        let (id, secret) = token_entropy()?;
        Ok(Entropy { id, secret, invite: random_text(16, CLEAR)? })
    }
}

pub struct SelfRevocation {
    id: String,
    key: String,
}

impl SelfRevocation {
    fn of(text: &str) -> Option<SelfRevocation> {
        let (id, secret) = text.trim().split_once('.')?;
        Some(SelfRevocation { id: id.into(), key: hex(&secure::psk_from_secret(secret.as_bytes())) })
    }
}

struct Invitation<'a> {
    user: Option<&'a str>,
    admin: bool,
    days: Option<u32>,
    uses: u32,
    token_days: Option<u32>,
}

impl Token {
    fn active(&self, at: u64) -> bool {
        !self.revoked && self.expires.is_none_or(|end| end > at)
    }
}

impl Invite {
    fn active(&self, at: u64) -> bool {
        self.uses_left > 0 && self.expires.is_none_or(|end| end > at)
    }
}

impl Tokens {
    fn read(bytes: Option<&[u8]>) -> (Tokens, Option<String>) {
        match bytes.map(Tokens::decode) {
            Some(Ok(tokens)) => (tokens, None),
            Some(Err(e)) => (Tokens::default(), Some(e.to_string())),
            None => (Tokens::default(), None),
        }
    }

    fn decode(bytes: &[u8]) -> serde_json::Result<Self> {
        serde_json::from_slice(bytes)
    }

    fn encode(&self) -> serde_json::Result<Vec<u8>> {
        serde_json::to_vec_pretty(self)
    }

    fn lookup(&self, id: &str, at: u64) -> Option<([u8; 32], bool, String)> {
        let t = self.tokens.iter().find(|t| t.id == id)?;
        if !t.active(at) { return None; }
        Some((unhex32(&t.key)?, t.admin, t.user.clone()))
    }

    fn create(&mut self, user: &str, admin: bool, days: Option<u32>, note: &str, at: u64, (id, secret): (&str, &str)) -> Result<Token> {
        validate_name(user)?;
        if self.tokens.iter().any(|t| t.id == id) {
            bail!("generated a duplicate token id; try again");
        }
        let token = Token {
            id: id.into(),
            user: user.into(),
            admin,
            key: hex(&secure::psk_from_secret(secret.as_bytes())),
            created: at,
            expires: days.map(|d| at.saturating_add(u64::from(d) * 86400)),
            revoked: false,
            note: note.into(),
        };
        self.tokens.push(token.clone());
        Ok(token)
    }

    fn revoke(&mut self, id: &str, owner: Option<&str>) -> usize {
        let mut count = 0;
        for t in self.tokens.iter_mut().filter(|t| !t.revoked && owner.is_none_or(|user| t.user == user) && (t.id == id || t.user == id)) {
            t.revoked = true;
            count += 1;
        }
        count
    }

    fn allow(&mut self, login: &str, admin: bool) -> Result<()> {
        if !valid_login(login) {
            bail!("`{login}` is not a GitHub login");
        }
        self.github.retain(|a| !a.login.eq_ignore_ascii_case(login));
        self.github.push(Allowed { login: login.into(), admin });
        Ok(())
    }

    fn disallow(&mut self, login: &str) -> (bool, usize) {
        let before = self.github.len();
        self.github.retain(|a| !a.login.eq_ignore_ascii_case(login));
        let mut count = 0;
        for t in self.tokens.iter_mut().filter(|t| !t.revoked && t.note == "github" && t.user.eq_ignore_ascii_case(login)) {
            t.revoked = true;
            count += 1;
        }
        (self.github.len() != before, count)
    }

    fn allowed(&self, login: &str) -> Option<Allowed> {
        self.github.iter().find(|a| a.login.eq_ignore_ascii_case(login)).cloned()
    }

    fn revoke_self(&mut self, r: &SelfRevocation) -> bool {
        let Some(t) = self.tokens.iter_mut().find(|t| t.id == r.id && t.key == r.key && !t.revoked) else { return false };
        t.revoked = true;
        true
    }

    fn holder(&self, text: &str, at: u64) -> Option<String> {
        let (id, secret) = text.trim().split_once('.')?;
        let key = hex(&secure::psk_from_secret(secret.as_bytes()));
        let token = self.tokens.iter().find(|t| t.id == id && t.key == key)?;
        token.active(at).then(|| token.user.clone())
    }

    fn invite(&mut self, spec: Invitation<'_>, at: u64, raw: &str) -> Result<String> {
        if let Some(user) = spec.user {
            validate_name(user)?;
        }
        if raw.len() != 16 || !raw.bytes().all(|b| CLEAR.contains(&b)) {
            bail!("invalid invitation entropy");
        }
        let code = format!("{}-{}-{}-{}", &raw[0..4], &raw[4..8], &raw[8..12], &raw[12..16]);
        let invite = Invite {
            code: code_hash(&code),
            user: spec.user.map(String::from),
            admin: spec.admin,
            expires: spec.days.map(|d| at.saturating_add(u64::from(d) * 86400)),
            uses_left: spec.uses.max(1),
            token_days: spec.token_days,
            created: at,
        };
        if self.invites.iter().any(|i| i.active(at) && i.code == invite.code) {
            bail!("generated a duplicate invitation code; try again");
        }
        self.invites.retain(|i| i.active(at));
        self.invites.push(invite);
        Ok(code)
    }

    fn invited(&self, code: &str, at: u64) -> Option<&Invite> {
        let h = code_hash(code);
        self.invites.iter().find(|i| i.code == h && i.active(at))
    }

    fn redeem(&mut self, code: &str, name: Option<&str>, at: u64, entropy: (&str, &str)) -> Result<Result<Token>> {
        let h = code_hash(code);
        let Some(invite) = self.invites.iter_mut().find(|i| i.code == h && i.active(at)) else {
            bail!("this invitation is not known, has been used or has expired");
        };
        let user = match (&invite.user, name) {
            (Some(user), _) => user.clone(),
            (None, Some(name)) if valid_name(name) => name.to_owned(),
            (None, _) => bail!("a name is needed: letters, digits, `-`, `_` and `.`"),
        };
        invite.uses_left -= 1;
        let (admin, days) = (invite.admin, invite.token_days);
        Ok(self.create(&user, admin, days, "invitation", at, entropy))
    }
}

#[cfg(test)]
impl StateMachine for Tokens {
    fn invariants(&self) {
        let unique = |what: &str, ids: Vec<&str>| {
            let n = ids.len();
            assert_eq!(ids.into_iter().collect::<std::collections::BTreeSet<_>>().len(), n, "every {what} is unique");
        };
        for t in &self.tokens {
            assert!(unhex32(&t.key).is_some(), "token {} has a 32-byte hex key", t.id);
        }
        unique("token id", self.tokens.iter().map(|t| t.id.as_str()).collect());
        unique("invite code", self.invites.iter().map(|i| i.code.as_str()).collect());
        for a in &self.github {
            assert!(valid_login(&a.login), "{:?} is a valid GitHub login", a.login);
        }
    }
}

pub fn token_text(t: &Token, secret: &str) -> String {
    format!("{}.{secret}", t.id)
}

pub struct Asker<'a> {
    pub user: &'a str,
    pub admin: bool,
    pub from: &'a str,
}

pub type Said = (String, String);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    List { all: bool, user: String },
    Revoke { id: String, owner: Option<String> },
    Invite { user: Option<String>, admin: bool, days: u32, uses: u32, token_days: Option<u32> },
    Create { user: String, admin: bool, days: Option<u32>, note: String },
    Allow { login: String, admin: bool },
    Disallow { login: String },
}

fn when(t: Option<u64>, at: u64) -> String {
    match t {
        None => "never".to_string(),
        Some(t) if t <= at => "expired".to_string(),
        Some(t) => format!("in {} days", (t - at).div_ceil(86400)),
    }
}

impl Command {
    pub fn parse(args: &[String], asker: &Asker, usage: &str) -> Result<Command> {
        let Some(what) = args.first().map(String::as_str) else { bail!("{usage}") };
        let (mut user, mut admin, mut days, mut uses, mut token_days) = (None::<String>, false, None::<u32>, 1u32, None::<u32>);
        let mut rest: Vec<&str> = Vec::new();
        let mut i = 1;
        while i < args.len() {
            let arg = args[i].as_str();
            let mut val = || -> Result<&String> {
                i += 1;
                args.get(i).with_context(|| format!("`{arg}` needs a value"))
            };
            match arg {
                "--user" => user = Some(val()?.clone()),
                "--admin" => admin = true,
                "--days" => days = Some(val()?.parse().context("--days takes a number")?),
                "--uses" => uses = val()?.parse().context("--uses takes a number")?,
                "--token-days" => token_days = Some(val()?.parse().context("--token-days takes a number")?),
                other if other.starts_with("--") => bail!("unknown option `{other}`\n{usage}"),
                other => rest.push(other),
            }
            i += 1;
        }
        let first = || rest.first().map(|s| s.to_string()).with_context(|| usage.to_string());
        Ok(match what {
            "list" => Command::List { all: asker.admin, user: asker.user.into() },
            "revoke" => Command::Revoke { id: first()?, owner: (!asker.admin).then(|| asker.user.into()) },
            _ if !asker.admin => bail!(
                "user `{}` may not administer this server (an administrator's token is made on the server with `rargod token create --admin`, `token invite --admin` or `token allow --admin`)",
                asker.user
            ),
            "invite" => Command::Invite { user, admin, days: days.unwrap_or(7), uses, token_days },
            "create" => Command::Create { user: user.context("--user NAME is required")?, admin, days, note: asker.from.into() },
            "allow" => Command::Allow { login: first()?, admin },
            "disallow" => Command::Disallow { login: first()? },
            _ => bail!("{usage}"),
        })
    }

    pub fn writes(&self) -> bool {
        !matches!(self, Command::List { .. })
    }

    pub fn draws(&self) -> bool {
        matches!(self, Command::Invite { .. } | Command::Create { .. })
    }

    pub fn asks_github(&self) -> bool {
        matches!(self, Command::Allow { .. })
    }

    pub fn apply(&self, tokens: &mut Tokens, at: u64, entropy: Option<&Entropy>, github: bool) -> Result<Said> {
        let (mut out, mut err) = (String::new(), String::new());
        let drawn = || entropy.context("the command needs entropy");
        use std::fmt::Write as _;
        match self {
            Command::List { all, user } => {
                let _ = writeln!(out, "{:<16} {:<20} {:<6} {:<12} FROM", "ID", "USER", "ADMIN", "EXPIRES");
                for t in tokens.tokens.iter().filter(|t| *all || t.user == *user) {
                    let state = if t.revoked { "revoked".to_string() } else { when(t.expires, at) };
                    let _ = writeln!(out, "{:<16} {:<20} {:<6} {:<12} {}", t.id, t.user, if t.admin { "yes" } else { "" }, state, t.note);
                }
                if *all {
                    if !tokens.github.is_empty() {
                        let names: Vec<String> = tokens.github.iter().map(|a| format!("{}{}", a.login, if a.admin { " (administrator)" } else { "" })).collect();
                        let _ = writeln!(out, "\nGitHub accounts that may sign in: {}", names.join(", "));
                    }
                    let open: Vec<_> = tokens.invites.iter().filter(|i| i.active(at)).collect();
                    if !open.is_empty() {
                        let _ = writeln!(out, "\ninvitations not used up: {}", open.len());
                        for i in open {
                            let _ = writeln!(
                                out,
                                "  for {}{}: {} use(s) left, expires {}",
                                i.user.as_deref().unwrap_or("anyone"),
                                if i.admin { " (administrator)" } else { "" },
                                i.uses_left,
                                when(i.expires, at)
                            );
                        }
                    }
                }
            }
            Command::Revoke { id, owner } => {
                let n = tokens.revoke(id, owner.as_deref());
                if n == 0 {
                    if owner.is_none() {
                        bail!("no token that still works has the id or user `{id}`");
                    }
                    bail!("none of your tokens that still work has the id `{id}` (you may revoke only your own)");
                }
                let _ = writeln!(out, "revoked {n} token(s); the next command on an open session is refused");
            }
            Command::Invite { user, admin, days, uses, token_days } => {
                let spec = Invitation { user: user.as_deref(), admin: *admin, days: Some(*days), uses: *uses, token_days: *token_days };
                let code = tokens.invite(spec, at, &drawn()?.invite)?;
                let _ = writeln!(out, "{code}");
                let _ = writeln!(
                    err,
                    "An invitation for {}{}, good for {} use(s), {}. Whoever has it runs `rargo auth` and enters it on the page that opens.",
                    user.as_deref().unwrap_or("whoever redeems it"),
                    if *admin { " (administrator)" } else { "" },
                    uses.max(&1),
                    when(Some(at + *days as u64 * 86400), at).replace("in ", "for "),
                );
            }
            Command::Create { user, admin, days, note } => {
                let e = drawn()?;
                let t = tokens.create(user, *admin, *days, note, at, (&e.id, &e.secret))?;
                let _ = writeln!(out, "{}", token_text(&t, &e.secret));
                let _ = writeln!(err, "A token for {user}; it is shown this once. A client uses it as RARGO_TOKEN or through `rargo auth --token`.");
            }
            Command::Allow { login, admin } => {
                tokens.allow(login, *admin)?;
                let _ = writeln!(out, "{login} may sign in with GitHub{}", if *admin { " (administrator)" } else { "" });
                if !github {
                    let _ = writeln!(err, "note: this server has no GitHub application yet (rargod token github ...); until then `rargo auth` asks for an invitation");
                }
            }
            Command::Disallow { login } => {
                let (was, n) = tokens.disallow(login);
                if !was && n == 0 {
                    bail!("`{login}` is not on the allow list");
                }
                let _ = writeln!(out, "{login} may no longer sign in; revoked {n} token(s) it had signed in for");
            }
        }
        Ok((out, err))
    }
}

pub fn command(store: &TokenStore, args: &[String], asker: &Asker, usage: &str) -> Result<Said> {
    let cmd = Command::parse(args, asker, usage)?;
    let entropy = if cmd.draws() { Some(Entropy::draw()?) } else { None };
    store.command(&cmd, entropy.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_tokens_are_verified_expired_and_revoked_without_the_secret() {
        let mut tokens = Tokens::decode(b"{}").unwrap();
        tokens.invariants();
        let secret = "fixture-credential";
        let token = tokens.create("ann", true, Some(1), "test", 100, ("t-ann", secret)).unwrap();
        tokens.invariants();
        let encoded = tokens.encode().unwrap();
        assert!(!String::from_utf8_lossy(&encoded).contains(secret));
        let mut tokens = Tokens::decode(&encoded).unwrap();
        tokens.invariants();
        assert_eq!(tokens.lookup("t-ann", 100), Some((secure::psk_from_secret(secret.as_bytes()), true, "ann".into())));
        assert!(tokens.lookup("t-ann", 86_500).is_none());
        assert_eq!(tokens.holder(&token_text(&token, secret), 101).as_deref(), Some("ann"));
        assert!(SelfRevocation::of("no-dot").is_none());
        assert!(!tokens.revoke_self(&SelfRevocation::of("t-ann.incorrect").unwrap()));
        tokens.invariants();
        assert_eq!(tokens.revoke("t-ann", Some("bob")), 0);
        tokens.invariants();
        assert!(tokens.revoke_self(&SelfRevocation::of(&format!(" {} ", token_text(&token, secret))).unwrap()));
        tokens.invariants();
        assert_eq!(tokens.revoke("ann", None), 0);
        tokens.invariants();
        assert!(tokens.lookup("t-ann", 100).is_none());
        assert!(Tokens::decode(b"{broken").is_err());
        assert!(Tokens::decode(br#"{"tokens":["broken"]}"#).is_err());
        let (read, warning) = Tokens::read(Some(&encoded));
        assert!(warning.is_none() && read.tokens.len() == 1);
        let (read, warning) = Tokens::read(Some(b"{broken"));
        assert!(warning.unwrap().contains("key must be a string") && read.tokens.is_empty(), "a malformed file reads as empty, with a warning");
        read.invariants();
        let (read, warning) = Tokens::read(None);
        assert!(warning.is_none() && read.tokens.is_empty());
    }

    fn entropy(n: u32) -> Entropy {
        Entropy { id: format!("t-{n:03}"), secret: format!("fixture-secret-{n}"), invite: format!("ABCD{n:04}EFGH{n:04}").replace('0', "Q").replace('1', "R") }
    }

    fn run(tokens: &mut Tokens, line: &str, asker: &Asker, at: u64, n: u32, github: bool) -> Result<Said> {
        let args: Vec<String> = line.split_whitespace().map(String::from).collect();
        let cmd = Command::parse(&args, asker, "usage")?;
        let before = tokens.encode().unwrap();
        let said = cmd.apply(tokens, at, cmd.draws().then(|| entropy(n)).as_ref(), github);
        rargo_machine::check(&tokens, format_args!("{line}"));
        if !cmd.writes() {
            assert_eq!(tokens.encode().unwrap(), before, "{line} changes nothing");
        }
        said
    }

    #[test]
    fn token_commands_do_everything_for_an_administrator_and_a_users_own_tokens_for_anyone() {
        let mut t = Tokens::default();
        let boss = Asker { user: "boss", admin: true, from: "test" };
        let ann = Asker { user: "ann", admin: false, from: "test" };
        let at = 1_000_000;
        let (out, err) = run(&mut t, "create --user ann --days 3", &boss, at, 1, false).unwrap();
        assert_eq!(out, "t-001.fixture-secret-1\n");
        assert!(err.starts_with("A token for ann; it is shown this once."), "{err}");
        assert_eq!(t.holder(out.trim(), at).as_deref(), Some("ann"));
        assert_eq!(t.tokens[0].note, "test");
        run(&mut t, "create --user bob --admin", &boss, at, 2, false).unwrap();
        assert!(t.lookup("t-002", at).unwrap().1, "--admin makes an administrator");
        assert!(run(&mut t, "create --user ann", &boss, at, 1, false).unwrap_err().to_string().contains("duplicate token id"));
        assert!(run(&mut t, "create --user bad/name", &boss, at, 3, false).unwrap_err().to_string().contains("a user name is"));
        assert!(run(&mut t, "create", &boss, at, 3, false).unwrap_err().to_string().contains("--user NAME is required"));

        let (code, err) = run(&mut t, "invite --user cat --admin --uses 2 --days 2", &boss, at, 4, false).unwrap();
        assert_eq!(code, "ABCD-QQQ4-EFGH-QQQ4\n");
        assert_eq!(err, "An invitation for cat (administrator), good for 2 use(s), for 2 days. Whoever has it runs `rargo auth` and enters it on the page that opens.\n");
        let (_, err) = run(&mut t, "invite --uses 0", &boss, at, 5, false).unwrap();
        assert!(err.starts_with("An invitation for whoever redeems it, good for 1 use(s), for 7 days."), "{err}");
        assert_eq!(t.invited("ABCD-QQQ5-EFGH-QQQ5", at + 7 * 86400 - 1).unwrap().uses_left, 1);
        assert!(t.invited("ABCD-QQQ5-EFGH-QQQ5", at + 7 * 86400).is_none(), "an invitation lasts seven days by default");
        assert!(run(&mut t, "invite --user no/way", &boss, at, 6, false).is_err());

        let (out, err) = run(&mut t, "allow some-login", &boss, at, 0, false).unwrap();
        assert_eq!(out, "some-login may sign in with GitHub\n");
        assert!(err.starts_with("note: this server has no GitHub application yet"), "{err}");
        let (out, err) = run(&mut t, "allow Lead --admin", &boss, at, 0, true).unwrap();
        assert_eq!((out.as_str(), err.as_str()), ("Lead may sign in with GitHub (administrator)\n", ""));
        assert!(run(&mut t, "allow not/a/login", &boss, at, 0, true).is_err());

        let (all, _) = run(&mut t, "list", &boss, at + 86400, 0, false).unwrap();
        assert_eq!(
            all,
            "ID               USER                 ADMIN  EXPIRES      FROM\n\
             t-001            ann                         in 2 days    test\n\
             t-002            bob                  yes    never        test\n\
             \nGitHub accounts that may sign in: some-login, Lead (administrator)\n\
             \ninvitations not used up: 2\n\
             \x20 for cat (administrator): 2 use(s) left, expires in 1 days\n\
             \x20 for anyone: 1 use(s) left, expires in 6 days\n"
        );
        let (later, _) = run(&mut t, "list", &boss, at + 3 * 86400, 0, false).unwrap();
        assert!(later.contains("ann                         expired") && later.contains("invitations not used up: 1"), "{later}");
        let (own, _) = run(&mut t, "list", &ann, at, 0, false).unwrap();
        assert_eq!(own, "ID               USER                 ADMIN  EXPIRES      FROM\nt-001            ann                         in 3 days    test\n");

        for refused in ["invite", "create --user ann2", "allow x", "disallow some-login", "frobnicate"] {
            let e = run(&mut t, refused, &ann, at, 9, false).unwrap_err().to_string();
            assert!(e.contains("user `ann` may not administer") && e.contains("rargod token create --admin"), "{refused}: {e}");
        }
        let e = run(&mut t, "revoke bob", &ann, at, 0, false).unwrap_err().to_string();
        assert!(e.contains("you may revoke only your own"), "{e}");
        assert!(t.lookup("t-002", at).is_some(), "not someone else's");
        let (n, _) = run(&mut t, "revoke t-001", &ann, at, 0, false).unwrap();
        assert_eq!(n, "revoked 1 token(s); the next command on an open session is refused\n");
        assert!(t.lookup("t-001", at).is_none(), "her own");
        let (n, _) = run(&mut t, "revoke bob", &boss, at, 0, false).unwrap();
        assert!(n.starts_with("revoked 1 token(s)"), "{n}");
        let e = run(&mut t, "revoke bob", &boss, at, 0, false).unwrap_err().to_string();
        assert_eq!(e, "no token that still works has the id or user `bob`");
        let (revoked, _) = run(&mut t, "list", &boss, at, 0, false).unwrap();
        assert!(revoked.contains("bob                  yes    revoked"), "{revoked}");

        t.create("some-login", false, None, "github", at, ("t-gh", "fixture-gh")).unwrap();
        let (out, _) = run(&mut t, "disallow SOME-LOGIN", &boss, at, 0, false).unwrap();
        assert_eq!(out, "SOME-LOGIN may no longer sign in; revoked 1 token(s) it had signed in for\n");
        assert_eq!(run(&mut t, "disallow some-login", &boss, at, 0, false).unwrap_err().to_string(), "`some-login` is not on the allow list");

        for (line, e) in [
            ("", "usage"),
            ("revoke", "usage"),
            ("allow", "usage"),
            ("disallow", "usage"),
            ("frobnicate", "usage"),
            ("list --bogus", "unknown option `--bogus`\nusage"),
            ("invite --days", "`--days` needs a value"),
            ("invite --uses many", "--uses takes a number"),
            ("invite --token-days x", "--token-days takes a number"),
            ("create --days -1 --user z", "--days takes a number"),
        ] {
            assert_eq!(run(&mut t, line, &boss, at, 0, false).unwrap_err().to_string(), e, "{line}");
        }
        assert_eq!(run(&mut t, "revoke", &ann, at, 0, false).unwrap_err().to_string(), "usage", "a missing id is usage for anyone");
        let cmd = Command::parse(&["invite".into()], &boss, "usage").unwrap();
        assert!(cmd.apply(&mut t, at, None, false).unwrap_err().to_string().contains("needs entropy"));
        assert_eq!(
            Command::parse(&["create".into(), "--user".into(), "z".into()], &Asker { user: "", admin: true, from: "command line" }, "usage").unwrap(),
            Command::Create { user: "z".into(), admin: false, days: None, note: "command line".into() }
        );
        assert!(!Command::List { all: true, user: String::new() }.writes() && Command::Revoke { id: String::new(), owner: None }.writes());
    }

    #[test]
    fn allow_and_disallow_only_revoke_tokens_from_github() {
        let mut tokens = Tokens::default();
        tokens.allow("Ann", false).unwrap();
        tokens.invariants();
        tokens.allow("ann", true).unwrap();
        tokens.invariants();
        assert_eq!(tokens.github.len(), 1);
        assert!(tokens.allowed("ANN").unwrap().admin);
        assert!(tokens.allow("not a login", false).is_err());
        tokens.invariants();
        tokens.create("ann", true, None, "github", 100, ("t-gh", "fixture-gh")).unwrap();
        tokens.invariants();
        tokens.create("ann", false, None, "invitation", 100, ("t-invite", "fixture-invite")).unwrap();
        tokens.invariants();
        assert_eq!(tokens.disallow("Ann"), (true, 1));
        tokens.invariants();
        assert!(tokens.lookup("t-gh", 100).is_none());
        assert!(tokens.lookup("t-invite", 100).is_some());
        assert_eq!(tokens.revoke("t-invite", Some("ann")), 1);
        tokens.invariants();
    }

    #[test]
    fn invitations_expire_and_successful_redemption_consumes_one_use() {
        let mut tokens = Tokens::default();
        let raw = "ABCD2345ABCD2345";
        let code = tokens.invite(Invitation { user: None, admin: false, days: Some(1), uses: 2, token_days: Some(30) }, 100, raw).unwrap();
        tokens.invariants();
        assert!(!String::from_utf8_lossy(&tokens.encode().unwrap()).contains(raw));
        assert!(tokens.invited(&code, 86_500).is_none());
        assert!(tokens.redeem(&code, None, 101, ("t-1", "fixture-1")).is_err(), "an invitation for anyone needs a name");
        tokens.invariants();
        assert_eq!(tokens.invited(&code, 101).unwrap().uses_left, 2);
        assert!(tokens.redeem(&code, Some("bad name"), 101, ("t-1", "fixture-1")).is_err());
        assert_eq!(tokens.invited(&code, 101).unwrap().uses_left, 2, "a refused name spends nothing");
        let token = tokens.redeem(&code.to_lowercase(), Some("ann"), 101, ("t-1", "fixture-1")).unwrap().unwrap();
        tokens.invariants();
        assert_eq!(token.expires, Some(101 + 30 * 86400));
        assert_eq!(tokens.invited(&code, 102).unwrap().uses_left, 1);
        let failed = tokens.redeem(&code, Some("ann"), 102, ("t-1", "duplicate")).unwrap();
        assert!(failed.unwrap_err().to_string().contains("duplicate token id"));
        tokens.invariants();
        assert!(tokens.invited(&code, 102).is_none(), "the use is spent even when making the token fails");
        assert!(tokens.redeem(&code, Some("bob"), 102, ("t-2", "fixture-2")).is_err());
        let fixed = tokens.invite(Invitation { user: Some("dan"), admin: true, days: None, uses: 1, token_days: None }, 100, "WXYZ2345WXYZ2345").unwrap();
        let dan = tokens.redeem(&fixed, Some("someone-else"), 10_000_000, ("t-3", "fixture-3")).unwrap().unwrap();
        tokens.invariants();
        assert!(dan.user == "dan" && dan.admin && dan.expires.is_none(), "an invitation for a user makes that user's token");
    }
}
