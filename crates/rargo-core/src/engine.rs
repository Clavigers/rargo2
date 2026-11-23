//! The action engine: given an action and the view it should run against,
//! produce its result by the cheapest sound means.
//!
//! 1. A recorded trace whose observations all hold in the view: replay.
//! 2. The same action already running for someone whose predicted inputs
//!    agree with ours: wait, then check its trace against our view.
//! 3. Otherwise run it in a sandbox, record the trace, store the outputs.

use crate::hash::{Hash, KeyHasher};
use crate::sandbox::{EXTRA_FD_BASE, SpawnReq, Zygote};
use crate::sandboxfs::{ActionFs, ActionFsConfig, SandboxFs, UpperKind};
use crate::sched::{Priority, Scheduler};
use crate::store::Store;
use crate::trace::*;
use crate::view::*;
use anyhow::{Context, Result, bail};
use parking_lot::Mutex;
use std::collections::{BTreeMap, HashMap};
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::AsyncReadExt;

/// Receives an action's output, live or replayed.
pub trait Observer: Send + Sync {
    /// The action was not in the cache; it is about to run (or to wait for
    /// an identical run already in flight).
    fn miss(&self) {}
    /// Whether [`Observer::explain`] should be called on a miss.
    fn wants_explanation(&self) -> bool {
        false
    }
    /// Why the cache did not have the action.
    fn explain(&self, _why: &str) {}
    fn stdout(&self, _data: &[u8]) {}
    /// One line of stderr without the newline. `live` is present while the
    /// action is actually running.
    fn stderr_line(&self, _line: &[u8], _live: Option<&dyn Live>) {}
}

pub struct NullObserver;
impl Observer for NullObserver {}

/// Access to a running action's upper layer.
pub trait Live {
    /// Stores a file the action has finished writing, before the action
    /// ends, and returns its manifest entry.
    fn ingest(&self, path: &[u8]) -> Option<MEntry>;
}

/// [`Live`] for an action running on this machine.
pub struct LiveCtl<'a> {
    action: &'a Arc<ActionFs>,
    store: &'a Store,
}

impl Live for LiveCtl<'_> {
    fn ingest(&self, path: &[u8]) -> Option<MEntry> {
        let (scratch, exec) = self.action.upper_file_at(path)?;
        let (hash, size) = self.store.put_file_link(&scratch, exec).ok()?;
        Some(MEntry::File { hash, exec, size })
    }
}

/// Asks a connected client for the values of variables.
pub type EnvAsker = Arc<
    dyn Fn(Vec<String>) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<(String, Option<String>)>>> + Send>>
        + Send
        + Sync,
>;

/// What the server knows of a client's environment.
///
/// A client sends the variables on the protocol's allow-list with its
/// request. Any other variable is unknown until some action declares that it
/// reads it; the client is then asked for it (and remembers to send it next
/// time).
pub struct ClientEnv {
    vars: parking_lot::RwLock<HashMap<String, String>>,
    /// Names outside the allow-list the client has answered for.
    checked: parking_lot::RwLock<std::collections::HashSet<String>>,
    asker: Option<EnvAsker>,
}

impl ClientEnv {
    pub fn new(vars: HashMap<String, String>, checked: Vec<String>, asker: Option<EnvAsker>) -> ClientEnv {
        ClientEnv {
            vars: parking_lot::RwLock::new(vars),
            checked: parking_lot::RwLock::new(checked.into_iter().collect()),
            asker,
        }
    }

    pub fn get(&self, name: &str) -> Option<String> {
        self.vars.read().get(name).cloned()
    }

    pub fn contains_key(&self, name: &str) -> bool {
        self.vars.read().contains_key(name)
    }

    pub fn to_map(&self) -> HashMap<String, String> {
        self.vars.read().clone()
    }

    /// Makes sure the client's values of `names` are known.
    pub async fn ensure(&self, names: &[String]) -> Result<()> {
        let Some(asker) = &self.asker else { return Ok(()) };
        let unknown: Vec<String> = {
            let (vars, checked) = (self.vars.read(), self.checked.read());
            names
                .iter()
                .filter(|n| !rargo_proto::env_allowed(n) && !vars.contains_key(*n) && !checked.contains(*n))
                .cloned()
                .collect()
        };
        if unknown.is_empty() {
            return Ok(());
        }
        let answers = asker(unknown.clone()).await?;
        let (mut vars, mut checked) = (self.vars.write(), self.checked.write());
        for (name, value) in answers {
            if let Some(v) = value {
                vars.insert(name, v);
            }
        }
        checked.extend(unknown);
        Ok(())
    }
}

pub struct Request {
    pub spec: ActionSpec,
    pub view: Arc<dyn View>,
    /// The requesting client's environment, consulted for variables the
    /// action declares an interest in.
    pub client_env: Arc<ClientEnv>,
    pub uid: u32,
    pub gid: u32,
    pub prio: Priority,
    /// Do not replay; always run.
    pub no_cache: bool,
    /// Whether a finished run may be stored.
    pub cacheable: Arc<dyn Fn(&ActionResult) -> bool + Send + Sync>,
    /// Environment variables the action turned out to depend on, derived
    /// from its outputs.
    pub declared_env: Arc<dyn Fn(&ActionResult, &Store) -> Vec<String> + Send + Sync>,
    pub observer: Arc<dyn Observer>,
    /// The files of the state the view shows under the spec's `discard`
    /// prefixes (an incremental seed), by content and size. Some of them
    /// may be on a worker rather than in the store.
    pub seed_blobs: Arc<Vec<(Hash, u64)>>,
}

/// The seed a request was given cannot be completed: files of it were on a
/// worker that is gone. The request is to be made again with another seed.
#[derive(Debug)]
pub struct SeedLost;

impl std::fmt::Display for SeedLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("files of the incremental state are no longer available")
    }
}

impl std::error::Error for SeedLost {}

/// Something that can run actions elsewhere.
pub trait RemoteRunner: Send + Sync {
    /// Whether a connected worker holds this blob.
    fn holds(&self, blob: &Hash) -> bool;

    /// Fetches blobs that are held by workers into the store.
    fn bring_home<'a>(
        &'a self,
        blobs: &'a [Hash],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>>;

    /// Runs the action on a worker if that is expected to finish sooner
    /// than waiting for a local core. `None` means "run it locally".
    fn try_run<'a>(
        &'a self,
        engine: &'a Engine,
        spec: &'a ActionSpec,
        key: &'a Hash,
        req: &'a Request,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Option<Result<(ActionResult, Vec<(VPath, Obs)>, bool, Vec<(Vec<u8>, MEntry)>)>>,
                > + Send
                + 'a,
        >,
    >;
}

pub struct Outcome {
    pub result: Arc<ActionResult>,
    /// The result was replayed from the cache.
    pub cached: bool,
    /// What the action wrote under the spec's `discard` prefixes. Only
    /// present when the action actually ran; never part of a trace.
    pub side: Vec<(Vec<u8>, MEntry)>,
}

#[derive(Default)]
pub struct EngineStats {
    pub runs: AtomicU64,
    pub joined: AtomicU64,
    pub joined_then_ran: AtomicU64,
    pub tainted: AtomicU64,
    pub env_reruns: AtomicU64,
    pub remote_runs: AtomicU64,
    pub remote_failed: AtomicU64,
    pub run_cpu_us: AtomicU64,
    pub run_wall_us: AtomicU64,
}

pub struct Engine {
    pub store: Arc<Store>,
    pub content: Arc<Content>,
    pub fs: Arc<SandboxFs>,
    pub mnt: PathBuf,
    pub zygote: Arc<Zygote>,
    pub traces: TraceCache,
    pub sched: Arc<Scheduler>,
    inflight: Mutex<HashMap<(Hash, Hash), tokio::sync::watch::Receiver<bool>>>,
    seq: AtomicU64,
    avg_wall_us: AtomicU64,
    /// The newest trace per program, for guessing what an action that has
    /// never run will look at (every rustc reads the same sysroot).
    profiles: dashmap::DashMap<Vec<u8>, Arc<Trace>>,
    /// Workers that can run actions, if any are configured.
    pub remote: std::sync::OnceLock<Arc<dyn RemoteRunner>>,
    pub stats: EngineStats,
    /// Per-action cgroups are created below this directory when set.
    pub cgroup_root: Option<PathBuf>,
}

struct InflightGuard<'a> {
    engine: &'a Engine,
    key: (Hash, Hash),
    tx: tokio::sync::watch::Sender<bool>,
}

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        self.engine.inflight.lock().remove(&self.key);
        let _ = self.tx.send(true);
    }
}

/// What an action holds while it runs. Dropped armed (its future was
/// cancelled), it kills the process and tears the action directory down once
/// the process is gone.
struct Teardown {
    fs: Arc<SandboxFs>,
    sched: Arc<Scheduler>,
    zygote: Arc<Zygote>,
    action: Arc<ActionFs>,
    scratch: PathBuf,
    running: Option<crate::sandbox::Running>,
    group: Option<crate::cgroup::ActionGroup>,
    armed: bool,
}

impl Drop for Teardown {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let running = self.running.take();
        let (fs, action, scratch, group) = (self.fs.clone(), self.action.clone(), self.scratch.clone(), self.group.take());
        let (zygote, sched) = (self.zygote.clone(), self.sched.clone());
        // Waiting for a process to stop and die is not something to do on
        // whichever thread happened to drop the future.
        let work = move || {
            // A compiler killed in the middle of code generation takes the
            // jobserver tokens of its helper threads with it. Stop it
            // first, count them, and put them back once it is dead.
            let mut held = 0;
            if let (Some(_), Some(g)) = (&running, &group) {
                g.freeze();
                held = g.helper_tokens();
            }
            if let Some(r) = running {
                zygote.kill(r.id);
                let _ = r.done.blocking_recv();
            }
            if held > 0 {
                sched.restore(held);
            }
            let _ = fs.finish_action(&action);
            drop(group);
            let _ = std::fs::remove_dir_all(&scratch);
        };
        if std::thread::Builder::new().name("cancelled-action".into()).spawn(work).is_err() {
            eprintln!("rargod: could not start a thread to clean up a cancelled action");
        }
    }
}

/// What runs of the same kind share: the program, and for a compiler what it
/// produces (a compile that links reads the standard library's object code
/// and runs a linker; one that makes a library does neither).
fn profile_key(spec: &ActionSpec) -> Option<Vec<u8>> {
    let mut key = spec.argv.first()?.clone();
    let mut it = spec.argv.iter();
    while let Some(a) = it.next() {
        if a == b"--crate-type" {
            if let Some(t) = it.next() {
                key.push(0);
                key.extend_from_slice(t);
            }
        }
    }
    Some(key)
}

fn default_mem_kb(kind: ActionKind) -> u64 {
    match kind {
        ActionKind::Rustc => 700 * 1024,
        ActionKind::Rustdoc | ActionKind::Doctest => 700 * 1024,
        ActionKind::Test => 400 * 1024,
        ActionKind::BuildScript => 200 * 1024,
        ActionKind::Plan => 300 * 1024,
        ActionKind::Other => 300 * 1024,
    }
}

fn pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: fds is a valid two-element array.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: both fds were just created.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn async_reader(fd: OwnedFd) -> std::io::Result<tokio::net::unix::pipe::Receiver> {
    use std::os::fd::AsRawFd;
    // SAFETY: setting O_NONBLOCK on our own read end.
    unsafe {
        let fl = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, fl | libc::O_NONBLOCK);
    }
    tokio::net::unix::pipe::Receiver::from_owned_fd(fd)
}

impl Engine {
    pub fn new(
        store: Arc<Store>,
        content: Arc<Content>,
        fs: Arc<SandboxFs>,
        mnt: PathBuf,
        zygote: Arc<Zygote>,
        sched: Arc<Scheduler>,
    ) -> Engine {
        let traces = TraceCache::new(store.db().clone());
        Engine {
            store,
            content,
            fs,
            mnt,
            zygote,
            traces,
            sched,
            inflight: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(1),
            avg_wall_us: AtomicU64::new(0),
            profiles: dashmap::DashMap::new(),
            remote: std::sync::OnceLock::new(),
            stats: EngineStats::default(),
            cgroup_root: None,
        }
    }

    /// The spec with the client's values for every variable actions under
    /// this key have declared an interest in.
    fn effective_spec(&self, base: &ActionSpec, client_env: &ClientEnv) -> (ActionSpec, Hash) {
        let base_key = base.weak_key();
        let needs = self.traces.needs_env(&base_key);
        if needs.is_empty() {
            return (base.clone(), base_key);
        }
        let mut spec = base.clone();
        for v in needs {
            if spec.env.contains_key(v.as_bytes()) {
                continue;
            }
            if let Some(val) = client_env.get(&v) {
                spec.env.insert(v.into_bytes(), val.into_bytes());
            }
        }
        (spec, base_key)
    }

    /// A digest of what the newest trace under `key` says the action reads,
    /// evaluated in `view`. Two requests with the same digest are expected
    /// to produce the same result.
    fn prediction(&self, key: &Hash, view: &dyn View) -> Hash {
        let traces = self.traces.traces(key);
        let Some((_, t)) = traces.first() else { return Hash::ZERO };
        let mut h = KeyHasher::new("prediction");
        for (path, obs) in &t.obs {
            h.field(path);
            if let Obs::Listing(_) = obs {
                match view.list(path) {
                    Ok(l) => h.field(&listing_hash(&l).0),
                    Err(_) => h.field(b"!"),
                };
                continue;
            }
            match view.stat(path) {
                Looked::Absent => h.field(b"a"),
                Looked::Other => h.field(b"o"),
                Looked::Dir(_) => h.field(b"d"),
                Looked::Symlink(t) => h.field(b"l").field(&t),
                Looked::File(f) if f.sem.is_some() => h.field(b"s").field(&f.sem.unwrap().0),
                Looked::File(f) => {
                    h.field(b"f").u64(f.exec() as u64);
                    match (self.content.known_hash(&f.src), &f.src) {
                        (Some(hash), _) => h.field(&hash.0),
                        (None, FileSrc::Host { sig, .. }) => h.field(&bincode::serialize(sig).unwrap()),
                        (None, FileSrc::Blob(b)) => h.field(&b.0),
                        (None, FileSrc::Lazy(_)) => h.field(b"?"),
                    }
                }
            };
        }
        h.finish()
    }

    /// A human-readable reason why no trace under `key` holds in `view`.
    fn explain_miss(&self, key: &Hash, view: &dyn View, no_cache: bool) -> String {
        if no_cache {
            return "caching was turned off for it".into();
        }
        let traces = self.traces.traces(key);
        let Some((_, newest)) = traces.first() else {
            return "this command line and environment have not been run before".into();
        };
        match newest.first_mismatch(view, &self.content) {
            Some((path, was)) => {
                let now = match was {
                    Obs::Listing(_) => match view.list(path) {
                        Ok(l) => format!("{} entries", l.len()),
                        Err(_) => "not a directory".into(),
                    },
                    _ => match view.stat(path) {
                        Looked::Absent => "absent".into(),
                        Looked::Dir(_) => "a directory".into(),
                        Looked::Symlink(t) => format!("a symlink to {}", show(&t)),
                        Looked::Other => "a special file".into(),
                        Looked::File(f) => match self.content.known_hash(&f.src) {
                            Some(h) => format!("a file of {} bytes ({h:?})", f.size),
                            None => format!("a file of {} bytes", f.size),
                        },
                    },
                };
                format!("{} was {:?}, is now {now} ({} recorded run(s))", show(path), was, traces.len())
            }
            None => "its recorded outputs are no longer in the store".into(),
        }
    }

    fn replay(&self, result: &ActionResult, observer: &dyn Observer) {
        if let Ok(out) = self.store.read(&result.stdout) {
            if !out.is_empty() {
                observer.stdout(&out);
            }
        }
        if let Ok(err) = self.store.read(&result.stderr) {
            for line in err.split(|&c| c == b'\n') {
                if !line.is_empty() {
                    observer.stderr_line(line, None);
                }
            }
        }
    }

    /// The recorded result of an action, if one holds for this view. Never
    /// runs anything.
    pub fn cached(&self, req: &Request) -> Option<Outcome> {
        let (spec, base_key) = self.effective_spec(&req.spec, &req.client_env);
        let key = if spec.env.len() == req.spec.env.len() { base_key } else { spec.weak_key() };
        let t = self.traces.lookup(&key, &*req.view, &self.content)?;
        self.replay(&t.result, &*req.observer);
        Some(Outcome { result: Arc::new(t.result.clone()), cached: true, side: Vec::new() })
    }

    /// The key the action's traces are stored under (its declared
    /// environment variables folded in).
    pub fn effective_key(&self, spec: &ActionSpec, client_env: &ClientEnv) -> Hash {
        let (eff, base_key) = self.effective_spec(spec, client_env);
        if eff.env.len() == spec.env.len() { base_key } else { eff.weak_key() }
    }

    /// Obtains the result of an action.
    pub async fn obtain(&self, req: Request) -> Result<Outcome> {
        let mut announced = false;
        let mut waited: Option<(Hash, Hash)> = None;
        loop {
            // Variables actions under this key declared: the client's
            // values must be known before the key can be formed.
            req.client_env.ensure(&self.traces.needs_env(&req.spec.weak_key())).await?;
            let (spec, base_key) = self.effective_spec(&req.spec, &req.client_env);
            let key = if spec.env.len() == req.spec.env.len() { base_key } else { spec.weak_key() };

            if !req.no_cache {
                let view = req.view.clone();
                let hit = self.traces.lookup(&key, &*view, &self.content);
                if let Some(t) = hit {
                    self.replay(&t.result, &*req.observer);
                    return Ok(Outcome { result: Arc::new(t.result.clone()), cached: true, side: Vec::new() });
                }
            }
            if !announced {
                req.observer.miss();
                announced = true;
                if req.observer.wants_explanation() {
                    req.observer.explain(&self.explain_miss(&key, &*req.view, req.no_cache));
                }
            }

            // Someone else may already be computing exactly this.
            let pred = self.prediction(&key, &*req.view);
            let slot = (key, pred);
            let guard = {
                let mut inflight = self.inflight.lock();
                match inflight.get(&slot) {
                    Some(rx) if !req.no_cache && waited != Some(slot) => Err(rx.clone()),
                    _ => {
                        let (tx, rx) = tokio::sync::watch::channel(false);
                        // If another run occupies the slot (we already
                        // waited on it once, or caching is off), ours simply
                        // replaces the advertisement.
                        inflight.insert(slot, rx);
                        Ok(InflightGuard { engine: self, key: slot, tx })
                    }
                }
            };
            let guard = match guard {
                Err(mut rx) => {
                    self.stats.joined.fetch_add(1, Ordering::Relaxed);
                    // Wait for the other run to finish, then look again. If
                    // its trace does not hold for us, the next round runs.
                    while !*rx.borrow() {
                        if rx.changed().await.is_err() {
                            break;
                        }
                    }
                    waited = Some(slot);
                    continue;
                }
                Ok(g) => g,
            };
            if waited.is_some() {
                self.stats.joined_then_ran.fetch_add(1, Ordering::Relaxed);
            }

            let mut attempt = 0;
            let (result, obs, side) = loop {
                attempt += 1;
                let (result, obs, tainted, side) = self.run(&spec, &req).await?;
                if !tainted {
                    break (result, Some(obs), side);
                }
                self.stats.tainted.fetch_add(1, Ordering::Relaxed);
                if attempt >= 3 {
                    // Inputs keep changing under the action; return what we
                    // have without recording it.
                    break (result, None, side);
                }
            };
            let result = Arc::new(result);
            if let Some(obs) = obs {
                if (req.cacheable)(&result) {
                    let stored = self.traces.insert(
                        &key,
                        Trace {
                            obs,
                            result: (*result).clone(),
                            created: std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs())
                                .unwrap_or(0),
                        },
                    );
                    if let Some(k) = profile_key(&spec) {
                        self.profiles.insert(k, stored);
                    }
                }
            }
            drop(guard);

            // Did the action tell us about environment variables it reads
            // that we did not give it?
            let declared = (req.declared_env)(&result, &self.store);
            if !declared.is_empty() {
                let added = self.traces.add_needs_env(&base_key, &declared);
                req.client_env.ensure(&added).await?;
                let relevant = added
                    .iter()
                    .any(|v| !spec.env.contains_key(v.as_bytes()) && req.client_env.contains_key(v));
                if relevant {
                    self.stats.env_reruns.fetch_add(1, Ordering::Relaxed);
                    waited = None;
                    continue;
                }
            }
            return Ok(Outcome { result, cached: false, side });
        }
    }

    pub(crate) fn estimate_mem_kb(&self, key: &Hash, kind: ActionKind) -> u64 {
        match self.traces.traces(key).first() {
            Some((_, t)) if t.result.stats.max_rss_kb > 0 => t.result.stats.max_rss_kb,
            _ => default_mem_kb(kind),
        }
    }

    /// Runs the action once, here or on a worker.
    async fn run(
        &self,
        spec: &ActionSpec,
        req: &Request,
    ) -> Result<(ActionResult, Vec<(VPath, Obs)>, bool, Vec<(Vec<u8>, MEntry)>)> {
        let key = spec.weak_key();
        if let Some(remote) = self.remote.get() {
            if let Some(r) = remote.try_run(self, spec, &key, req).await {
                return r;
            }
        }
        // Bytes can cross the link while the action waits for a core.
        self.seed_home(req).await?;
        let permit = self.sched.acquire(req.prio, self.estimate_mem_kb(&key, spec.kind)).await;
        self.stats.runs.fetch_add(1, Ordering::Relaxed);
        let started = std::time::Instant::now();
        let r = self.execute(spec, req.view.clone(), req.uid, req.gid, &*req.observer).await;
        drop(permit);
        self.note_action_wall(started.elapsed());
        r
    }

    /// The files of the request's seed that are not in the store.
    pub fn seed_away(&self, req: &Request) -> Vec<(Hash, u64)> {
        req.seed_blobs.iter().filter(|(h, _)| !self.store.has(h)).copied().collect()
    }

    /// Makes sure the request's seed is in the store, as an action that
    /// runs here needs it: what is still on a worker is fetched now.
    pub async fn seed_home(&self, req: &Request) -> Result<()> {
        let away: Vec<Hash> = self.seed_away(req).into_iter().map(|(h, _)| h).collect();
        if away.is_empty() {
            return Ok(());
        }
        let fetched = match self.remote.get() {
            Some(remote) => remote.bring_home(&away).await,
            None => Err(anyhow::anyhow!("no worker is connected")),
        };
        fetched.map_err(|e| {
            eprintln!("rargod: `{}`: its incremental state could not be completed: {e:#}", req.spec.label);
            SeedLost.into()
        })
    }

    /// A trace of some other run of the same program, if any.
    pub fn profile(&self, spec: &ActionSpec) -> Option<Arc<Trace>> {
        self.profiles.get(&profile_key(spec)?).map(|t| t.clone())
    }

    pub(crate) fn note_action_wall(&self, d: std::time::Duration) {
        // Exponentially weighted average, for estimating queue waits.
        let old = self.avg_wall_us.load(Ordering::Relaxed);
        let new = if old == 0 { d.as_micros() as u64 } else { (old * 7 + d.as_micros() as u64) / 8 };
        self.avg_wall_us.store(new, Ordering::Relaxed);
    }

    /// Average wall time of recent local actions.
    pub fn avg_action_wall(&self) -> std::time::Duration {
        std::time::Duration::from_micros(self.avg_wall_us.load(Ordering::Relaxed).max(200_000))
    }

    /// Runs the action in a sandbox on this machine, without consulting the
    /// scheduler or the cache.
    pub async fn execute(
        &self,
        spec: &ActionSpec,
        view: Arc<dyn View>,
        uid: u32,
        gid: u32,
        observer: &dyn Observer,
    ) -> Result<(ActionResult, Vec<(VPath, Obs)>, bool, Vec<(Vec<u8>, MEntry)>)> {
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        let scratch = self.store.tmp_dir(&format!("act-{}-{n}", std::process::id()))?;
        let action = self.fs.create_action(ActionFsConfig {
            view,
            uid,
            gid,
            scratch: scratch.clone(),
            mountpoints: vec![b"proc".to_vec(), b"dev".to_vec(), b"sys".to_vec(), b"tmp".to_vec()],
            // cargo checks that target source files exist while planning but
            // never acts on their size; recording sizes would re-plan after
            // every edit to a `lib.rs`.
            size_blind: spec.kind == ActionKind::Plan,
        });
        // From here on the action directory must be torn down on every path,
        // including the one where nobody waits for the result any more.
        let mut teardown = Teardown {
            fs: self.fs.clone(),
            sched: self.sched.clone(),
            zygote: self.zygote.clone(),
            action: action.clone(),
            scratch: scratch.clone(),
            running: None,
            group: None,
            armed: true,
        };
        let run = self.run_in(spec, uid, gid, observer, &action, &mut teardown).await;
        teardown.armed = false;
        drop(teardown);
        let fin = {
            let fs = self.fs.clone();
            let a = action.clone();
            tokio::task::spawn_blocking(move || fs.finish_action(&a)).await?
        };
        let (info, stdout, stderr) = match run {
            Ok(v) => v,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&scratch);
                return Err(e);
            }
        };

        // Turn the upper layer into a manifest, moving files into the store.
        let store = self.store.clone();
        let content = self.content.clone();
        let upper = fin.upper;
        let (entries, out_hash, err_hash) = tokio::task::spawn_blocking(move || -> Result<_> {
            use rayon::prelude::*;
            // Several names may be hard links to one written file (rustc
            // links its incremental `metadata.rmeta` to the output `.rmeta`):
            // store each scratch file once.
            let mut scratch: Vec<(PathBuf, bool)> = upper
                .iter()
                .filter_map(|u| match &u.kind {
                    UpperKind::File { path, exec } => Some((path.clone(), *exec)),
                    _ => None,
                })
                .collect();
            scratch.sort();
            scratch.dedup_by(|a, b| a.0 == b.0);
            let stored: HashMap<PathBuf, (Hash, u64)> = scratch
                .into_par_iter()
                .map(|(path, exec)| -> Result<(PathBuf, (Hash, u64))> {
                    let r = store
                        .put_file_move(&path, exec)
                        .with_context(|| format!("storing output file {}", path.display()))?;
                    Ok((path, r))
                })
                .collect::<Result<_>>()?;
            let entries: Vec<(Vec<u8>, MEntry)> = upper
                .into_par_iter()
                .map(|u| -> Result<(Vec<u8>, MEntry)> {
                    let e = match u.kind {
                        UpperKind::Dir => MEntry::Dir,
                        UpperKind::Whiteout => MEntry::Whiteout,
                        UpperKind::Symlink(t) => MEntry::Symlink(t),
                        UpperKind::File { path, exec } => {
                            let (hash, size) = stored[&path];
                            MEntry::File { hash, exec, size }
                        }
                        UpperKind::LowerFile { info } => {
                            let hash = content.hash(&info.src)?;
                            if !store.has(&hash) {
                                let (f, _, _) = content.open(&info.src)?;
                                store.put_file_copy(&f, info.exec())?;
                            }
                            MEntry::File { hash, exec: info.exec(), size: info.size }
                        }
                    };
                    Ok((u.path, e))
                })
                .collect::<Result<_>>()?;
            let out_hash = store.put_bytes(&stdout)?;
            let err_hash = store.put_bytes(&stderr)?;
            Ok((entries, out_hash, err_hash))
        })
        .await??;
        let _ = std::fs::remove_dir_all(&scratch);

        let mut entries = entries;
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        // Paths under a discard prefix are not part of the result.
        let (side, entries): (Vec<_>, Vec<_>) =
            entries.into_iter().partition(|(p, _)| spec.discard.iter().any(|d| vpath_starts_with(p, d)));
        let stats = Stats { wall_us: info.wall_us, cpu_us: info.user_us + info.sys_us, max_rss_kb: info.max_rss_kb };
        self.stats.run_cpu_us.fetch_add(stats.cpu_us, Ordering::Relaxed);
        self.stats.run_wall_us.fetch_add(stats.wall_us, Ordering::Relaxed);
        let result = ActionResult {
            status: info.status,
            stdout: out_hash,
            stderr: err_hash,
            outputs: Arc::new(Manifest { entries }),
            stats,
        };
        Ok((result, fin.obs, fin.tainted, side))
    }

    async fn run_in(
        &self,
        spec: &ActionSpec,
        uid: u32,
        gid: u32,
        observer: &dyn Observer,
        action: &Arc<ActionFs>,
        teardown: &mut Teardown,
    ) -> Result<(crate::sandbox::ExitInfo, Vec<u8>, Vec<u8>)> {
        let Teardown { running: slot, group: group_slot, .. } = teardown;
        for d in &spec.mkdirs {
            action
                .mkdir_p(d)
                .map_err(|e| anyhow::anyhow!("creating {}: errno {}", show(d), e.0))?;
        }
        let (out_r, out_w) = pipe()?;
        let (err_r, err_w) = pipe()?;
        let mut env: BTreeMap<Vec<u8>, Vec<u8>> = spec.env.clone();
        // The jobserver is how the action shares the machine; it is not an
        // input, so it is added here and not part of the key.
        let js = format!(
            "-j --jobserver-fds={},{} --jobserver-auth={},{}",
            EXTRA_FD_BASE,
            EXTRA_FD_BASE + 1,
            EXTRA_FD_BASE,
            EXTRA_FD_BASE + 1
        );
        for var in ["CARGO_MAKEFLAGS", "MAKEFLAGS", "MFLAGS"] {
            env.insert(var.as_bytes().to_vec(), js.clone().into_bytes());
        }
        *group_slot = self.cgroup_root.as_ref().and_then(|r| crate::cgroup::ActionGroup::create(r, action.seq));
        let group = group_slot.as_ref();
        let sreq = SpawnReq {
            root: self.mnt.join(SandboxFs::dir_name(action.seq)),
            argv: spec.argv.clone(),
            env: env.into_iter().collect(),
            cwd: spec.cwd.clone(),
            uid,
            gid,
            net: spec.net,
            binds: vec![],
            cgroup: group.as_ref().map(|g| g.dir.clone()),
        };
        // The process is kept where the caller can reach it: if this future
        // is dropped (the client went away, another copy of the action won),
        // the process is killed instead of running on unobserved.
        let running = slot.insert(self.zygote.spawn(sreq, None, out_w, err_w, &self.sched.jobserver_fds())?);
        let mut out_rd = async_reader(out_r)?;
        let mut err_rd = async_reader(err_r)?;
        let live = LiveCtl { action, store: &self.store };

        let read_out = async {
            let mut all = Vec::new();
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match out_rd.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        observer.stdout(&buf[..n]);
                        all.extend_from_slice(&buf[..n]);
                    }
                }
            }
            all
        };
        let read_err = async {
            let mut all: Vec<u8> = Vec::new();
            let mut done = 0usize;
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match err_rd.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        all.extend_from_slice(&buf[..n]);
                        while let Some(i) = all[done..].iter().position(|&c| c == b'\n') {
                            let line = &all[done..done + i];
                            if !line.is_empty() {
                                observer.stderr_line(line, Some(&live));
                            }
                            done += i + 1;
                        }
                    }
                }
            }
            if done < all.len() {
                observer.stderr_line(&all[done..], Some(&live));
            }
            all
        };
        // While it runs, watch how much memory all its processes hold.
        let peak_kb = std::sync::atomic::AtomicU64::new(0);
        let watch = async {
            let Some(g) = group else { return std::future::pending::<()>().await };
            let mut every = tokio::time::interval(std::time::Duration::from_millis(100));
            loop {
                every.tick().await;
                peak_kb.fetch_max(g.anon_kb(), Ordering::Relaxed);
            }
        };
        let (stdout, stderr, info) = tokio::select! {
            r = async { tokio::join!(read_out, read_err, &mut running.done) } => r,
            _ = watch => unreachable!("the memory watch never finishes"),
        };
        let mut info = info.context("the sandbox helper process died")?;
        if let Some(g) = group {
            let (user, sys) = g.cpu_us();
            if user + sys > 0 {
                info.user_us = user;
                info.sys_us = sys;
            }
            info.max_rss_kb = info.max_rss_kb.max(peak_kb.load(Ordering::Relaxed));
        }
        *slot = None;
        *group_slot = None;
        if info.setup_failed {
            bail!(
                "could not start `{}`: {}",
                spec.label,
                String::from_utf8_lossy(&stderr).trim()
            );
        }
        Ok((info, stdout, stderr))
    }
}
