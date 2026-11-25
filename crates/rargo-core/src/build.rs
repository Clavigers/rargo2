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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;
use tokio::sync::watch;

mod nextest;
mod pure;
mod request;
mod nextest_run;

use request::{BuildRequest, Effect, End, Go, OnFailure, Unit};

use pure::*;

const PLAN_PATH: &str = "/.rargo-plan";

pub struct Config {
    pub state_dir: PathBuf,
    pub planner: PathBuf,
    pub cargo_home: PathBuf,
    pub uid: u32,
    pub gid: u32,
    pub server_env: HashMap<String, String>,
}

#[derive(Default)]
pub struct DaemonStats {
    pub requests: AtomicU64,
    pub plans_decoded: AtomicU64,
    pub verify_units: AtomicU64,
    pub verify_mismatches: AtomicU64,
    pub bytes_in: AtomicU64,
    pub bytes_out: AtomicU64,
    pub links: AtomicU64,
    pub files_in: AtomicU64,
}

pub struct Daemon {
    fork_seen: DashMap<(String, String), bool>,
    pub cfg: Config,
    pub engine: Arc<Engine>,
    pub memo: Arc<HostMemo>,
    pub trees: Arc<Trees>,
    pub parser: Parser,
    pub incr: crate::incr::IncrStore,
    pub pool: OnceLock<Arc<crate::remote::Pool>>,
    pub(crate) nix_attached: Arc<Mutex<HashSet<String>>>,
    pub drift_report: Mutex<Option<String>>,
    pub last_clients: Mutex<HashMap<String, crate::drift::LastClient>>,
    pub deliveries: Mutex<HashMap<String, (u64, Arc<std::collections::HashSet<rargo_proto::Digest>>)>>,
    pub client_blobs: Arc<crate::remote::Blobs>,
    plans: DashMap<Hash, Arc<PlanData>>,
    masks: DashMap<String, Vec<Vec<u8>>>,
    fetching: tokio::sync::Mutex<()>,
    scratch_seq: AtomicU64,
    pub stats: Arc<DaemonStats>,
}

pub struct Home {
    pub dir: PathBuf,
    pub files: Vec<String>,
}

pub struct Client {
    pub args: Vec<String>,
    pub root: String,
    pub cwd: String,
    pub cargo_home: String,
    pub env: Arc<ClientEnv>,
    pub uid: u32,
    pub gid: u32,
    pub prio: Priority,
    pub out: Output,
    pub session: Option<Arc<Session>>,
    pub snapshot: Arc<Snapshot>,
    pub worktree: String,
    pub commits: Arc<Vec<[u8; 20]>>,
    pub home: Arc<Home>,
    pub outbox: Arc<Mutex<(Vec<Delivery>, Vec<Write>, std::collections::BTreeSet<String>)>>,
    pub targets: Vec<String>,
}

impl Client {
    fn policy_dir(&self) -> Result<PathBuf> {
        rargo_proto::workspace::command_dir(Path::new(&self.root), Path::new(&self.cwd), &self.args)
            .context("the manifest must be inside the synced worktree")
    }

    pub fn deliver(&self, files: Vec<Delivery>, writes: Vec<Write>) {
        if let Some(s) = self.session.as_ref().filter(|s| s.is_lazy()) {
            s.deliver_lazy(files, writes, self.targets.clone());
            return;
        }
        let mut q = self.outbox.lock();
        q.0.extend(files);
        q.1.extend(writes);
        q.2.extend(self.targets.iter().cloned());
    }

    pub async fn flush(&self) -> Result<()> {
        let (files, writes, targets) = std::mem::take(&mut *self.outbox.lock());
        match &self.session {
            Some(s) => s.deliver(files, writes, targets.into_iter().collect()).await,
            None => Ok(()),
        }
    }
}

const _: () = assert!(LEVEL_NONE < LEVEL_META);
const _: () = assert!(LEVEL_META < LEVEL_FULL);

pub struct PlanData {
    pub plan: rp::Plan,
    facts: PlanFacts,
    levels: Vec<OnceLock<Arc<Vec<u8>>>>,
    meta_levels: Vec<OnceLock<Arc<Vec<u8>>>>,
    needs: OnceLock<Arc<Vec<u8>>>,
}

impl PlanData {
    fn new(plan: rp::Plan) -> PlanData {
        let n = plan.units.len();
        let facts = plan_facts(&plan.units, &plan.roots);
        PlanData {
            plan,
            facts,
            levels: (0..n).map(|_| OnceLock::new()).collect(),
            meta_levels: (0..n).map(|_| OnceLock::new()).collect(),
            needs: OnceLock::new(),
        }
    }

    fn meta_levels(&self, i: usize) -> Arc<Vec<u8>> {
        self.meta_levels[i].get_or_init(|| Arc::new(meta_levels(&self.plan.units, &self.facts, i))).clone()
    }

    fn needs(&self) -> Arc<Vec<u8>> {
        self.needs
            .get_or_init(|| {
                Arc::new(needs(&self.plan.units, &self.plan.roots, &|i| self.meta_levels(i), &|i| self.levels(i)))
            })
            .clone()
    }

    fn levels(&self, i: usize) -> Arc<Vec<u8>> {
        self.levels[i].get_or_init(|| Arc::new(levels(&self.plan.units, &self.facts, i))).clone()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum UState {
    Pending,
    Meta,
    Done,
    Failed,
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
    unused_externs: BTreeMap<usize, String>,
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
    times: Mutex<Vec<crate::timings::UnitTime>>,
    early: Mutex<HashMap<usize, Vec<(Vec<u8>, MEntry)>>>,
    ui: Mutex<Ui>,
    req: Arc<Mutex<BuildRequest>>,
    base_env: BTreeMap<String, String>,
    no_cache_tests: bool,
    explain: bool,
    root_id: Hash,
    cargo_override: Option<String>,
    policy: crate::policy::Policy,
    verify: bool,
    verified: AtomicU64,
    verify_mismatches: Mutex<Vec<(String, Vec<Vec<u8>>)>>,
    verify_unreproducible: Mutex<Vec<String>>,
    unit_keys: Vec<OnceLock<Hash>>,
    need: Option<Arc<Vec<u8>>>,
    script_warnings: Mutex<Vec<String>>,
}

fn bytes(s: &str) -> Vec<u8> {
    s.as_bytes().to_vec()
}

impl Daemon {
    pub fn new(cfg: Config, engine: Arc<Engine>) -> Daemon {
        let parser = Parser::new(cfg.planner.clone());
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_micros() as u64).unwrap_or(1);
        let incr = crate::incr::IncrStore::new(engine.store.db().clone(), engine.store.blobs(), seed);
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
            nix_attached: Arc::new(Mutex::new(HashSet::new())),
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

    pub fn scratch(&self, what: &str) -> Result<PathBuf> {
        let n = self.scratch_seq.fetch_add(1, Ordering::Relaxed);
        let dir = self.store().tmp_dir(&format!("{what}-{}-{n}", std::process::id()))?;
        Ok(dir)
    }

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

    pub fn collect_garbage(&self, min_age_secs: u64) -> (usize, u64) {
        let mut live = HashSet::new();
        self.engine.traces.mark(&mut live);
        self.incr.mark(&mut live);
        self.trees.mark(&mut live);
        let fs = self.engine.fs.clone();
        self.store().sweep(&live, min_age_secs, &|h| fs.forget_backing(h))
    }
}

pub async fn handle(d: Arc<Daemon>, c: Client) -> Result<i32, ExecRequest> {
    let _hold = crate::nixmanifest::hold_build();
    d.stats.requests.fetch_add(1, Ordering::Relaxed);
    let c = Arc::new(c);
    if subcommand_pos(&c.args).is_some_and(|i| c.args[i] == "nextest") {
        let result = match nextest::run(&d, &c).await {
            Ok(code) => code,
            Err(e) => {
                c.out.error(&format!("{e:#}"));
                101
            }
        };
        if let Err(e) = c.flush().await {
            c.out.error(&format!("delivering results: {e:#}"));
            return Ok(101);
        }
        return Ok(result);
    }
    let mut result = Ok(101);
    for attempt in 0..3 {
        match run_request(&d, &c, None).await {
            Ok(Finish::Code(code)) => result = Ok(code),
            Ok(Finish::Exec(e)) => result = Err(e),
            Ok(Finish::Retry) if attempt < 2 => {
                c.outbox.lock().0.clear();
                continue;
            }
            Ok(Finish::Retry) => c.out.error("the build kept producing inconsistent intermediate artifacts"),
            Err(e) => c.out.error(&format!("{e:#}")),
        }
        break;
    }
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
    parts: ViewParts,
    generation: u64,
    policy: crate::policy::Policy,
    plan_base: Arc<BaseView>,
    flake: Arc<crate::flake::FlakeEnv>,
    base: Arc<BaseView>,
    tr: Translator,
    sandbox_cwd: String,
    env: BTreeMap<String, String>,
}

async fn prepare(d: &Arc<Daemon>, c: &Arc<Client>, snapshot: Arc<Snapshot>) -> Result<Prepared> {
    let dir = c.policy_dir()?;
    let policy = crate::policy::snapshot_policy(&snapshot, &dir).map_err(|e| anyhow!(e))?;
    let generation = d.memo.advance();
    let mut masks = vec![vjoin(WORKTREE_AT, b"target")];
    if let Some(extra) = d.masks.get(&c.worktree) {
        for m in extra.iter() {
            if !masks.contains(m) {
                masks.push(m.clone());
            }
        }
    }
    let (dd, snap, uid, gid, files) = (d.clone(), snapshot.clone(), c.uid, c.gid, policy.flake.files.clone());
    let flake = tokio::task::spawn_blocking(move || crate::flake::resolve(&dd, &snap, &dir, &files, uid, gid))
        .await?
        .context("evaluating the build flake (name extra inputs under metadata.rargo.flake.files in Cargo.toml)")?;
    let mut links = Vec::new();
    for (at, tool) in [("/bin/sh", "sh"), ("/usr/bin/env", "env")] {
        if let Some(target) = flake.which(tool).or_else(|| if tool == "sh" { flake.which("bash") } else { None }) {
            links.push((bytes(at), bytes(&target)));
        }
    }
    let store_paths = Arc::new(
        flake.closure.iter().zip(flake.roots.iter()).map(|(n, r)| (n.as_bytes().to_vec(), *r)).collect(),
    );
    let restricted = Some(Restricted {
        store_names: flake.closure_set(),
        store_paths,
        etc: flake.etc(d.store(), c.uid, c.gid)?,
        links,
    });
    let parts = ViewParts {
        snapshot: snapshot.clone(),
        cargo_home: d.cfg.cargo_home.clone(),
        home_files: c.home.files.iter().map(|n| (vjoin(CARGO_HOME_AT, n.as_bytes()), c.home.dir.join(n))).collect(),
        masks,
        hidden: vec![d.cfg.state_dir.as_os_str().as_encoded_bytes().to_vec()],
        planner: d.cfg.planner.clone(),
        tools: Vec::new(),
        restricted,
    };
    let base = parts.build(d.memo.clone(), generation, false);
    let plan_base = Arc::new(parts.build(d.memo.clone(), generation, true));
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
    for k in ["PATH", "HOME", "USER", "LOGNAME", "SHELL", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN"] {
        env.remove(k);
    }
    env.retain(|k, _| !k.starts_with("NIX_"));
    env.extend(flake.env.clone());
    env.insert("HOME".into(), "/tmp".into());
    env.insert("CARGO_HOME".into(), String::from_utf8_lossy(CARGO_HOME_AT).into_owned());
    env.insert("RUSTC_WRAPPER".into(), String::new());
    env.insert("RUSTC_WORKSPACE_WRAPPER".into(), String::new());
    for (k, v) in policy.passed_env(&client_env) {
        env.entry(k).or_insert(v);
    }
    Ok(Prepared { snapshot, parts, generation, policy, plan_base, flake, base: Arc::new(base), tr, sandbox_cwd, env })
}

struct Planned {
    data: Arc<PlanData>,
    stderr: Vec<u8>,
}

enum PlanOutcome {
    Ok(Planned),
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
        root_id: p.flake.id,
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

async fn fetch(
    d: &Arc<Daemon>,
    c: &Arc<Client>,
    p: &Prepared,
    args: &[String],
    components: Vec<(String, String)>,
    offline: bool,
) -> Result<Option<Arc<Snapshot>>> {
    let mut fetch_args = vec!["fetch".to_string()];
    let scratch = d.scratch("fetch")?;
    let scratch_s = scratch.to_string_lossy().into_owned();
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
            "--locked" | "--frozen" | "--offline" => {}
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
    let mut env: HashMap<String, String> = d.cfg.server_env.clone();
    for (k, v) in base_env(&d.cfg.server_env, &c.env.to_map()) {
        env.insert(k, v);
    }
    env.insert("CARGO_HOME".into(), c.home.dir.to_string_lossy().into_owned());
    let (uid, gid) = (c.uid, c.gid);
    let (snap, scratch2) = (p.snapshot.clone(), scratch.clone());
    let out = tokio::task::spawn_blocking(move || -> Result<std::result::Result<Option<std::process::Output>, Vec<u8>>> {
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
        let _ = run(Path::new("rustc"), &["-V".to_string()]);
        if let Err(said) = add_components(&components, &run) {
            return Ok(Err(said));
        }
        if offline {
            return Ok(Ok(None));
        }
        Ok(Ok(Some(run(&planner, &fetch_args)?)))
    })
    .await?;
    let out = match out {
        Ok(Ok(Some(o))) => o,
        Ok(Err(said)) => {
            let _ = std::fs::remove_dir_all(&scratch);
            c.out.raw_err(&said);
            return Ok(None);
        }
        Ok(Ok(None)) => {
            let _ = std::fs::remove_dir_all(&scratch);
            return Ok(Some(p.snapshot.clone()));
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&scratch);
            return Err(e);
        }
    };
    c.out.raw_err(String::from_utf8_lossy(&out.stderr).replace(&scratch_s, &c.root).as_bytes());
    let mut snapshot = p.snapshot.clone();
    let mut writes = Vec::new();
    if out.status.success() {
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

fn add_components(
    components: &[(String, String)],
    run: &dyn Fn(&Path, &[String]) -> std::io::Result<std::process::Output>,
) -> std::result::Result<(), Vec<u8>> {
    for add in rustup_adds(components) {
        let ran = run(Path::new("rustup"), &add);
        let failed = match &ran {
            Ok(o) => rustup_failure(Ok((o.status.success(), &o.stderr))),
            Err(e) => rustup_failure(Err(&e.to_string())),
        };
        if let Some(said) = failed {
            return Err(said);
        }
    }
    Ok(())
}

async fn add_components_for(d: &Arc<Daemon>, c: &Client, components: Vec<(String, String)>) -> Result<bool> {
    let _one_at_a_time = d.fetching.lock().await;
    let mut env: HashMap<String, String> = d.cfg.server_env.clone();
    for (k, v) in base_env(&d.cfg.server_env, &c.env.to_map()) {
        env.insert(k, v);
    }
    let (uid, gid) = (c.uid, c.gid);
    let added = tokio::task::spawn_blocking(move || {
        use std::os::unix::process::CommandExt;
        let run = |program: &Path, args: &[String]| {
            let mut cmd = std::process::Command::new(program);
            cmd.args(args).current_dir("/").env_clear().envs(&env);
            cmd.stdin(std::process::Stdio::null());
            if uid != unsafe { libc::geteuid() } {
                cmd.uid(uid).gid(gid);
            }
            cmd.output()
        };
        add_components(&components, &run)
    })
    .await?;
    Ok(match added {
        Ok(()) => true,
        Err(said) => {
            c.out.raw_err(&said);
            false
        }
    })
}

fn phase(started: Instant, c: &Client, what: &str) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("RARGO_PHASES").is_some()) {
        eprintln!("rargod: phase {:>8.3} s  {what}  ({} {})", started.elapsed().as_secs_f64(), c.worktree, c.args.join(" "));
    }
}

struct CancelOnDrop(Arc<Mutex<BuildRequest>>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let mut req = self.0.lock();
        if !matches!(req.phase(), request::Phase::Done(_)) {
            req.cancel();
        }
    }
}

fn remember_client(d: &Arc<Daemon>, c: &Arc<Client>) -> Result<()> {
if c.session.is_some() && !c.worktree.is_empty() {
    d.trees.remember(&c.worktree, &c.snapshot);
    let mut last = d.last_clients.lock();
    let dir = c.policy_dir()?;
    let checked = last.get(&c.worktree).filter(|l| l.dir == dir).and_then(|l| l.checked);
    last.insert(
        c.worktree.clone(),
        crate::drift::LastClient {
            env: Arc::new(c.env.to_map()),
            home: c.home.clone(),
            root: c.root.clone(),
            dir,
            at: Instant::now(),
            checked,
        },
    );
}
    Ok(())
}

struct Built {
    layer: Arc<OutputsLayer>,
    units: usize,
    target_dirs: Vec<String>,
    env: BTreeMap<String, String>,
    cargo_override: Option<String>,
}

async fn run_request(d: &Arc<Daemon>, c: &Arc<Client>, keep: Option<&Mutex<Option<Built>>>) -> Result<Finish> {
    let started = Instant::now();
    let mut args: Vec<String> = c.args.clone();
    let mut no_cache_tests = false;
    if let Some(pos) = args.iter().take_while(|a| *a != "--").position(|a| a == "--no-cache") {
        args.remove(pos);
        no_cache_tests = true;
    }

    let env_known = {
        let (env, names) = (c.env.clone(), d.engine.traces.every_env_name());
        tokio::spawn(async move { env.ensure(&names).await })
    };
    let check_at = subcommand_pos(&args).filter(|&i| args[i] == "check" || args[i] == "c");
    let check_at = check_at.filter(|_| c.env.get("RARGO_CHECK_AS_BUILD").as_deref() == Some("1"));
    let fork_key = (c.worktree.clone(), c.env.get("RUSTUP_TOOLCHAIN").unwrap_or_default());
    let seen_fork = d.fork_seen.get(&fork_key).map(|v| *v).unwrap_or(false);
    let (req, first) = BuildRequest::new(check_at.is_some(), seen_fork, offline_args(&args));
    let req = Arc::new(Mutex::new(req));
    let _cancel = CancelOnDrop(req.clone());
    let mut todo: std::collections::VecDeque<Effect> = first.into();
    let mut snapshot = c.snapshot.clone();
    let mut prepared: Option<Prepared> = None;
    let mut planned: Option<Planned> = None;
    let mut plan_failure: Option<(Vec<u8>, Vec<u8>)> = None;
    let mut plan_args = args.clone();
    while let Some(effect) = todo.pop_front() {
        let next = match effect {
            Effect::Prepare => {
                let first = prepared.is_none();
                prepared = Some(prepare(d, c, snapshot.clone()).await?);
                if first {
                    phase(started, c, "worktree prepared");
                    remember_client(d, c)?;
                }
                req.lock().prepared()
            }
            Effect::Plan { as_build } => {
                let p = prepared.as_ref().expect("a plan follows a prepared worktree");
                plan_args = args.clone();
                if let (Some(i), true) = (check_at, as_build) {
                    plan_args[i] = "build".into();
                }
                match plan(d, c, p, &plan_args).await? {
                    PlanOutcome::Ok(pl) => {
                        let plan = &pl.data.plan;
                        let fx = req.lock().planned(plan.rustc_fork, plan.units.len(), plan.keep_going);
                        planned = Some(pl);
                        fx
                    }
                    PlanOutcome::Failed { stderr, stdout, code } => {
                        let fx = req.lock().plan_failed(&stderr, code);
                        plan_failure = Some((stdout, stderr));
                        fx
                    }
                }
            }
            Effect::RememberFork(b) => {
                d.fork_seen.insert(fork_key.clone(), b);
                Vec::new()
            }
            Effect::Fetch { components, offline } => {
                let p = prepared.as_ref().expect("a fetch follows a prepared worktree");
                let got = fetch(d, c, p, &args, components, offline).await?;
                let ok = got.is_some();
                if let Some(s) = got {
                    snapshot = s;
                }
                req.lock().fetched(ok)
            }
            Effect::ShowPlanFailure => {
                let p = prepared.as_ref().expect("a plan failure follows a prepared worktree");
                let (stdout, stderr) = plan_failure.take().expect("a plan failed");
                c.out.raw_out(p.tr.text(&String::from_utf8_lossy(&stdout)).as_bytes());
                c.out.err_text(&String::from_utf8_lossy(&stderr));
                Vec::new()
            }
            Effect::End(End::Code(code)) => return Ok(Finish::Code(code)),
            Effect::End(End::Retry) => return Ok(Finish::Retry),
            Effect::End(End::Cancelled) => return Ok(Finish::Code(101)),
            Effect::RunUnits => break,
            Effect::Deliver => unreachable!("delivery follows the units"),
        };
        req.lock().invariants();
        todo.extend(next);
    }
    let p = prepared.expect("units follow a prepared worktree");
    let planned = planned.expect("units follow a plan");
    let as_build = req.lock().as_build();
    let pd = planned.data;
    let plan = &pd.plan;
    env_known.await.map_err(|e| anyhow!("asking the client for its environment: {e}"))??;
    phase(started, c, "planned");

    for dir in [&plan.target_dir, &plan.build_dir] {
        let b = dir.as_bytes().to_vec();
        if vpath_starts_with(&b, WORKTREE_AT) && b != vjoin(WORKTREE_AT, b"target") {
            let mut m = d.masks.entry(c.worktree.clone()).or_default();
            if !m.contains(&b) {
                m.push(b);
            }
        }
    }

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
        targets: [&plan.target_dir, &plan.build_dir].into_iter().map(|d| p.tr.path(d)).collect(),
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
        req: req.clone(),
        base_env: env,
        no_cache_tests,
        explain: c.env.contains_key("RARGO_EXPLAIN"),
        root_id: p.flake.id,
        cargo_override: p.flake.which("cargo"),
        policy: p.policy.clone(),
        verify: c.env.contains_key("RARGO_VERIFY"),
        verified: AtomicU64::new(0),
        verify_mismatches: Mutex::new(Vec::new()),
        verify_unreproducible: Mutex::new(Vec::new()),
        unit_keys: (0..n).map(|_| OnceLock::new()).collect(),
        need: if as_build { Some(pd.needs()) } else { None },
        script_warnings: Mutex::new(Vec::new()),
    });
    if let Some(keep) = keep {
        *keep.lock() = Some(Built {
            layer: run.layer.clone(),
            units: n,
            target_dirs: vec![plan.target_dir.clone(), plan.build_dir.clone()],
            env: run.base_env.clone(),
            cargo_override: run.cargo_override.clone(),
        });
    }

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
                    UState::Failed
                }
            };
            let end = match st {
                UState::Done => Unit::Done,
                UState::Failed => Unit::Failed,
                UState::Skipped => Unit::Skipped,
                UState::Pending | UState::Meta => unreachable!("a unit's task ends with the unit"),
            };
            r.req.lock().unit_ended(i, end);
            r.states[i].send_replace(st);
        }));
    }
    for t in tasks.0.iter_mut() {
        t.await.context("a unit task panicked")?;
    }
    let (ok, after) = req.lock().units_ended();
    if after == [Effect::End(End::Retry)] {
        return Ok(Finish::Retry);
    }
    if matches!(plan.message_format, rp::MessageFormat::Json { .. }) {
        c2.out.raw_out(format!("{{\"reason\":\"build-finished\",\"success\":{ok}}}\n").as_bytes());
    }
    match after.as_slice() {
        [Effect::Deliver] => {}
        [Effect::End(End::Code(code))] => return Ok(Finish::Code(*code)),
        other => unreachable!("units end in a delivery or an end: {other:?}"),
    }

    if run.verify {
        let all = run.verify_mismatches.lock().clone();
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
    {
        let unused = std::mem::take(&mut run.ui.lock().unused_externs);
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
        finished();
        req.lock().delivered(0);
        return Ok(Finish::Code(0));
    }
    run.materialize_roots();
    run.write_depinfo();
    phase(started, c, "results listed for the client");
    finished();

    let done = run.steps().await;
    phase(started, c, "done");
    if let Ok(Finish::Code(code)) = &done {
        req.lock().delivered(*code);
    }
    done
}

struct UnitObserver {
    run: Arc<Run>,
    i: usize,
    shown: String,
    pending: Mutex<Vec<u8>>,
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
                self.note_script_warnings(data);
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

impl UnitObserver {
    fn note_script_warnings(&self, data: &[u8]) {
        let u = &self.run.pd.plan.units[self.i];
        if !u.show_warnings {
            return;
        }
        let ready = {
            let mut pending = self.pending.lock();
            pending.extend_from_slice(data);
            let mut ready = Vec::new();
            while let Some(nl) = pending.iter().position(|b| *b == b'\n') {
                ready.push(pending.drain(..=nl).collect::<Vec<u8>>());
            }
            ready
        };
        for mut line in ready {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let Ok(line) = std::str::from_utf8(&line) else { continue };
            let Some(msg) = script_warning_message(line) else { continue };
            let text = format!("{}@{}: {}", u.pkg_name, u.pkg_version, msg);
            self.run.script_warnings.lock().push(text.clone());
            self.run.c.out.warn(&text);
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
                assert!(!name.contains('='));
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

    fn meta_only(&self, i: usize) -> bool {
        let u = &self.plan().units[i];
        self.need.as_ref().is_some_and(|n| n[i] == LEVEL_META) && u.kind == rp::UnitKind::Compile && u.cmd.is_some()
    }

    async fn wait_dep(&self, dep: &rp::Dep) -> bool {
        let j = dep.unit as usize;
        let meta_ok = dep.only_rmeta && (self.plan().units[j].rmeta_early || self.meta_only(j));
        let mut rx = self.states[j].subscribe();
        loop {
            rx.borrow_and_update();
            match self.req.lock().dep(j, meta_ok) {
                Go::Ready => return true,
                Go::Skip => return false,
                Go::Wait => {}
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
        assert_eq!(levels.len(), self.plan().units.len());
        assert_eq!(levels[i], LEVEL_NONE, "a unit's own view excludes itself unless asked");
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

    fn commits_back(&self, commit: &[u8; 20]) -> Option<usize> {
        self.c.commits.iter().position(|c| c == commit)
    }

    async fn exec_unit(self: &Arc<Self>, i: usize) -> Result<UState> {
        let plan = self.plan();
        let u = &plan.units[i];
        if self.need.as_ref().is_some_and(|n| n[i] == LEVEL_NONE) {
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
        if self.pd.facts.doc_dir[i].is_some() {
            let pos = self.pd.facts.doc_pos[i].unwrap_or(0);
            self.layer.set_rank(i as u32, (plan.units.len() + pos) as u64);
            let before = &self.pd.facts.doc_order[..pos];
            if let Some(&prev) = before.iter().rev().find(|&&j| self.pd.facts.doc_dir[j] == self.pd.facts.doc_dir[i]) {
                let mut rx = self.states[prev].subscribe();
                while matches!(*rx.borrow_and_update(), UState::Pending | UState::Meta) {
                    if rx.changed().await.is_err() {
                        break;
                    }
                }
            }
        }
        if u.needs_objects && !meta_only {
            let levels = self.pd.levels(i);
            for (j, lv) in levels.iter().enumerate() {
                if *lv == LEVEL_FULL && !self.wait_dep(&rp::Dep { unit: j as u32, only_rmeta: false }).await {
                    return Ok(UState::Skipped);
                }
            }
        }
        if !self.req.lock().may_start(i) {
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
        let full_argv: Option<Vec<Vec<u8>>> = meta_only.then(|| {
            let mut v = vec![bytes(&cmd.program)];
            v.extend(args.iter().map(|a| bytes(a)));
            v
        });
        if meta_only {
            if self.pd.facts.can_meta(i) {
                args.push("-Zno-codegen".into());
            } else {
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
                observer: Arc::new(UnitObserver {
                    run: self.clone(),
                    i,
                    shown: display_cmd(&cmd.program, &args),
                    pending: Mutex::new(Vec::new()),
                }),
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
                        pending: Mutex::new(Vec::new()),
                    }),
                })
                .await;
            let outcome = match outcome {
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
            let crashed = r.signal().is_some() || r.code() == Some(101);
            if crashed && !outcome.cached {
                if let Some(bad) = seed.take() {
                    self.d.incr.discard(&crate_key, bad.id);
                    continue;
                }
            }
            if !r.success() {
                let stderr = self.d.store().read(&r.stderr).unwrap_or_default();
                let on_failure = self.req.lock().unit_failed(i, &stderr);
                if let OnFailure::Install(missing) = on_failure {
                    let added = add_components_for(&self.d, &self.c, missing).await?;
                    if self.req.lock().components_installed(i, added) {
                        self.base.renew();
                        continue;
                    }
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
        let _ = self.unit_keys[i].set(self.d.engine.effective_key(&spec, &self.c.env));
        if self.verify && outcome.cached && outcome.result.success() {
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
        if let (Some(dir), true, false, false) =
            (&incr_dir, outcome.result.success(), outcome.cached, outcome.side.is_empty())
        {
            let state = crate::incr::apply_changes(dir, seed.as_ref().map(|s| &*s.manifest), &outcome.side);
            if !state.entries.is_empty() {
                self.d.incr.save(&crate_key, &wt, self.c.commits.first().copied(), &state, crate::tokens::now());
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
            self.report_failure(i, &cmd, &r);
            return Ok(UState::Failed);
        }

        if let Some(early) = self.early.lock().get(&i) {
            for (path, e) in early {
                if r.outputs.get(path) != Some(e) {
                    self.req.lock().unit_inconsistent(i);
                    return Ok(UState::Failed);
                }
            }
        }
        if u.kind == rp::UnitKind::RunScript {
            if !self.finish_script(i, &r).await? {
                return Ok(UState::Failed);
            }
        }
        let meta = &self.pd.facts.rmeta_out[i];
        if meta_only {
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
                    let mut seen = self.script_warnings.lock();
                    if let Some(pos) = seen.iter().position(|s| s == &text) {
                        seen.swap_remove(pos);
                    } else {
                        drop(seen);
                        self.c.out.warn(&text);
                    }
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
                time: time.relative_to(started),
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
        let mut per_package: BTreeMap<String, String> = BTreeMap::new();
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
                for l in tr.text(item).lines() {
                    text.push_str(&format!("> {l}\n"));
                }
            }
            pkgs.entry(u.pkg_id.clone()).or_insert(u);
        }
        let mut suggestions: Vec<String> = Vec::new();
        if !pkgs.values().all(|u| u.is_local) {
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
        if let Some(artifact) = v.get("artifact").and_then(|a| a.as_str()) {
            if artifact.ends_with(".rmeta") && u.rmeta_early {
                if let Some(live) = live {
                    if let Some(entry) = live.ingest(artifact.as_bytes()) {
                        let ents = vec![(artifact.as_bytes().to_vec(), entry)];
                        self.layer.add(i as u32, LEVEL_META, &ents);
                        self.early.lock().entry(i).or_default().extend(ents);
                        self.times.lock()[i].meta = Some(std::time::Instant::now());
                        self.req.lock().unit_meta(i);
                        self.states[i].send_replace(UState::Meta);
                    }
                }
            }
            return;
        }
        if v.get("unused_extern_names").is_some() {
            self.ui.lock().unused_externs.insert(i, line.to_string());
            return;
        }
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

    fn apply_worktree_writes(&self, m: &Manifest) {
        let plan = self.plan();
        apply_writes(&self.c, &self.c.out.tr, &[&plan.target_dir, &plan.build_dir], m)
    }

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

    fn materialize_roots(&self) {
        let plan = self.plan();
        let tr = &self.c.out.tr;
        let mut files: Vec<Delivery> = Vec::new();
        let mut writes = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut push = |files: &mut Vec<Delivery>, path: String, hash: &Hash, exec: bool, size: u64, own: bool| {
            if seen.insert(path.clone()) {
                files.push(Delivery { path, hash: hash.0, size, exec, own });
            }
        };
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
                for d in o.hardlink.iter().chain(o.export_path.iter()) {
                    push(&mut files, tr.path(d), hash, *exec, *size, false);
                }
                if u.executable.as_deref() == Some(o.path.as_str()) && !run_here.contains(&ri) {
                    push(&mut files, tr.path(&o.path), hash, *exec, *size, true);
                    if let Some(fp) = stock_fingerprint(&o.path, &u.pkg_name) {
                        writes.push(Write::Remove { path: tr.path(&fp) });
                    }
                }
            }
        }
        let mut docs: BTreeMap<&[u8], (Hash, bool, u64)> = BTreeMap::new();
        for &i in &self.pd.facts.doc_order {
            let (Some(dir), Some((r, _))) = (&self.pd.facts.doc_dir[i], self.results[i].get()) else { continue };
            for (path, e) in &r.outputs.entries {
                if let (MEntry::File { hash, exec, size }, true) = (e, vpath_starts_with(path, dir)) {
                    docs.insert(path, (*hash, *exec, *size));
                }
            }
        }
        for (path, (hash, exec, size)) in docs {
            push(&mut files, tr.path(&String::from_utf8_lossy(path)), &hash, exec, size, true);
        }
        if files.is_empty() {
            return;
        }
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
                    let real = |s: &String| out.tr.path(s);
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

fn stock_fingerprint(exe: &str, pkg: &str) -> Option<String> {
    let (dir, file) = exe.rsplit_once('/')?;
    if let Some(unit) = dir.strip_suffix("/out") {
        let mut up = unit.rsplit('/');
        let (_hash, _pkg, build) = (up.next()?, up.next()?, up.next()?);
        return (build == "build").then(|| format!("{unit}/fingerprint"));
    }
    let profile = dir.strip_suffix("/deps")?;
    let (_, hash) = file.rsplit_once('-')?;
    Some(format!("{profile}/.fingerprint/{pkg}-{hash}"))
}

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

fn parse_dep_info(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        if line.starts_with('#') {
            continue;
        }
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
    assert!(out.iter().all(|p| !p.is_empty()), "a prerequisite is never empty");
    out
}

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
    assert!(out.components().all(|c| !matches!(c, std::path::Component::CurDir | std::path::Component::ParentDir)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stock_fingerprint_of_an_executable_s_unit() {
        assert_eq!(
            stock_fingerprint("/w/target/debug/build/app/02ffaad58935a0bc/out/it-02ffaad58935a0bc", "app").as_deref(),
            Some("/w/target/debug/build/app/02ffaad58935a0bc/fingerprint")
        );
        assert_eq!(
            stock_fingerprint("/w/target/debug/deps/it-02ffaad58935a0bc", "my-app").as_deref(),
            Some("/w/target/debug/.fingerprint/my-app-02ffaad58935a0bc")
        );
        assert_eq!(stock_fingerprint("/w/target/debug/out/x", "app"), None);
        assert_eq!(stock_fingerprint("/w/target/debug/app", "app"), None);
    }
}
