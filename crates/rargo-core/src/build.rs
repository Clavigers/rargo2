//! Handling one client request: sync, plan, execute, present, deliver.

use crate::buildview::*;
use crate::engine::*;
use crate::expand::{self, EnvRule, Scripts};
use crate::hash::Hash;
use crate::parse::Parser;
use crate::present::*;
use crate::proto::{Delivery, ExecRequest, Write};
use crate::session::Session;
use crate::sandbox::NetMode;
use crate::sched::Priority;
use crate::snapshot::{Snapshot, Trees};
use crate::store::Store;
use crate::trace::*;
use crate::view::*;
use anyhow::{Context, Result, anyhow, bail};
use dashmap::DashMap;
use parking_lot::Mutex;
use rargo_plan as rp;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;
use tokio::sync::watch;

/// Where the planner writes its plan inside the sandbox.
const PLAN_PATH: &str = "/.rargo-plan";

pub struct Config {
    /// rargod's private directory (store, mount point, scratch space).
    pub state_dir: PathBuf,
    /// The forked cargo.
    pub planner: PathBuf,
    /// The server's cargo home: registry and git checkouts shared by every
    /// client. Clients' own cargo configuration is laid over it per request.
    pub cargo_home: PathBuf,
    /// The user actions run as.
    pub uid: u32,
    pub gid: u32,
    /// The server's own environment: where its toolchains and tools are.
    pub server_env: HashMap<String, String>,
}

#[derive(Default)]
pub struct DaemonStats {
    pub requests: AtomicU64,
    pub plans_decoded: AtomicU64,
    pub verify_units: AtomicU64,
    pub verify_mismatches: AtomicU64,
    /// Bytes of worktree content received from clients.
    pub bytes_in: AtomicU64,
    /// Bytes of artifacts sent to clients.
    pub bytes_out: AtomicU64,
    /// Client daemons that have connected.
    pub links: AtomicU64,
    /// File contents received from clients, asked for or pushed.
    pub files_in: AtomicU64,
}

/// State shared by all requests.
pub struct Daemon {
    /// Per worktree and toolchain: the last plan said the compiler is
    /// rargo's rustc fork.
    fork_seen: DashMap<(String, String), bool>,
    pub cfg: Config,
    pub engine: Arc<Engine>,
    pub memo: Arc<HostMemo>,
    /// Every worktree state clients have synced.
    pub trees: Arc<Trees>,
    pub parser: Parser,
    pub incr: crate::incr::IncrStore,
    /// Workers, when the server accepts them.
    pub pool: OnceLock<Arc<crate::remote::Pool>>,
    /// The most recent drift report, for `--rargo-status`.
    pub drift_report: Mutex<Option<String>>,
    /// Who last built in each worktree, for the background drift check.
    pub last_clients: Mutex<HashMap<String, crate::drift::LastClient>>,
    /// Per worktree: the token of its last delivery and everything that
    /// worktree has been delivered up to then (see `serve`).
    pub deliveries: Mutex<HashMap<String, (u64, Arc<std::collections::HashSet<rargo_proto::Digest>>)>>,
    /// Chunk recipes of contents clients fetch by chunk.
    pub client_blobs: Arc<crate::remote::Blobs>,
    plans: DashMap<Hash, Arc<PlanData>>,
    /// Target directories seen per worktree, masked in later views.
    masks: DashMap<String, Vec<Vec<u8>>>,
    /// Downloads into the shared cargo home happen one at a time.
    fetching: tokio::sync::Mutex<()>,
    scratch_seq: AtomicU64,
    pub stats: Arc<DaemonStats>,
}

/// A client's own cargo configuration, laid over the shared cargo home.
pub struct Home {
    /// A cargo home for unsandboxed cargo runs on behalf of this client:
    /// its configuration files plus links to the shared downloads.
    pub dir: PathBuf,
    /// The configuration files present in `dir`.
    pub files: Vec<String>,
}

/// One client request.
pub struct Client {
    pub args: Vec<String>,
    /// The worktree root, current directory and cargo home as the client
    /// knows them. Never opened here: they translate paths in messages and
    /// arguments, and name where artifacts go on the client.
    pub root: String,
    pub cwd: String,
    pub cargo_home: String,
    pub env: Arc<ClientEnv>,
    pub uid: u32,
    pub gid: u32,
    pub prio: Priority,
    pub out: Output,
    /// The connection, absent for builds the server runs for itself.
    pub session: Option<Arc<Session>>,
    pub snapshot: Arc<Snapshot>,
    /// Stable identity of the client's worktree.
    pub worktree: String,
    /// HEAD and recent ancestors on the client (a hint, see D16).
    pub commits: Arc<Vec<[u8; 20]>>,
    pub home: Arc<Home>,
    /// Files and writes waiting to go to the client.
    pub outbox: Arc<Mutex<(Vec<Delivery>, Vec<Write>)>>,
}

impl Client {
    /// Queues files to place and writes to apply on the client. They go
    /// out together when the request ends ([`Client::flush`]): one round
    /// trip, however many steps produced something.
    pub fn deliver(&self, files: Vec<Delivery>, writes: Vec<Write>) {
        // A client with a virtual output directory is told at once.
        if let Some(s) = self.session.as_ref().filter(|s| s.is_lazy()) {
            s.deliver_lazy(files, writes);
            return;
        }
        let mut q = self.outbox.lock();
        q.0.extend(files);
        q.1.extend(writes);
    }

    /// Sends everything queued.
    pub async fn flush(&self) -> Result<()> {
        let (files, writes) = std::mem::take(&mut *self.outbox.lock());
        match &self.session {
            Some(s) => s.deliver(files, writes).await,
            None => Ok(()),
        }
    }
}

/// A decoded plan with derived per-unit data.
pub struct PlanData {
    pub plan: rp::Plan,
    /// Unit links against its dependencies (needs their object code).
    linking: Vec<bool>,
    levels: Vec<OnceLock<Arc<Vec<u8>>>>,
    /// Sandbox paths of each unit's metadata outputs, announced or not.
    rmeta_out: Vec<Vec<Vec<u8>>>,
    /// For a documentation unit: the directory rustdoc writes to.
    doc_dir: Vec<Option<Vec<u8>>>,
    /// Documentation units in the order they run (see `PlanData::new`).
    doc_order: Vec<usize>,
    /// For a documentation unit: its place in `doc_order`.
    doc_pos: Vec<Option<usize>>,
    /// Like `levels`, for a unit compiled for its metadata only.
    meta_levels: Vec<OnceLock<Arc<Vec<u8>>>>,
    /// How much of each unit a metadata-only request for the roots needs.
    needs: OnceLock<Arc<Vec<u8>>>,
}

impl PlanData {
    fn new(plan: rp::Plan) -> PlanData {
        let n = plan.units.len();
        let linking = plan.units.iter().map(|u| u.needs_objects).collect();
        let rmeta_out = plan
            .units
            .iter()
            .map(|u| u.outputs.iter().filter(|o| o.flavor == "Rmeta").map(|o| o.path.clone().into_bytes()).collect())
            .collect();
        // rustdoc keeps files that cover all crates (the search index, the
        // crate list) in the output directory and extends them on every
        // run. cargo lets the runs race under rustdoc's own lock; here the
        // runs for one directory form a chain, each seeing its
        // predecessors' output, so the last one's shared files are complete.
        let doc_dir = plan
            .units
            .iter()
            .map(|u| match (&u.kind, &u.cmd) {
                (rp::UnitKind::Doc, Some(cmd)) => {
                    ["-o", "--out-dir"].iter().find_map(|f| arg_value(&cmd.args, f)).map(|d| d.as_bytes().to_vec())
                }
                _ => None,
            })
            .collect();
        let doc_dir: Vec<Option<Vec<u8>>> = doc_dir;
        // The chain's order: dependencies first (cargo documents a crate
        // after the crates it links to), otherwise by unit index.
        let mut doc_order: Vec<usize> = Vec::new();
        {
            let mut seen = vec![false; n];
            fn visit(i: usize, units: &[rp::Unit], is_doc: &dyn Fn(usize) -> bool, seen: &mut [bool], out: &mut Vec<usize>) {
                if seen[i] {
                    return;
                }
                seen[i] = true;
                let mut deps: Vec<usize> = units[i].deps.iter().map(|d| d.unit as usize).filter(|&j| is_doc(j)).collect();
                deps.sort_unstable();
                for j in deps {
                    visit(j, units, is_doc, seen, out);
                }
                out.push(i);
            }
            let is_doc = |j: usize| doc_dir[j].is_some();
            for i in 0..n {
                if is_doc(i) {
                    visit(i, &plan.units, &is_doc, &mut seen, &mut doc_order);
                }
            }
        }
        let mut doc_pos = vec![None; n];
        for (pos, &i) in doc_order.iter().enumerate() {
            doc_pos[i] = Some(pos);
        }
        PlanData {
            plan,
            linking,
            doc_dir,
            doc_order,
            doc_pos,
            levels: (0..n).map(|_| OnceLock::new()).collect(),
            rmeta_out,
            meta_levels: (0..n).map(|_| OnceLock::new()).collect(),
            needs: OnceLock::new(),
        }
    }

    /// The unit can stand in for itself with its metadata alone: it is a
    /// library whose dependents compile against a `.rmeta`.
    fn can_meta(&self, j: usize) -> bool {
        self.plan.units[j].kind == rp::UnitKind::Compile && !self.rmeta_out[j].is_empty()
    }

    /// What unit `i` may see of every other unit when it is compiled for
    /// its metadata only (nothing is linked, so libraries are needed as
    /// metadata; things that are executed, such as proc macros and build
    /// script runs, are needed whole).
    fn meta_levels(&self, i: usize) -> Arc<Vec<u8>> {
        self.meta_levels[i]
            .get_or_init(|| {
                let units = &self.plan.units;
                let mut lv = vec![LEVEL_NONE; units.len()];
                let mut stack: Vec<usize> = Vec::new();
                for d in &units[i].deps {
                    stack.push(d.unit as usize);
                }
                while let Some(j) = stack.pop() {
                    if lv[j] != LEVEL_NONE {
                        continue;
                    }
                    lv[j] = if self.can_meta(j) { LEVEL_META } else { LEVEL_FULL };
                    for d in &units[j].deps {
                        stack.push(d.unit as usize);
                    }
                }
                Arc::new(lv)
            })
            .clone()
    }

    /// For a request that wants the roots' metadata only (`check` planned
    /// as a build): how far each unit has to be taken. A unit nothing needs
    /// is not run; one needed for its metadata is compiled without code
    /// generation; one that something executes or links is built in full.
    fn needs(&self) -> Arc<Vec<u8>> {
        self.needs
            .get_or_init(|| {
                let units = &self.plan.units;
                let mut need = vec![LEVEL_NONE; units.len()];
                let mut work: Vec<usize> = Vec::new();
                for &r in &self.plan.roots {
                    let r = r as usize;
                    need[r] = if units[r].kind == rp::UnitKind::Compile { LEVEL_META } else { LEVEL_FULL };
                    work.push(r);
                }
                while let Some(i) = work.pop() {
                    let sees = if need[i] == LEVEL_META { self.meta_levels(i) } else { self.levels(i) };
                    for (k, &lv) in sees.iter().enumerate() {
                        if lv == LEVEL_NONE {
                            continue;
                        }
                        // Only a compile can stop at metadata.
                        let lv = if units[k].kind == rp::UnitKind::Compile { lv } else { LEVEL_FULL };
                        if lv > need[k] {
                            need[k] = lv;
                            work.push(k);
                        }
                    }
                }
                Arc::new(need)
            })
            .clone()
    }

    /// How much of every other unit's output unit `i` may see.
    fn levels(&self, i: usize) -> Arc<Vec<u8>> {
        self.levels[i]
            .get_or_init(|| {
                let units = &self.plan.units;
                let mut lv = vec![LEVEL_NONE; units.len()];
                let mut stack: Vec<usize> = Vec::new();
                for d in &units[i].deps {
                    let j = d.unit as usize;
                    let direct = if self.linking[i] || !(d.only_rmeta && units[j].rmeta_early) {
                        LEVEL_FULL
                    } else {
                        LEVEL_META
                    };
                    if lv[j] < direct {
                        if lv[j] == LEVEL_NONE {
                            stack.push(j);
                        }
                        lv[j] = direct;
                    }
                }
                while let Some(j) = stack.pop() {
                    for d in &units[j].deps {
                        let k = d.unit as usize;
                        let want = if self.linking[i] || !units[k].rmeta_early { LEVEL_FULL } else { LEVEL_META };
                        if lv[k] == LEVEL_NONE {
                            stack.push(k);
                        }
                        if lv[k] < want {
                            lv[k] = want;
                        }
                    }
                }
                if let Some(pos) = self.doc_pos[i] {
                    for &j in &self.doc_order[..pos] {
                        if self.doc_dir[j] == self.doc_dir[i] {
                            lv[j] = LEVEL_FULL;
                        }
                    }
                }
                Arc::new(lv)
            })
            .clone()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum UState {
    Pending,
    /// The unit's metadata is available; it is still running.
    Meta,
    Done,
    Failed,
    /// Not run because something it needs failed.
    Skipped,
}

#[derive(Default, Clone)]
struct Counts {
    total: usize,
    duplicates: usize,
    lints: usize,
    fixable: usize,
    fix_disallowed: bool,
    errors: usize,
    warnings_seen: usize,
}

#[derive(Default)]
struct Ui {
    compiled: HashSet<String>,
    documented: HashSet<String>,
    scraped: HashSet<String>,
    seen_diag: HashSet<Hash>,
    /// rustc's `unused_extern` message, by unit.
    unused_externs: BTreeMap<usize, String>,
    /// The future-incompatibility warnings each unit's compiler reported,
    /// as rendered by the compiler.
    future_incompat: BTreeMap<usize, Vec<String>>,
    counts: HashMap<usize, Counts>,
    said_waiting: bool,
}

struct Run {
    d: Arc<Daemon>,
    c: Arc<Client>,
    pd: Arc<PlanData>,
    base: Arc<BaseView>,
    layer: Arc<OutputsLayer>,
    scripts: Mutex<Scripts>,
    states: Vec<watch::Sender<UState>>,
    results: Vec<OnceLock<(Arc<ActionResult>, bool)>>,
    /// When each unit was asked for and answered (for `--timings`).
    times: Mutex<Vec<crate::timings::UnitTime>>,
    early: Mutex<HashMap<usize, Vec<(Vec<u8>, MEntry)>>>,
    ui: Mutex<Ui>,
    failed: AtomicBool,
    /// An announced `.rmeta` turned out different in the final result.
    inconsistent: AtomicBool,
    base_env: BTreeMap<String, String>,
    no_cache_tests: bool,
    /// The client asked to be told why things run (`RARGO_EXPLAIN`).
    explain: bool,
    root_id: Hash,
    /// In a flake sandbox the forked cargo cannot run (its libraries are
    /// the host's); processes that ask for `$CARGO` get the flake's cargo.
    cargo_override: Option<String>,
    policy: crate::policy::Policy,
    /// Re-run every unit that came from the cache, cleanly, and compare.
    verify: bool,
    verified: AtomicU64,
    /// `(message, canonical paths that differ)`; no paths means the
    /// difference is in what the action said or whether it succeeded.
    verify_mismatches: Mutex<Vec<(String, Vec<Vec<u8>>)>>,
    /// Units whose two runs differ in a way the tools themselves produce
    /// (see `exec_unit`): reported, not counted as drift.
    verify_unreproducible: Mutex<Vec<String>>,
    /// Each unit's cache key, once known (for looking up who read what).
    unit_keys: Vec<OnceLock<Hash>>,
    /// Set when the request is a `check` planned as a build: how far each
    /// unit has to be taken (see `PlanData::needs`).
    need: Option<Arc<Vec<u8>>>,
}

/// Position of the cargo subcommand in a command line.
fn subcommand_pos(args: &[String]) -> Option<usize> {
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            return None;
        }
        if !a.starts_with('-') {
            return Some(i);
        }
        // Global options that take a separate value.
        if matches!(a.as_str(), "--config" | "-Z" | "--color" | "-C" | "--explain") {
            i += 1;
        }
        i += 1;
    }
    None
}

fn bytes(s: &str) -> Vec<u8> {
    s.as_bytes().to_vec()
}

/// The environment every action starts from: where things are on the
/// server, and how the client wants cargo and rustc to behave. Everything
/// else is withheld unless an action declares that it reads it.
fn base_env(server: &HashMap<String, String>, client: &HashMap<String, String>) -> BTreeMap<String, String> {
    const SERVER: &[&str] =
        &["PATH", "HOME", "USER", "LOGNAME", "SHELL", "RUSTUP_HOME", "SSL_CERT_FILE", "SSL_CERT_DIR"];
    // Allowed through the protocol but only about presentation, which is
    // applied when output is shown, or about rargo itself.
    const NOT_FOR_ACTIONS: &[&str] = &["TERM", "NO_COLOR", "BROWSER", "CLICOLOR", "CLICOLOR_FORCE", "COLORTERM"];
    const DROP_PREFIXES: &[&str] = &[
        "CARGO_PKG_", "CARGO_MANIFEST_", "CARGO_CRATE_", "CARGO_BIN_", "CARGO_CFG_", "CARGO_FEATURE_",
        "CARGO_TERM_", "CARGO_DEP_", "RARGO_",
    ];
    const DROP: &[&str] = &[
        "CARGO_HOME", "CARGO_MAKEFLAGS", "CARGO_PRIMARY_PACKAGE", "CARGO_TARGET_TMPDIR", "CARGO_SBOM_PATH",
        "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "CARGO_NET_OFFLINE", "CARGO_CACHE_RUSTC_INFO",
        "CARGO_RUSTC_CURRENT_DIR",
    ];
    let mut out = BTreeMap::new();
    for (k, v) in server {
        if SERVER.contains(&k.as_str()) || k.starts_with("NIX_") {
            out.insert(k.clone(), v.clone());
        }
    }
    for (k, v) in client {
        let dropped = DROP.contains(&k.as_str())
            || NOT_FOR_ACTIONS.contains(&k.as_str())
            || DROP_PREFIXES.iter().any(|p| k.starts_with(p));
        if rargo_proto::env_allowed(k) && !dropped {
            out.insert(k.clone(), v.clone());
        }
    }
    out.insert("CARGO_HOME".into(), String::from_utf8_lossy(CARGO_HOME_AT).into_owned());
    // rargo is the compiler cache; a wrapper such as sccache would be run
    // inside the sandbox where it cannot reach its server.
    out.insert("RUSTC_WRAPPER".into(), String::new());
    out.insert("RUSTC_WORKSPACE_WRAPPER".into(), String::new());
    out
}

impl Daemon {
    pub fn new(cfg: Config, engine: Arc<Engine>) -> Daemon {
        let parser = Parser::new(cfg.planner.clone());
        let incr = crate::incr::IncrStore::new(engine.store.clone());
        {
            let engine = engine.clone();
            incr.set_held(Box::new(move |h| engine.remote.get().is_some_and(|r| r.holds(h))));
        }
        let trees = Trees::new(engine.store.clone());
        Daemon {
            trees,
            fetching: tokio::sync::Mutex::new(()),
            scratch_seq: AtomicU64::new(0),
            incr,
            pool: OnceLock::new(),
            drift_report: Mutex::new(None),
            last_clients: Mutex::new(HashMap::new()),
            deliveries: Mutex::new(HashMap::new()),
            client_blobs: Arc::new(crate::remote::Blobs::new(engine.store.clone(), None)),
            cfg,
            engine,
            memo: Arc::new(HostMemo::default()),
            parser,
            plans: DashMap::new(),
            fork_seen: DashMap::new(),
            masks: DashMap::new(),
            stats: Arc::new(DaemonStats::default()),
        }
    }

    /// A fresh scratch directory on the store's filesystem. The caller
    /// removes it.
    pub fn scratch(&self, what: &str) -> Result<PathBuf> {
        let n = self.scratch_seq.fetch_add(1, Ordering::Relaxed);
        let dir = self.store().tmp_dir(&format!("{what}-{}-{n}", std::process::id()))?;
        Ok(dir)
    }

    /// The cargo home overlay for a client's configuration files.
    pub fn home(&self, files: &[(String, Vec<u8>)]) -> Result<Arc<Home>> {
        const ALLOWED: &[&str] = &["config.toml", "config", "credentials.toml", "credentials"];
        let mut files: Vec<&(String, Vec<u8>)> = files.iter().filter(|(n, _)| ALLOWED.contains(&n.as_str())).collect();
        files.sort();
        let mut h = crate::hash::KeyHasher::new("cargo-home-overlay");
        for (name, content) in &files {
            h.field(name.as_bytes()).field(content);
        }
        let dir = self.cfg.state_dir.join("homes").join(h.finish().hex());
        let names: Vec<String> = files.iter().map(|(n, _)| n.clone()).collect();
        // Many clients with the same configuration arrive at once.
        static MAKING: Mutex<()> = Mutex::new(());
        let _one = MAKING.lock();
        if !dir.join(".complete").exists() {
            use std::os::unix::fs::PermissionsExt;
            std::fs::create_dir_all(&dir)?;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
            crate::snapshot::chown(&dir, self.cfg.uid, self.cfg.gid);
            for (name, content) in &files {
                let p = dir.join(name);
                std::fs::write(&p, content)?;
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
                crate::snapshot::chown(&p, self.cfg.uid, self.cfg.gid);
            }
            // Downloads and the locks that guard them are the shared ones.
            for shared in ["registry", "git", ".package-cache", ".package-cache-mutate", ".global-cache"] {
                let link = dir.join(shared);
                if std::fs::symlink_metadata(&link).is_err() {
                    std::os::unix::fs::symlink(self.cfg.cargo_home.join(shared), &link)?;
                }
            }
            std::fs::write(dir.join(".complete"), b"")?;
        }
        Ok(Arc::new(Home { dir, files: names }))
    }

    fn store(&self) -> &Arc<Store> {
        &self.engine.store
    }

    /// Writes every trace and the blobs it refers to into `dir`, for
    /// [`Daemon::import_cache`] on another machine.
    pub fn export_cache(&self, dir: &Path) -> Result<(usize, usize)> {
        let blobs = dir.join("blobs");
        std::fs::create_dir_all(&blobs)?;
        let traces = self.engine.traces.all();
        let mut wanted: HashSet<Hash> = HashSet::new();
        for (_, t) in &traces {
            wanted.insert(t.result.stdout);
            wanted.insert(t.result.stderr);
            for (_, e) in &t.result.outputs.entries {
                if let MEntry::File { hash, .. } = e {
                    wanted.insert(*hash);
                }
            }
        }
        let mut written = 0;
        for h in &wanted {
            let dest = blobs.join(h.hex());
            if dest.exists() {
                continue;
            }
            let Ok(mut src) = self.store().open_blob(h) else { continue };
            let mut out = std::fs::File::create(&dest)?;
            std::io::copy(&mut src, &mut out)?;
            written += 1;
        }
        let bundle = (traces, self.engine.traces.all_needs_env());
        let raw = bincode::serialize(&bundle)?;
        std::fs::write(dir.join("traces.bin"), zstd::encode_all(&raw[..], 3)?)?;
        Ok((bundle.0.len(), written))
    }

    /// Reads an export. Blob contents are hashed again on the way in, so a
    /// damaged or forged export cannot put wrong bytes under a hash.
    pub fn import_cache(&self, dir: &Path) -> Result<(usize, usize)> {
        let raw = zstd::decode_all(&std::fs::read(dir.join("traces.bin"))?[..])?;
        let (traces, needs): (Vec<(Hash, Trace)>, Vec<(Hash, Vec<String>)>) = bincode::deserialize(&raw)?;
        let mut blobs = 0;
        for e in std::fs::read_dir(dir.join("blobs"))?.flatten() {
            let Some(want) = e.file_name().to_str().and_then(Hash::from_hex) else { continue };
            if self.store().has(&want) {
                continue;
            }
            let (got, _) = self.store().put_file_copy(&std::fs::File::open(e.path())?, false)?;
            if got != want {
                self.store().remove(&got);
                bail!("blob {} in the export does not have the content its name claims", want);
            }
            blobs += 1;
        }
        let mut n = 0;
        for (key, t) in traces {
            if t.complete(self.store()) {
                self.engine.traces.insert(&key, t);
                n += 1;
            }
        }
        for (key, vars) in needs {
            self.engine.traces.add_needs_env(&key, &vars);
        }
        Ok((n, blobs))
    }

    /// Removes blobs nothing refers to any more: outputs of traces that
    /// were displaced, superseded incremental states, captures of files
    /// that have since changed. Blobs used within `min_age_secs` are left
    /// alone so that running builds keep what they are using.
    pub fn collect_garbage(&self, min_age_secs: u64) -> (usize, u64) {
        let mut live = HashSet::new();
        self.engine.traces.mark(&mut live);
        self.incr.mark(&mut live);
        self.trees.mark(&mut live);
        let fs = self.engine.fs.clone();
        self.store().sweep(&live, min_age_secs, &|h| fs.forget_backing(h))
    }
}

/// Handles a request to completion. Returns the client's exit code, or the
/// process the client should turn into.
pub async fn handle(d: Arc<Daemon>, c: Client) -> Result<i32, ExecRequest> {
    d.stats.requests.fetch_add(1, Ordering::Relaxed);
    let c = Arc::new(c);
    // An inconsistency between an early-announced artifact and the final
    // one invalidates work that started on the early one; everything is
    // cached, so starting over is cheap.
    let mut result = Ok(101);
    for attempt in 0..3 {
        match run_request(&d, &c).await {
            Ok(Finish::Code(code)) => result = Ok(code),
            Ok(Finish::Exec(e)) => result = Err(e),
            Ok(Finish::Retry) if attempt < 2 => {
                // Whatever the abandoned attempt meant to place is stale.
                c.outbox.lock().0.clear();
                continue;
            }
            Ok(Finish::Retry) => c.out.error("the build kept producing inconsistent intermediate artifacts"),
            Err(e) => c.out.error(&format!("{e:#}")),
        }
        break;
    }
    // What the request produced for the client goes out whatever the
    // outcome: cargo would have left a lockfile it wrote, or files tests
    // wrote before one failed, behind as well.
    if let Err(e) = c.flush().await {
        c.out.error(&format!("delivering results: {e:#}"));
        return Ok(101);
    }
    result
}

enum Finish {
    Code(i32),
    Exec(ExecRequest),
    Retry,
}

/// Observer for the planning action.
struct PlanObserver {
    client: Arc<Client>,
}
impl Observer for PlanObserver {
    fn wants_explanation(&self) -> bool {
        self.client.env.contains_key("RARGO_EXPLAIN")
    }
    fn explain(&self, why: &str) {
        self.client.out.note(&format!("planning runs because {why}"));
    }
}

struct Prepared {
    snapshot: Arc<Snapshot>,
    policy: crate::policy::Policy,
    /// The view the planner runs against (always the host root).
    plan_base: Arc<BaseView>,
    flake: Option<Arc<crate::flake::FlakeEnv>>,
    base: Arc<BaseView>,
    tr: Translator,
    sandbox_cwd: String,
    env: BTreeMap<String, String>,
}

async fn prepare(d: &Arc<Daemon>, c: &Arc<Client>, snapshot: Arc<Snapshot>) -> Result<Prepared> {
    // The repository's policy file, as of this snapshot.
    let loaded = {
        let text = |name: &str| snapshot.read(name.as_bytes()).map(|b| String::from_utf8_lossy(&b).into_owned());
        crate::policy::Policy::load(text(crate::policy::MANIFEST).as_deref(), text(crate::policy::POLICY_FILE).as_deref()).map_err(|e| anyhow!(e))?
    };
    let policy = loaded.policy;
    for notice in &loaded.notices {
        c.out.warn_once(notice);
    }
    if c.env.get("RARGO_QUIET_DEPRECATIONS").is_none() {
        for d in &loaded.deprecations {
            c.out.note_once(d);
        }
    }
    let generation = d.memo.advance();
    let mut masks = vec![vjoin(WORKTREE_AT, b"target")];
    if let Some(extra) = d.masks.get(&c.worktree) {
        for m in extra.iter() {
            if !masks.contains(m) {
                masks.push(m.clone());
            }
        }
    }
    // A flake, when present and not switched off, defines the environment.
    let has_flake = matches!(snapshot.node(b"flake.nix"), Some(rargo_proto::tree::Node::File { .. }));
    let flake = if has_flake && policy.flake.enabled != Some(false) {
        let (dd, snap, uid, gid, files) = (d.clone(), snapshot.clone(), c.uid, c.gid, policy.flake.files.clone());
        Some(
            tokio::task::spawn_blocking(move || crate::flake::resolve(&dd, &snap, &files, uid, gid))
                .await?
                .context(
                    "evaluating the repository's flake (if it reads files of the repository, name them in `flake.files` in .rargo.toml; \
                     `flake.enabled = false` builds without it)",
                )?,
        )
    } else {
        None
    };
    let restricted = match &flake {
        Some(f) => {
            let mut links = Vec::new();
            for (at, tool) in [("/bin/sh", "sh"), ("/usr/bin/env", "env")] {
                if let Some(target) = f.which(tool).or_else(|| if tool == "sh" { f.which("bash") } else { None }) {
                    links.push((bytes(at), bytes(&target)));
                }
            }
            Some(Restricted { store_names: f.closure_set(), etc: f.etc(d.store(), c.uid, c.gid)?, links })
        }
        None => None,
    };
    let parts = ViewParts {
        snapshot: snapshot.clone(),
        cargo_home: d.cfg.cargo_home.clone(),
        home_files: c.home.files.iter().map(|n| (vjoin(CARGO_HOME_AT, n.as_bytes()), c.home.dir.join(n))).collect(),
        masks,
        hidden: vec![d.cfg.state_dir.as_os_str().as_encoded_bytes().to_vec()],
        planner: d.cfg.planner.clone(),
        restricted,
    };
    let base = parts.build(d.memo.clone(), generation, false);
    let plan_base = if flake.is_some() { Arc::new(parts.build(d.memo.clone(), generation, true)) } else { Arc::new(parts.build(d.memo.clone(), generation, false)) };
    let tr = Translator::new(vec![
        (String::from_utf8_lossy(WORKTREE_AT).into_owned(), c.root.clone()),
        (String::from_utf8_lossy(CARGO_HOME_AT).into_owned(), c.cargo_home.clone()),
    ]);
    let sandbox_cwd = tr.to_canonical(&c.cwd);
    if !vpath_starts_with(sandbox_cwd.as_bytes(), WORKTREE_AT) {
        bail!("the current directory `{}` is not inside the worktree `{}`", c.cwd, c.root);
    }
    let client_env = c.env.to_map();
    let mut env = base_env(&d.cfg.server_env, &client_env);
    if let Some(f) = &flake {
        // The flake's variables replace the server's: PATH, compilers and
        // their flags all come from the devShell. HOME is a scratch place.
        for k in ["PATH", "HOME", "USER", "LOGNAME", "SHELL", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN"] {
            env.remove(k);
        }
        env.retain(|k, _| !k.starts_with("NIX_"));
        for (k, v) in &f.env {
            env.insert(k.clone(), v.clone());
        }
        env.insert("HOME".into(), "/tmp".into());
        env.insert("CARGO_HOME".into(), String::from_utf8_lossy(CARGO_HOME_AT).into_owned());
        env.insert("RUSTC_WRAPPER".into(), String::new());
        env.insert("RUSTC_WORKSPACE_WRAPPER".into(), String::new());
    }
    for (k, v) in policy.passed_env(&client_env) {
        env.entry(k).or_insert(v);
    }
    Ok(Prepared { snapshot, policy, plan_base, flake, base: Arc::new(base), tr, sandbox_cwd, env })
}

struct Planned {
    data: Arc<PlanData>,
    /// Cargo's own messages while planning (warnings about manifests etc.).
    stderr: Vec<u8>,
}

enum PlanOutcome {
    Ok(Planned),
    /// Cargo refused; its message and exit code go to the client.
    Failed { stderr: Vec<u8>, stdout: Vec<u8>, code: i32 },
}

async fn plan(d: &Arc<Daemon>, c: &Arc<Client>, p: &Prepared, args: &[String]) -> Result<PlanOutcome> {
    plan_with(d, c, p, args, &[]).await
}

async fn plan_with(
    d: &Arc<Daemon>,
    c: &Arc<Client>,
    p: &Prepared,
    args: &[String],
    extra_env: &[(String, String)],
) -> Result<PlanOutcome> {
    let mut env: BTreeMap<Vec<u8>, Vec<u8>> =
        p.env.iter().map(|(k, v)| (bytes(k), bytes(v))).collect();
    for (k, v) in extra_env {
        env.insert(bytes(k), bytes(v));
    }
    env.insert(bytes(rp::PLAN_ENV), bytes(PLAN_PATH));
    env.insert(bytes("CARGO_NET_OFFLINE"), bytes("true"));
    env.insert(bytes("CARGO_CACHE_RUSTC_INFO"), bytes("0"));
    let mut argv = vec![PLANNER_AT.to_vec()];
    for a in args {
        // Paths the user typed refer to the real worktree.
        let a = match a.split_once('=') {
            Some((flag, val)) if flag.starts_with("--") && val.starts_with('/') => {
                format!("{flag}={}", p.tr.to_canonical(val))
            }
            _ if a.starts_with('/') => p.tr.to_canonical(a),
            _ => a.clone(),
        };
        argv.push(bytes(&a));
    }
    let spec = ActionSpec {
        kind: ActionKind::Plan,
        argv,
        env,
        cwd: bytes(&p.sandbox_cwd),
        net: NetMode::Loopback,
        mkdirs: vec![],
        root_id: p.flake.as_ref().map(|f| f.id).unwrap_or(Hash::ZERO),
        discard: vec![],
        label: "cargo (planning)".into(),
    };
    let outcome = d
        .engine
        .obtain(Request {
            spec,
            view: p.plan_base.clone(),
            client_env: c.env.clone(),
            uid: c.uid,
            gid: c.gid,
            prio: c.prio,
            no_cache: false,
            seed_blobs: Default::default(),
            cacheable: Arc::new(|r| r.success()),
            declared_env: Arc::new(|_, _| Vec::new()),
            observer: Arc::new(PlanObserver { client: c.clone() }),
        })
        .await?;
    let r = &outcome.result;
    let stderr = d.store().read(&r.stderr)?;
    if !r.success() {
        let stdout = d.store().read(&r.stdout)?;
        return Ok(PlanOutcome::Failed { stderr, stdout, code: r.code().unwrap_or(101) });
    }
    let Some(MEntry::File { hash, .. }) = r.outputs.get(PLAN_PATH.as_bytes()) else {
        bail!("the planner did not write a plan");
    };
    // cargo may have written or updated Cargo.lock; plain cargo would have
    // done that in the worktree.
    let lock_writes = Manifest {
        entries: r.outputs.entries.iter().filter(|(p, _)| p.ends_with(b"/Cargo.lock")).cloned().collect(),
    };
    apply_writes(c, &p.tr, &[], &lock_writes);
    let data = match d.plans.get(hash) {
        Some(pd) => pd.clone(),
        None => {
            let raw = d.store().read(hash)?;
            let plan: rp::Plan = rp::decode(&raw).map_err(|e| anyhow!("unreadable plan: {e}"))?;
            if plan.version != rp::PLAN_VERSION {
                bail!("plan format {} does not match this rargod ({})", plan.version, rp::PLAN_VERSION);
            }
            d.stats.plans_decoded.fetch_add(1, Ordering::Relaxed);
            let pd = Arc::new(PlanData::new(plan));
            d.plans.insert(*hash, pd.clone());
            pd
        }
    };
    Ok(PlanOutcome::Ok(Planned { data, stderr }))
}

/// Runs things only a networked, unsandboxed cargo can do (installing the
/// toolchain, updating the index, downloading crates, writing the lockfile)
/// on the server, in a scratch copy of the snapshot, into the shared cargo
/// home. A lockfile cargo wrote or changed goes back to the client, and the
/// returned snapshot is the client's worktree with it applied. `None` when
/// the fetch failed or was not allowed.
async fn fetch(d: &Arc<Daemon>, c: &Arc<Client>, p: &Prepared, args: &[String]) -> Result<Option<Arc<Snapshot>>> {
    let mut fetch_args = vec!["fetch".to_string()];
    let scratch = d.scratch("fetch")?;
    let scratch_s = scratch.to_string_lossy().into_owned();
    // Paths the user typed refer to the client's worktree.
    let local = |v: &str| -> String {
        let canon = p.tr.to_canonical(v);
        match canon.strip_prefix("/w") {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("{scratch_s}{rest}"),
            _ => v.to_string(),
        }
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--" => break,
            "--manifest-path" | "--config" | "--lockfile-path" => {
                fetch_args.push(a.clone());
                if let Some(v) = it.next() {
                    fetch_args.push(if a == "--config" { v.clone() } else { local(v) });
                }
            }
            "--locked" | "--frozen" | "--offline" => {
                let _ = std::fs::remove_dir_all(&scratch);
                return Ok(None);
            }
            _ if a.starts_with("--manifest-path=") => {
                fetch_args.push(format!("--manifest-path={}", local(&a["--manifest-path=".len()..])))
            }
            _ if a.starts_with("--config=") => fetch_args.push(a.clone()),
            _ => {}
        }
    }
    let _one_at_a_time = d.fetching.lock().await;
    let planner = d.cfg.planner.clone();
    let cwd = PathBuf::from(format!("{scratch_s}{}", &p.sandbox_cwd[2..]));
    // The server's own environment (so the toolchain manager and the
    // network work), steered by what the client is allowed to pass.
    let mut env: HashMap<String, String> = d.cfg.server_env.clone();
    for (k, v) in base_env(&d.cfg.server_env, &c.env.to_map()) {
        env.insert(k, v);
    }
    env.insert("CARGO_HOME".into(), c.home.dir.to_string_lossy().into_owned());
    let (uid, gid) = (c.uid, c.gid);
    let (snap, scratch2) = (p.snapshot.clone(), scratch.clone());
    let out = tokio::task::spawn_blocking(move || -> Result<std::process::Output> {
        use std::os::unix::process::CommandExt;
        snap.materialize(&scratch2, uid, gid, &|_, _| true)?;
        let run = |program: &Path, args: &[String]| {
            let mut cmd = std::process::Command::new(program);
            cmd.args(args).current_dir(&cwd).env_clear().envs(&env);
            cmd.stdin(std::process::Stdio::null());
            if uid != unsafe { libc::geteuid() } {
                cmd.uid(uid).gid(gid);
            }
            cmd.output()
        };
        // Asking the proxy for its version installs a missing toolchain.
        let _ = run(Path::new("rustc"), &["-V".to_string()]);
        Ok(run(&planner, &fetch_args)?)
    })
    .await?;
    let out = match out {
        Ok(o) => o,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&scratch);
            return Err(e);
        }
    };
    c.out.raw_err(String::from_utf8_lossy(&out.stderr).replace(&scratch_s, &c.root).as_bytes());
    let mut snapshot = p.snapshot.clone();
    let mut writes = Vec::new();
    if out.status.success() {
        // Lockfiles cargo wrote or changed.
        let mut stack = vec![scratch.clone()];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let Ok(ft) = e.file_type() else { continue };
                let path = e.path();
                if ft.is_dir() {
                    stack.push(path);
                } else if ft.is_file() && e.file_name() == "Cargo.lock" {
                    let Ok(rel) = path.strip_prefix(&scratch) else { continue };
                    let rel_b = rel.as_os_str().as_encoded_bytes();
                    let content = std::fs::read(&path)?;
                    let hash = d.store().put_bytes(&content)?;
                    let node = rargo_proto::tree::Node::File { hash: hash.0, size: content.len() as u64, exec: false };
                    if snapshot.node(rel_b).as_ref() != Some(&node) {
                        snapshot = snapshot.with_file(rel_b, Some(node))?;
                        writes.push(Write::File {
                            path: format!("{}/{}", c.root, rel.to_string_lossy()),
                            hash: hash.0,
                            size: content.len() as u64,
                            exec: false,
                        });
                    }
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);
    if !out.status.success() {
        return Ok(None);
    }
    c.deliver(Vec::new(), writes);
    Ok(Some(snapshot))
}

fn needs_network(stderr: &[u8]) -> bool {
    let s = String::from_utf8_lossy(stderr);
    ["--offline", "offline mode", "is not installed", "could not download", "syncing channel updates"]
        .iter()
        .any(|m| s.contains(m))
}

/// `RARGO_PHASES` in rargod's environment: one line per phase of every
/// request on its stderr, with the time since the request was taken up.
fn phase(started: Instant, c: &Client, what: &str) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("RARGO_PHASES").is_some()) {
        eprintln!("rargod: phase {:>8.3} s  {what}  ({} {})", started.elapsed().as_secs_f64(), c.worktree, c.args.join(" "));
    }
}

async fn run_request(d: &Arc<Daemon>, c: &Arc<Client>) -> Result<Finish> {
    let started = Instant::now();
    let mut args: Vec<String> = c.args.clone();
    // rargo's own flag; cargo never sees it.
    let mut no_cache_tests = false;
    if let Some(pos) = args.iter().take_while(|a| *a != "--").position(|a| a == "--no-cache") {
        args.remove(pos);
        no_cache_tests = true;
    }

    // The client's values of every variable any action has declared: one
    // question, asked while the build is planned, where there used to be
    // one per action looked up. A client that has answered before sends
    // them with its request and is asked nothing.
    let env_known = {
        let (env, names) = (c.env.clone(), d.engine.traces.every_env_name());
        tokio::spawn(async move { env.ensure(&names).await })
    };
    let mut p = prepare(d, c, c.snapshot.clone()).await?;
    phase(started, c, "worktree prepared");
    if c.session.is_some() && !c.worktree.is_empty() {
        d.trees.remember(&c.worktree, &c.snapshot);
        let mut last = d.last_clients.lock();
        let checked = last.get(&c.worktree).and_then(|l| l.checked);
        last.insert(
            c.worktree.clone(),
            crate::drift::LastClient {
                env: Arc::new(c.env.to_map()),
                home: c.home.clone(),
                root: c.root.clone(),
                at: Instant::now(),
                checked,
            },
        );
    }
    // With rargo's rustc, `check` is a build that stops at metadata: the
    // same compiler invocations as `build`, minus code generation, so the
    // two commands (and `test`) share every crate's front-end work instead
    // of each doing its own. Whether the compiler is the fork is only known
    // from a plan; what the last plan for this worktree said is remembered.
    let check_at = subcommand_pos(&args).filter(|&i| args[i] == "check" || args[i] == "c");
    // Opt-in: measured slower and larger in the 20-agent benchmark (D23).
    let check_at = check_at.filter(|_| c.env.get("RARGO_CHECK_AS_BUILD").as_deref() == Some("1"));
    let fork_key = (c.worktree.clone(), c.env.get("RUSTUP_TOOLCHAIN").unwrap_or_default());
    let mut as_build = check_at.is_some() && d.fork_seen.get(&fork_key).map(|v| *v).unwrap_or(false);
    let mut fetched = false;
    let mut plan_args;
    let planned = loop {
        plan_args = args.clone();
        if let (Some(i), true) = (check_at, as_build) {
            plan_args[i] = "build".into();
        }
        match plan(d, c, &p, &plan_args).await? {
            PlanOutcome::Ok(pl) => {
                if check_at.is_some() && pl.data.plan.rustc_fork != as_build {
                    as_build = pl.data.plan.rustc_fork;
                    d.fork_seen.insert(fork_key.clone(), as_build);
                    continue;
                }
                break pl;
            }
            PlanOutcome::Failed { stderr, .. } if !fetched && needs_network(&stderr) => {
                fetched = true;
                let Some(snapshot) = fetch(d, c, &p, &args).await? else {
                    return Ok(Finish::Code(101));
                };
                p = prepare(d, c, snapshot).await?;
            }
            PlanOutcome::Failed { stderr, stdout, code } => {
                c.out.raw_out(p.tr.text(&String::from_utf8_lossy(&stdout)).as_bytes());
                c.out.err_text(&String::from_utf8_lossy(&stderr));
                return Ok(Finish::Code(code));
            }
        }
    };
    let pd = planned.data;
    let plan = &pd.plan;
    env_known.await.map_err(|e| anyhow!("asking the client for its environment: {e}"))??;
    phase(started, c, "planned");

    // A target directory elsewhere in the worktree must be masked too; the
    // next request for this worktree will know.
    for dir in [&plan.target_dir, &plan.build_dir] {
        let b = dir.as_bytes().to_vec();
        if vpath_starts_with(&b, WORKTREE_AT) && b != vjoin(WORKTREE_AT, b"target") {
            let mut m = d.masks.entry(c.worktree.clone()).or_default();
            if !m.contains(&b) {
                m.push(b);
            }
        }
    }

    // The request-scoped presentation settings come from the plan (cargo
    // parsed the command line), the colour choice from the client.
    let out = c.out.derive(
        plan.verbosity == rp::Verbosity::Quiet,
        plan.verbosity == rp::Verbosity::Verbose,
        p.tr.clone(),
    );
    let c2 = Arc::new(Client {
        args: c.args.clone(),
        root: c.root.clone(),
        cwd: c.cwd.clone(),
        cargo_home: c.cargo_home.clone(),
        env: c.env.clone(),
        uid: c.uid,
        gid: c.gid,
        prio: c.prio,
        out,
        session: c.session.clone(),
        snapshot: p.snapshot.clone(),
        worktree: c.worktree.clone(),
        commits: c.commits.clone(),
        home: c.home.clone(),
        outbox: c.outbox.clone(),
    });
    c2.out.err_text(&String::from_utf8_lossy(&planned.stderr));

    let mut env = p.env.clone();
    if let Some(tc) = &plan.toolchain {
        env.entry("RUSTUP_TOOLCHAIN".into()).or_insert_with(|| tc.clone());
    }
    let n = plan.units.len();
    let run = Arc::new(Run {
        d: d.clone(),
        c: c2.clone(),
        pd: pd.clone(),
        base: p.base.clone(),
        layer: Arc::new(OutputsLayer::new()),
        scripts: Mutex::new(Scripts::new()),
        states: (0..n).map(|_| watch::channel(UState::Pending).0).collect(),
        results: (0..n).map(|_| OnceLock::new()).collect(),
        times: Mutex::new(vec![Default::default(); n]),
        early: Mutex::new(HashMap::new()),
        ui: Mutex::new(Ui::default()),
        failed: AtomicBool::new(false),
        inconsistent: AtomicBool::new(false),
        base_env: env,
        no_cache_tests,
        explain: c.env.contains_key("RARGO_EXPLAIN"),
        root_id: p.flake.as_ref().map(|f| f.id).unwrap_or(Hash::ZERO),
        cargo_override: p.flake.as_ref().and_then(|f| f.which("cargo")),
        policy: p.policy.clone(),
        verify: c.env.contains_key("RARGO_VERIFY"),
        verified: AtomicU64::new(0),
        verify_mismatches: Mutex::new(Vec::new()),
        verify_unreproducible: Mutex::new(Vec::new()),
        unit_keys: (0..n).map(|_| OnceLock::new()).collect(),
        need: if as_build { Some(pd.needs()) } else { None },
    });

    // If this build is dropped (its client went away), so are its units:
    // those waiting never start, those running are killed.
    struct Units(Vec<tokio::task::JoinHandle<()>>);
    impl Drop for Units {
        fn drop(&mut self) {
            for t in &self.0 {
                t.abort();
            }
        }
    }
    let mut tasks = Units(Vec::with_capacity(n));
    for i in 0..n {
        let r = run.clone();
        tasks.0.push(tokio::spawn(async move {
            let st = match r.exec_unit(i).await {
                Ok(st) => st,
                Err(e) => {
                    r.c.out.error(&format!("{}: {e:#}", r.pd.plan.units[i].desc));
                    r.failed.store(true, Ordering::SeqCst);
                    UState::Failed
                }
            };
            r.states[i].send_replace(st);
        }));
    }
    for t in tasks.0.iter_mut() {
        t.await.context("a unit task panicked")?;
    }
    if run.inconsistent.load(Ordering::SeqCst) {
        return Ok(Finish::Retry);
    }
    let ok = !run.failed.load(Ordering::SeqCst)
        && (0..n).all(|i| matches!(*run.states[i].borrow(), UState::Done));
    if matches!(plan.message_format, rp::MessageFormat::Json { .. }) {
        c2.out.raw_out(format!("{{\"reason\":\"build-finished\",\"success\":{ok}}}\n").as_bytes());
    }
    if !ok {
        return Ok(Finish::Code(101));
    }

    if run.verify {
        let all = run.verify_mismatches.lock().clone();
        // A file that differs but that no unit of this build read (an
        // autoconf `config.log` left in OUT_DIR, say) cannot have changed
        // anything; only differences someone consumed count.
        let read_by_someone = |path: &[u8]| -> bool {
            run.unit_keys.iter().filter_map(|k| k.get()).any(|k| {
                d.engine.traces.traces(k).iter().any(|(_, t)| t.obs.iter().any(|(p, _)| p.as_ref() == path))
            })
        };
        let mut bad: Vec<String> = Vec::new();
        for (msg, paths) in &all {
            if paths.is_empty() || paths.iter().any(|p| read_by_someone(p)) {
                bad.push(msg.clone());
            } else {
                c2.out.note(&format!("verify: {msg}; nothing in this build reads those files, so this is not drift"));
            }
        }
        for m in &bad {
            c2.out.warn(&format!("verify: {m}"));
        }
        let unreproducible = std::mem::take(&mut *run.verify_unreproducible.lock());
        for m in &unreproducible {
            c2.out.note(&format!("verify: not compared: {m}"));
        }
        c2.out.note(&format!(
            "verify: {} cached unit(s) re-run cleanly, {} differed, {} not reproducible by the tools themselves",
            run.verified.load(Ordering::Relaxed),
            bad.len(),
            unreproducible.len()
        ));
        d.stats.verify_units.fetch_add(run.verified.load(Ordering::Relaxed), Ordering::Relaxed);
        d.stats.verify_mismatches.fetch_add(bad.len() as u64, Ordering::Relaxed);
    }
    // cargo's unused_dependencies lint, when a local crate has an unused
    // extern: the planner is run once more with rustc's reports and its
    // post-build lint speaks for itself.
    {
        let unused = std::mem::take(&mut run.ui.lock().unused_externs);
        // Only when cargo would say something: an extern of a local package
        // that every unit of the same dependency kind left unused (cargo's
        // `dep_kind_of`; dev-dependencies are never linted).
        let mut groups: HashMap<(&str, &str), Option<std::collections::BTreeSet<String>>> = HashMap::new();
        for (i, line) in &unused {
            let u = &plan.units[*i];
            if !u.is_local {
                continue;
            }
            let target: serde_json::Value = serde_json::from_str(&u.target_json).unwrap_or_default();
            let profile: serde_json::Value = serde_json::from_str(&u.profile_json).unwrap_or_default();
            let kinds: Vec<&str> = target["kind"].as_array().map(|a| a.iter().filter_map(|k| k.as_str()).collect()).unwrap_or_default();
            let dep_kind = if kinds.contains(&"custom-build") {
                "build"
            } else if kinds.contains(&"bin") {
                "normal"
            } else if kinds.iter().any(|k| matches!(*k, "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro"))
                && profile["test"] != serde_json::Value::Bool(true)
            {
                "normal"
            } else {
                continue;
            };
            let names: std::collections::BTreeSet<String> = serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|v| v["unused_extern_names"].as_array().map(|a| a.iter().filter_map(|n| n.as_str().map(String::from)).collect()))
                .unwrap_or_default();
            let g = groups.entry((u.pkg_id.as_str(), dep_kind)).or_default();
            match g {
                Some(set) => set.retain(|n| names.contains(n)),
                None => *g = Some(names),
            }
        }
        let any = groups.values().any(|g| g.as_ref().is_some_and(|s| !s.is_empty()));
        if any {
            let map: BTreeMap<usize, (&str, &str)> =
                unused.iter().map(|(i, l)| (*i, (plan.units[*i].desc.as_str(), l.as_str()))).collect();
            let json = serde_json::to_string(&map)?;
            if let PlanOutcome::Ok(lint) = plan_with(d, c, &p, &plan_args, &rp::split_env(rp::UNUSED_ENV, &json)).await? {
                // Whatever the planner said while loading the workspace was
                // shown already; what is new is the lint.
                let before = String::from_utf8_lossy(&planned.stderr).into_owned();
                let after = String::from_utf8_lossy(&lint.stderr).into_owned();
                let new = match after.strip_prefix(before.as_str()) {
                    Some(rest) => rest.to_string(),
                    None => {
                        let seen: HashSet<&str> = before.lines().collect();
                        after.lines().filter(|l| !seen.contains(l)).map(|l| format!("{l}\n")).collect()
                    }
                };
                c2.out.err_text(&new);
            }
        }
    }
    phase(started, c, "units answered");
    let incompat = run.future_incompat_summary().await?;
    let finished = || {
        c2.out.status("Finished", &format!("{}{}", plan.finished, elapsed(started.elapsed())));
        for (kind, text) in &incompat {
            match kind {
                'w' => c2.out.warn(text),
                'h' => c2.out.help(text),
                _ => c2.out.note(text),
            }
        }
    };
    if plan.timings {
        run.write_timings(started);
    }
    if run.need.is_some() {
        // A check leaves nothing in the target directory and has no steps.
        finished();
        return Ok(Finish::Code(0));
    }
    // Place what was asked for into the real target directory.
    run.materialize_roots();
    run.write_depinfo();
    phase(started, c, "results listed for the client");
    finished();

    let done = run.steps().await;
    phase(started, c, "done");
    done
}

struct UnitObserver {
    run: Arc<Run>,
    i: usize,
    /// The command as cargo would display it with `-v`.
    shown: String,
}

impl Observer for UnitObserver {
    fn miss(&self) {
        self.run.note_working_on(self.i);
        if self.run.c.out.verbose {
            self.run.c.out.status("Running", &self.shown);
        }
    }

    fn wants_explanation(&self) -> bool {
        self.run.explain
    }

    fn explain(&self, why: &str) {
        self.run.c.out.note(&format!("{} runs because {why}", self.run.pd.plan.units[self.i].desc));
    }

    fn stdout(&self, data: &[u8]) {
        let u = &self.run.pd.plan.units[self.i];
        match u.kind {
            rp::UnitKind::RunScript => {
                if self.run.pd.plan.extra_verbose {
                    if let Some(s) = &u.script {
                        for line in String::from_utf8_lossy(data).lines() {
                            self.run.c.out.out_text(&format!("{}{}\n", s.vv_prefix, line));
                        }
                    }
                }
            }
            _ => self.run.c.out.out_text(&String::from_utf8_lossy(data)),
        }
    }

    fn stderr_line(&self, line: &[u8], live: Option<&dyn Live>) {
        let u = &self.run.pd.plan.units[self.i];
        match u.kind {
            rp::UnitKind::RunScript => {
                if self.run.pd.plan.extra_verbose {
                    if let Some(s) = &u.script {
                        self.run.c.out.err_text(&format!("{}{}\n", s.vv_prefix, String::from_utf8_lossy(line)));
                    }
                }
            }
            _ => self.run.compiler_line(self.i, &String::from_utf8_lossy(line), live),
        }
    }
}

struct TestObserver {
    out: Arc<Client>,
    explain: bool,
    label: String,
}

impl Observer for TestObserver {
    fn wants_explanation(&self) -> bool {
        self.explain
    }
    fn explain(&self, why: &str) {
        self.out.out.note(&format!("{} runs because {why}", self.label));
    }
    fn stdout(&self, data: &[u8]) {
        self.out.out.raw_out(self.out.out.tr.text(&String::from_utf8_lossy(data)).as_bytes());
    }
    fn stderr_line(&self, line: &[u8], _live: Option<&dyn Live>) {
        let mut l = self.out.out.tr.text(&String::from_utf8_lossy(line));
        l.push('\n');
        self.out.out.raw_err(l.as_bytes());
    }
}

/// The value following `flag` (either `flag value` or `flag=value`).
fn arg_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == flag {
            return it.next().map(|s| s.as_str());
        }
        if let Some(rest) = a.strip_prefix(flag) {
            if let Some(v) = rest.strip_prefix('=') {
                return Some(v);
            }
        }
    }
    None
}

/// Variables a rustc invocation read through `env!`/`option_env!`, from its
/// dep-info output.
fn rustc_env_deps(r: &ActionResult, store: &Store) -> Vec<String> {
    let mut out = Vec::new();
    for (path, e) in &r.outputs.entries {
        if !path.ends_with(b".d") {
            continue;
        }
        let MEntry::File { hash, .. } = e else { continue };
        let Ok(text) = store.read(hash) else { continue };
        for line in String::from_utf8_lossy(&text).lines() {
            if let Some(rest) = line.strip_prefix("# env-dep:") {
                let name = rest.split('=').next().unwrap_or(rest);
                if !name.is_empty() && !out.iter().any(|n| n == name) {
                    out.push(name.to_string());
                }
            }
        }
    }
    out
}

impl Run {
    fn plan(&self) -> &rp::Plan {
        &self.pd.plan
    }

    /// The unit is compiled for its metadata only in this request.
    fn meta_only(&self, i: usize) -> bool {
        let u = &self.plan().units[i];
        self.need.as_ref().is_some_and(|n| n[i] == LEVEL_META) && u.kind == rp::UnitKind::Compile && u.cmd.is_some()
    }

    async fn wait_dep(&self, dep: &rp::Dep) -> bool {
        let j = dep.unit as usize;
        let meta_ok = dep.only_rmeta && (self.plan().units[j].rmeta_early || self.meta_only(j));
        let mut rx = self.states[j].subscribe();
        loop {
            let st = *rx.borrow_and_update();
            match st {
                UState::Done => return true,
                UState::Meta if meta_ok => return true,
                UState::Failed | UState::Skipped => return false,
                _ => {}
            }
            if rx.changed().await.is_err() {
                return false;
            }
        }
    }

    fn action_env(&self, cmd: &rp::Cmd) -> BTreeMap<Vec<u8>, Vec<u8>> {
        let mut env: BTreeMap<Vec<u8>, Vec<u8>> =
            self.base_env.iter().map(|(k, v)| (bytes(k), bytes(v))).collect();
        for (k, v) in &cmd.env {
            match v {
                Some(v) => {
                    env.insert(bytes(k), bytes(v));
                }
                None => {
                    env.remove(k.as_bytes());
                }
            }
        }
        if let Some(cargo) = &self.cargo_override {
            env.insert(bytes("CARGO"), bytes(cargo));
        }
        env
    }

    fn view_for(
        &self,
        i: usize,
        include_self: bool,
        untraced: Vec<Vec<u8>>,
        seed: Option<Arc<OutputsLayer>>,
    ) -> Arc<dyn View> {
        let mut levels = if self.meta_only(i) { self.pd.meta_levels(i) } else { self.pd.levels(i) };
        if include_self {
            let mut lv = (*levels).clone();
            lv[i] = LEVEL_FULL;
            levels = Arc::new(lv);
        }
        Arc::new(BuildView {
            base: self.base.clone(),
            layer: self.layer.clone(),
            levels,
            untraced,
            seed,
            content: Some(self.d.engine.content.clone()),
        })
    }

    /// How many commits back from HEAD `commit` is, if it is HEAD or one
    /// of the recent ancestors the client named.
    fn commits_back(&self, commit: &[u8; 20]) -> Option<usize> {
        self.c.commits.iter().position(|c| c == commit)
    }

    async fn exec_unit(self: &Arc<Self>, i: usize) -> Result<UState> {
        let plan = self.plan();
        let u = &plan.units[i];
        if self.need.as_ref().is_some_and(|n| n[i] == LEVEL_NONE) {
            // Nothing in this request needs the unit.
            return Ok(UState::Done);
        }
        let meta_only = self.meta_only(i);
        if meta_only {
            let levels = self.pd.meta_levels(i);
            for (j, lv) in levels.iter().enumerate() {
                if *lv != LEVEL_NONE
                    && !self.wait_dep(&rp::Dep { unit: j as u32, only_rmeta: *lv == LEVEL_META }).await
                {
                    return Ok(UState::Skipped);
                }
            }
        } else {
            for dep in &u.deps {
                if !self.wait_dep(dep).await {
                    return Ok(UState::Skipped);
                }
            }
        }
        if self.pd.doc_dir[i].is_some() {
            // The documentation chain: after the previous run for the same
            // directory, however that one ended.
            let pos = self.pd.doc_pos[i].unwrap_or(0);
            // Later runs rewrite the files earlier ones wrote.
            self.layer.set_rank(i as u32, (plan.units.len() + pos) as u64);
            let before = &self.pd.doc_order[..pos];
            if let Some(&prev) = before.iter().rev().find(|&&j| self.pd.doc_dir[j] == self.pd.doc_dir[i]) {
                let mut rx = self.states[prev].subscribe();
                while matches!(*rx.borrow_and_update(), UState::Pending | UState::Meta) {
                    if rx.changed().await.is_err() {
                        break;
                    }
                }
            }
        }
        if u.needs_objects && !meta_only {
            // A linked unit needs the finished object code of everything
            // below it. A library two levels down may have let its dependent
            // start on its metadata alone and still be generating code, so
            // direct dependencies being done is not enough (cargo adds the
            // same transitive edges to its queue).
            let levels = self.pd.levels(i);
            for (j, lv) in levels.iter().enumerate() {
                if *lv == LEVEL_FULL && !self.wait_dep(&rp::Dep { unit: j as u32, only_rmeta: false }).await {
                    return Ok(UState::Skipped);
                }
            }
        }
        if self.failed.load(Ordering::SeqCst) && !plan.keep_going {
            return Ok(UState::Skipped);
        }
        let Some(cmd) = &u.cmd else { return Ok(UState::Done) };
        if u.kind == rp::UnitKind::Noop {
            return Ok(UState::Done);
        }
        let cmd = {
            let scripts = self.scripts.lock();
            expand::expand(cmd, &scripts, &plan.dylib_var, EnvRule::ScriptWins)?
        };
        let (kind, mut args) = match u.kind {
            rp::UnitKind::Compile => (ActionKind::Rustc, cmd.args.clone()),
            rp::UnitKind::Doc => (ActionKind::Rustdoc, cmd.args.clone()),
            _ => (ActionKind::BuildScript, cmd.args.clone()),
        };
        let mut mkdirs: Vec<Vec<u8>> = Vec::new();
        match u.kind {
            rp::UnitKind::RunScript => {
                if let Some(s) = &u.script {
                    mkdirs.push(bytes(&s.out_dir));
                }
            }
            _ => {
                for flag in ["--out-dir", "-o"] {
                    if let Some(dir) = arg_value(&args, flag) {
                        mkdirs.push(bytes(dir));
                    }
                }
            }
        }
        // The crate's incremental directory: not an input and not an
        // output of the build proper, but kept and shared (see `incr`).
        let incr_dir: Option<Vec<u8>> = match u.kind {
            rp::UnitKind::Compile => args
                .iter()
                .enumerate()
                .find(|(n, a)| a.starts_with("incremental=") && *n > 0 && args[*n - 1] == "-C")
                .map(|(_, a)| bytes(&a["incremental=".len()..])),
            _ => None,
        };
        if let Some(dir) = &incr_dir {
            mkdirs.push(dir.clone());
        }
        // The same action as the full build's, for looking up whether the
        // full build has already been done (its metadata is then at hand).
        let full_argv: Option<Vec<Vec<u8>>> = meta_only.then(|| {
            let mut v = vec![bytes(&cmd.program)];
            v.extend(args.iter().map(|a| bytes(a)));
            v
        });
        if meta_only {
            if self.pd.can_meta(i) {
                // Everything up to and including metadata exactly as in the
                // full build, then stop.
                args.push("-Zno-codegen".into());
            } else {
                // A linked target has no dependents; type-check it the way
                // `cargo check` does.
                // `cargo check` does: metadata out, libraries in as metadata.
                let mut after_extern = false;
                for a in args.iter_mut() {
                    if let Some(kinds) = a.strip_prefix("--emit=") {
                        let kinds: Vec<&str> =
                            kinds.split(',').map(|k| if k == "link" { "metadata" } else { k }).collect();
                        *a = format!("--emit={}", kinds.join(","));
                    } else if after_extern {
                        if let Some(stem) = a.strip_suffix(".rlib") {
                            *a = format!("{stem}.rmeta");
                        }
                    }
                    after_extern = a == "--extern";
                }
            }
        }
        let mut argv = vec![bytes(&cmd.program)];
        argv.extend(args.iter().map(|a| bytes(a)));
        let spec = ActionSpec {
            kind,
            argv,
            env: self.action_env(&cmd),
            cwd: bytes(cmd.cwd.as_deref().unwrap_or(&plan.cwd)),
            net: if u.kind == rp::UnitKind::RunScript && self.policy.build_script_network(&u.pkg_name) {
                NetMode::Host
            } else {
                NetMode::Loopback
            },
            mkdirs,
            root_id: self.root_id,
            discard: incr_dir.iter().cloned().collect(),
            label: u.desc.clone(),
        };
        let crate_key = spec.weak_key();
        let wt = self.c.worktree.clone();
        let mut seed = match &incr_dir {
            Some(_) => self.d.incr.pick(&crate_key, &wt, &|c| self.commits_back(c)),
            None => None,
        };

        let script = u.script.clone();
        let parser_d = self.d.clone();
        let declared_env: Arc<dyn Fn(&ActionResult, &Store) -> Vec<String> + Send + Sync> = match (&u.kind, script) {
            (rp::UnitKind::RunScript, Some(s)) => Arc::new(move |r, store| {
                if !r.success() {
                    return Vec::new();
                }
                let Ok(stdout) = store.read(&r.stdout) else { return Vec::new() };
                match &*parser_d.parser.parse(&s, &stdout) {
                    Ok(o) => o.rerun_if_env_changed.clone(),
                    Err(_) => Vec::new(),
                }
            }),
            _ => Arc::new(rustc_env_deps),
        };
        // Only successes are recorded. A failure can come from something no
        // trace captures (a full disk, memory pressure, a bug in rargo), and
        // replaying such a failure forever would be exactly the kind of wrong
        // answer that makes people delete caches. Concurrent identical
        // failures are still shared through in-flight deduplication.
        let cacheable: Arc<dyn Fn(&ActionResult) -> bool + Send + Sync> = Arc::new(|r| r.success());
        let mut from_full = match (&full_argv, self.verify) {
            (Some(argv), false) => self.d.engine.cached(&Request {
                spec: ActionSpec { argv: argv.clone(), ..spec.clone() },
                view: self.view_for(i, false, incr_dir.iter().cloned().collect(), None),
                client_env: self.c.env.clone(),
                uid: self.c.uid,
                gid: self.c.gid,
                prio: self.c.prio,
                no_cache: false,
                seed_blobs: Default::default(),
                cacheable: cacheable.clone(),
                declared_env: declared_env.clone(),
                observer: Arc::new(UnitObserver { run: self.clone(), i, shown: display_cmd(&cmd.program, &args) }),
            }),
            _ => None,
        };
        self.times.lock()[i].start = Some(std::time::Instant::now());
        let outcome = loop {
            if let Some(o) = from_full.take() {
                break o;
            }
            let seed_layer = seed.as_ref().map(|s| {
                let l = OutputsLayer::new();
                l.add_manifest(0, LEVEL_FULL, &s.manifest);
                Arc::new(l)
            });
            let outcome = self
                .d
                .engine
                .obtain(Request {
                    spec: spec.clone(),
                    view: self.view_for(i, false, incr_dir.iter().cloned().collect(), seed_layer),
                    client_env: self.c.env.clone(),
                    uid: self.c.uid,
                    gid: self.c.gid,
                    prio: self.c.prio,
                    no_cache: false,
                    seed_blobs: seed.as_ref().map(|s| s.blobs.clone()).unwrap_or_default(),
                    cacheable: cacheable.clone(),
                    declared_env: declared_env.clone(),
                    observer: Arc::new(UnitObserver {
                        run: self.clone(),
                        i,
                        shown: display_cmd(&cmd.program, &args),
                    }),
                })
                .await;
            let outcome = match outcome {
                // Files of the seed were on a worker that has gone: the
                // state is lost, the next best one will do.
                Err(e) if e.downcast_ref::<crate::engine::SeedLost>().is_some() && seed.is_some() => {
                    if let Some(lost) = seed.take() {
                        self.d.incr.forget(&crate_key, lost.id);
                    }
                    seed = self.d.incr.pick(&crate_key, &wt, &|c| self.commits_back(c));
                    continue;
                }
                other => other?,
            };
            let r = &outcome.result;
            // A compiler that crashed (signal, or the exit code of an
            // internal error) while reading someone's incremental state may
            // have crashed because of that state: drop it and compile clean.
            let crashed = r.signal().is_some() || r.code() == Some(101);
            if crashed && !outcome.cached {
                if let Some(bad) = seed.take() {
                    self.d.incr.discard(&crate_key, bad.id);
                    continue;
                }
            }
            break outcome;
        };
        {
            let st = &outcome.result.stats;
            let mut times = self.times.lock();
            let t = &mut times[i];
            t.end = Some(std::time::Instant::now());
            t.cached = outcome.cached;
            (t.wall_us, t.cpu_us, t.rss_kb) = (st.wall_us, st.cpu_us, st.max_rss_kb);
        }
        // After the run: a first run is what reveals declared variables.
        let _ = self.unit_keys[i].set(self.d.engine.effective_key(&spec, &self.c.env));
        if self.verify && outcome.cached && outcome.result.success() {
            // The cached result claims to be what this action produces from
            // these inputs. Run it again from nothing (no cache, no
            // incremental seed) and see.
            let clean = self
                .d
                .engine
                .obtain(Request {
                    spec: spec.clone(),
                    view: self.view_for(i, false, incr_dir.iter().cloned().collect(), None),
                    client_env: self.c.env.clone(),
                    uid: self.c.uid,
                    gid: self.c.gid,
                    prio: self.c.prio,
                    no_cache: true,
                    seed_blobs: Default::default(),
                    cacheable: Arc::new(|_| false),
                    declared_env: Arc::new(|_, _| Vec::new()),
                    observer: Arc::new(NullObserver),
                })
                .await?;
            self.verified.fetch_add(1, Ordering::Relaxed);
            let (a, b) = (&outcome.result.outputs.entries, &clean.result.outputs.entries);
            if !clean.result.success() {
                self.verify_mismatches.lock().push((
                    format!("{}: cached as a success, but a clean run fails ({})", u.desc, describe_status(clean.result.status)),
                    Vec::new(),
                ));
            } else if u.kind == rp::UnitKind::RunScript && {
                // Compared as cargo reads it (the parsed instructions), not
                // byte for byte: build scripts also print chatter that
                // differs between runs and that cargo ignores.
                let parsed = |h: &Hash| -> Option<String> {
                    let out = self.d.store().read(h).ok()?;
                    let s = u.script.as_ref()?;
                    match &*self.d.parser.parse(s, &out) {
                        Ok(o) => Some(format!("{o:?}")),
                        Err(e) => Some(format!("error: {e}")),
                    }
                };
                parsed(&outcome.result.stdout) != parsed(&clean.result.stdout)
            } {
                // What a build script prints is what cargo acts on.
                // Say which lines: the first few `cargo:` lines only one of
                // the two runs printed.
                let lines = |h: &Hash| -> Vec<String> {
                    let out = self.d.store().read(h).unwrap_or_default();
                    String::from_utf8_lossy(&out).lines().filter(|l| l.starts_with("cargo:")).map(|l| l.to_string()).collect()
                };
                let (cached, clean_lines) = (lines(&outcome.result.stdout), lines(&clean.result.stdout));
                let only = |a: &[String], b: &[String]| -> Vec<String> {
                    a.iter().filter(|l| !b.contains(l)).take(2).map(|l| self.c.out.tr.text(l).chars().take(200).collect()).collect()
                };
                let sorted = |v: &[String]| {
                    let mut v = v.to_vec();
                    v.sort();
                    v
                };
                if sorted(&cached) == sorted(&clean_lines) {
                    // A script that compiles C in parallel prints as the
                    // compilers finish.
                    self.verify_unreproducible
                        .lock()
                        .push(format!("{}: prints the same instructions to cargo in a different order from run to run", u.desc));
                } else {
                    self.verify_mismatches.lock().push((
                        format!(
                            "{}: a clean run prints different instructions to cargo (cached only: {:?}; clean only: {:?})",
                            u.desc,
                            only(&cached, &clean_lines),
                            only(&clean_lines, &cached)
                        ),
                        Vec::new(),
                    ));
                }
            } else if a != b {
                // rustc's incremental mode names each codegen unit's files
                // (objects inside an rlib, split debug info beside it) after
                // the compilation session, and an object names its debug
                // info file. Outputs that differ only in that session
                // component are the same result.
                let store = self.d.store().clone();
                let neutral = incr_dir.is_some();
                let stable_path = |p: &[u8]| -> Vec<u8> {
                    match p.iter().rposition(|&c| c == b'/') {
                        Some(cut) if neutral => {
                            let mut v = p[..=cut].to_vec();
                            v.extend_from_slice(crate::ar::stable_member_name(&String::from_utf8_lossy(&p[cut + 1..])).as_bytes());
                            v
                        }
                        _ => p.to_vec(),
                    }
                };
                let cached_by: BTreeMap<Vec<u8>, (&Vec<u8>, &MEntry)> = a.iter().map(|(p, e)| (stable_path(p), (p, e))).collect();
                let clean_by: BTreeMap<Vec<u8>, (&Vec<u8>, &MEntry)> = b.iter().map(|(p, e)| (stable_path(p), (p, e))).collect();
                let contents = |x: &MEntry, y: &MEntry| -> Option<(Vec<u8>, Vec<u8>)> {
                    let (MEntry::File { hash: hx, .. }, MEntry::File { hash: hy, .. }) = (x, y) else { return None };
                    Some((store.read(hx).ok()?, store.read(hy).ok()?))
                };
                // The identifier that ties an object to its split debug
                // info covers the session name as well: learn, from each
                // `.dwo` pair, which identifier stands for which.
                let mut ids = crate::ar::DwoIds::default();
                let mut odd_dwo: HashSet<&Vec<u8>> = HashSet::new();
                if neutral {
                    for (k, (_, e)) in cached_by.iter().filter(|(k, _)| k.ends_with(b".rcgu.dwo")) {
                        let Some((_, e2)) = clean_by.get(k) else { continue };
                        if e != e2 && !contents(e, e2).is_some_and(|(x, y)| ids.learn(&x, &y)) {
                            odd_dwo.insert(k);
                        }
                    }
                }
                let same = |k: &Vec<u8>, x: &MEntry, y: &MEntry| -> bool {
                    if x == y {
                        return true;
                    }
                    neutral && !odd_dwo.contains(k) && contents(x, y).is_some_and(|(bx, by)| ids.same(&bx, &by))
                };
                let differing: Vec<Vec<u8>> = cached_by
                    .iter()
                    .filter(|(k, (_, e))| !clean_by.get(*k).is_some_and(|(_, e2)| same(k, e, e2)))
                    .map(|(_, (p, _))| (*p).clone())
                    .chain(clean_by.iter().filter(|(k, _)| !cached_by.contains_key(*k)).map(|(_, (p, _))| (*p).clone()))
                    .collect();
                // Two ways in which the compiler's own output varies from
                // run to run, both seen with stock cargo alone (D55):
                // dependency information that lists the same files in
                // another order (a procedural macro that tracks files in
                // hash order; metadata and object code follow), and, with
                // split debug info, anything linked (the linker's tables
                // are laid out by names that include the session).
                let reordered_inputs = differing.iter().filter(|p| p.ends_with(b".d")).any(|p| {
                    let k = stable_path(p);
                    let (Some((_, x)), Some((_, y))) = (cached_by.get(&k), clean_by.get(&k)) else { return false };
                    let Some((bx, by)) = contents(x, y) else { return false };
                    let words = |b: &[u8]| {
                        let mut w: Vec<Vec<u8>> = b.split(|c| c.is_ascii_whitespace()).filter(|w| !w.is_empty()).map(|w| w.to_vec()).collect();
                        w.sort();
                        w
                    };
                    bx != by && words(&bx) == words(&by)
                });
                let compiled = |p: &[u8]| [&b".rlib"[..], b".rmeta", b".d", b".dwo", b".o"].iter().any(|e| p.ends_with(e));
                let split_debug = a.iter().any(|(p, _)| p.ends_with(b".rcgu.dwo"));
                let only_linked = u.needs_objects && split_debug && differing.iter().all(|p| !compiled(p));
                if !differing.is_empty() && (reordered_inputs || only_linked) {
                    self.verify_unreproducible.lock().push(format!(
                        "{}: {}",
                        u.desc,
                        if reordered_inputs {
                            "reports the same input files in a different order from run to run, and its output differs with that"
                        } else {
                            "is linked with split debug info, which the compiler and linker do not reproduce byte for byte"
                        }
                    ));
                } else if !differing.is_empty() {
                    let shown: Vec<String> =
                        differing.iter().take(3).map(|p| self.c.out.tr.path(&String::from_utf8_lossy(p))).collect();
                    // How the first of them differs, for whoever has to
                    // decide what kind of difference this is.
                    let how = differing
                        .first()
                        .and_then(|p| {
                            let k = stable_path(p);
                            let ((_, x), (_, y)) = (cached_by.get(&k)?, clean_by.get(&k)?);
                            let (bx, by) = contents(x, y)?;
                            Some(crate::ar::describe_difference(&bx, &by))
                        })
                        .unwrap_or_else(|| "present in only one of the two".to_string());
                    self.verify_mismatches.lock().push((
                        format!(
                            "{}: a clean run produces different output than the cached one ({}; the first: {how})",
                            u.desc,
                            shown.join(", ")
                        ),
                        differing,
                    ));
                }
            }
        }
        // The state is recorded whether its files are here or still on the
        // worker that ran the compile (see `incr`).
        if let (Some(dir), true, false, false) =
            (&incr_dir, outcome.result.success(), outcome.cached, outcome.side.is_empty())
        {
            let state = crate::incr::apply_changes(dir, seed.as_ref().map(|s| &*s.manifest), &outcome.side);
            if !state.entries.is_empty() {
                self.d.incr.save(&crate_key, &wt, self.c.commits.first().copied(), &state);
                // Workers get it in the background (what a worker made
                // itself it already holds, and nothing is sent).
                if let Some(remote) = self.d.engine.remote.get() {
                    let mut blobs: Vec<(Hash, u64)> = state
                        .entries
                        .iter()
                        .filter_map(|(_, e)| match e {
                            MEntry::File { hash, size, .. } => Some((*hash, *size)),
                            _ => None,
                        })
                        .collect();
                    blobs.sort();
                    blobs.dedup();
                    remote.offer_state((crate_key, wt.clone()), Arc::new(blobs));
                }
            }
        }
        let r = outcome.result.clone();
        let _ = self.results[i].set((r.clone(), outcome.cached));

        if !r.success() {
            self.failed.store(true, Ordering::SeqCst);
            self.report_failure(i, &cmd, &r);
            return Ok(UState::Failed);
        }

        // What was announced early must be what the unit finally produced.
        if let Some(early) = self.early.lock().get(&i) {
            for (path, e) in early {
                if r.outputs.get(path) != Some(e) {
                    self.inconsistent.store(true, Ordering::SeqCst);
                    self.failed.store(true, Ordering::SeqCst);
                    return Ok(UState::Failed);
                }
            }
        }
        if u.kind == rp::UnitKind::RunScript {
            if !self.finish_script(i, &r).await? {
                self.failed.store(true, Ordering::SeqCst);
                return Ok(UState::Failed);
            }
        }
        let meta = &self.pd.rmeta_out[i];
        if meta_only {
            // Only the metadata is this unit's result here; whatever else
            // the compiler left behind is not the real thing.
            let early: Vec<_> = r.outputs.entries.iter().filter(|(p, _)| meta.contains(p)).cloned().collect();
            self.layer.add(i as u32, LEVEL_META, &early);
            self.finish_unit_messages(i, &r, outcome.cached);
            return Ok(UState::Done);
        }
        if meta.is_empty() {
            self.layer.add_manifest(i as u32, LEVEL_FULL, &r.outputs);
        } else {
            let (early, late): (Vec<_>, Vec<_>) =
                r.outputs.entries.iter().cloned().partition(|(p, _)| meta.contains(p));
            self.layer.add(i as u32, LEVEL_META, &early);
            self.layer.add(i as u32, LEVEL_FULL, &late);
        }
        // cargo links some outputs to friendlier names (`target/debug/app`);
        // later units and tests refer to those names (`CARGO_BIN_EXE_*`).
        let mut uplifted: Vec<(Vec<u8>, MEntry)> = Vec::new();
        for o in &u.outputs {
            if let Some(e) = r.outputs.get(o.path.as_bytes()) {
                for dest in o.hardlink.iter().chain(o.export_path.iter()) {
                    uplifted.push((bytes(dest), e.clone()));
                }
            }
        }
        if !uplifted.is_empty() {
            self.layer.add(i as u32, LEVEL_FULL, &uplifted);
        }
        self.apply_worktree_writes(&r.outputs);
        self.finish_unit_messages(i, &r, outcome.cached);
        Ok(UState::Done)
    }

    /// Parses a build script's output and records what cargo would have.
    async fn finish_script(self: &Arc<Self>, i: usize, r: &ActionResult) -> Result<bool> {
        let u = &self.plan().units[i];
        let Some(s) = u.script.clone() else { return Ok(true) };
        let stdout = self.d.store().read(&r.stdout)?;
        let d = self.d.clone();
        let s2 = s.clone();
        let parsed = tokio::task::spawn_blocking(move || d.parser.parse(&s2, &stdout)).await?;
        let o = match &*parsed {
            Ok(o) => o.clone(),
            Err(msg) => {
                self.c.out.error(msg);
                return Ok(false);
            }
        };
        let has_error = o.log_messages.iter().any(|(is_err, _)| *is_err);
        if !o.log_messages.is_empty() && (u.show_warnings || has_error) {
            for (is_err, msg) in &o.log_messages {
                let text = format!("{}@{}: {}", u.pkg_name, u.pkg_version, msg);
                if *is_err {
                    self.c.out.error(&text);
                } else {
                    self.c.out.warn(&text);
                }
            }
        }
        if matches!(self.plan().message_format, rp::MessageFormat::Json { .. }) {
            let paths: Vec<&String> = o.library_paths.iter().map(|(_, p)| p).collect();
            let msg = format!(
                "{{\"reason\":\"build-script-executed\",\"package_id\":{},\"linked_libs\":{},\"linked_paths\":{},\"cfgs\":{},\"env\":{},\"out_dir\":{}}}\n",
                serde_json::to_string(&u.pkg_spec)?,
                serde_json::to_string(&o.library_links)?,
                serde_json::to_string(&paths)?,
                serde_json::to_string(&o.cfgs)?,
                serde_json::to_string(&o.env)?,
                serde_json::to_string(&s.out_dir)?,
            );
            self.c.out.raw_out(self.c.out.tr.text(&msg).as_bytes());
        }
        self.scripts.lock().insert(s.meta.clone(), Arc::new(o));
        if has_error {
            self.c.out.error("build script logged errors");
            return Ok(false);
        }
        Ok(true)
    }

    /// cargo's `note_working_on`: one status line per package.
    fn note_working_on(&self, i: usize) {
        let u = &self.plan().units[i];
        let mut ui = self.ui.lock();
        let (set, verb) = match u.verb {
            rp::Verb::Silent => return,
            rp::Verb::Documenting => (&mut ui.documented, "Documenting"),
            rp::Verb::Scraping => (&mut ui.scraped, "Scraping"),
            rp::Verb::Checking => (&mut ui.compiled, "Checking"),
            rp::Verb::Compiling if self.meta_only(i) => (&mut ui.compiled, "Checking"),
            rp::Verb::Compiling => (&mut ui.compiled, "Compiling"),
        };
        if set.insert(u.pkg_id.clone()) {
            self.c.out.status(verb, &u.pkg);
        }
    }

    /// `--timings`: the report goes where cargo puts its own.
    fn write_timings(&self, started: std::time::Instant) {
        let plan = self.plan();
        let times = self.times.lock().clone();
        let rows: Vec<crate::timings::Row> = plan
            .units
            .iter()
            .zip(times)
            .enumerate()
            .map(|(index, (u, time))| crate::timings::Row {
                runs: u.cmd.is_some() && u.kind != rp::UnitKind::Noop,
                name: format!("{} {}", u.pkg_name, u.desc),
                time,
                index,
                deps: u.deps.iter().map(|d| (d.unit as usize, d.only_rmeta)).collect(),
                needs_objects: u.needs_objects,
            })
            .collect();
        let unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let stamp = crate::timings::stamp(unix);
        let html = crate::timings::render(
            &format!("cargo {}", self.c.args.join(" ")),
            &stamp,
            started,
            started.elapsed(),
            self.d.engine.sched.tokens(),
            &rows,
        );
        let dir = self.c.out.tr.path(&format!("{}/cargo-timings", plan.target_dir));
        let dated = format!("{dir}/cargo-timing-{stamp}.html");
        self.c.deliver(
            Vec::new(),
            vec![
                Write::Text { path: dated.clone(), text: Some(html.clone()) },
                Write::Text { path: format!("{dir}/cargo-timing.html"), text: Some(html) },
            ],
        );
        self.c.out.status("Timing", &format!("report saved to {dated}"));
    }

    /// cargo's `save_and_display_report`: the warnings compilers gave about
    /// code a future Rust will reject go into the report file in the
    /// client's target directory, where `cargo report
    /// future-incompatibilities` finds them, and a closing message says so.
    /// Returns the lines to show: `('w' | 'h' | 'n', text)`.
    async fn future_incompat_summary(&self) -> Result<Vec<(char, String)>> {
        let plan = self.plan();
        let reports = std::mem::take(&mut self.ui.lock().future_incompat);
        if reports.is_empty() {
            return Ok(if plan.future_incompat_report {
                vec![('n', "0 dependencies had future-incompatible warnings".to_string())]
            } else {
                Vec::new()
            });
        }
        let tr = &self.c.out.tr;
        // Per package (`name@version`): what its units reported.
        let mut per_package: BTreeMap<String, String> = BTreeMap::new();
        // The packages themselves, once each, in a stable order.
        let mut pkgs: BTreeMap<String, &rp::Unit> = BTreeMap::new();
        for (&i, items) in &reports {
            let u = &plan.units[i];
            let spec = format!("{}@{}", u.pkg_name, u.pkg_version);
            let text = per_package.entry(spec).or_default();
            text.push_str(&format!(
                "The package `{}` currently triggers the following future incompatibility lints:\n",
                tr.text(&u.pkg)
            ));
            for item in items {
                // With the compiler's colours, as cargo keeps them; `cargo
                // report` strips them when its output is not a terminal.
                for l in tr.text(item).lines() {
                    text.push_str(&format!("> {l}\n"));
                }
            }
            pkgs.entry(u.pkg_id.clone()).or_insert(u);
        }
        let mut suggestions: Vec<String> = Vec::new();
        if !pkgs.values().all(|u| u.is_local) {
            // cargo would first look in the registry for newer versions of
            // the packages; rargo does not ask the registry here.
            let upstream: Vec<String> = pkgs
                .values()
                .map(|u| {
                    let spec = format!("{}@{}", u.pkg_name, u.pkg_version);
                    format!(
                        "  - {spec}\n  - repository: {}\n  - detailed warning command: `cargo report future-incompatibilities --id {} --package {spec}`",
                        u.repository.as_deref().unwrap_or("<not found>"),
                        rargo_proto::REPORT_ID,
                    )
                })
                .collect();
            suggestions.push(format!(
                "ensure the maintainers know of this problem (e.g. creating a bug report if needed)\n\
                 or even helping with a fix (e.g. by creating a pull request)\n{}",
                upstream.join("\n\n")
            ));
            suggestions.push(
                "use your own version of the dependency with the `[patch]` section in `Cargo.toml`\n\
                 For more information, see:\n\
                 https://doc.rust-lang.org/cargo/reference/overriding-dependencies.html#the-patch-section"
                    .to_string(),
            );
        }
        let mut suggestion = String::new();
        if !suggestions.is_empty() {
            suggestion.push_str("to solve this problem, you can try the following approaches:\n");
            for s in &suggestions {
                suggestion.push_str(&format!("\n- {s}\n"));
            }
        }
        let names: Vec<String> = pkgs.values().map(|u| tr.text(&u.pkg)).collect();
        let mut out = vec![(
            'w',
            format!(
                "the following packages contain code that will be rejected by a future version of Rust: {}",
                names.join(", ")
            ),
        )];
        // The report file is the client's; a build the server runs for
        // itself has nowhere to keep one.
        let Some(session) = &self.c.session else { return Ok(out) };
        let id = session
            .report(rargo_proto::FutureReport {
                path: tr.path(&format!("{}/.future-incompat-report.json", plan.build_dir)),
                per_package: per_package.into_iter().collect(),
                suggestion,
            })
            .await?;
        if plan.future_incompat_report {
            for s in &suggestions {
                out.push(('h', s.replace(rargo_proto::REPORT_ID, &id.to_string())));
            }
            out.push(('n', format!("this report can be shown with `cargo report future-incompatibilities --id {id}`")));
        } else if plan.future_incompat_display {
            out.push((
                'n',
                format!(
                    "to see what the problems were, use the option `--future-incompat-report`, \
                     or run `cargo report future-incompatibilities --id {id}`"
                ),
            ));
        } else {
            out.clear();
        }
        Ok(out)
    }

    /// One line of rustc or rustdoc stderr (cargo's `on_stderr_line_inner`).
    fn compiler_line(&self, i: usize, line: &str, live: Option<&dyn Live>) {
        let plan = self.plan();
        let u = &plan.units[i];
        if !line.starts_with('{') {
            self.c.out.err_text(&format!("{line}\n"));
            return;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            self.c.out.err_text(&format!("{line}\n"));
            return;
        };
        if let Some(items) = v.get("future_incompat_report") {
            // Collected into the report file and a closing warning, as
            // cargo does (`future_incompat_summary`).
            let rendered: Vec<String> = items
                .as_array()
                .map(|a| a.iter().filter_map(|it| it["diagnostic"]["rendered"].as_str().map(|s| s.to_string())).collect())
                .unwrap_or_default();
            self.ui.lock().future_incompat.insert(i, rendered);
            return;
        }
        let is_summary = |m: &str| {
            m.starts_with("aborting due to") || m.ends_with("warning emitted") || m.ends_with("warnings emitted")
        };
        let render = match plan.message_format {
            rp::MessageFormat::Human | rp::MessageFormat::Short => true,
            rp::MessageFormat::Json { render_diagnostics, .. } => render_diagnostics,
        };
        let diag = match (v.get("rendered").and_then(|r| r.as_str()), v.get("message").and_then(|m| m.as_str()), v.get("level").and_then(|l| l.as_str())) {
            (Some(rendered), Some(message), Some(level)) if v.get("children").is_some() => {
                Some((rendered, message, level))
            }
            _ => None,
        };
        if render {
            if let Some((rendered, message, level)) = diag {
                if is_summary(message) {
                    return;
                }
                let rendered = rendered.strip_suffix('\n').unwrap_or(rendered);
                let fixable = v["children"].as_array().is_some_and(|ch| {
                    ch.iter().any(|c| {
                        c["spans"].as_array().is_some_and(|sp| {
                            sp.iter().any(|s| s["suggestion_applicability"].as_str() == Some("MachineApplicable"))
                        })
                    })
                });
                let lint = !v["code"].is_null();
                let mut ui = self.ui.lock();
                let emitted = ui.seen_diag.insert(Hash::of(rendered.as_bytes()));
                if emitted {
                    self.c.out.err_text(&format!("{rendered}\n"));
                }
                let cnt = ui.counts.entry(i).or_default();
                match level {
                    "warning" => {
                        cnt.warnings_seen += 1;
                        cnt.total += 1;
                        if lint && u.is_local {
                            cnt.lints += 1;
                        }
                        if !emitted {
                            cnt.duplicates += 1;
                        } else if fixable {
                            cnt.fixable += 1;
                        }
                    }
                    "error" => {
                        cnt.errors += 1;
                        cnt.fix_disallowed = true;
                    }
                    _ => {}
                }
                return;
            }
        }
        // Artifact notifications: the `.rmeta` is ready before the rest.
        if let Some(artifact) = v.get("artifact").and_then(|a| a.as_str()) {
            if artifact.ends_with(".rmeta") && u.rmeta_early {
                if let Some(live) = live {
                    if let Some(entry) = live.ingest(artifact.as_bytes()) {
                        let ents = vec![(artifact.as_bytes().to_vec(), entry)];
                        self.layer.add(i as u32, LEVEL_META, &ents);
                        self.early.lock().entry(i).or_default().extend(ents);
                        self.times.lock()[i].meta = Some(std::time::Instant::now());
                        self.states[i].send_replace(UState::Meta);
                    }
                }
            }
            return;
        }
        if v.get("unused_extern_names").is_some() {
            // For cargo's unused_dependencies lint, after the build.
            self.ui.lock().unused_externs.insert(i, line.to_string());
            return;
        }
        // JSON mode: forward the compiler's message wrapped like cargo does.
        let Some((_, message, level)) = diag else { return };
        if is_summary(message) {
            return;
        }
        {
            let mut ui = self.ui.lock();
            let cnt = ui.counts.entry(i).or_default();
            match level {
                "warning" => cnt.warnings_seen += 1,
                "error" => cnt.errors += 1,
                _ => {}
            }
        }
        let ansi = matches!(plan.message_format, rp::MessageFormat::Json { ansi: true, .. });
        let message_json = if ansi {
            line.to_string()
        } else {
            // cargo re-serialises with the rendered text stripped of colour.
            let mut obj = v.clone();
            if let Some(r) = obj.get("rendered").and_then(|r| r.as_str()).map(strip_ansi) {
                obj["rendered"] = serde_json::Value::String(r);
            }
            let mut parts = vec![format!("\"rendered\":{}", obj["rendered"])];
            if let Some(map) = obj.as_object() {
                for (k, val) in map {
                    if k != "rendered" && k != "code" {
                        parts.push(format!("{}:{}", serde_json::Value::String(k.clone()), val));
                    }
                }
            }
            parts.push(format!("\"code\":{}", obj.get("code").cloned().unwrap_or(serde_json::Value::Null)));
            format!("{{{}}}", parts.join(","))
        };
        let msg = format!(
            "{{\"reason\":\"compiler-message\",\"package_id\":{},\"manifest_path\":{},\"target\":{},\"message\":{}}}\n",
            serde_json::Value::String(u.pkg_spec.clone()),
            serde_json::Value::String(u.manifest_path.clone()),
            u.target_json,
            message_json
        );
        self.c.out.raw_out(self.c.out.tr.text(&msg).as_bytes());
    }

    /// After a unit succeeded: warning summary and the artifact message.
    fn finish_unit_messages(&self, i: usize, r: &ActionResult, cached: bool) {
        let plan = self.plan();
        let u = &plan.units[i];
        let cnt = self.ui.lock().counts.get(&i).cloned().unwrap_or_default();
        if cnt.total > 0 {
            self.report_warning_count(u, &cnt);
        }
        if matches!(plan.message_format, rp::MessageFormat::Json { .. })
            && matches!(u.kind, rp::UnitKind::Compile | rp::UnitKind::Doc)
        {
            let mut filenames: Vec<&str> = Vec::new();
            let meta_only = self.meta_only(i);
            for o in &u.outputs {
                if r.outputs.get(o.path.as_bytes()).is_none() || (meta_only && o.flavor != "Rmeta") {
                    continue;
                }
                filenames.push(o.hardlink.as_deref().unwrap_or(&o.path));
            }
            let msg = format!(
                "{{\"reason\":\"compiler-artifact\",\"package_id\":{},\"manifest_path\":{},\"target\":{},\"profile\":{},\"features\":{},\"filenames\":{},\"executable\":{},\"fresh\":{}}}\n",
                serde_json::Value::String(u.pkg_spec.clone()),
                serde_json::Value::String(u.manifest_path.clone()),
                u.target_json,
                u.profile_json,
                serde_json::to_string(&u.features).unwrap_or_default(),
                serde_json::to_string(&filenames).unwrap_or_default(),
                serde_json::to_string(&u.executable).unwrap_or_default(),
                cached
            );
            self.c.out.raw_out(self.c.out.tr.text(&msg).as_bytes());
        }
    }

    /// cargo's `report_warning_count`.
    fn report_warning_count(&self, u: &rp::Unit, cnt: &Counts) {
        let mut msg = format!("{} generated ", u.desc);
        match cnt.total {
            1 => msg.push_str("1 warning"),
            n => msg.push_str(&format!("{n} warnings")),
        }
        match cnt.duplicates {
            0 => {}
            1 => msg.push_str(" (1 duplicate)"),
            n => msg.push_str(&format!(" ({n} duplicates)")),
        }
        if let (Some(hint), false, true) = (&u.fix_hint, cnt.fix_disallowed, cnt.fixable > 0) {
            let is_clippy = self.c.args.first().is_some_and(|a| a == "clippy");
            let command = if is_clippy { "cargo clippy --fix" } else { "cargo fix" };
            let s = if cnt.fixable > 1 { "s" } else { "" };
            msg.push_str(&format!(" (run `{command} --{hint}` to apply {} suggestion{s})", cnt.fixable));
        }
        if self.plan().deny_warnings && cnt.lints > 0 {
            self.c.out.error(&msg);
        } else {
            self.c.out.warn(&msg);
        }
    }

    fn report_failure(&self, i: usize, cmd: &rp::Cmd, r: &ActionResult) {
        let plan = self.plan();
        let u = &plan.units[i];
        // Warnings from the package's build scripts are shown with the error.
        {
            let scripts = self.scripts.lock();
            for meta in &u.script_metas {
                if let Some(o) = scripts.get(meta) {
                    for (is_err, m) in &o.log_messages {
                        let text = format!("{}@{}: {}", u.pkg_name, u.pkg_version, m);
                        if *is_err {
                            self.c.out.error(&text);
                        } else {
                            self.c.out.warn(&text);
                        }
                    }
                }
            }
        }
        let cnt = self.ui.lock().counts.get(&i).cloned().unwrap_or_default();
        if cnt.total > 0 {
            self.report_warning_count(u, &cnt);
        }
        let status = describe_status(r.status);
        let shown = display_cmd(&cmd.program, &cmd.args);
        match u.kind {
            rp::UnitKind::RunScript => {
                let indent = |h: &Hash| -> String {
                    let text = self.d.store().read(h).unwrap_or_default();
                    String::from_utf8_lossy(&text).lines().map(|l| format!("  {l}\n")).collect()
                };
                let pkg_descr = u.script.as_ref().map(|s| s.pkg_descr.as_str()).unwrap_or(&u.pkg);
                self.c.out.error(&format!(
                    "failed to run custom build command for `{pkg_descr}`\n\nCaused by:\n  process didn't exit successfully: {shown} ({status})\n  --- stdout\n{}  --- stderr\n{}",
                    indent(&r.stdout),
                    indent(&r.stderr)
                ));
            }
            _ => {
                let errors = match cnt.errors {
                    0 => String::new(),
                    1 => " due to 1 previous error".to_string(),
                    n => format!(" due to {n} previous errors"),
                };
                let warnings = match cnt.warnings_seen {
                    0 => String::new(),
                    1 => "; 1 warning emitted".to_string(),
                    n => format!("; {n} warnings emitted"),
                };
                let what = if u.kind == rp::UnitKind::Doc {
                    format!("could not document `{}`", u.pkg_name)
                } else {
                    format!("could not compile {}{errors}{warnings}", u.desc)
                };
                // cargo hides the command for plain "exit status 1 with
                // errors shown" unless asked to be verbose.
                let simple = r.code() == Some(1) && cnt.errors > 0;
                if simple && !self.c.out.verbose {
                    self.c.out.error(&what);
                } else {
                    self.c.out.error(&format!(
                        "{what}\n\nCaused by:\n  process didn't exit successfully: {shown} ({status})"
                    ));
                }
            }
        }
        let mut ui = self.ui.lock();
        if !ui.said_waiting && !plan.keep_going {
            ui.said_waiting = true;
            let others_running = self.d.engine.sched.snapshot().0 > 1;
            if others_running {
                self.c.out.warn("build failed, waiting for other jobs to finish...");
            }
        }
    }

    /// Files an action wrote into the worktree itself (outside the target
    /// directory) are written to the real worktree, as cargo would have let
    /// the process do directly.
    fn apply_worktree_writes(&self, m: &Manifest) {
        let plan = self.plan();
        apply_writes(&self.c, &self.c.out.tr, &[&plan.target_dir, &plan.build_dir], m)
    }

    /// The files cargo's `.d` file for root unit `root` lists (cargo's
    /// `output_depinfo`): what rustc's dep-info names for the unit and for
    /// every local unit below it, plus their build scripts'
    /// `rerun-if-changed` paths. Canonical paths. `None` when a dep-info file
    /// is missing, in which case cargo removes the `.d` file.
    fn depinfo_files(&self, root: usize) -> Option<std::collections::BTreeSet<PathBuf>> {
        let plan = self.plan();
        let store = self.d.store();
        let mut files = std::collections::BTreeSet::new();
        let mut seen = HashSet::new();
        let mut stack = vec![root];
        while let Some(j) = stack.pop() {
            if !seen.insert(j) {
                continue;
            }
            let u = &plan.units[j];
            if u.kind == rp::UnitKind::Compile {
                let cmd = u.cmd.as_ref()?;
                let out_dir = arg_value(&cmd.args, "--out-dir")?;
                let (r, _) = self.results[j].get()?;
                let (_, e) = r.outputs.entries.iter().find(|(p, _)| {
                    p.ends_with(b".d") && vpath_starts_with(p, out_dir.as_bytes())
                })?;
                let MEntry::File { hash, .. } = e else { return None };
                let text = store.read(hash).ok()?;
                let cwd = PathBuf::from(cmd.cwd.as_deref().unwrap_or(&plan.cwd));
                for f in parse_dep_info(&String::from_utf8_lossy(&text)) {
                    files.insert(normalize(&cwd.join(f)));
                }
            }
            if let Some(root_dir) = Path::new(&u.manifest_path).parent() {
                let scripts = self.scripts.lock();
                for meta in &u.script_metas {
                    if let Some(o) = scripts.get(meta) {
                        for f in &o.rerun_if_changed {
                            files.insert(if f.is_empty() { root_dir.to_path_buf() } else { normalize(&root_dir.join(f)) });
                        }
                    }
                }
            }
            for d in &u.deps {
                if plan.units[d.unit as usize].is_local {
                    stack.push(d.unit as usize);
                }
            }
        }
        Some(files)
    }

    /// Writes cargo's `.d` files next to the uplifted artifacts of the roots.
    fn write_depinfo(&self) {
        let plan = self.plan();
        let mut writes: Vec<(PathBuf, Option<String>)> = Vec::new();
        for &ri in &plan.roots {
            let ri = ri as usize;
            let u = &plan.units[ri];
            if u.kind != rp::UnitKind::Compile {
                continue;
            }
            let links: Vec<&str> = u
                .outputs
                .iter()
                .filter(|o| !matches!(o.flavor.as_str(), "DebugInfo" | "Auxiliary" | "Sbom" | "Unremap"))
                .filter_map(|o| o.hardlink.as_deref())
                .collect();
            if links.is_empty() {
                continue;
            }
            let files = self.depinfo_files(ri);
            // cargo's `render_filename`: relative to the base directory when
            // under it, as written otherwise.
            let base = plan.dep_info_basedir.as_ref().map(|b| PathBuf::from(self.c.out.tr.path(b)));
            let esc = |p: String| {
                let p = match &base {
                    Some(b) => match normalize(Path::new(&p)).strip_prefix(normalize(b)) {
                        Ok(rel) => rel.to_string_lossy().into_owned(),
                        Err(_) => p,
                    },
                    None => p,
                };
                p.replace(' ', "\\ ")
            };
            for link in links {
                let real = PathBuf::from(self.c.out.tr.path(link));
                let dest = real.with_extension("d");
                let text = files.as_ref().map(|files| {
                    let mut t = format!("{}:", esc(real.to_string_lossy().into_owned()));
                    for f in files {
                        t.push(' ');
                        t.push_str(&esc(self.c.out.tr.path(&f.to_string_lossy())));
                    }
                    t.push('\n');
                    t
                });
                writes.push((dest, text));
            }
        }
        let writes: Vec<Write> = writes
            .into_iter()
            .map(|(dest, text)| Write::Text { path: dest.to_string_lossy().into_owned(), text })
            .collect();
        self.c.deliver(Vec::new(), writes);
    }

    /// Sends the requested artifacts to the client's target directory.
    fn materialize_roots(&self) {
        let plan = self.plan();
        let tr = &self.c.out.tr;
        let mut files: Vec<Delivery> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut push = |files: &mut Vec<Delivery>, path: String, hash: &Hash, exec: bool, size: u64, own: bool| {
            if seen.insert(path.clone()) {
                files.push(Delivery { path, hash: hash.0, size, exec, own });
            }
        };
        // Units whose executables rargo runs itself in this request.
        let run_here: HashSet<u32> = plan
            .steps
            .iter()
            .filter_map(|s| match s {
                rp::Step::Exec { unit, .. } => Some(*unit),
                _ => None,
            })
            .collect();
        for &ri in &plan.roots {
            let u = &plan.units[ri as usize];
            let Some((r, _)) = self.results[ri as usize].get() else { continue };
            for o in &u.outputs {
                let Some(MEntry::File { hash, exec, size }) = r.outputs.get(o.path.as_bytes()) else { continue };
                // Names cargo links (it removes the old file first): the
                // client may share one copy between worktrees.
                for d in o.hardlink.iter().chain(o.export_path.iter()) {
                    push(&mut files, tr.path(d), hash, *exec, *size, false);
                }
                // An executable at its own path, where rustc writes it in
                // place. Wanted when the user asked for executables (test
                // binaries have no other name), not when rargo ran them
                // here. A file of its own: stock cargo's next compile
                // writes over it.
                if u.executable.as_deref() == Some(o.path.as_str()) && !run_here.contains(&ri) {
                    push(&mut files, tr.path(&o.path), hash, *exec, *size, true);
                }
            }
        }
        // Generated documentation: every file rustdoc wrote, the later run
        // winning where several wrote the same file.
        let mut docs: BTreeMap<&[u8], (Hash, bool, u64)> = BTreeMap::new();
        for &i in &self.pd.doc_order {
            let (Some(dir), Some((r, _))) = (&self.pd.doc_dir[i], self.results[i].get()) else { continue };
            for (path, e) in &r.outputs.entries {
                if let (MEntry::File { hash, exec, size }, true) = (e, vpath_starts_with(path, dir)) {
                    docs.insert(path, (*hash, *exec, *size));
                }
            }
        }
        for (path, (hash, exec, size)) in docs {
            // rustdoc rewrites its shared files in place on the next run.
            push(&mut files, tr.path(&String::from_utf8_lossy(path)), &hash, exec, size, true);
        }
        if files.is_empty() {
            return;
        }
        // What cargo leaves at the top of a target directory it creates.
        let mut writes = Vec::new();
        let mut tagged: HashSet<String> = HashSet::new();
        for dir in [&plan.target_dir, &plan.build_dir] {
            let dir = tr.path(dir);
            if tagged.insert(dir.clone()) {
                writes.push(Write::Text {
                    path: format!("{dir}/CACHEDIR.TAG"),
                    text: Some(rargo_proto::CACHEDIR_TAG.to_string()),
                });
            }
        }
        self.c.deliver(files, writes);
    }

    /// Runs what cargo does after the build: tests, doctests, `cargo run`.
    async fn steps(self: &Arc<Self>) -> Result<Finish> {
        let plan = self.plan();
        let out = &self.c.out;
        let mut failed: Vec<String> = Vec::new();
        for step in &plan.steps {
            match step {
                rp::Step::Status { verb, msg, when } => {
                    let show = match when {
                        rp::When::Always => true,
                        rp::When::Concise => !out.verbose,
                        rp::When::Verbose => out.verbose,
                    };
                    if show {
                        out.status(verb, msg);
                    }
                }
                rp::Step::Run { cmd, .. } => {
                    let cmd = {
                        let scripts = self.scripts.lock();
                        expand::expand(cmd, &scripts, &plan.dylib_var, EnvRule::CargoVarsWin)?
                    };
                    out.status("Running", &display_cmd(&cmd.program, &cmd.args));
                    // The program is the user's and runs on the user's
                    // machine: it gets the user's whole environment there
                    // plus what cargo adds.
                    return Ok(Finish::Exec(ExecRequest {
                        program: cmd.program.clone(),
                        args: cmd.args.clone(),
                        env: cmd.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                        cwd: self.c.cwd.clone(),
                        binds: vec![
                            (self.c.root.clone(), String::from_utf8_lossy(WORKTREE_AT).into_owned()),
                            (self.c.cargo_home.clone(), String::from_utf8_lossy(CARGO_HOME_AT).into_owned()),
                        ],
                    }));
                }
                rp::Step::Open { cmd } => {
                    // Whatever opens the documentation lives outside any
                    // sandbox and has to be given the real path.
                    let real = |s: &String| out.tr.path(s);
                    // The planner does not see the user's `$BROWSER` (it is
                    // not an input to any build); cargo's order is the
                    // configured browser, then `$BROWSER`, then the system
                    // opener, which is what the plan falls back to.
                    let program = match self.c.env.get("BROWSER") {
                        Some(b) if cmd.program == "xdg-open" && !b.is_empty() => b,
                        _ => real(&cmd.program),
                    };
                    return Ok(Finish::Exec(ExecRequest {
                        program,
                        args: cmd.args.iter().map(real).collect(),
                        env: Vec::new(),
                        cwd: self.c.cwd.clone(),
                        binds: vec![],
                    }));
                }
                rp::Step::Exec { kind, unit, cmd, rerun, harness } => {
                    let cmd = {
                        let scripts = self.scripts.lock();
                        expand::expand(cmd, &scripts, &plan.dylib_var, EnvRule::CargoVarsWin)?
                    };
                    if out.verbose {
                        out.status("Running", &display_cmd(&cmd.program, &cmd.args));
                    }
                    let mut argv = vec![bytes(&cmd.program)];
                    argv.extend(cmd.args.iter().map(|a| bytes(a)));
                    let akind = if *kind == rp::ExecKind::Doctest { ActionKind::Doctest } else { ActionKind::Test };
                    let spec = ActionSpec {
                        kind: akind,
                        argv,
                        env: self.action_env(&cmd),
                        cwd: bytes(cmd.cwd.as_deref().unwrap_or(&plan.cwd)),
                        net: if self.policy.test_network(&plan.units[*unit as usize].pkg_name) {
                            NetMode::Host
                        } else {
                            NetMode::Loopback
                        },
                        mkdirs: vec![],
                        root_id: self.root_id,
                        discard: vec![],
                        label: format!("test {}", plan.units[*unit as usize].desc),
                    };
                    let outcome = self
                        .d
                        .engine
                        .obtain(Request {
                            spec,
                            view: self.view_for(*unit as usize, true, vec![], None),
                            client_env: self.c.env.clone(),
                            uid: self.c.uid,
                            gid: self.c.gid,
                            prio: self.c.prio,
                            no_cache: self.no_cache_tests,
                            seed_blobs: Default::default(),
                            // A passing run is a fact about its inputs; a
                            // failing one may be a flake and is run again.
                            cacheable: Arc::new(|r| r.success()),
                            declared_env: Arc::new(|_, _| Vec::new()),
                            observer: Arc::new(TestObserver {
                                out: self.c.clone(),
                                explain: self.explain,
                                label: format!("test of {}", plan.units[*unit as usize].desc),
                            }),
                        })
                        .await?;
                    let r = &outcome.result;
                    if r.success() {
                        self.apply_worktree_writes(&r.outputs);
                        continue;
                    }
                    let which = match kind {
                        rp::ExecKind::Test => "test failed",
                        rp::ExecKind::Bench => "bench failed",
                        rp::ExecKind::Doctest => "doctest failed",
                    };
                    let simple = r.code() == Some(101);
                    if simple {
                        out.error(&format!("{which}, to rerun pass `{rerun}`"));
                    } else {
                        out.error(&format!(
                            "{which}, to rerun pass `{rerun}`\n\nCaused by:\n  process didn't exit successfully: {} ({})",
                            display_cmd(&cmd.program, &cmd.args),
                            describe_status(r.status)
                        ));
                        if *harness && !plan.nocapture {
                            out.note("test exited abnormally; to see the full output pass --no-capture to the harness.");
                        }
                    }
                    if !plan.no_fail_fast {
                        return Ok(Finish::Code(r.code().unwrap_or(101)));
                    }
                    failed.push(rerun.clone());
                }
            }
        }
        if !failed.is_empty() {
            let list: Vec<String> = failed.iter().map(|a| format!("    `{a}`")).collect();
            let head = match failed.len() {
                1 => "1 target failed".to_string(),
                n => format!("{n} targets failed"),
            };
            out.error(&format!("{head}:\n{}", list.join("\n")));
            return Ok(Finish::Code(101));
        }
        Ok(Finish::Code(0))
    }
}

/// Sends the worktree part of an action's outputs to the client's worktree.
fn apply_writes(c: &Client, tr: &Translator, target_dirs: &[&String], m: &Manifest) {
    let inside = |p: &[u8]| {
        vpath_starts_with(p, WORKTREE_AT)
            && p != WORKTREE_AT
            && !target_dirs.iter().any(|t| vpath_starts_with(p, t.as_bytes()))
            && !vpath_starts_with(p, &vjoin(WORKTREE_AT, b"target"))
            && !vpath_starts_with(p, &vjoin(WORKTREE_AT, b".git"))
            && !vpath_starts_with(p, &vjoin(WORKTREE_AT, rargo_proto::GIT_SIDE.as_bytes()))
    };
    let writes: Vec<Write> = m
        .entries
        .iter()
        .filter(|(p, _)| inside(p))
        .map(|(p, e)| {
            let path = tr.path(&String::from_utf8_lossy(p));
            match e {
                MEntry::Dir => Write::Dir { path },
                MEntry::Whiteout => Write::Remove { path },
                MEntry::Symlink(t) => Write::Symlink { path, target: t.clone() },
                MEntry::File { hash, exec, size } => Write::File { path, hash: hash.0, size: *size, exec: *exec },
            }
        })
        .collect();
    c.deliver(Vec::new(), writes);
}

/// The prerequisites named in a Makefile-style dep-info file (rustc's).
fn parse_dep_info(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        if line.starts_with('#') {
            continue;
        }
        // `target: a b\ c`: everything after the first unescaped `: `.
        let Some(at) = line.find(": ") else { continue };
        let mut cur = String::new();
        let mut chars = line[at + 2..].chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\\' if chars.peek() == Some(&' ') => {
                    cur.push(' ');
                    chars.next();
                }
                ' ' => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                }
                c => cur.push(c),
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
    }
    out
}

/// Removes `.` and `..` components without looking at the file system.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}
