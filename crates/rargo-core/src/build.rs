//! Handling one client request: snapshot, plan, execute, present.

use crate::buildview::*;
use crate::engine::*;
use crate::expand::{self, EnvRule, Scripts};
use crate::hash::Hash;
use crate::parse::Parser;
use crate::present::*;
use crate::proto::ExecRequest;
use crate::sandbox::NetMode;
use crate::sched::Priority;
use crate::snapshot::{RepoCache, Watcher, Worktree};
use crate::store::Store;
use crate::trace::*;
use crate::view::*;
use anyhow::{Context, Result, anyhow, bail};
use dashmap::DashMap;
use parking_lot::Mutex;
use rargo_plan as rp;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;
use tokio::sync::watch;

/// Where the planner writes its plan inside the sandbox.
const PLAN_PATH: &str = "/.rargo-plan";

pub struct Config {
    /// rargod's private directory (store, mount point, socket).
    pub state_dir: PathBuf,
    /// The forked cargo.
    pub planner: PathBuf,
}

#[derive(Default)]
pub struct DaemonStats {
    pub requests: AtomicU64,
    pub plans_decoded: AtomicU64,
}

/// State shared by all requests.
pub struct Daemon {
    pub cfg: Config,
    pub engine: Arc<Engine>,
    pub memo: Arc<HostMemo>,
    pub watcher: Arc<Watcher>,
    pub repos: RepoCache,
    pub worktrees: Mutex<HashMap<PathBuf, Arc<Worktree>>>,
    pub parser: Parser,
    pub incr: crate::incr::IncrStore,
    plans: DashMap<Hash, Arc<PlanData>>,
    /// Target directories seen per worktree, masked in later views.
    masks: DashMap<PathBuf, Vec<Vec<u8>>>,
    pub stats: DaemonStats,
}

/// One client request.
pub struct Client {
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Arc<HashMap<String, String>>,
    pub uid: u32,
    pub gid: u32,
    pub prio: Priority,
    pub out: Output,
}

/// A decoded plan with derived per-unit data.
pub struct PlanData {
    pub plan: rp::Plan,
    /// Unit links against its dependencies (needs their object code).
    linking: Vec<bool>,
    levels: Vec<OnceLock<Arc<Vec<u8>>>>,
    /// Sandbox paths of each unit's early-announced metadata outputs.
    meta_paths: Vec<Vec<Vec<u8>>>,
}

impl PlanData {
    fn new(plan: rp::Plan) -> PlanData {
        let n = plan.units.len();
        let linking = plan.units.iter().map(|u| u.needs_objects).collect();
        let meta_paths = plan
            .units
            .iter()
            .map(|u| {
                if u.rmeta_early {
                    u.outputs.iter().filter(|o| o.flavor == "Rmeta").map(|o| o.path.clone().into_bytes()).collect()
                } else {
                    Vec::new()
                }
            })
            .collect();
        PlanData { plan, linking, levels: (0..n).map(|_| OnceLock::new()).collect(), meta_paths }
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
    wt_root: PathBuf,
    head: Option<[u8; 20]>,
    policy: crate::policy::Policy,
}

fn bytes(s: &str) -> Vec<u8> {
    s.as_bytes().to_vec()
}

/// Variables passed from the client to every action. Everything else is
/// withheld unless an action declares that it reads it.
fn base_env(client: &HashMap<String, String>) -> BTreeMap<String, String> {
    const EXACT: &[&str] = &[
        "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "LANGUAGE", "TZ", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN",
        "SSL_CERT_FILE", "SSL_CERT_DIR", "RUSTFLAGS", "RUSTDOCFLAGS",
    ];
    const PREFIXES: &[&str] = &["CARGO_", "RUSTC", "RUSTDOC", "RUST_", "LC_", "NIX_", "__CARGO"];
    const DROP_PREFIXES: &[&str] = &[
        "CARGO_PKG_", "CARGO_MANIFEST_", "CARGO_CRATE_", "CARGO_BIN_", "CARGO_CFG_", "CARGO_FEATURE_",
        "CARGO_TERM_", "CARGO_DEP_",
    ];
    const DROP: &[&str] = &[
        "CARGO_HOME", "CARGO_MAKEFLAGS", "CARGO_PRIMARY_PACKAGE", "CARGO_TARGET_TMPDIR", "CARGO_SBOM_PATH",
        "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "CARGO_NET_OFFLINE", "CARGO_CACHE_RUSTC_INFO",
        "CARGO_RUSTC_CURRENT_DIR",
    ];
    let mut out = BTreeMap::new();
    for (k, v) in client {
        let wanted = EXACT.contains(&k.as_str()) || PREFIXES.iter().any(|p| k.starts_with(p));
        let dropped = DROP.contains(&k.as_str()) || DROP_PREFIXES.iter().any(|p| k.starts_with(p));
        if wanted && !dropped {
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

fn find_worktree_root(cwd: &Path) -> Result<PathBuf> {
    let mut cur = Some(cwd);
    while let Some(d) = cur {
        if d.join(".git").exists() {
            return Ok(d.to_path_buf());
        }
        cur = d.parent();
    }
    bail!("`{}` is not inside a git worktree; rargo needs one to take snapshots", cwd.display())
}

fn real_cargo_home(env: &HashMap<String, String>) -> PathBuf {
    match env.get("CARGO_HOME") {
        Some(h) if !h.is_empty() => PathBuf::from(h),
        _ => PathBuf::from(env.get("HOME").map(|s| s.as_str()).unwrap_or("/root")).join(".cargo"),
    }
}

impl Daemon {
    pub fn new(cfg: Config, engine: Arc<Engine>, watcher: Arc<Watcher>) -> Daemon {
        let parser = Parser::new(cfg.planner.clone());
        let incr = crate::incr::IncrStore::new(engine.store.clone());
        Daemon {
            incr,
            cfg,
            engine,
            memo: Arc::new(HostMemo::default()),
            watcher,
            repos: RepoCache::default(),
            worktrees: Mutex::new(HashMap::new()),
            parser,
            plans: DashMap::new(),
            masks: DashMap::new(),
            stats: DaemonStats::default(),
        }
    }

    fn worktree(&self, root: &Path) -> Result<Arc<Worktree>> {
        if let Some(w) = self.worktrees.lock().get(root) {
            return Ok(w.clone());
        }
        let w = Worktree::open(root, self.engine.content.clone(), self.watcher.clone(), &self.repos)?;
        self.worktrees.lock().insert(root.to_path_buf(), w.clone());
        Ok(w)
    }

    fn store(&self) -> &Arc<Store> {
        &self.engine.store
    }

    /// Removes blobs nothing refers to any more: outputs of traces that
    /// were displaced, superseded incremental states, captures of files
    /// that have since changed. Blobs used within `min_age_secs` are left
    /// alone so that running builds keep what they are using.
    pub fn collect_garbage(&self, min_age_secs: u64) -> (usize, u64) {
        let mut live = HashSet::new();
        self.engine.traces.mark(&mut live);
        self.incr.mark(&mut live);
        for wt in self.worktrees.lock().values() {
            wt.mark(&mut live);
        }
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
    for attempt in 0..3 {
        match run_request(&d, &c).await {
            Ok(Finish::Code(code)) => return Ok(code),
            Ok(Finish::Exec(e)) => return Err(e),
            Ok(Finish::Retry) if attempt < 2 => continue,
            Ok(Finish::Retry) => {
                c.out.error("the build kept producing inconsistent intermediate artifacts");
                return Ok(101);
            }
            Err(e) => {
                c.out.error(&format!("{e:#}"));
                return Ok(101);
            }
        }
    }
    Ok(101)
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
    root: PathBuf,
    /// HEAD commit of the worktree, if it has one.
    head: Option<[u8; 20]>,
    policy: crate::policy::Policy,
    cargo_home: PathBuf,
    /// The view the planner runs against (always the host root).
    plan_base: Arc<BaseView>,
    flake: Option<Arc<crate::flake::FlakeEnv>>,
    base: Arc<BaseView>,
    tr: Translator,
    sandbox_cwd: String,
    env: BTreeMap<String, String>,
}

async fn prepare(d: &Arc<Daemon>, c: &Arc<Client>) -> Result<Prepared> {
    let root = find_worktree_root(&c.cwd)?;
    let worktree = d.worktree(&root)?;
    let root = worktree.root.clone();
    let wt = worktree.clone();
    let snapshot = tokio::task::spawn_blocking(move || wt.snapshot()).await??;
    // The repository's policy file, as of this snapshot.
    let policy = match snapshot.lookup(b"/", crate::policy::POLICY_FILE.as_bytes()) {
        Looked::File(f) => {
            let hash = d.engine.content.hash(&f.src)?;
            let text = d.store().read(&hash)?;
            crate::policy::Policy::parse(&String::from_utf8_lossy(&text)).map_err(|e| anyhow!(e))?
        }
        _ => Default::default(),
    };
    let cargo_home = real_cargo_home(&c.env);
    let generation = d.memo.advance();
    let mut masks = vec![vjoin(WORKTREE_AT, b"target")];
    if let Some(extra) = d.masks.get(&root) {
        for m in extra.iter() {
            if !masks.contains(m) {
                masks.push(m.clone());
            }
        }
    }
    // A flake, when present and not switched off, defines the environment.
    let flake = if root.join("flake.nix").is_file() && policy.flake.enabled != Some(false) {
        let (store, r, uid, gid) = (d.store().clone(), root.clone(), c.uid, c.gid);
        Some(
            tokio::task::spawn_blocking(move || crate::flake::resolve(&store, &r, uid, gid))
                .await?
                .context("evaluating the repository's flake (set `flake.enabled = false` in .rargo.toml to build without it)")?,
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
        snapshot,
        git_path: root.join(".git"),
        cargo_home: cargo_home.clone(),
        masks,
        hidden: vec![d.cfg.state_dir.as_os_str().as_encoded_bytes().to_vec()],
        planner: d.cfg.planner.clone(),
        restricted,
    };
    let base = parts.build(d.memo.clone(), generation, false);
    let plan_base = if flake.is_some() { Arc::new(parts.build(d.memo.clone(), generation, true)) } else { Arc::new(parts.build(d.memo.clone(), generation, false)) };
    let tr = Translator::new(vec![
        (String::from_utf8_lossy(WORKTREE_AT).into_owned(), root.to_string_lossy().into_owned()),
        (String::from_utf8_lossy(CARGO_HOME_AT).into_owned(), cargo_home.to_string_lossy().into_owned()),
    ]);
    let cwd = c.cwd.canonicalize().unwrap_or_else(|_| c.cwd.clone());
    let sandbox_cwd = tr.to_canonical(&cwd.to_string_lossy());
    let head = git2::Repository::open(&root)
        .ok()
        .and_then(|r| r.head().ok().and_then(|h| h.target()))
        .and_then(|o| <[u8; 20]>::try_from(o.as_bytes()).ok());
    let mut env = base_env(&c.env);
    if let Some(f) = &flake {
        // The flake's variables replace the host's: PATH, compilers and
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
    for (k, v) in policy.passed_env(&c.env) {
        env.entry(k).or_insert(v);
    }
    Ok(Prepared { root, head, policy, cargo_home, plan_base, flake, base: Arc::new(base), tr, sandbox_cwd, env })
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
    let mut env: BTreeMap<Vec<u8>, Vec<u8>> =
        p.env.iter().map(|(k, v)| (bytes(k), bytes(v))).collect();
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
    apply_writes(d.store().clone(), &p.tr, c.uid, c.gid, &[], &lock_writes).await?;
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
/// toolchain, updating the index, downloading crates, writing the lockfile),
/// as the client, in the real worktree.
async fn fetch(d: &Arc<Daemon>, c: &Arc<Client>, args: &[String]) -> Result<bool> {
    let planner = d.cfg.planner.clone();
    let cwd = c.cwd.clone();
    let env: HashMap<String, String> = (*c.env).clone();
    let (uid, gid) = (c.uid, c.gid);
    let mut fetch_args = vec!["fetch".to_string()];
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--" => break,
            "--manifest-path" | "--config" | "--lockfile-path" => {
                fetch_args.push(a.clone());
                if let Some(v) = it.next() {
                    fetch_args.push(v.clone());
                }
            }
            "--locked" | "--frozen" | "--offline" => return Ok(false),
            _ if a.starts_with("--manifest-path=") || a.starts_with("--config=") => fetch_args.push(a.clone()),
            _ => {}
        }
    }
    let out = tokio::task::spawn_blocking(move || {
        use std::os::unix::process::CommandExt;
        let run = |program: &Path, args: &[String]| {
            let mut cmd = std::process::Command::new(program);
            cmd.args(args).current_dir(&cwd).env_clear().envs(&env).env("RUSTC_WRAPPER", "");
            cmd.stdin(std::process::Stdio::null());
            if uid != unsafe { libc::geteuid() } {
                cmd.uid(uid).gid(gid);
            }
            cmd.output()
        };
        // Asking the proxy for its version installs a missing toolchain.
        let _ = run(Path::new("rustc"), &["-V".to_string()]);
        run(&planner, &fetch_args)
    })
    .await??;
    c.out.raw_err(&out.stderr);
    Ok(out.status.success())
}

fn needs_network(stderr: &[u8]) -> bool {
    let s = String::from_utf8_lossy(stderr);
    ["--offline", "offline mode", "is not installed", "could not download", "syncing channel updates"]
        .iter()
        .any(|m| s.contains(m))
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

    let mut p = prepare(d, c).await?;
    let mut fetched = false;
    let planned = loop {
        match plan(d, c, &p, &args).await? {
            PlanOutcome::Ok(pl) => break pl,
            PlanOutcome::Failed { stderr, .. } if !fetched && needs_network(&stderr) => {
                fetched = true;
                if !fetch(d, c, &args).await? {
                    return Ok(Finish::Code(101));
                }
                p = prepare(d, c).await?;
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

    // A target directory elsewhere in the worktree must be masked too; the
    // next request for this worktree will know.
    for dir in [&plan.target_dir, &plan.build_dir] {
        let b = dir.as_bytes().to_vec();
        if vpath_starts_with(&b, WORKTREE_AT) && b != vjoin(WORKTREE_AT, b"target") {
            let mut m = d.masks.entry(p.root.clone()).or_default();
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
        cwd: c.cwd.clone(),
        env: c.env.clone(),
        uid: c.uid,
        gid: c.gid,
        prio: c.prio,
        out,
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
        early: Mutex::new(HashMap::new()),
        ui: Mutex::new(Ui::default()),
        failed: AtomicBool::new(false),
        inconsistent: AtomicBool::new(false),
        base_env: env,
        no_cache_tests,
        explain: c.env.contains_key("RARGO_EXPLAIN"),
        root_id: p.flake.as_ref().map(|f| f.id).unwrap_or(Hash::ZERO),
        cargo_override: p.flake.as_ref().and_then(|f| f.which("cargo")),
        wt_root: p.root.clone(),
        head: p.head,
        policy: p.policy.clone(),
    });

    let mut tasks = Vec::with_capacity(n);
    for i in 0..n {
        let r = run.clone();
        tasks.push(tokio::spawn(async move {
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
    for t in tasks {
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

    // Place what was asked for into the real target directory.
    run.materialize_roots(&p.root).await?;
    c2.out.status("Finished", &format!("{}{}", plan.finished, elapsed(started.elapsed())));

    run.steps(&p).await
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

    fn stderr_line(&self, line: &[u8], live: Option<&LiveCtl<'_>>) {
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
    fn stderr_line(&self, line: &[u8], _live: Option<&LiveCtl<'_>>) {
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

    async fn wait_dep(&self, dep: &rp::Dep) -> bool {
        let j = dep.unit as usize;
        let meta_ok = dep.only_rmeta && self.plan().units[j].rmeta_early;
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
        let mut levels = self.pd.levels(i);
        if include_self {
            let mut lv = (*levels).clone();
            lv[i] = LEVEL_FULL;
            levels = Arc::new(lv);
        }
        Arc::new(BuildView { base: self.base.clone(), layer: self.layer.clone(), levels, untraced, seed })
    }

    /// Whether `commit` is HEAD of this worktree or one of its ancestors.
    fn is_ancestor_of_head(&self, commit: &[u8; 20]) -> bool {
        let Some(head) = self.head else { return false };
        if head == *commit {
            return true;
        }
        let Ok(repo) = git2::Repository::open(&self.wt_root) else { return false };
        match (git2::Oid::from_bytes(&head), git2::Oid::from_bytes(commit)) {
            (Ok(h), Ok(c)) => repo.graph_descendant_of(h, c).unwrap_or(false),
            _ => false,
        }
    }

    async fn exec_unit(self: &Arc<Self>, i: usize) -> Result<UState> {
        let plan = self.plan();
        let u = &plan.units[i];
        for dep in &u.deps {
            if !self.wait_dep(dep).await {
                return Ok(UState::Skipped);
            }
        }
        if u.needs_objects {
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
        let _ = &mut args;
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
        let wt = self.wt_root.to_string_lossy().into_owned();
        let mut seed = match &incr_dir {
            Some(_) => self.d.incr.pick(&crate_key, &wt, &|c| self.is_ancestor_of_head(c)),
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
        let outcome = loop {
            let seed_layer = seed.as_ref().map(|(_, m, _)| {
                let l = OutputsLayer::new();
                l.add_manifest(0, LEVEL_FULL, m);
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
                    cacheable: cacheable.clone(),
                    declared_env: declared_env.clone(),
                    observer: Arc::new(UnitObserver {
                        run: self.clone(),
                        i,
                        shown: display_cmd(&cmd.program, &args),
                    }),
                })
                .await?;
            let r = &outcome.result;
            // A compiler that crashed (signal, or the exit code of an
            // internal error) while reading someone's incremental state may
            // have crashed because of that state: drop it and compile clean.
            let crashed = r.signal().is_some() || r.code() == Some(101);
            if crashed && !outcome.cached {
                if let Some((id, _, _)) = seed.take() {
                    self.d.incr.discard(&crate_key, id);
                    continue;
                }
            }
            break outcome;
        };
        if let (Some(dir), true, false) = (&incr_dir, outcome.result.success(), outcome.cached) {
            let state = crate::incr::apply_changes(dir, seed.as_ref().map(|(_, m, _)| &**m), &outcome.side);
            if !state.entries.is_empty() {
                self.d.incr.save(&crate_key, &wt, self.head, &state);
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
        let meta = &self.pd.meta_paths[i];
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
        self.apply_worktree_writes(&r.outputs).await?;
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
            rp::Verb::Compiling => (&mut ui.compiled, "Compiling"),
        };
        if set.insert(u.pkg_id.clone()) {
            self.c.out.status(verb, &u.pkg);
        }
    }

    /// One line of rustc or rustdoc stderr (cargo's `on_stderr_line_inner`).
    fn compiler_line(&self, i: usize, line: &str, live: Option<&LiveCtl<'_>>) {
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
        if v.get("future_incompat_report").is_some() {
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
                        self.states[i].send_replace(UState::Meta);
                    }
                }
            }
            return;
        }
        if v.get("unused_extern_names").is_some() {
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
            for o in &u.outputs {
                if r.outputs.get(o.path.as_bytes()).is_none() {
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
    async fn apply_worktree_writes(&self, m: &Manifest) -> Result<()> {
        let plan = self.plan();
        apply_writes(self.d.store().clone(), &self.c.out.tr, self.c.uid, self.c.gid, &[&plan.target_dir, &plan.build_dir], m)
            .await
    }

    /// Links the requested artifacts into the real target directory.
    async fn materialize_roots(&self, _root: &Path) -> Result<()> {
        let plan = self.plan();
        let mut jobs: Vec<(Hash, bool, PathBuf)> = Vec::new();
        for &ri in &plan.roots {
            let u = &plan.units[ri as usize];
            let Some((r, _)) = self.results[ri as usize].get() else { continue };
            for o in &u.outputs {
                let Some(MEntry::File { hash, exec, .. }) = r.outputs.get(o.path.as_bytes()) else { continue };
                let mut dests: Vec<&str> = Vec::new();
                dests.extend(o.hardlink.as_deref());
                dests.extend(o.export_path.as_deref());
                // Executables are also wanted at their own path: test
                // binaries have no uplifted name.
                if u.executable.as_deref() == Some(o.path.as_str()) {
                    dests.push(&o.path);
                }
                for d in dests {
                    jobs.push((*hash, *exec, PathBuf::from(self.c.out.tr.path(d))));
                }
            }
        }
        if jobs.is_empty() {
            return Ok(());
        }
        let store = self.d.store().clone();
        let (uid, gid) = (self.c.uid, self.c.gid);
        tokio::task::spawn_blocking(move || -> Result<()> {
            for (hash, exec, dest) in jobs {
                if let Some(parent) = dest.parent() {
                    mkdir_owned(parent, uid, gid)?;
                }
                // Already the same inode or content: nothing to do.
                if let (Ok(md), Some(size)) = (std::fs::metadata(&dest), store.size_of(&hash)) {
                    if md.len() == size && (md.mode() & 0o111 != 0) == exec {
                        if let Ok((h, _)) = Hash::of_path(&dest) {
                            if h == hash {
                                continue;
                            }
                        }
                    }
                }
                store.materialize(&hash, exec, &dest)?;
            }
            Ok(())
        })
        .await?
    }

    /// Runs what cargo does after the build: tests, doctests, `cargo run`.
    async fn steps(self: &Arc<Self>, p: &Prepared) -> Result<Finish> {
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
                    // The program is the user's: it gets the user's whole
                    // environment plus what cargo adds.
                    let mut env: BTreeMap<String, String> =
                        self.c.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                    for (k, v) in &cmd.env {
                        match v {
                            Some(v) => {
                                env.insert(k.clone(), v.clone());
                            }
                            None => {
                                env.remove(k);
                            }
                        }
                    }
                    return Ok(Finish::Exec(ExecRequest {
                        program: cmd.program.clone(),
                        args: cmd.args.clone(),
                        env: env.into_iter().collect(),
                        cwd: self.c.cwd.to_string_lossy().into_owned(),
                        binds: vec![
                            (p.root.to_string_lossy().into_owned(), String::from_utf8_lossy(WORKTREE_AT).into_owned()),
                            (
                                p.cargo_home.to_string_lossy().into_owned(),
                                String::from_utf8_lossy(CARGO_HOME_AT).into_owned(),
                            ),
                        ],
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
                        self.apply_worktree_writes(&r.outputs).await?;
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

/// Writes the worktree part of an action's outputs to the real worktree.
async fn apply_writes(
    store: Arc<Store>,
    tr: &Translator,
    uid: u32,
    gid: u32,
    target_dirs: &[&String],
    m: &Manifest,
) -> Result<()> {
    let inside = |p: &[u8]| {
        vpath_starts_with(p, WORKTREE_AT)
            && p != WORKTREE_AT
            && !target_dirs.iter().any(|t| vpath_starts_with(p, t.as_bytes()))
            && !vpath_starts_with(p, &vjoin(WORKTREE_AT, b"target"))
            && !vpath_starts_with(p, &vjoin(WORKTREE_AT, b".git"))
    };
    let writes: Vec<(String, MEntry)> = m
        .entries
        .iter()
        .filter(|(p, _)| inside(p))
        .map(|(p, e)| (tr.path(&String::from_utf8_lossy(p)), e.clone()))
        .collect();
    if writes.is_empty() {
        return Ok(());
    }
    tokio::task::spawn_blocking(move || -> Result<()> {
        for (path, e) in writes {
            let path = PathBuf::from(path);
            match e {
                MEntry::Dir => {
                    if !path.is_dir() {
                        std::fs::create_dir_all(&path)?;
                        chown(&path, uid, gid);
                    }
                }
                MEntry::Whiteout => {
                    if let Ok(md) = std::fs::symlink_metadata(&path) {
                        if md.is_dir() {
                            let _ = std::fs::remove_dir_all(&path);
                        } else {
                            let _ = std::fs::remove_file(&path);
                        }
                    }
                }
                MEntry::Symlink(t) => {
                    use std::os::unix::ffi::OsStrExt;
                    let _ = std::fs::remove_file(&path);
                    std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(&t), &path)?;
                }
                MEntry::File { hash, exec, size } => {
                    // Leave an identical file alone so its mtime and any
                    // watchers stay quiet.
                    let same = std::fs::metadata(&path)
                        .ok()
                        .filter(|md| md.is_file() && md.len() == size)
                        .and_then(|_| crate::hash::Hash::of_path(&path).ok())
                        .is_some_and(|(h, _)| h == hash);
                    if same {
                        continue;
                    }
                    if let Some(parent) = path.parent() {
                        if !parent.is_dir() {
                            std::fs::create_dir_all(parent)?;
                            chown(parent, uid, gid);
                        }
                    }
                    // A source file must be an ordinary writable file, never
                    // a link into the store.
                    use std::os::unix::fs::PermissionsExt;
                    let bytes = store.read(&hash)?;
                    std::fs::write(&path, bytes)?;
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(if exec { 0o755 } else { 0o644 }))?;
                    chown(&path, uid, gid);
                }
            }
        }
        Ok(())
    })
    .await?
}

fn chown(path: &Path, uid: u32, gid: u32) {
    use std::os::unix::ffi::OsStrExt;
    if unsafe { libc::geteuid() } != 0 {
        return;
    }
    if let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) {
        // SAFETY: valid C string.
        unsafe { libc::lchown(c.as_ptr(), uid, gid) };
    }
}

/// `mkdir -p`, giving directories this call creates to the client.
fn mkdir_owned(dir: &Path, uid: u32, gid: u32) -> Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    if let Some(parent) = dir.parent() {
        mkdir_owned(parent, uid, gid)?;
    }
    match std::fs::create_dir(dir) {
        Ok(()) => {
            chown(dir, uid, gid);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e).with_context(|| format!("creating {}", dir.display())),
    }
}
