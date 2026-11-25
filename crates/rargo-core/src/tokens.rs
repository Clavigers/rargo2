use anyhow::{Context, Result, anyhow, bail};
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
struct Tokens {
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
}

impl Github {
    pub fn path(state: &Path) -> PathBuf {
        state.join("github.json")
    }

    pub fn load(state: &Path) -> Option<Github> {
        let g: Github = serde_json::from_slice(&std::fs::read(Self::path(state)).ok()?).ok()?;
        (!g.client_id.is_empty() && !g.client_secret.is_empty()).then_some(g)
    }

    pub fn save(&self, state: &Path) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let path = Self::path(state);
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        let mut w = std::fs::OpenOptions::new().create(true).truncate(true).write(true).mode(0o600).open(&tmp)?;
        w.write_all(&serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, &path)?;
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
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).context("reading /dev/urandom")?;
    Ok(b)
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
        let stamp = std::fs::metadata(&self.path).ok().and_then(|m| Some((m.modified().ok()?, m.len())));
        let mut c = self.cached.lock();
        if c.0 != stamp || stamp.is_none() {
            let file = match std::fs::read(&self.path) {
                Ok(bytes) => match Tokens::decode(&bytes) {
                    Ok(f) => f,
                    Err(e) => {
                        eprintln!("rargod: {}: {e}", self.path.display());
                        Tokens::default()
                    }
                },
                Err(_) => Tokens::default(),
            };
            *c = (stamp, Arc::new(file));
        }
        c.1.clone()
    }

    fn change<T>(&self, f: impl FnOnce(&mut Tokens) -> Result<T>) -> Result<T> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(self.path.with_extension("lock"))
            .context("opening the token lock file")?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            bail!("locking the token file: {}", std::io::Error::last_os_error());
        }
        let mut file: Tokens = match std::fs::read(&self.path) {
            Ok(bytes) => Tokens::decode(&bytes).with_context(|| format!("{}", self.path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Tokens::default(),
            Err(e) => return Err(e).with_context(|| format!("{}", self.path.display())),
        };
        let out = f(&mut file)?;
        let tmp = self.path.with_extension(format!("tmp{}", std::process::id()));
        {
            use std::io::Write;
            let mut w = std::fs::OpenOptions::new().create(true).truncate(true).write(true).mode(0o600).open(&tmp)?;
            w.write_all(&file.encode()?)?;
            w.sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)?;
        *self.cached.lock() = (None, Arc::new(Tokens::default()));
        Ok(out)
    }

    pub fn lookup(&self, id: &str) -> Option<([u8; 32], bool, String)> {
        self.file().lookup(id, now())
    }

    pub fn create(&self, user: &str, admin: bool, days: Option<u32>, note: &str) -> Result<(Token, String)> {
        validate_name(user)?;
        let (id, secret) = token_entropy()?;
        let token = self.change(|tokens| tokens.create(user, admin, days, note, now(), (&id, &secret)))?;
        Ok((token, secret))
    }

    pub fn list(&self) -> Vec<Token> {
        self.file().tokens.clone()
    }

    pub fn invites(&self) -> Vec<Invite> {
        self.file().invites.clone()
    }

    pub fn revoke(&self, id: &str) -> Result<usize> {
        self.change(|tokens| Ok(tokens.revoke(id, None)))
    }

    pub fn revoke_own(&self, id: &str, user: &str) -> Result<usize> {
        self.change(|tokens| Ok(tokens.revoke(id, Some(user))))
    }

    pub fn allow(&self, login: &str, admin: bool) -> Result<()> {
        self.change(|tokens| tokens.allow(login, admin))
    }

    pub fn disallow(&self, login: &str) -> Result<(bool, usize)> {
        self.change(|tokens| Ok(tokens.disallow(login)))
    }

    pub fn allowed(&self, login: &str) -> Option<Allowed> {
        self.file().allowed(login)
    }

    pub fn allow_list(&self) -> Vec<Allowed> {
        self.file().github.clone()
    }

    pub fn revoke_self(&self, text: &str) -> Result<bool> {
        if !text.trim().contains('.') {
            return Ok(false);
        }
        self.change(|tokens| Ok(tokens.revoke_self(text)))
    }

    pub fn holder(&self, text: &str) -> Option<String> {
        self.file().holder(text, now())
    }

    pub fn invite(&self, user: Option<&str>, admin: bool, days: Option<u32>, uses: u32, token_days: Option<u32>) -> Result<String> {
        if let Some(user) = user {
            validate_name(user)?;
        }
        let raw = random_text(16, CLEAR)?;
        self.change(|tokens| tokens.invite(Invitation { user, admin, days, uses, token_days }, now(), &raw))
    }

    pub fn invited(&self, code: &str) -> Option<Invite> {
        self.file().invited(code, now()).cloned()
    }

    pub fn redeem(&self, code: &str, name: Option<&str>) -> Result<(Token, String)> {
        let (id, secret) = token_entropy()?;
        let token = self.change(|tokens| tokens.redeem(code, name, now(), (&id, &secret)))?;
        Ok((token, secret))
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

    fn revoke_self(&mut self, text: &str) -> bool {
        let Some((id, secret)) = text.trim().split_once('.') else { return false };
        let key = hex(&secure::psk_from_secret(secret.as_bytes()));
        let Some(t) = self.tokens.iter_mut().find(|t| t.id == id && t.key == key && !t.revoked) else { return false };
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

    fn redeem(&mut self, code: &str, name: Option<&str>, at: u64, entropy: (&str, &str)) -> Result<Token> {
        let invite = self.invited(code, at).context("this invitation is not known, has been used or has expired")?.clone();
        let user = match (invite.user, name) {
            (Some(user), _) => user,
            (None, Some(name)) if valid_name(name) => name.to_owned(),
            (None, _) => bail!("a name is needed: letters, digits, `-`, `_` and `.`"),
        };
        let token = self.create(&user, invite.admin, invite.token_days, "invitation", at, entropy)?;
        let used = self.invites.iter_mut().find(|i| i.code == invite.code && i.active(at)).expect("selected an active invitation");
        used.uses_left -= 1;
        Ok(token)
    }

    #[cfg(test)]
    fn invariants(&self) -> bool {
        let unique = |ids: Vec<&str>| {
            let n = ids.len();
            ids.into_iter().collect::<std::collections::BTreeSet<_>>().len() == n
        };
        self.tokens.iter().all(|t| unhex32(&t.key).is_some())
            && unique(self.tokens.iter().map(|t| t.id.as_str()).collect())
            && unique(self.invites.iter().map(|i| i.code.as_str()).collect())
            && self.github.iter().all(|a| valid_login(&a.login))
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

pub fn command(store: &TokenStore, args: &[String], asker: &Asker, usage: &str) -> Result<Said> {
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
    let refuse = || {
        anyhow!(
            "user `{}` may not administer this server (an administrator's token is made on the server with `rargod token create --admin`, `token invite --admin` or `token allow --admin`)",
            asker.user
        )
    };
    let when = |t: Option<u64>| match t {
        None => "never".to_string(),
        Some(t) if t <= now() => "expired".to_string(),
        Some(t) => format!("in {} days", (t - now()).div_ceil(86400)),
    };
    let (mut out, mut err) = (String::new(), String::new());
    use std::fmt::Write as _;
    match what {
        "list" => {
            let _ = writeln!(out, "{:<16} {:<20} {:<6} {:<12} FROM", "ID", "USER", "ADMIN", "EXPIRES");
            for t in store.list().into_iter().filter(|t| asker.admin || t.user == asker.user) {
                let state = if t.revoked { "revoked".to_string() } else { when(t.expires) };
                let _ = writeln!(out, "{:<16} {:<20} {:<6} {:<12} {}", t.id, t.user, if t.admin { "yes" } else { "" }, state, t.note);
            }
            if asker.admin {
                let allowed = store.allow_list();
                if !allowed.is_empty() {
                    let names: Vec<String> = allowed.iter().map(|a| format!("{}{}", a.login, if a.admin { " (administrator)" } else { "" })).collect();
                    let _ = writeln!(out, "\nGitHub accounts that may sign in: {}", names.join(", "));
                }
                let open: Vec<_> = store.invites().into_iter().filter(|i| i.uses_left > 0 && i.expires.is_none_or(|e| e > now())).collect();
                if !open.is_empty() {
                    let _ = writeln!(out, "\ninvitations not used up: {}", open.len());
                    for i in open {
                        let _ = writeln!(
                            out,
                            "  for {}{}: {} use(s) left, expires {}",
                            i.user.as_deref().unwrap_or("anyone"),
                            if i.admin { " (administrator)" } else { "" },
                            i.uses_left,
                            when(i.expires)
                        );
                    }
                }
            }
        }
        "revoke" => {
            let Some(id) = rest.first() else { bail!("{usage}") };
            let n = if asker.admin { store.revoke(id)? } else { store.revoke_own(id, asker.user)? };
            if n == 0 {
                if asker.admin {
                    bail!("no token that still works has the id or user `{id}`");
                }
                bail!("none of your tokens that still work has the id `{id}` (you may revoke only your own)");
            }
            let _ = writeln!(out, "revoked {n} token(s); the next command on an open session is refused");
        }
        _ if !asker.admin => return Err(refuse()),
        "invite" => {
            let code = store.invite(user.as_deref(), admin, days.or(Some(7)), uses, token_days)?;
            let _ = writeln!(out, "{code}");
            let _ = writeln!(
                err,
                "An invitation for {}{}, good for {} use(s), {}. Whoever has it runs `rargo auth` and enters it on the page that opens.",
                user.as_deref().unwrap_or("whoever redeems it"),
                if admin { " (administrator)" } else { "" },
                uses.max(1),
                when(Some(now() + days.unwrap_or(7) as u64 * 86400)).replace("in ", "for "),
            );
        }
        "create" => {
            let user = user.context("--user NAME is required")?;
            let (t, secret) = store.create(&user, admin, days, asker.from)?;
            let _ = writeln!(out, "{}", token_text(&t, &secret));
            let _ = writeln!(err, "A token for {user}; it is shown this once. A client uses it as RARGO_TOKEN or through `rargo auth --token`.");
        }
        "allow" => {
            let Some(login) = rest.first() else { bail!("{usage}") };
            store.allow(login, admin)?;
            let _ = writeln!(out, "{login} may sign in with GitHub{}", if admin { " (administrator)" } else { "" });
            if store.path().parent().and_then(Github::load).is_none() {
                let _ = writeln!(err, "note: this server has no GitHub application yet (rargod token github ...); until then `rargo auth` asks for an invitation");
            }
        }
        "disallow" => {
            let Some(login) = rest.first() else { bail!("{usage}") };
            let (was, n) = store.disallow(login)?;
            if !was && n == 0 {
                bail!("`{login}` is not on the allow list");
            }
            let _ = writeln!(out, "{login} may no longer sign in; revoked {n} token(s) it had signed in for");
        }
        _ => bail!("{usage}"),
    }
    Ok((out, err))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_tokens_are_verified_expired_and_revoked_without_the_secret() {
        let mut tokens = Tokens::decode(b"{}").unwrap();
        assert!(tokens.invariants());
        let secret = "fixture-credential";
        let token = tokens.create("ann", true, Some(1), "test", 100, ("t-ann", secret)).unwrap();
        assert!(tokens.invariants());
        let encoded = tokens.encode().unwrap();
        assert!(!String::from_utf8_lossy(&encoded).contains(secret));
        let mut tokens = Tokens::decode(&encoded).unwrap();
        assert!(tokens.invariants());
        assert_eq!(tokens.lookup("t-ann", 100), Some((secure::psk_from_secret(secret.as_bytes()), true, "ann".into())));
        assert!(tokens.lookup("t-ann", 86_500).is_none());
        assert_eq!(tokens.holder(&token_text(&token, secret), 101).as_deref(), Some("ann"));
        assert!(!tokens.revoke_self("t-ann.incorrect"));
        assert!(tokens.invariants());
        assert_eq!(tokens.revoke("t-ann", Some("bob")), 0);
        assert!(tokens.invariants());
        assert!(tokens.revoke_self(&token_text(&token, secret)));
        assert!(tokens.invariants());
        assert_eq!(tokens.revoke("ann", None), 0);
        assert!(tokens.invariants());
        assert!(tokens.lookup("t-ann", 100).is_none());
        assert!(Tokens::decode(b"{broken").is_err());
        assert!(Tokens::decode(br#"{"tokens":["broken"]}"#).is_err());
    }

    #[test]
    fn token_commands_do_everything_for_an_administrator_and_a_users_own_tokens_for_anyone() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(dir.path().join("tokens.json"));
        let args = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
        let boss = Asker { user: "boss", admin: true, from: "test" };
        let ann = Asker { user: "ann", admin: false, from: "test" };
        let (out, _) = command(&store, &args("create --user ann"), &boss, "usage").unwrap();
        let ann_token = out.trim().to_string();
        let ann_id = ann_token.split('.').next().unwrap().to_string();
        command(&store, &args("create --user bob"), &boss, "usage").unwrap();
        let (code, err) = command(&store, &args("invite --user cat --admin"), &boss, "usage").unwrap();
        assert!(err.contains("An invitation for cat (administrator)"), "{err}");
        command(&store, &args("allow some-login"), &boss, "usage").unwrap();
        let (all, _) = command(&store, &args("list"), &boss, "usage").unwrap();
        assert!(all.contains(" ann ") && all.contains(" bob ") && all.contains("some-login") && all.contains("for cat (administrator)"), "{all}");
        assert!(!all.contains(ann_token.split('.').nth(1).unwrap()) && !all.contains(code.trim()), "{all}");
        let (own, _) = command(&store, &args("list"), &ann, "usage").unwrap();
        assert!(own.contains(&ann_id) && !own.contains(" bob ") && !own.contains("some-login") && !own.contains("cat"), "{own}");
        for refused in ["invite", "create --user ann2", "allow x", "disallow some-login"] {
            let e = command(&store, &args(refused), &ann, "usage").unwrap_err().to_string();
            assert!(e.contains("may not administer") && e.contains("rargod token create --admin"), "{refused}: {e}");
        }
        assert!(command(&store, &args("revoke bob"), &ann, "usage").is_err(), "not someone else's");
        assert!(store.list().iter().any(|t| t.user == "bob" && !t.revoked));
        command(&store, &args(&format!("revoke {ann_id}")), &ann, "usage").unwrap();
        assert!(store.lookup(&ann_id).is_none(), "her own");
        let (n, _) = command(&store, &args("revoke bob"), &boss, "usage").unwrap();
        assert!(n.contains("revoked 1 token"), "{n}");
        assert!(command(&store, &args("frobnicate"), &boss, "usage").unwrap_err().to_string().contains("usage"));
    }

    #[test]
    fn allow_and_disallow_only_revoke_tokens_from_github() {
        let mut tokens = Tokens::default();
        tokens.allow("Ann", false).unwrap();
        assert!(tokens.invariants());
        tokens.allow("ann", true).unwrap();
        assert!(tokens.invariants());
        assert_eq!(tokens.github.len(), 1);
        assert!(tokens.allowed("ANN").unwrap().admin);
        assert!(tokens.allow("not a login", false).is_err());
        assert!(tokens.invariants());
        tokens.create("ann", true, None, "github", 100, ("t-gh", "fixture-gh")).unwrap();
        assert!(tokens.invariants());
        tokens.create("ann", false, None, "invitation", 100, ("t-invite", "fixture-invite")).unwrap();
        assert!(tokens.invariants());
        assert_eq!(tokens.disallow("Ann"), (true, 1));
        assert!(tokens.invariants());
        assert!(tokens.lookup("t-gh", 100).is_none());
        assert!(tokens.lookup("t-invite", 100).is_some());
        assert_eq!(tokens.revoke("t-invite", Some("ann")), 1);
        assert!(tokens.invariants());
    }

    #[test]
    fn invitations_expire_and_successful_redemption_consumes_one_use() {
        let mut tokens = Tokens::default();
        let raw = "ABCD2345ABCD2345";
        let code = tokens.invite(Invitation { user: None, admin: false, days: Some(1), uses: 2, token_days: Some(30) }, 100, raw).unwrap();
        assert!(tokens.invariants());
        assert!(!String::from_utf8_lossy(&tokens.encode().unwrap()).contains(raw));
        assert!(tokens.invited(&code, 86_500).is_none());
        assert!(tokens.redeem(&code, None, 101, ("t-1", "fixture-1")).is_err());
        assert!(tokens.invariants());
        assert_eq!(tokens.invited(&code, 101).unwrap().uses_left, 2);
        let token = tokens.redeem(&code.to_lowercase(), Some("ann"), 101, ("t-1", "fixture-1")).unwrap();
        assert!(tokens.invariants());
        assert_eq!(token.expires, Some(101 + 30 * 86400));
        assert!(tokens.redeem(&code, Some("ann"), 102, ("t-1", "duplicate")).is_err());
        assert!(tokens.invariants());
        assert_eq!(tokens.invited(&code, 102).unwrap().uses_left, 1);
        tokens.redeem(&code, Some("bob"), 102, ("t-2", "fixture-2")).unwrap();
        assert!(tokens.invariants());
        assert!(tokens.invited(&code, 102).is_none());
    }

    #[test]
    fn github_application_configuration_is_written_privately() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Github::load(dir.path()).is_none());
        Github { client_id: "id".into(), client_secret: "s".into(), url: "https://x".into() }.save(dir.path()).unwrap();
        assert_eq!(Github::load(dir.path()).unwrap().url, "https://x");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(Github::path(dir.path())).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
