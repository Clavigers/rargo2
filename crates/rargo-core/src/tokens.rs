//! Tokens and invitations: who may connect.
//!
//! One JSON file in the state directory (`tokens.json`), rewritten whole
//! under a lock by whoever changes it (`rargod token ...` on the server's
//! machine, or the running server when an invitation is redeemed) and read
//! again by the running server whenever it has changed, so nothing here
//! needs a restart.
//!
//! A token is what a client presents: an id (sent as the user name of the
//! handshake) and a secret. The file holds the key derived from the secret,
//! not the secret; the derived key is what the handshake needs, so the file
//! is as sensitive as the secrets themselves and is kept readable by its
//! owner only (D67). An invitation is a code an administrator hands to a person;
//! redeeming it (through `rargo auth`) makes a token.

use anyhow::{Context, Result, bail};
use rargo_proto::secure;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Token {
    /// What the client sends as its user name.
    pub id: String,
    /// Who it is.
    pub user: String,
    #[serde(default)]
    pub admin: bool,
    /// The handshake key derived from the secret, in hex.
    pub key: String,
    pub created: u64,
    /// Seconds since the epoch after which it no longer works.
    #[serde(default)]
    pub expires: Option<u64>,
    #[serde(default)]
    pub revoked: bool,
    /// Where it came from (an invitation, the command line).
    #[serde(default)]
    pub note: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Invite {
    /// A hash of the code, in hex.
    pub code: String,
    /// The name tokens made from it carry; none: whoever redeems it says.
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub admin: bool,
    #[serde(default)]
    pub expires: Option<u64>,
    pub uses_left: u32,
    /// How long tokens made from it last; none: until revoked.
    #[serde(default)]
    pub token_days: Option<u32>,
    pub created: u64,
}

/// A GitHub account that may get a token by signing in (`rargo auth`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Allowed {
    pub login: String,
    #[serde(default)]
    pub admin: bool,
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct File {
    #[serde(default)]
    tokens: Vec<Token>,
    #[serde(default)]
    invites: Vec<Invite>,
    /// The allow list of GitHub accounts.
    #[serde(default)]
    github: Vec<Allowed>,
}

/// The GitHub OAuth application a server signs people in with, and the
/// address its pages are reached at (`github.json` in the state directory,
/// written by `rargod token github`, read whenever it is needed).
#[derive(Serialize, Deserialize, Clone)]
pub struct Github {
    pub client_id: String,
    pub client_secret: String,
    /// `https://rargo.rs`: where GitHub sends the browser back to.
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

/// A GitHub login: letters, digits and single hyphens, at most 39.
pub fn valid_login(login: &str) -> bool {
    !login.is_empty() && login.len() <= 39 && login.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') && !login.starts_with('-')
}

pub struct TokenStore {
    path: PathBuf,
    cached: parking_lot::Mutex<(Option<(SystemTime, u64)>, Arc<File>)>,
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

/// Letters and digits that are not mistaken for one another when read off
/// a screen.
const CLEAR: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

pub fn random_text(n: usize, alphabet: &[u8]) -> Result<String> {
    // (256 is a multiple of 32, and of 62 it is not: bytes that would bias
    // the choice are drawn again.)
    let limit = 256 - 256 % alphabet.len();
    let mut out = String::new();
    while out.len() < n {
        for b in random(n)? {
            if (b as usize) < limit && out.len() < n {
                out.push(alphabet[b as usize % alphabet.len()] as char);
            }
        }
    }
    Ok(out)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
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

/// Codes are compared without regard to case, dashes or spaces.
pub fn normal_code(code: &str) -> String {
    code.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect()
}

fn code_hash(code: &str) -> String {
    hex(blake3::Hasher::new_derive_key("rargo invitation v1").update(normal_code(code).as_bytes()).finalize().as_bytes())
}

/// A name that is safe to show, store and put into a file name.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

impl TokenStore {
    pub fn new(path: PathBuf) -> TokenStore {
        TokenStore { path, cached: parking_lot::Mutex::new((None, Arc::new(File::default()))) }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The file as it is now (read again only when it has changed).
    fn file(&self) -> Arc<File> {
        let stamp = std::fs::metadata(&self.path).ok().and_then(|m| Some((m.modified().ok()?, m.len())));
        let mut c = self.cached.lock();
        if c.0 != stamp || stamp.is_none() {
            let file = match std::fs::read(&self.path) {
                Ok(bytes) => match serde_json::from_slice::<File>(&bytes) {
                    Ok(f) => f,
                    Err(e) => {
                        // A file that cannot be read admits nobody.
                        eprintln!("rargod: {}: {e}", self.path.display());
                        File::default()
                    }
                },
                Err(_) => File::default(),
            };
            *c = (stamp, Arc::new(file));
        }
        c.1.clone()
    }

    /// Changes the file under a lock held against other writers.
    fn change<T>(&self, f: impl FnOnce(&mut File) -> Result<T>) -> Result<T> {
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
        // SAFETY: a valid descriptor; released when `lock` is dropped.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            bail!("locking the token file: {}", std::io::Error::last_os_error());
        }
        let mut file: File = match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("{}", self.path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => File::default(),
            Err(e) => return Err(e).with_context(|| format!("{}", self.path.display())),
        };
        let out = f(&mut file)?;
        let tmp = self.path.with_extension(format!("tmp{}", std::process::id()));
        {
            use std::io::Write;
            let mut w = std::fs::OpenOptions::new().create(true).truncate(true).write(true).mode(0o600).open(&tmp)?;
            w.write_all(&serde_json::to_vec_pretty(&file)?)?;
            w.sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)?;
        *self.cached.lock() = (None, Arc::new(File::default()));
        Ok(out)
    }

    /// The key and standing of the token `id`, if it works now.
    pub fn lookup(&self, id: &str) -> Option<([u8; 32], bool, String)> {
        let file = self.file();
        let t = file.tokens.iter().find(|t| t.id == id)?;
        if t.revoked || t.expires.is_some_and(|e| e <= now()) {
            return None;
        }
        Some((unhex32(&t.key)?, t.admin, t.user.clone()))
    }

    /// Makes a token. Returns it and its secret, which is not kept.
    pub fn create(&self, user: &str, admin: bool, days: Option<u32>, note: &str) -> Result<(Token, String)> {
        if !valid_name(user) {
            bail!("a user name is letters, digits, `-`, `_` and `.`, at most 64 of them");
        }
        let secret = random_text(40, b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789")?;
        let token = Token {
            id: format!("t-{}", random_text(12, b"abcdefghijklmnopqrstuvwxyz0123456789")?),
            user: user.to_string(),
            admin,
            key: hex(&secure::psk_from_secret(secret.as_bytes())),
            created: now(),
            expires: days.map(|d| now() + d as u64 * 86400),
            revoked: false,
            note: note.to_string(),
        };
        let t = token.clone();
        self.change(move |f| {
            f.tokens.push(t);
            Ok(())
        })?;
        Ok((token, secret))
    }

    pub fn list(&self) -> Vec<Token> {
        self.file().tokens.clone()
    }

    pub fn invites(&self) -> Vec<Invite> {
        self.file().invites.clone()
    }

    /// Revokes the token `id`, or every token of the user `id`. Returns how
    /// many were revoked.
    pub fn revoke(&self, id: &str) -> Result<usize> {
        self.change(|f| {
            let mut n = 0;
            for t in f.tokens.iter_mut().filter(|t| !t.revoked && (t.id == id || t.user == id)) {
                t.revoked = true;
                n += 1;
            }
            Ok(n)
        })
    }

    /// Puts a GitHub account on the allow list (or changes its standing).
    pub fn allow(&self, login: &str, admin: bool) -> Result<()> {
        if !valid_login(login) {
            bail!("`{login}` is not a GitHub login");
        }
        self.change(|f| {
            f.github.retain(|a| !a.login.eq_ignore_ascii_case(login));
            f.github.push(Allowed { login: login.to_string(), admin });
            Ok(())
        })
    }

    /// Takes a GitHub account off the allow list and revokes the tokens it
    /// got by signing in. Returns whether it was on the list and how many
    /// tokens were revoked.
    pub fn disallow(&self, login: &str) -> Result<(bool, usize)> {
        self.change(|f| {
            let before = f.github.len();
            f.github.retain(|a| !a.login.eq_ignore_ascii_case(login));
            let mut n = 0;
            for t in f.tokens.iter_mut().filter(|t| !t.revoked && t.note == "github" && t.user.eq_ignore_ascii_case(login)) {
                t.revoked = true;
                n += 1;
            }
            Ok((f.github.len() != before, n))
        })
    }

    /// Whether a GitHub account may sign in, and as what (GitHub logins
    /// do not distinguish case).
    pub fn allowed(&self, login: &str) -> Option<Allowed> {
        self.file().github.iter().find(|a| a.login.eq_ignore_ascii_case(login)).cloned()
    }

    pub fn allow_list(&self) -> Vec<Allowed> {
        self.file().github.clone()
    }

    /// A token revokes itself: whoever holds its text may end it. Returns
    /// whether it was a token that still worked.
    pub fn revoke_self(&self, text: &str) -> Result<bool> {
        let Some((id, secret)) = text.trim().split_once('.') else { return Ok(false) };
        let key = hex(&secure::psk_from_secret(secret.as_bytes()));
        self.change(|f| {
            match f.tokens.iter_mut().find(|t| t.id == id && t.key == key && !t.revoked) {
                Some(t) => {
                    t.revoked = true;
                    Ok(true)
                }
                None => Ok(false),
            }
        })
    }

    /// Whose token this is, given its whole text, if it works now.
    pub fn holder(&self, text: &str) -> Option<String> {
        let (id, secret) = text.trim().split_once('.')?;
        let key = hex(&secure::psk_from_secret(secret.as_bytes()));
        let file = self.file();
        let t = file.tokens.iter().find(|t| t.id == id && t.key == key)?;
        if t.revoked || t.expires.is_some_and(|e| e <= now()) {
            return None;
        }
        Some(t.user.clone())
    }

    /// Makes an invitation. Returns its code, which is not kept.
    pub fn invite(&self, user: Option<&str>, admin: bool, days: Option<u32>, uses: u32, token_days: Option<u32>) -> Result<String> {
        if let Some(u) = user {
            if !valid_name(u) {
                bail!("a user name is letters, digits, `-`, `_` and `.`, at most 64 of them");
            }
        }
        let raw = random_text(16, CLEAR)?;
        let code = format!("{}-{}-{}-{}", &raw[0..4], &raw[4..8], &raw[8..12], &raw[12..16]);
        let invite = Invite {
            code: code_hash(&code),
            user: user.map(String::from),
            admin,
            expires: days.map(|d| now() + d as u64 * 86400),
            uses_left: uses.max(1),
            token_days,
            created: now(),
        };
        self.change(move |f| {
            f.invites.retain(|i| i.uses_left > 0 && i.expires.is_none_or(|e| e > now()));
            f.invites.push(invite);
            Ok(())
        })?;
        Ok(code)
    }

    /// Whether `code` is an invitation that can be used now, and for whom.
    pub fn invited(&self, code: &str) -> Option<Invite> {
        let h = code_hash(code);
        self.file().invites.iter().find(|i| i.code == h && i.uses_left > 0 && i.expires.is_none_or(|e| e > now())).cloned()
    }

    /// Uses an invitation once: makes a token for `name` (or the name the
    /// invitation carries). Returns the token and its secret.
    pub fn redeem(&self, code: &str, name: Option<&str>) -> Result<(Token, String)> {
        let h = code_hash(code);
        let (invite, user) = self.change(|f| {
            let Some(i) = f.invites.iter_mut().find(|i| i.code == h && i.uses_left > 0 && i.expires.is_none_or(|e| e > now())) else {
                bail!("this invitation is not known, has been used or has expired");
            };
            let user = match (&i.user, name) {
                (Some(u), _) => u.clone(),
                (None, Some(n)) if valid_name(n) => n.to_string(),
                (None, _) => bail!("a name is needed: letters, digits, `-`, `_` and `.`"),
            };
            i.uses_left -= 1;
            Ok((i.clone(), user))
        })?;
        self.create(&user, invite.admin, invite.token_days, "invitation")
    }
}

/// The text a client keeps: the id and the secret in one string.
pub fn token_text(t: &Token, secret: &str) -> String {
    format!("{}.{secret}", t.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_made_found_expired_and_revoked_without_keeping_the_secret() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(dir.path().join("tokens.json"));
        assert!(store.lookup("t-none").is_none());
        let (t, secret) = store.create("ann", true, None, "test").unwrap();
        let (key, admin, user) = store.lookup(&t.id).unwrap();
        assert_eq!(key, secure::psk_from_secret(secret.as_bytes()));
        assert!(admin);
        assert_eq!(user, "ann");
        let text = std::fs::read_to_string(dir.path().join("tokens.json")).unwrap();
        assert!(!text.contains(&secret), "the secret is not in the file");
        // Another handle on the same file (the command line beside the
        // server) sees it, and what it changes is seen here.
        let other = TokenStore::new(dir.path().join("tokens.json"));
        assert!(other.lookup(&t.id).is_some());
        assert_eq!(other.revoke(&t.id).unwrap(), 1);
        assert!(store.lookup(&t.id).is_none(), "revoked");
        assert_eq!(other.revoke(&t.id).unwrap(), 0);
        // Expired at once.
        let (t2, _) = store.create("bob", false, Some(0), "test").unwrap();
        assert!(store.lookup(&t2.id).is_none(), "expired");
        assert!(store.create("no spaces", false, None, "").is_err());
        // A token may end itself, given its whole text and nothing less.
        let (t3, secret3) = store.create("carol", false, None, "test").unwrap();
        assert!(!store.revoke_self(&format!("{}.not-the-secret-of-it", t3.id)).unwrap());
        assert!(!store.revoke_self(&t3.id).unwrap());
        assert!(store.lookup(&t3.id).is_some());
        // Its whole text also says whose it is, while it works.
        assert_eq!(store.holder(&token_text(&t3, &secret3)).as_deref(), Some("carol"));
        assert_eq!(store.holder(&format!("{}.not-the-secret-of-it", t3.id)), None);
        assert_eq!(store.holder(&t3.id), None);
        assert!(store.revoke_self(&token_text(&t3, &secret3)).unwrap());
        assert!(store.lookup(&t3.id).is_none());
        assert_eq!(store.holder(&token_text(&t3, &secret3)), None, "revoked");
        assert!(!store.revoke_self(&token_text(&t3, &secret3)).unwrap(), "once");
        assert_eq!(store.list().len(), 3);
    }

    #[test]
    fn the_allow_list_admits_github_accounts_and_taking_one_off_revokes_its_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(dir.path().join("tokens.json"));
        assert!(store.allowed("ann").is_none());
        store.allow("Ann", false).unwrap();
        store.allow("ann", true).unwrap();
        assert_eq!(store.allow_list().len(), 1, "one entry per account, whatever the case");
        assert!(store.allowed("ANN").unwrap().admin);
        assert!(store.allow("not a login", false).is_err());
        let (signed_in, _) = store.create("ann", true, None, "github").unwrap();
        let (invited, _) = store.create("ann", false, None, "invitation").unwrap();
        assert_eq!(store.disallow("Ann").unwrap(), (true, 1));
        assert!(store.lookup(&signed_in.id).is_none(), "the token it signed in for is revoked");
        assert!(store.lookup(&invited.id).is_some(), "a token from an invitation is another matter");
        assert_eq!(store.disallow("ann").unwrap(), (false, 0));
        // The application's settings: kept beside the tokens, private.
        assert!(Github::load(dir.path()).is_none());
        Github { client_id: "id".into(), client_secret: "s".into(), url: "https://x".into() }.save(dir.path()).unwrap();
        assert_eq!(Github::load(dir.path()).unwrap().url, "https://x");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(Github::path(dir.path())).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn an_invitation_makes_tokens_as_often_as_it_says_and_no_more() {
        let dir = tempfile::tempdir().unwrap();
        let store = TokenStore::new(dir.path().join("tokens.json"));
        let code = store.invite(Some("ann"), false, Some(1), 2, Some(30)).unwrap();
        assert!(!std::fs::read_to_string(store.path()).unwrap().contains(&normal_code(&code)));
        assert!(store.invited(&code.to_lowercase().replace('-', " ")).is_some(), "case and dashes do not matter");
        let (t, secret) = store.redeem(&code, Some("ignored")).unwrap();
        assert_eq!(t.user, "ann");
        assert!(t.expires.is_some());
        assert!(store.lookup(&t.id).is_some());
        assert!(token_text(&t, &secret).starts_with("t-"));
        store.redeem(&code, None).unwrap();
        assert!(store.redeem(&code, None).is_err(), "used up");
        assert!(store.redeem("AAAA-BBBB-CCCC-DDDD", None).is_err());
        // Without a name of its own, the redeemer's is taken, if it is one.
        let open = store.invite(None, false, None, 2, None).unwrap();
        assert!(store.redeem(&open, Some("../etc")).is_err());
        assert_eq!(store.redeem(&open, Some("carol")).unwrap().0.user, "carol");
    }
}
