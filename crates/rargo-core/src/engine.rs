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

pub trait Observer: Send + Sync {
    fn miss(&self) {}
    fn wants_explanation(&self) -> bool {
        false
    }
    fn explain(&self, _why: &str) {}
    fn stdout(&self, _data: &[u8]) {}
    fn stderr_line(&self, _line: &[u8], _live: Option<&dyn Live>) {}
}

pub struct NullObserver;
impl Observer for NullObserver {}

pub trait Live {
    fn ingest(&self, path: &[u8]) -> Option<MEntry>;
}

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

pub type EnvAsker = Arc<
    dyn Fn(Vec<String>) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<(String, Option<String>)>>> + Send>>
        + Send
        + Sync,
>;

pub struct ClientEnv {
    vars: parking_lot::RwLock<HashMap<String, String>>,
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
    pub client_env: Arc<ClientEnv>,
    pub uid: u32,
    pub gid: u32,
    pub prio: Priority,
    pub no_cache: bool,
    pub cacheable: Arc<dyn Fn(&ActionResult) -> bool + Send + Sync>,
    pub declared_env: Arc<dyn Fn(&ActionResult, &Store) -> Vec<String> + Send + Sync>,
    pub observer: Arc<dyn Observer>,
    pub seed_blobs: Arc<Vec<(Hash, u64)>>,
}

#[derive(Debug)]
pub struct SeedLost;

impl std::fmt::Display for SeedLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("files of the incremental state are no longer available")
    }
}

impl std::error::Error for SeedLost {}

pub trait RemoteRunner: Send + Sync {
    fn holds(&self, blob: &Hash) -> bool;

    fn offer_state(&self, _whose: (Hash, String), _blobs: Arc<Vec<(Hash, u64)>>) {}

    fn bring_home<'a>(
        &'a self,
        blobs: &'a [Hash],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>>;

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
    pub cached: bool,
    pub side: Vec<(Vec<u8>, MEntry)>,
}

#[derive(Default)]
pub struct EngineStats {
    pub runs: AtomicU64,
    pub runs_by_kind: [AtomicU64; 8],
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
    inflight: Mutex<HashMap<(Hash, Hash), tokio::sync::watch::Receiver<Flight>>>,
    seq: AtomicU64,
    avg_wall_us: AtomicU64,
    profiles: dashmap::DashMap<Vec<u8>, Arc<Trace>>,
    pub remote: std::sync::OnceLock<Arc<dyn RemoteRunner>>,
    pub stats: EngineStats,
    pub cgroup_root: Option<PathBuf>,
    pub action_timeout: std::time::Duration,
}

#[derive(Clone)]
enum Flight {
    Running,
    Done,
    TimedOut(Arc<str>),
}

#[derive(Debug)]
pub struct ActionTimedOut {
    label: String,
    secs: u64,
}

impl std::fmt::Display for ActionTimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "`{}` ran longer than {} s and was stopped", self.label, self.secs)
    }
}

impl std::error::Error for ActionTimedOut {}

struct InflightGuard<'a> {
    engine: &'a Engine,
    key: (Hash, Hash),
    tx: tokio::sync::watch::Sender<Flight>,
    end: Flight,
}

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        self.engine.inflight.lock().remove(&self.key);
        let _ = self.tx.send(self.end.clone());
    }
}

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
        let work = move || {
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
        ActionKind::Test | ActionKind::Nextest => 400 * 1024,
        ActionKind::BuildScript => 200 * 1024,
        ActionKind::Plan => 300 * 1024,
        ActionKind::Other => 300 * 1024,
    }
}

fn pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn async_reader(fd: OwnedFd) -> std::io::Result<tokio::net::unix::pipe::Receiver> {
    use std::os::fd::AsRawFd;
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
            action_timeout: std::time::Duration::from_secs(3600),
        }
    }

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
        assert!(spec.env.len() >= base.env.len());
        assert!(base.env.keys().all(|k| spec.env.get(k) == base.env.get(k)), "declared variables never override the spec");
        (spec, base_key)
    }

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

    pub fn cached(&self, req: &Request) -> Option<Outcome> {
        let (spec, base_key) = self.effective_spec(&req.spec, &req.client_env);
        let key = if spec.env.len() == req.spec.env.len() { base_key } else { spec.weak_key() };
        let t = self.traces.lookup(&key, &*req.view, &self.content)?;
        self.replay(&t.result, &*req.observer);
        Some(Outcome { result: Arc::new(t.result.clone()), cached: true, side: Vec::new() })
    }

    pub fn effective_key(&self, spec: &ActionSpec, client_env: &ClientEnv) -> Hash {
        let (eff, base_key) = self.effective_spec(spec, client_env);
        if eff.env.len() == spec.env.len() { base_key } else { eff.weak_key() }
    }

    pub async fn obtain(&self, req: Request) -> Result<Outcome> {
        let mut announced = false;
        let mut waited: Option<(Hash, Hash)> = None;
        loop {
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

            let pred = self.prediction(&key, &*req.view);
            let slot = (key, pred);
            let guard = {
                let mut inflight = self.inflight.lock();
                match inflight.get(&slot) {
                    Some(rx) if !req.no_cache && waited != Some(slot) => Err(rx.clone()),
                    _ => {
                        let (tx, rx) = tokio::sync::watch::channel(Flight::Running);
                        inflight.insert(slot, rx);
                        Ok(InflightGuard { engine: self, key: slot, tx, end: Flight::Done })
                    }
                }
            };
            let mut guard = match guard {
                Err(mut rx) => {
                    self.stats.joined.fetch_add(1, Ordering::Relaxed);
                    while matches!(*rx.borrow(), Flight::Running) {
                        if rx.changed().await.is_err() {
                            break;
                        }
                    }
                    let ended = rx.borrow().clone();
                    if let Flight::TimedOut(why) = ended {
                        bail!("{why}");
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
                assert!(attempt <= 3);
                let ran = self.run(&spec, &req).await;
                let (result, obs, tainted, side) = match ran {
                    Ok(v) => v,
                    Err(e) => {
                        if let Some(t) = e.downcast_ref::<ActionTimedOut>() {
                            guard.end = Flight::TimedOut(Arc::from(t.to_string()));
                        }
                        return Err(e);
                    }
                };
                if !tainted {
                    break (result, Some(obs), side);
                }
                self.stats.tainted.fetch_add(1, Ordering::Relaxed);
                if attempt >= 3 {
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
        self.seed_home(req).await?;
        let permit = self.sched.acquire(req.prio, self.estimate_mem_kb(&key, spec.kind)).await;
        self.stats.runs.fetch_add(1, Ordering::Relaxed);
        self.stats.runs_by_kind[spec.kind as usize].fetch_add(1, Ordering::Relaxed);
        let started = std::time::Instant::now();
        let r = self.execute(spec, req.view.clone(), req.uid, req.gid, &*req.observer).await;
        drop(permit);
        self.note_action_wall(started.elapsed());
        static LOG_ACTIONS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *LOG_ACTIONS.get_or_init(|| std::env::var_os("RARGO_LOG_ACTIONS").is_some()) {
            let outcome = match &r {
                Ok((result, _, tainted, _)) => format!("exit={:?} tainted={tainted}", result.code()),
                Err(e) => format!("error={e:#}"),
            };
            eprintln!("rargod: action {:?} [{}] {outcome}", spec.kind, spec.label);
        }
        r
    }

    pub fn seed_away(&self, req: &Request) -> Vec<(Hash, u64)> {
        req.seed_blobs.iter().filter(|(h, _)| !self.store.has(h)).copied().collect()
    }

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

    pub fn profile(&self, spec: &ActionSpec) -> Option<Arc<Trace>> {
        self.profiles.get(&profile_key(spec)?).map(|t| t.clone())
    }

    pub(crate) fn note_action_wall(&self, d: std::time::Duration) {
        let old = self.avg_wall_us.load(Ordering::Relaxed);
        let new = if old == 0 { d.as_micros() as u64 } else { (old * 7 + d.as_micros() as u64) / 8 };
        self.avg_wall_us.store(new, Ordering::Relaxed);
    }

    pub fn avg_action_wall(&self) -> std::time::Duration {
        std::time::Duration::from_micros(self.avg_wall_us.load(Ordering::Relaxed).max(200_000))
    }

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
            size_blind: spec.kind == ActionKind::Plan,
        });
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

        let store = self.store.clone();
        let content = self.content.clone();
        let upper = fin.upper;
        let (entries, out_hash, err_hash) = tokio::task::spawn_blocking(move || -> Result<_> {
            use rayon::prelude::*;
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
        assert!(entries.windows(2).all(|w| w[0].0 < w[1].0), "each output path appears once");
        let total = entries.len();
        let (side, entries): (Vec<_>, Vec<_>) =
            entries.into_iter().partition(|(p, _)| spec.discard.iter().any(|d| vpath_starts_with(p, d)));
        assert_eq!(side.len() + entries.len(), total);
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
        assert!(slot.is_none(), "one process per action");
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
                            assert!(done <= all.len());
                        }
                    }
                }
            }
            if done < all.len() {
                observer.stderr_line(&all[done..], Some(&live));
            }
            all
        };
        let peak_kb = std::sync::atomic::AtomicU64::new(0);
        let watch = async {
            let Some(g) = group else { return std::future::pending::<()>().await };
            let mut every = tokio::time::interval(std::time::Duration::from_millis(100));
            loop {
                every.tick().await;
                peak_kb.fetch_max(g.anon_kb(), Ordering::Relaxed);
            }
        };
        let id = running.id;
        let (stdout, stderr, info) = tokio::select! {
            r = async { tokio::join!(read_out, read_err, &mut running.done) } => r,
            _ = watch => unreachable!("the memory watch never finishes"),
            _ = tokio::time::sleep(self.action_timeout) => {
                self.zygote.kill(id);
                return Err(ActionTimedOut { label: spec.label.clone(), secs: self.action_timeout.as_secs() }.into());
            }
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
