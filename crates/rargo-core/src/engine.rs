use crate::action_table::{ActionTable, Failure, Ran, ReqId, Start, Woken};
use crate::hash::{Hash, KeyHasher};
use crate::sandbox::{HostLayout, Place, SetupSpec, Zygote};
use crate::sandboxfs::{ActionFs, ActionFsConfig, SandboxFs, UpperKind};
use crate::sched::{Priority, Scheduler};
use crate::store::Store;
use crate::trace::*;
use crate::view::*;
use crate::io;
use anyhow::{Context, Result, bail};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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

pub type EnvAsker = Arc<dyn Fn(Vec<String>) -> Result<Vec<(String, Option<String>)>> + Send + Sync>;

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

    pub fn ensure(&self, names: &[String]) -> Result<()> {
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
        let answers = asker(unknown.clone())?;
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

    fn bring_home(&self, blobs: &[Hash]) -> Result<()>;

    fn try_run(
        &self,
        engine: &Engine,
        spec: &ActionSpec,
        key: &Hash,
        req: &Request,
    ) -> Option<Result<(ActionResult, Vec<(VPath, Obs)>, bool, Vec<(Vec<u8>, MEntry)>)>>;
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
    actions: Mutex<ActionTable>,
    flight_ended: io::watch::Sender<()>,
    seq: AtomicU64,
    avg_wall_us: AtomicU64,
    profiles: dashmap::DashMap<Vec<u8>, Arc<Trace>>,
    pub remote: std::sync::OnceLock<Arc<dyn RemoteRunner>>,
    pub stats: EngineStats,
    pub cgroup_root: Option<PathBuf>,
    pub place: Place,
    pub action_timeout: std::time::Duration,
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

struct Requester<'a> {
    engine: &'a Engine,
    id: ReqId,
}

impl Drop for Requester<'_> {
    fn drop(&mut self) {
        let woke = self.engine.actions.lock().cancel(self.id);
        if !woke.is_empty() {
            let _ = self.engine.flight_signal().send_replace(());
        }
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
        let (fs, action, scratch, group) =
            (self.fs.clone(), self.action.clone(), self.scratch.clone(), self.group.take());
        let (zygote, sched) = (self.zygote.clone(), self.sched.clone());
        drop(io::spawn_off(move || {
            let held = match (&running, &group) {
                (Some(_), Some(g)) => io::cpu(|| {
                    g.freeze();
                    g.helper_tokens()
                }),
                _ => 0,
            };
            if let Some(r) = running {
                zygote.kill(r.id);
                if let Some(rx) = r.done {
                    let _ = rx.recv();
                }
            }
            if held > 0 {
                sched.restore(held);
            }
            io::cpu(move || {
                let _ = fs.finish_action(&action);
                drop(group);
                let _ = std::fs::remove_dir_all(&scratch);
            });
        }));
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

fn read_pipe(fd: &OwnedFd) -> std::io::Result<Vec<u8>> {
    io::read_at(fd.as_raw_fd(), 0, 64 * 1024)
}

#[derive(Clone, Copy)]
struct ObsPtr(*const dyn Observer);

unsafe impl Send for ObsPtr {}

fn read_stdout(fd: OwnedFd, obs: ObsPtr) -> Vec<u8> {
    let mut all = Vec::new();
    loop {
        let buf = match read_pipe(&fd) {
            Ok(buf) if !buf.is_empty() => buf,
            _ => break,
        };
        unsafe { (&*obs.0).stdout(&buf) };
        all.extend_from_slice(&buf);
    }
    all
}

fn read_stderr(fd: OwnedFd, obs: ObsPtr, action: Arc<ActionFs>, store: Arc<Store>) -> Vec<u8> {
    let mut all = Vec::new();
    let mut at = 0usize;
    loop {
        let buf = match read_pipe(&fd) {
            Ok(buf) if !buf.is_empty() => buf,
            _ => break,
        };
        all.extend_from_slice(&buf);
        while let Some(i) = all[at..].iter().position(|&c| c == b'\n') {
            let line = &all[at..at + i];
            if !line.is_empty() {
                let live = LiveCtl { action: &action, store: &store };
                unsafe { (&*obs.0).stderr_line(line, Some(&live)) };
            }
            at += i + 1;
            assert!(at <= all.len());
        }
    }
    if at < all.len() {
        let live = LiveCtl { action: &action, store: &store };
        unsafe { (&*obs.0).stderr_line(&all[at..], Some(&live)) };
    }
    all
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
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_micros() as u64).unwrap_or(1);
        let traces = TraceCache::new(store.db().clone(), seed);
        Engine {
            store,
            content,
            fs,
            mnt,
            zygote,
            traces,
            sched,
            actions: Mutex::new(ActionTable::new()),
            flight_ended: io::watch::channel(()).0,
            seq: AtomicU64::new(1),
            avg_wall_us: AtomicU64::new(0),
            profiles: dashmap::DashMap::new(),
            remote: std::sync::OnceLock::new(),
            stats: EngineStats::default(),
            cgroup_root: None,
            place: Place::Server,
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
        assert!(
            base.env.keys().all(|k| spec.env.get(k) == base.env.get(k)),
            "declared variables never override the spec"
        );
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

    fn flight_signal(&self) -> &io::watch::Sender<()> {
        &self.flight_ended
    }

    pub fn obtain(&self, req: Request) -> Result<Outcome> {
        let id = self.actions.lock().begin();
        let _requester = Requester { engine: self, id };
        let mut announced = false;
        loop {
            req.client_env.ensure(&self.traces.needs_env(&req.spec.weak_key()))?;
            let (spec, base_key) = self.effective_spec(&req.spec, &req.client_env);
            let key = if spec.env.len() == req.spec.env.len() { base_key } else { spec.weak_key() };

            let hit = if req.no_cache { None } else { self.traces.lookup(&key, &*req.view, &self.content) };
            if hit.is_none() && !announced {
                req.observer.miss();
                announced = true;
                if req.observer.wants_explanation() {
                    req.observer.explain(&self.explain_miss(&key, &*req.view, req.no_cache));
                }
            }
            let pred = if hit.is_some() { Hash::ZERO } else { self.prediction(&key, &*req.view) };
            let start = self.actions.lock().start(id, (key, pred), hit.is_some(), req.no_cache);
            match start {
                Start::Hit => {
                    let t = hit.expect("a hit was found");
                    self.replay(&t.result, &*req.observer);
                    return Ok(Outcome { result: Arc::new(t.result.clone()), cached: true, side: Vec::new() });
                }
                Start::Join => {
                    self.stats.joined.fetch_add(1, Ordering::Relaxed);
                    let mut ended = self.flight_signal().subscribe();
                    loop {
                        ended.borrow_and_update();
                        match self.actions.lock().woken(id) {
                            Woken::Waiting => {}
                            Woken::Retry => break,
                            Woken::Fail(why) => bail!("{why}"),
                        }
                        let _ = ended.changed();
                    }
                    continue;
                }
                Start::Run { after_wait } => {
                    if after_wait {
                        self.stats.joined_then_ran.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }

            let (result, obs, side) = loop {
                let (result, obs, tainted, side) = match self.run(&spec, &req) {
                    Ok(v) => v,
                    Err(e) => {
                        let why = if let Some(t) = e.downcast_ref::<ActionTimedOut>() {
                            Failure::TimedOut(Arc::from(t.to_string()))
                        } else if e.downcast_ref::<SeedLost>().is_some() {
                            Failure::SeedLost
                        } else {
                            Failure::Error
                        };
                        self.actions.lock().failed(id, why);
                        let _ = self.flight_signal().send_replace(());
                        return Err(e);
                    }
                };
                if tainted {
                    self.stats.tainted.fetch_add(1, Ordering::Relaxed);
                }
                let cacheable = !tainted && (req.cacheable)(&result);
                let ran = self.actions.lock().ran(id, tainted, cacheable);
                match ran {
                    Ran::Again => continue,
                    Ran::Record => break (result, Some(obs), side),
                    Ran::Keep => break (result, None, side),
                }
            };
            let result = Arc::new(result);
            if let Some(obs) = obs {
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
            self.actions.lock().settled(id);
            let _ = self.flight_signal().send_replace(());

            let declared = (req.declared_env)(&result, &self.store);
            if !declared.is_empty() {
                let added = self.traces.add_needs_env(&base_key, &declared);
                req.client_env.ensure(&added)?;
                let relevant =
                    added.iter().any(|v| !spec.env.contains_key(v.as_bytes()) && req.client_env.contains_key(v));
                if relevant {
                    self.stats.env_reruns.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            }
            self.actions.lock().done(id);
            return Ok(Outcome { result, cached: false, side });
        }
    }

    pub(crate) fn estimate_mem_kb(&self, key: &Hash, kind: ActionKind) -> u64 {
        match self.traces.traces(key).first() {
            Some((_, t)) if t.result.stats.max_rss_kb > 0 => t.result.stats.max_rss_kb,
            _ => default_mem_kb(kind),
        }
    }

    fn run(
        &self,
        spec: &ActionSpec,
        req: &Request,
    ) -> Result<(ActionResult, Vec<(VPath, Obs)>, bool, Vec<(Vec<u8>, MEntry)>)> {
        let key = spec.weak_key();
        if let Some(remote) = self.remote.get() {
            if let Some(r) = remote.try_run(self, spec, &key, req) {
                return r;
            }
        }
        self.seed_home(req)?;
        let permit = self.sched.acquire(req.prio, self.estimate_mem_kb(&key, spec.kind));
        self.stats.runs.fetch_add(1, Ordering::Relaxed);
        self.stats.runs_by_kind[spec.kind as usize].fetch_add(1, Ordering::Relaxed);
        let started = std::time::Instant::now();
        let r = self.execute(spec, req.view.clone(), req.uid, req.gid, &*req.observer);
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

    pub fn seed_home(&self, req: &Request) -> Result<()> {
        let away: Vec<Hash> = self.seed_away(req).into_iter().map(|(h, _)| h).collect();
        if away.is_empty() {
            return Ok(());
        }
        let fetched = match self.remote.get() {
            Some(remote) => remote.bring_home(&away),
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

    pub(crate) fn run_local(
        &self,
        spec: &ActionSpec,
        req: &Request,
        observer: &dyn Observer,
    ) -> Result<(ActionResult, Vec<(VPath, Obs)>, bool, Vec<(Vec<u8>, MEntry)>)> {
        self.stats.runs.fetch_add(1, Ordering::Relaxed);
        let started = std::time::Instant::now();
        let r = self.execute(spec, req.view.clone(), req.uid, req.gid, observer);
        self.note_action_wall(started.elapsed());
        r
    }

    pub fn avg_action_wall(&self) -> std::time::Duration {
        std::time::Duration::from_micros(self.avg_wall_us.load(Ordering::Relaxed).max(200_000))
    }

    pub fn execute(
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
        let run = self.run_in(spec, uid, gid, observer, &action, &mut teardown);
        teardown.armed = false;
        drop(teardown);
        let fin = {
            let fs = self.fs.clone();
            let a = action.clone();
            io::cpu(move || fs.finish_action(&a))
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
        let (entries, out_hash, err_hash) = io::cpu(move || -> Result<_> {
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
        })?;
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

    fn run_in(
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
            action.mkdir_p(d).map_err(|e| anyhow::anyhow!("creating {}: errno {}", show(d), e.0))?;
        }
        let (out_r, out_w) = pipe()?;
        let (err_r, err_w) = pipe()?;
        let setup = SetupSpec { argv: spec.argv.clone(), env: spec.env.clone(), cwd: spec.cwd.clone(), net: spec.net };
        let host = HostLayout {
            place: self.place,
            root: self.mnt.join(SandboxFs::dir_name(action.seq)),
            seq: action.seq,
            cgroup_root: self.cgroup_root.clone(),
            uid,
            gid,
            caller: unsafe { (libc::getuid(), libc::getgid()) },
            flake: false,
            binds: vec![],
        };
        let plan = crate::sandbox::plan(&setup, &host)?;
        *group_slot = plan.cgroup_dir().map(crate::cgroup::ActionGroup::open);
        let group = group_slot.as_ref();
        assert!(slot.is_none(), "one process per action");
        let running = slot.insert(self.zygote.spawn(plan, None, out_w, err_w, &self.sched.jobserver_fds())?);
        let id = running.id;
        let exit = running.done.take().expect("the exit is waited once");
        let obs = ObsPtr(unsafe {
            std::mem::transmute::<*const (dyn Observer + '_), *const (dyn Observer + 'static)>(observer)
        });
        let peak_kb = Arc::new(AtomicU64::new(0));
        let mut watch = None;
        if let Some(g) = group {
            let dir = g.dir.clone();
            let peak = Arc::clone(&peak_kb);
            watch = Some(io::spawn(move || {
                let group = crate::cgroup::ActionGroup::open(dir);
                let mut every = io::interval(std::time::Duration::from_millis(100));
                loop {
                    every.tick();
                    let kb = io::cpu(|| group.anon_kb());
                    peak.fetch_max(kb, Ordering::Relaxed);
                }
            }));
        }
        let action_r = Arc::clone(action);
        let store_r = Arc::clone(&self.store);
        let timeout = self.action_timeout;
        enum ActionWait {
            Finished(Vec<u8>, Vec<u8>, Option<crate::sandbox::ExitInfo>),
            TimedOut,
        }
        let mut out_h = io::spawn(move || read_stdout(out_r, obs));
        let mut err_h = io::spawn(move || read_stderr(err_r, obs, action_r, store_r));
        let mut done_h = io::spawn(move || exit.recv());
        let arms: Vec<Box<dyn FnOnce() -> ActionWait + '_>> = vec![
            Box::new(|| {
                let stdout = out_h.join().unwrap_or_default();
                let stderr = err_h.join().unwrap_or_default();
                let info = done_h.join().ok().flatten();
                ActionWait::Finished(stdout, stderr, info)
            }),
            Box::new(move || {
                io::sleep(timeout);
                ActionWait::TimedOut
            }),
        ];
        let waited = io::race_biased(arms);
        if let Some(w) = watch.as_ref() {
            w.abort();
        }
        let (stdout, stderr, info) = match waited {
            ActionWait::TimedOut => {
                self.zygote.kill(id);
                out_h.abort();
                err_h.abort();
                done_h.abort();
                return Err(ActionTimedOut { label: spec.label.clone(), secs: self.action_timeout.as_secs() }.into());
            }
            ActionWait::Finished(stdout, stderr, info) => (stdout, stderr, info),
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
            bail!("could not start `{}`: {}", spec.label, String::from_utf8_lossy(&stderr).trim());
        }
        Ok((info, stdout, stderr))
    }
}
