use rargo_machine::StateMachine;
use rargo_net::io::file::PathRing as _;
use rargo_proto::waits::{self, Wait};
use rargo::daemon_state::{self, DaemonState, Head, Need, ScanPlan};
use crate::mux::Mux;
use crate::outfs::{OutFs, Reader, VFile};
use crate::partial::Partial;
use crate::place::{Placed, Placer};
use crate::watch::{Registration, Watcher};
use crate::{Config, Policy, git, report, scan};
use rargo_net::link::Link;
use rargo_proto::{Blob, CONTROL, ClientMsg, Digest, LocalReply, LocalRequest, Offer, PROTO_VERSION, RunRequest, ServerMsg};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use rargo_net::io::{self, UnboundedReceiver, UnboundedSender, unbounded_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PROBE_FROM: u64 = 32 << 10;
const MOST_PARTIALS: usize = 64;

const PUSH_BLIND: u64 = 1 << 20;
const SETTLE: Duration = Duration::from_millis(20);

#[derive(Default)]
pub struct Counters {
    pub commands: AtomicU64,
    pub scans: AtomicU64,
    pub rescans: AtomicU64,
    pub rescanned_dirs: AtomicU64,
    pub scans_spared: AtomicU64,
    pub offers: AtomicU64,
    pub probes_known: AtomicU64,
    pub offered_bytes: AtomicU64,
    pub fetched_contents: AtomicU64,
    pub fetched_bytes: AtomicU64,
    pub reused_bytes: AtomicU64,
}

struct Mounted {
    fs: Arc<OutFs>,
    session: Arc<fusewire::Session>,
    dir: PathBuf,
    dev: u64,
}

struct Snap {
    scan: scan::Scan,
    policy: Policy,
    head: Option<Head>,
    left_out: Option<scan::Ignored>,
}

struct WtState {
    policy_dir: PathBuf,
    loaded: Option<scan::Index>,
    last: Option<Arc<Snap>>,
    watched: HashMap<PathBuf, i32>,
    unwatched: HashSet<PathBuf>,
    told_limit: bool,
    note: Option<String>,
}

struct Worktree {
    id: u64,
    root: PathBuf,
    wt_id: String,
    wt_cache: PathBuf,
    state: io::Mutex<WtState>,
    events: Mutex<HashSet<PathBuf>>,
    changed: io::Notify,
    running: io::Mutex<()>,
    last_used: Mutex<Instant>,
    out: Mutex<Option<Mounted>>,
    out_failed: AtomicBool,
}

pub struct Daemon {
    cfg: Config,
    cache: PathBuf,
    host: String,
    transport: String,
    mux: io::Mutex<Option<Arc<Mux>>>,
    worktrees: Mutex<HashMap<PathBuf, Arc<Worktree>>>,
    watcher: Option<Arc<Watcher>>,
    next_wt: AtomicU64,
    asked: Mutex<Vec<String>>,
    active: AtomicUsize,
    last_active: Mutex<Instant>,
    pub counters: Counters,
    big_sync: io::Mutex<()>,
    fetching: Mutex<HashMap<Digest, Arc<io::Mutex<()>>>>,
    partials: Mutex<HashMap<Digest, (u64, Arc<Partial>)>>,
    chunks: Mutex<crate::known::Chunks>,
    chunks_changed: AtomicBool,
    knowledge: Mutex<DaemonState>,
    held_file: crate::known::HeldFile,
    saving_held: Mutex<()>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Probe {
    Wait,
    Bet,
    Whole,
}

enum Offered {
    Sent,
    Changed,
    Bet(io::oneshot::Receiver<(Vec<Digest>, Vec<Digest>)>),
}

enum CommandEffect {
    Relay(LocalReply),
    Trees(Vec<Digest>),
    Upload(Vec<Digest>),
    Environment(Vec<String>),
    Deliver(Vec<rargo_proto::Delivery>, Vec<rargo_proto::Write>, Vec<String>),
    Blobs(Vec<Blob>),
    Part(Digest, rargo_proto::Part, bool),
    VerifyFetch,
    Report(Box<rargo_proto::FutureReport>),
    ExplainExit,
    Abandon,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandKind {
    Admin,
    Sync,
    Run,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandPhase {
    Link,
    Scan,
    Offer,
    Request,
    Finished,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Link,
    Scan,
    Offer(Probe),
    Request,
    Rescan,
    Exit(i32),
    Fail(&'static str),
    Error(&'static str),
    GiveUp,
    Silent { quiet: Duration, waiting_for: &'static str },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sent {
    All,
    Bet,
    Changed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum After {
    NotThere(Probe),
    Changed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BetAnswer {
    Known,
    Lacking,
}

const KEPT_CHANGING: &str = "the worktree kept changing while it was being synced";
const UNEXPECTED: &str = "unexpected answer from the server";

struct CommandState {
    kind: CommandKind,
    phase: CommandPhase,
    bet: bool,
    after: Option<After>,
    retry_used: bool,
    changes: u8,
    probe: Probe,
    visible: bool,
    capture_errors: bool,
    heard: Vec<u8>,
    fetching: Option<(u64, Vec<Digest>)>,
    delivery_ready: bool,
    last: Option<LocalReply>,
    token: Option<u64>,
    silence: Option<(Duration, &'static str)>,
}

impl CommandState {
    fn new(kind: CommandKind, probe: Probe) -> Self {
        Self {
            kind, phase: CommandPhase::Link, bet: false, after: None, retry_used: false, changes: 0, probe, visible: false,
            capture_errors: false, heard: Vec::new(), fetching: None, delivery_ready: false, last: None, token: None,
            silence: None,
        }
    }

    fn begin(&self) -> Step {
        assert_eq!(self.phase, CommandPhase::Link);
        Step::Link
    }

    fn linked(&mut self) -> Step {
        assert_eq!(self.phase, CommandPhase::Link, "a link is made when one is asked for");
        if self.kind == CommandKind::Admin {
            self.phase = CommandPhase::Request;
            return Step::Request;
        }
        self.phase = CommandPhase::Scan;
        Step::Scan
    }

    fn scanned(&mut self) -> Step {
        assert_eq!(self.phase, CommandPhase::Scan, "a scan is made when one is asked for");
        self.phase = CommandPhase::Offer;
        Step::Offer(self.probe)
    }

    fn offered(&mut self, sent: Sent) -> Step {
        assert_eq!(self.phase, CommandPhase::Offer, "an offer is made when one is asked for");
        match sent {
            Sent::Changed => self.change(),
            Sent::All | Sent::Bet if self.kind == CommandKind::Sync => {
                self.phase = CommandPhase::Finished;
                Step::Exit(0)
            }
            Sent::All | Sent::Bet => {
                self.bet = sent == Sent::Bet;
                self.phase = CommandPhase::Request;
                Step::Request
            }
        }
    }

    fn change(&mut self) -> Step {
        self.changes += 1;
        self.visible = false;
        if self.changes < 5 {
            self.phase = CommandPhase::Link;
            Step::Link
        } else {
            self.phase = CommandPhase::Finished;
            Step::Fail(KEPT_CHANGING)
        }
    }

    /// The link closed; `silent` is how long the server had said nothing, and what was awaited, when that is why.
    fn link_lost(&mut self, silent: Option<(Duration, &'static str)>) -> Step {
        self.bet = false;
        self.after = None;
        self.fetching = None;
        self.delivery_ready = false;
        if silent.is_some() {
            self.silence = silent;
        }
        if self.visible || self.retry_used || self.phase == CommandPhase::Finished {
            self.phase = CommandPhase::Finished;
            return match self.silence {
                Some((quiet, waiting_for)) => Step::Silent { quiet, waiting_for },
                None => Step::GiveUp,
            };
        }
        self.retry_used = true;
        self.phase = CommandPhase::Link;
        Step::Link
    }

    /// Connecting failed: after a server went silent, the command ends with that silence, not a new error.
    fn connect_failed(&mut self) -> Option<Step> {
        let (quiet, waiting_for) = self.silence?;
        self.phase = CommandPhase::Finished;
        Some(Step::Silent { quiet, waiting_for })
    }

    fn again(&mut self) -> Step {
        assert_eq!(self.phase, CommandPhase::Request, "a request ends before it is made again");
        self.bet = false;
        self.fetching = None;
        self.delivery_ready = false;
        let after = self.after.take();
        if self.kind == CommandKind::Admin {
            self.phase = CommandPhase::Finished;
            return Step::Error(UNEXPECTED);
        }
        match after.expect("a request that ends without an answer says why") {
            After::NotThere(probe) => {
                self.probe = probe;
                self.visible = false;
                self.phase = CommandPhase::Link;
                Step::Link
            }
            After::Changed => match self.change() {
                Step::Link => Step::Rescan,
                other => other,
            },
        }
    }

    fn rescanned(&mut self) -> Step {
        assert_eq!(self.phase, CommandPhase::Link);
        Step::Link
    }

    fn bet_answered(&mut self, trees_lacking: bool, blobs_lacking: bool) -> BetAnswer {
        assert!(self.bet, "a bet is answered once");
        self.bet = false;
        if !trees_lacking && !blobs_lacking {
            return BetAnswer::Known;
        }
        self.after = Some(After::NotThere(if trees_lacking { Probe::Whole } else { Probe::Wait }));
        BetAnswer::Lacking
    }

    fn uploaded(&mut self, ok: bool) -> CommandEffect {
        if ok {
            return CommandEffect::None;
        }
        self.after = Some(After::Changed);
        CommandEffect::Abandon
    }

    fn start(&mut self, capture_errors: bool) {
        assert_eq!(self.phase, CommandPhase::Request);
        self.visible = false;
        self.capture_errors = capture_errors;
        self.heard.clear();
        self.fetching = None;
        self.delivery_ready = false;
        self.last = None;
        self.token = None;
    }

    fn server(&mut self, msg: ServerMsg) -> CommandEffect {
        if self.bet && matches!(msg, ServerMsg::WantTrees(_) | ServerMsg::WantBlobs(_)) {
            self.bet = false;
            self.after = Some(After::NotThere(Probe::Whole));
            return CommandEffect::Abandon;
        }
        match msg {
            ServerMsg::Stdout(bytes) => {
                self.visible = true;
                CommandEffect::Relay(LocalReply::Stdout(bytes))
            }
            ServerMsg::Stderr(bytes) => {
                self.visible = true;
                if self.capture_errors && self.heard.len() < HEARD_MAX {
                    self.heard.extend_from_slice(&bytes[..bytes.len().min(HEARD_MAX - self.heard.len())]);
                }
                CommandEffect::Relay(LocalReply::Stderr(bytes))
            }
            ServerMsg::WantTrees(want) => CommandEffect::Trees(want),
            ServerMsg::WantBlobs(want) => CommandEffect::Upload(want),
            ServerMsg::WantEnv(names) => CommandEffect::Environment(names),
            ServerMsg::Deliver { files, writes, targets, .. } => {
                self.visible = true;
                CommandEffect::Deliver(files, writes, targets)
            }
            ServerMsg::Blobs(blobs) => CommandEffect::Blobs(blobs),
            ServerMsg::BlobPart { hash, part, last } => CommandEffect::Part(hash, part, last),
            ServerMsg::Delivered { .. } => {
                self.delivery_ready = true;
                CommandEffect::None
            }
            ServerMsg::Fetched { id } if self.fetching.as_ref().is_some_and(|(sent, _)| *sent == id) => CommandEffect::VerifyFetch,
            ServerMsg::DeliveryToken(token) => {
                self.token = Some(token);
                CommandEffect::None
            }
            ServerMsg::Report(report) => CommandEffect::Report(report),
            ServerMsg::Exit(code) => {
                self.last = Some(LocalReply::Exit(code));
                if code != 0 && self.capture_errors { CommandEffect::ExplainExit } else { CommandEffect::None }
            }
            ServerMsg::Exec(exec) => {
                self.last = Some(LocalReply::Exec(exec));
                CommandEffect::None
            }
            _ => CommandEffect::None,
        }
    }

    fn ready_to_fetch(&mut self) -> bool {
        if self.fetching.is_none() { std::mem::take(&mut self.delivery_ready) } else { false }
    }

    fn request_fetch(&mut self, id: u64, hashes: Vec<Digest>) {
        self.fetching = Some((id, hashes));
    }

    fn verified_fetch(&mut self, complete: bool) -> Result<(), &'static str> {
        if !complete {
            return Err("the server did not send every file it announced");
        }
        self.fetching = None;
        Ok(())
    }

    fn finish(&mut self) -> Option<LocalReply> {
        if self.fetching.is_some() {
            return None;
        }
        let reply = self.last.take()?;
        self.bet = false;
        self.phase = CommandPhase::Finished;
        Some(reply)
    }
}

#[cfg(test)]
impl StateMachine for CommandState {
    fn invariants(&self) {
        assert!(self.changes <= 5 && self.heard.len() <= HEARD_MAX);
        assert!(self.fetching.as_ref().is_none_or(|(_, hashes)| !hashes.is_empty()), "a fetch asks for something");
        assert!(!self.bet || self.phase == CommandPhase::Request, "a bet is pending only while a request runs");
        assert!(self.after.is_none() || self.phase == CommandPhase::Request, "why a request ended is kept until it is asked");
        assert!(self.kind != CommandKind::Admin || !matches!(self.phase, CommandPhase::Scan | CommandPhase::Offer));
        if self.phase == CommandPhase::Finished {
            assert!(self.fetching.is_none() && !self.bet, "a finished command waits for nothing");
        }
    }
}

fn lost() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::BrokenPipe, "the server went away")
}

fn sync_stats(vars: &[(String, String)], args: &[String]) -> bool {
    waits::shown(args, var(vars, "RARGO_SYNC_STATS"))
}

fn var<'a>(vars: &'a [(String, String)], name: &str) -> Option<&'a str> {
    vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str()).filter(|v| !v.is_empty())
}

#[derive(Debug)]
struct NoLink(std::io::Error);

impl std::fmt::Display for NoLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for NoLink {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

fn refused_text(server: &str, had_token: bool) -> String {
    if had_token {
        format!(
            "error: the rargo server at {server} does not accept this machine's token: it does not know it, or it was revoked \
             or has expired. Run `rargo auth` to get a new one\n"
        )
    } else {
        format!(
            "error: this machine is not authenticated to the rargo server at {server}: run `rargo auth`, which also says how to get \
             access if the server has not let you in yet\n"
        )
    }
}

const LOW_SPACE: u64 = 64 << 20;

fn low_space_text(cache: &Path, free: u64) -> Option<String> {
    (free < LOW_SPACE).then(|| {
        format!(
            "warning: the disk that holds rargo's cache ({}) has {} MiB free. Built files are read through that cache, \
             so running them fails once it is full. Free space on that disk; the cache itself may be deleted after \
             `rargo --rargo-daemon-stop`\n",
            cache.display(),
            free >> 20
        )
    })
}

fn free_bytes(dir: &Path) -> Option<u64> {
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    crate::kernel::statvfs(&c).ok().map(|s| s.f_bavail as u64 * s.f_frsize as u64)
}

fn no_link(e: std::io::Error) -> std::io::Error {
    use std::io::ErrorKind::*;
    if matches!(e.kind(), ConnectionRefused | TimedOut | NotFound | HostUnreachable | NetworkUnreachable) {
        std::io::Error::new(e.kind(), NoLink(e))
    } else {
        e
    }
}

fn is_no_link(e: &std::io::Error) -> bool {
    e.get_ref().is_some_and(|inner| inner.is::<NoLink>())
}

impl Daemon {
    pub fn new(cfg: Config, cache: PathBuf, watch: bool) -> Arc<Daemon> {
        let chunks = crate::known::load_chunks(&cache);
        let (held_file, held) = crate::known::HeldFile::load(&cache, &cfg.server, &cfg.user);
        let watcher = if watch {
            match Watcher::new() {
                Ok(w) => Some(Arc::new(w)),
                Err(e) => {
                    eprintln!("rargo daemon: cannot watch worktrees ({e}); every command will scan");
                    None
                }
            }
        } else {
            None
        };
        let d = Arc::new(Daemon {
            asked: Mutex::new(crate::load_asked(&cache)),
            cfg,
            cache,
            host: crate::hostname(),
            transport: io::env::var("RARGO_TRANSPORT").unwrap_or_default(),
            mux: io::Mutex::new(None),
            worktrees: Mutex::new(HashMap::new()),
            watcher,
            next_wt: AtomicU64::new(1),
            active: AtomicUsize::new(0),
            last_active: Mutex::new(io::now()),
            counters: Counters::default(),
            big_sync: io::Mutex::new(()),
            fetching: Mutex::new(HashMap::new()),
            partials: Mutex::new(HashMap::new()),
            chunks: Mutex::new(chunks),
            chunks_changed: AtomicBool::new(false),
            knowledge: Mutex::new(DaemonState::new(held)),
            held_file,
            saving_held: Mutex::new(()),
        });
        if let Some(w) = d.watcher.clone() {
            let d2 = d.clone();
            io::spawn(move || {
                while let Ok(events) = w.next() {
                    d2.take_changes(&events);
                }
            });
        }
        d
    }

    fn take_changes(&self, events: &crate::watch::Changes) {
        let all: Vec<Arc<Worktree>> = self.worktrees.lock().unwrap().values().cloned().collect();
        for wt in all {
            let paths = events.dirty.get(&wt.id);
            let rescan = events.rescan_all || events.rescan.contains(&wt.id);
            if let Some(paths) = paths {
                wt.events.lock().unwrap().extend(paths.iter().cloned());
            }
            if paths.is_some() || rescan {
                self.knowledge.lock().unwrap().changed(wt.id, rescan);
                wt.changed.notify_one();
            }
        }
    }

    fn take_queued_changes(&self) {
        let Some(w) = &self.watcher else { return };
        while let Ok(Some(events)) = w.queued() {
            self.take_changes(&events);
        }
    }

    fn mux(&self) -> std::io::Result<Arc<Mux>> {
        let mut g = self.mux.lock();
        let alive = g.as_ref().is_some_and(|mux| !mux.is_dead());
        if self.knowledge.lock().unwrap().link(alive, Need::Must) == daemon_state::Link::Use {
            return Ok(g.as_ref().unwrap().clone());
        }
        let addr = io::lookup_host(&self.cfg.server)
            .map_err(no_link)?
            .into_iter()
            .next()
            .ok_or_else(|| no_link(std::io::Error::new(std::io::ErrorKind::NotFound, "the server's name has no address")))?;
        let link = Link::connect(addr, &self.cfg.user, &self.cfg.psk, &self.transport).map_err(no_link)?;
        let m = Mux::new(link);
        *g = Some(m.clone());
        self.knowledge.lock().unwrap().connected();
        Ok(m)
    }

    /// Whether a live link to the server is already open, so a command need not connect.
    fn link_up(&self) -> bool {
        self.mux.lock().as_ref().is_some_and(|mux| !mux.is_dead())
    }

    fn mux_if_up(&self) -> Option<Arc<Mux>> {
        let link = self.mux.lock();
        let alive = link.as_ref().is_some_and(|mux| !mux.is_dead());
        match self.knowledge.lock().unwrap().link(alive, Need::IfUp) {
            daemon_state::Link::Use => link.clone(),
            _ => None,
        }
    }

    fn worktree(self: &Arc<Self>, root: &Path) -> Arc<Worktree> {
        let mut all = self.worktrees.lock().unwrap();
        if let Some(wt) = all.get(root) {
            *wt.last_used.lock().unwrap() = io::now();
            return wt.clone();
        }
        let wt_id = {
            let mut h = blake3::Hasher::new();
            h.update(self.host.as_bytes());
            h.update(&[0]);
            h.update(root.as_os_str().as_bytes());
            h.finalize().to_hex()[..24].to_string()
        };
        let wt_cache = self.cache.join("wt").join(&wt_id);
        let wt = Arc::new(Worktree {
            id: self.next_wt.fetch_add(1, Ordering::Relaxed),
            root: root.to_path_buf(),
            state: io::Mutex::new(WtState {
                policy_dir: PathBuf::new(),
                loaded: Some(scan::Index::load(&wt_cache.join("index"))),
                last: None,
                watched: HashMap::new(),
                unwatched: HashSet::new(),
                told_limit: false,
                note: None,
            }),
            wt_id,
            wt_cache,
            events: Mutex::new(HashSet::new()),
            changed: io::Notify::new(),
            running: io::Mutex::new(()),
            last_used: Mutex::new(io::now()),
            out: Mutex::new(None),
            out_failed: AtomicBool::new(false),
        });
        self.knowledge.lock().unwrap().added(wt.id);
        all.insert(root.to_path_buf(), wt.clone());
        if self.watcher.is_some() {
            let (d, w) = (self.clone(), Arc::downgrade(&wt));
            io::spawn(move || {
                loop {
                    let Some(wt) = w.upgrade() else { break };
                    wt.changed.notified();
                    if !d.worktrees.lock().unwrap().values().any(|x| x.id == wt.id) {
                        break;
                    }
                    io::sleep(SETTLE);
                    let Some(mux) = d.mux_if_up() else { continue };
                    let _running = wt.running.lock();
                    let mut st = wt.state.lock();
                    if let Ok((snap, _)) = d.fresh(&wt, &mut st) {
                        let _ = d.offer(&mux, &wt, &snap, Probe::Wait);
                    }
                }
            });
        }
        wt
    }

    fn fresh(&self, wt: &Arc<Worktree>, st: &mut WtState) -> std::io::Result<(Arc<Snap>, bool)> {
        self.take_queued_changes();
        let head = git::head(&wt.root);
        let watching = self.watcher.is_some();
        let plan = self.knowledge.lock().unwrap().plan_scan(wt.id, head.as_ref(), watching);
        let everything = match (plan, &st.last) {
            (ScanPlan::Reuse, Some(last)) => {
                self.counters.scans_spared.fetch_add(1, Ordering::Relaxed);
                return Ok((last.clone(), false));
            }
            (ScanPlan::Reuse, None) => true,
            (ScanPlan::Scan { everything }, _) => everything,
        };
        let mut changed: std::collections::BTreeSet<PathBuf> = std::mem::take(&mut *wt.events.lock().unwrap()).into_iter().collect();
        changed.extend(st.unwatched.iter().cloned());
        if let Some(w) = &self.watcher {
            changed.extend(st.watched.iter().filter(|(d, wd)| !w.watches(**wd, d)).map(|(d, _)| d.clone()));
        }
        let (root, cache, wt_cache) = (wt.root.clone(), self.cache.clone(), wt.wt_cache.clone());
        let (previous, loaded) = (st.last.take(), st.loaded.take());
        let policy_dir = st.policy_dir.clone();
        let snap = io::cpu(move || -> std::io::Result<Snap> {
            let mut policy = crate::load_policy(&root, &policy_dir);
            if let Ok(inside) = cache.ring_canonicalize().unwrap_or_else(|_| cache.clone()).strip_prefix(&root) {
                policy.exclude.push(format!("/{}", inside.to_string_lossy()));
            }
            let left_out = if policy.gitignored { None } else { scan::Ignored::from_git(&root, &policy.include) };
            let reusable = previous.as_ref().is_some_and(|p| {
                !everything
                    && p.policy.exclude == policy.exclude
                    && p.policy.gitignored == policy.gitignored
                    && p.policy.include == policy.include
                    && p.left_out == left_out
            });
            let whole = |old: &scan::Index| scan::scan(&root, old, &policy.exclude, left_out.as_ref());
            let scanned = match previous {
                Some(p) if reusable => {
                    let prev = Arc::try_unwrap(p).map(|s| s.scan).unwrap_or_else(|shared| shared.scan.clone());
                    scan::rescan(&root, prev, &changed, &policy.exclude, left_out.as_ref())
                        .or_else(|_| whole(&scan::Index::load(&wt_cache.join("index"))))
                }
                Some(p) => whole(&p.scan.index),
                None => whole(&loaded.unwrap_or_default()),
            };
            let mut scan = scanned.map_err(|e| std::io::Error::new(e.kind(), format!("scanning {}: {e}", root.display())))?;
            if scan.index_changed {
                let _ = scan.index.save(&wt_cache.join("index"));
            }
            if let Some(h) = &head {
                let history = match policy.git {
                    git::Mode::Off => None,
                    git::Mode::Head => Some(false),
                    git::Mode::History => Some(true),
                    git::Mode::Detect => Some(git::asks_for_history(&root, scan.index.files.keys().map(|k| k.as_slice()))),
                };
                let objects = if history == Some(true) { git::history(h, &cache) } else { None };
                let thin = history.and_then(|_| git::thin(&root, h, &cache, objects.is_some()));
                if let Some(thin) = thin {
                    let stored = match &objects {
                        Some(dir) => {
                            let at = dir.parent().and_then(Path::parent).unwrap_or(dir).join("index");
                            let s = scan::scan_store(dir, &scan::Index::load(&at))?;
                            if s.index_changed {
                                let _ = s.index.save(&at);
                            }
                            Some(s)
                        }
                        None => None,
                    };
                    scan::graft_git(&mut scan, &root, &thin, stored)?;
                }
            }
            Ok(Snap { scan, policy, head, left_out })
        })?;
        if snap.scan.listed_dirs < snap.scan.dirs.len() as u64 {
            self.counters.rescans.fetch_add(1, Ordering::Relaxed);
            self.counters.rescanned_dirs.fetch_add(snap.scan.listed_dirs, Ordering::Relaxed);
        } else {
            self.counters.scans.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(w) = &self.watcher {
            self.watch(w, wt, st, &snap);
        }
        self.knowledge.lock().unwrap().scanned(wt.id, snap.head.clone());
        let snap = Arc::new(snap);
        st.last = Some(snap.clone());
        Ok((snap, true))
    }

    fn watch(&self, w: &Watcher, wt: &Worktree, st: &mut WtState, snap: &Snap) {
        let now: HashSet<&PathBuf> = snap.scan.dirs.iter().collect();
        let mut unwatched = HashSet::new();
        let mut added = Vec::new();
        let mut over_limit = 0usize;
        let mut failed = None;
        for dir in &snap.scan.dirs {
            if st.watched.get(dir).is_some_and(|wd| w.watches(*wd, dir)) {
                continue;
            }
            st.watched.remove(dir);
            match w.add(dir, wt.id) {
                Ok(Registration::Watched(wd)) => {
                    st.watched.insert(dir.clone(), wd);
                    added.push(dir.clone());
                }
                Ok(Registration::Poll(dir)) => {
                    over_limit += 1;
                    unwatched.insert(dir);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => added.push(dir.clone()),
                Err(e) => {
                    if failed.is_none() && !st.unwatched.contains(dir) {
                        failed = Some(format!("rargo: cannot watch {} ({e}); it is looked at again for every command\n", dir.display()));
                    }
                    unwatched.insert(dir.clone());
                }
            }
        }
        st.watched.retain(|d, wd| {
            let keep = now.contains(d);
            if !keep {
                w.remove(*wd, d);
            }
            keep
        });
        if !added.is_empty() {
            wt.events.lock().unwrap().extend(added);
            self.knowledge.lock().unwrap().changed(wt.id, false);
        }
        let mut note = failed.unwrap_or_default();
        if over_limit > 0 && !st.told_limit {
            let limit = Watcher::limit().unwrap_or(0);
            let enough = (limit + 2 * over_limit as u64).div_ceil(65536) * 65536;
            note.push_str(&format!(
                "rargo: {over_limit} of the {} directories of {} are not watched: the kernel allows {limit} inotify watches per user, and they are in use. \
                 Those directories are looked at again for every command. To watch them all: sysctl fs.inotify.max_user_watches={enough}\n",
                snap.scan.dirs.len(),
                wt.root.display()
            ));
        }
        st.told_limit = over_limit > 0;
        self.knowledge.lock().unwrap().watched(wt.id, unwatched.is_empty());
        st.unwatched = unwatched;
        if !note.is_empty() {
            st.note = Some(note);
        }
    }

    fn offer(&self, mux: &Arc<Mux>, wt: &Arc<Worktree>, snap: &Arc<Snap>, probe: Probe) -> std::io::Result<Offered> {
        let delta = || -> (Vec<Digest>, Vec<(Digest, u64)>) {
            self.knowledge.lock().unwrap().offer(
                Some(wt.id),
                &snap.scan.root,
                snap.scan.trees.keys(),
                snap.scan.files.iter().map(|(d, (_, size))| (d, *size)),
            )
        };
        let (mut trees, mut blobs) = delta();
        if trees.is_empty() && blobs.is_empty() {
            return Ok(Offered::Sent);
        }
        let _one_at_a_time = if blobs.iter().map(|(_, s)| *s).sum::<u64>() > PUSH_BLIND {
            let turn = self.big_sync.lock();
            (trees, blobs) = delta();
            if trees.is_empty() && blobs.is_empty() {
                return Ok(Offered::Sent);
            }
            Some(turn)
        } else {
            None
        };
        self.counters.offers.fetch_add(1, Ordering::Relaxed);
        if io::env::var_os("RARGO_DAEMON_TRACE").is_some() {
            let new: HashSet<&Digest> = trees.iter().collect();
            let mut paths: Vec<String> = Vec::new();
            let mut todo: Vec<(String, Digest)> = vec![(String::new(), snap.scan.root)];
            while let Some((path, d)) = todo.pop() {
                if new.contains(&d) {
                    paths.push(if path.is_empty() { ".".into() } else { path.clone() });
                }
                let Some(Ok(tree)) = snap.scan.trees.get(&d).map(|e| rargo_proto::tree::Tree::decode(e)) else { continue };
                for (name, node) in &tree.entries {
                    if let rargo_proto::tree::Node::Dir(sub) = node {
                        todo.push((format!("{path}/{}", String::from_utf8_lossy(name)), *sub));
                    }
                }
            }
            eprintln!(
                "rargo daemon: {}: pushing {} manifests ({}) and {} files",
                wt.root.display(),
                trees.len(),
                paths.join(" "),
                blobs.len()
            );
        }
        let encoded: Vec<Vec<u8>> = trees.iter().filter_map(|d| snap.scan.trees.get(d).cloned()).collect();
        let tree_bytes: u64 = encoded.iter().map(|t| t.len() as u64).sum();
        let blob_bytes: u64 = blobs.iter().map(|(_, s)| *s).sum();
        let id = mux.next_id();
        if blob_bytes <= PUSH_BLIND && probe != Probe::Wait {
            let snap2 = snap.clone();
            let wanted: Vec<Digest> = blobs.iter().map(|(d, _)| *d).collect();
            let packed = io::cpu(move || -> Option<Vec<Blob>> {
                wanted
                    .iter()
                    .map(|h| {
                        let (path, _) = snap2.scan.files.get(h)?;
                        let bytes = rargo_net::io::file::read(path).ok()?;
                        (*blake3::hash(&bytes).as_bytes() == *h).then(|| Blob::pack(*h, &bytes))
                    })
                    .collect()
            });
            let Some(packed) = packed else {
                self.knowledge.lock().unwrap().changed(wt.id, true);
                return Ok(Offered::Changed);
            };
            self.counters.offered_bytes.fetch_add(tree_bytes + blob_bytes, Ordering::Relaxed);
            let msg = ClientMsg::Offer(Box::new(Offer { id, root: snap.scan.root, trees: encoded, blobs: packed, ask: false }));
            mux.post_bulk(CONTROL, &msg, (tree_bytes + blob_bytes) as usize)?;
        } else {
            let mut encoded = encoded;
            let mut tree_bytes = tree_bytes;
            let mut known = if tree_bytes > PROBE_FROM && probe != Probe::Whole {
                let answer = mux.expect_missing(id);
                let msg = ClientMsg::Offer(Box::new(Offer { id, root: snap.scan.root, trees: Vec::new(), blobs: Vec::new(), ask: true }));
                mux.post_bulk(CONTROL, &msg, 0)?;
                if probe == Probe::Bet {
                    return Ok(Offered::Bet(answer));
                }
                let (lacking_trees, lacking_blobs) = answer.recv().ok_or_else(lost)?;
                if !lacking_trees.is_empty() || !lacking_blobs.is_empty() {
                    self.knowledge.lock().unwrap().missing(lacking_trees.iter().chain(&lacking_blobs));
                }
                lacking_trees.is_empty().then_some(lacking_blobs)
            } else {
                None
            };
            let missing = loop {
                if let Some(blobs) = known.take() {
                    self.counters.probes_known.fetch_add(1, Ordering::Relaxed);
                    break blobs;
                }
                let answer = mux.expect_missing(id);
                self.counters.offered_bytes.fetch_add(tree_bytes, Ordering::Relaxed);
                let msg = ClientMsg::Offer(Box::new(Offer { id, root: snap.scan.root, trees: encoded, blobs: Vec::new(), ask: true }));
                mux.post_bulk(CONTROL, &msg, tree_bytes as usize)?;
                let (lacking_trees, lacking_blobs) = answer.recv().ok_or_else(lost)?;
                if !lacking_trees.is_empty() || !lacking_blobs.is_empty() {
                    self.knowledge.lock().unwrap().missing(lacking_trees.iter().chain(&lacking_blobs));
                }
                if lacking_trees.is_empty() {
                    break lacking_blobs;
                }
                encoded = lacking_trees.iter().filter_map(|d| snap.scan.trees.get(d).cloned()).collect();
                if encoded.len() != lacking_trees.len() {
                    return Err(std::io::Error::other("the server asked for a directory this worktree does not have"));
                }
                tree_bytes = encoded.iter().map(|t| t.len() as u64).sum();
            };
            let bytes: u64 = missing.iter().filter_map(|h| snap.scan.files.get(h)).map(|(_, s)| *s).sum();
            self.counters.offered_bytes.fetch_add(bytes, Ordering::Relaxed);
            if !upload(mux, CONTROL, snap, missing)? {
                self.knowledge.lock().unwrap().changed(wt.id, true);
                return Ok(Offered::Changed);
            }
        }
        self.has_now(Some(wt), snap);
        Ok(Offered::Sent)
    }

    fn has_now(&self, worktree: Option<&Arc<Worktree>>, snap: &Snap) {
        self.knowledge.lock().unwrap().synced(worktree.map(|wt| wt.id), snap.scan.root, snap.scan.trees.keys().chain(snap.scan.files.keys()));
    }

    pub fn save_known(&self) {
        {
            let _serial = self.saving_held.lock().unwrap();
            let state = self.knowledge.lock().unwrap().held.take_changes();
            if let Some(body) = state && let Err(e) = self.held_file.save(&body) {
                self.knowledge.lock().unwrap().held.save_failed();
                eprintln!("rargo daemon: cannot keep what the server holds: {e}");
            }
        }
        if self.chunks_changed.swap(false, Ordering::SeqCst) {
            let chunks = self.chunks.lock().unwrap().clone();
            if let Err(e) = crate::known::save_chunks(&self.cache, &chunks) {
                eprintln!("rargo daemon: cannot keep the chunk index: {e}");
            }
        }
    }

    fn out_for(self: &Arc<Self>, wt: &Arc<Worktree>, req: &LocalRequest) -> std::io::Result<Option<(Arc<OutFs>, PathBuf)>> {
        let elsewhere = var(&req.vars, "CARGO_TARGET_DIR").is_some()
            || var(&req.vars, "CARGO_BUILD_TARGET_DIR").is_some()
            || req.args.iter().any(|a| a == "--target-dir" || a.starts_with("--target-dir="));
        let same_ns = req.mount_ns == crate::mount_ns();
        let want = self.watcher.is_some() && var(&req.vars, "RARGO_OUTPUT") != Some("copy") && !elsewhere && same_ns;
        let mut g = wt.out.lock().unwrap();
        if !want {
            if let Some(m) = g.take() {
                m.fs.save();
                io::blocking(|| m.session.unmount());
            }
            return Ok(None);
        }
        if let Some(m) = &*g {
            if m.session.shown() && dev_at(&mountinfo(), &m.dir) == Some(m.dev) {
                return Ok(Some((m.fs.clone(), m.dir.clone())));
            }
            if let Some(m) = g.take() {
                m.fs.save();
                io::blocking(|| m.session.unmount());
            }
        }
        if wt.out_failed.load(Ordering::SeqCst) && var(&req.vars, "RARGO_OUTPUT") != Some("fuse") {
            return Ok(None);
        }
        match self.mount_out(wt) {
            Ok(m) => {
                let r = (m.fs.clone(), m.dir.clone());
                *g = Some(m);
                Ok(Some(r))
            }
            Err(e) if e.kind() == std::io::ErrorKind::Unsupported => Err(e),
            Err(e) => {
                if !wt.out_failed.swap(true, Ordering::SeqCst) {
                    eprintln!("rargo daemon: {}: no virtual output directory ({e}); results are copied into it", wt.root.display());
                }
                Ok(None)
            }
        }
    }

    fn mount_out(self: &Arc<Self>, wt: &Arc<Worktree>) -> std::io::Result<Mounted> {
        let dir = wt.root.join("target");
        release_mounts(&dir);
        if rargo_net::io::file::symlink_metadata(&dir).map(|m| m.file_type().is_symlink()).unwrap_or(false) {
            return Err(std::io::Error::other("`target` is a symbolic link"));
        }
        rargo_net::io::file::create_dir_all(&dir)?;
        let tag = dir.join("CACHEDIR.TAG");
        if !tag.ring_exists() {
            rargo_net::io::file::write(&tag, rargo_proto::CACHEDIR_TAG)?;
        }
        let source = Box::new(Contents { d: Arc::downgrade(self) });
        let fs = OutFs::new(&dir, &wt.wt_cache.join("virtual"), source)?;
        let root = io::process::euid() == 0;
        let opts = fusewire::MountOptions {
            fsname: "rargo".into(),
            allow_other: root,
            default_permissions: true,
            passthrough: true,
            auto_unmount: true,
        };
        let session = Arc::new(io::blocking(|| fusewire::Session::mount_uring(&dir, &opts))?);
        fs.attach(session.clone());
        rargo_net::io::FuseDrive::spawn(&session, &fs)?.detach();
        let dev = dev_at(&mountinfo(), &dir).ok_or_else(|| std::io::Error::other("the mount does not show in /proc/self/mountinfo"))?;
        Ok(Mounted { fs, session, dir, dev })
    }

    fn unmount_all(&self) {
        let all: Vec<Arc<Worktree>> = self.worktrees.lock().unwrap().values().cloned().collect();
        for wt in all {
            if let Some(m) = wt.out.lock().unwrap().take() {
                m.fs.save();
                io::blocking(|| m.session.unmount());
            }
        }
    }

    fn release(&self, cwd: &Path) {
        let Some(root) = crate::find_root(cwd) else { return };
        let wt = self.worktrees.lock().unwrap().get(&root).cloned();
        if let Some(wt) = wt {
            if let Some(m) = wt.out.lock().unwrap().take() {
                io::blocking(|| m.session.unmount());
            }
            let _ = rargo_net::io::file::remove_file(wt.wt_cache.join("virtual"));
            let _ = rargo_net::io::file::remove_file(wt.wt_cache.join("delivered"));
        }
    }

    fn partial(self: &Arc<Self>, hash: Digest, size: u64) -> std::io::Result<Option<Arc<Partial>>> {
        if let Some((_, p)) = self.partials.lock().unwrap().get(&hash) {
            return Ok(Some(p.clone()));
        }
        let turn = self.fetching.lock().unwrap().entry(hash).or_default().clone();
        let _turn = turn.lock();
        if let Some((_, p)) = self.partials.lock().unwrap().get(&hash) {
            return Ok(Some(p.clone()));
        }
        let mux = self.mux()?;
        let (tx, mut rx) = unbounded_channel();
        let id = mux.ask(&tx, |id| ClientMsg::Recipe { id, hash });
        let mut answer = None;
        loop {
            match next_msg(&mux, &mut rx)? {
                ServerMsg::Recipe { chunks, outline, lazy, .. } => answer = Some((chunks, outline, lazy)),
                ServerMsg::Fetched { id: got } if got == id => break,
                _ => {}
            }
        }
        let Some((chunks, outline, lazy)) = answer.filter(|(c, o, _)| !c.is_empty() && !o.is_empty()) else { return Ok(None) };
        let fetcher = Arc::new(ChunkFetcher { d: Arc::downgrade(self) });
        let slot = crate::place::cache_slot(&self.cache, &hash);
        let p = io::cpu(|| Partial::new(hash, size, chunks, outline, &lazy, &slot, fetcher))?;
        self.counters.fetched_contents.fetch_add(1, Ordering::Relaxed);
        let mut partials = self.partials.lock().unwrap();
        while partials.len() >= MOST_PARTIALS {
            let Some(oldest) = partials.iter().min_by_key(|(_, (n, _))| *n).map(|(h, _)| *h) else { break };
            partials.remove(&oldest);
        }
        let n = self.counters.fetched_contents.load(Ordering::Relaxed);
        partials.insert(hash, (n, p.clone()));
        Ok(Some(p))
    }

    fn reuse_chunks(&self, p: &Partial, idx: Vec<u32>) -> Vec<u32> {
        let mut need = Vec::new();
        for i in idx {
            let (chunk, len) = p.recipe()[i as usize];
            let held = self.chunks.lock().unwrap().get(&chunk).copied();
            let local = held.and_then(|(blob, off, l)| {
                let slot = crate::place::cache_slot(&self.cache, &blob);
                let path = crate::place::cached(&self.cache, &blob, rargo_net::io::file::metadata(&slot).ok()?.len())?;
                let mut buf = vec![0u8; l as usize];
                rargo_net::io::file::File::open(path).ok()?.read_exact_at(&mut buf, off).ok()?;
                (l == len).then_some(buf)
            });
            match local {
                Some(buf) => {
                    self.counters.reused_bytes.fetch_add(buf.len() as u64, Ordering::Relaxed);
                    p.deliver(i, &buf);
                }
                None => need.push(i),
            }
        }
        need
    }

    fn fetch_chunks(self: &Arc<Self>, p: &Arc<Partial>, idx: Vec<u32>) -> Vec<u32> {
        let Ok(mux) = self.mux() else { return idx };
        let mut left: HashSet<u32> = idx.iter().copied().collect();
        let (tx, mut rx) = unbounded_channel();
        let hash = *p.hash();
        let id = mux.ask(&tx, |id| ClientMsg::Chunks { id, hash, idx });
        loop {
            match next_msg(&mux, &mut rx) {
                Ok(ServerMsg::Chunks { chunks, .. }) => io::cpu(|| {
                    for (i, part) in chunks {
                        let Ok(data) = part.unpack() else { continue };
                        self.counters.fetched_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                        p.deliver(i, &data);
                        left.remove(&i);
                    }
                }),
                Ok(ServerMsg::Fetched { id: got }) if got == id => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        let mut left: Vec<u32> = left.into_iter().collect();
        left.sort_unstable();
        left
    }

    fn read_through(&self, p: &Partial) {
        let mut index = self.chunks.lock().unwrap();
        if index.len() > crate::known::MOST_CHUNKS {
            index.clear();
        }
        for (i, (chunk, len)) in p.recipe().iter().enumerate() {
            index.insert(*chunk, (*p.hash(), p.offset(i as u32), *len));
        }
        drop(index);
        self.chunks_changed.store(true, Ordering::SeqCst);
        self.partials.lock().unwrap().remove(p.hash());
    }

    fn fetch_content(self: &Arc<Self>, hash: Digest, size: u64) -> std::io::Result<PathBuf> {
        if let Some(p) = crate::place::cached(&self.cache, &hash, size) {
            return Ok(p);
        }
        let turn = self.fetching.lock().unwrap().entry(hash).or_default().clone();
        let _turn = turn.lock();
        if let Some(p) = crate::place::cached(&self.cache, &hash, size) {
            return Ok(p);
        }
        let mux = self.mux()?;
        self.counters.fetched_contents.fetch_add(1, Ordering::Relaxed);
        let (tx, mut rx) = unbounded_channel();
        if size >= rargo_proto::CHUNKED_FROM {
            let id = mux.ask(&tx, |id| ClientMsg::Recipe { id, hash });
            let mut recipe: Vec<(Digest, u32)> = Vec::new();
            loop {
                match next_msg(&mux, &mut rx)? {
                    ServerMsg::Recipe { chunks, .. } => recipe = chunks,
                    ServerMsg::Fetched { id: got } if got == id => break,
                    _ => {}
                }
            }
            if !recipe.is_empty() {
                let slot = crate::place::cache_slot(&self.cache, &hash);
                let tmp = slot.with_file_name(format!(".incoming-{}-{}", io::process::id(), mux.next_id()));
                if let Some(parent) = slot.parent() {
                    rargo_net::io::file::create_dir_all(parent)?;
                }
                let file = rargo_net::io::file::File::create(&tmp)?;
                let mut offsets = Vec::with_capacity(recipe.len());
                let mut at = 0u64;
                for (_, len) in &recipe {
                    offsets.push(at);
                    at += *len as u64;
                }
                let mut need: Vec<u32> = Vec::new();
                io::cpu(|| {
                    for (i, (chunk, len)) in recipe.iter().enumerate() {
                        let held = self.chunks.lock().unwrap().get(chunk).copied();
                        let local = held.and_then(|(blob, off, l)| {
                            let path = crate::place::cached(&self.cache, &blob, rargo_net::io::file::metadata(crate::place::cache_slot(&self.cache, &blob)).ok()?.len())?;
                            let mut buf = vec![0u8; l as usize];
                            rargo_net::io::file::File::open(path).ok()?.read_exact_at(&mut buf, off).ok()?;
                            (l == *len && *blake3::hash(&buf).as_bytes() == *chunk).then_some(buf)
                        });
                        match local {
                            Some(buf) => {
                                self.counters.reused_bytes.fetch_add(buf.len() as u64, Ordering::Relaxed);
                                let _ = file.write_all_at(&buf, offsets[i]);
                            }
                            None => need.push(i as u32),
                        }
                    }
                });
                if !need.is_empty() {
                    let id = mux.ask(&tx, |id| ClientMsg::Chunks { id, hash, idx: need });
                    loop {
                        match next_msg(&mux, &mut rx)? {
                            ServerMsg::Chunks { chunks, .. } => io::cpu(|| -> std::io::Result<()> {
                                for (i, part) in chunks {
                                    let data = part.unpack()?;
                                    self.counters.fetched_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                                    let at = *offsets.get(i as usize).ok_or_else(|| std::io::Error::other("chunk out of range"))?;
                                    file.write_all_at(&data, at)?;
                                }
                                Ok(())
                            })?,
                            ServerMsg::Fetched { id: got } if got == id => break,
                            _ => {}
                        }
                    }
                }
                drop(file);
                let whole = io::cpu(|| scan::hash_file(&tmp, size));
                if whole.ok() != Some(hash) {
                    let _ = rargo_net::io::file::remove_file(&tmp);
                    return Err(std::io::Error::other("the server sent content that does not match its hash"));
                }
                let _ = rargo_net::io::file::set_mode(&tmp, 0o444);
                rargo_net::io::file::rename(&tmp, &slot)?;
                let mut index = self.chunks.lock().unwrap();
                if index.len() > crate::known::MOST_CHUNKS {
                    index.clear();
                }
                for (i, (chunk, len)) in recipe.iter().enumerate() {
                    index.insert(*chunk, (hash, offsets[i], *len));
                }
                self.chunks_changed.store(true, Ordering::SeqCst);
                return Ok(slot);
            }
        }
        let id = mux.fetch(vec![hash], &tx);
        let mut placer = Placer::new(&self.cache, Placed::default());
        loop {
            match next_msg(&mux, &mut rx)? {
                ServerMsg::Blobs(blobs) => io::cpu(|| -> std::io::Result<()> {
                    for b in blobs {
                        let h = b.hash;
                        let bytes = b.unpack()?;
                        self.counters.fetched_bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                        placer.blob(h, &bytes)?;
                    }
                    Ok(())
                })?,
                ServerMsg::BlobPart { hash: h, part, last } => io::cpu(|| -> std::io::Result<()> {
                    let bytes = part.unpack()?;
                    self.counters.fetched_bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    placer.part(h, &bytes, last)
                })?,
                ServerMsg::Fetched { id: got } if got == id => break,
                _ => {}
            }
        }
        crate::place::cached(&self.cache, &hash, size)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "the server no longer has this content"))
    }

    fn touch(&self) {
        *self.last_active.lock().unwrap() = io::now();
    }

    fn close_down(&self, socket: &Path) {
        self.unmount_all();
        self.save_known();
        let _ = rargo_net::io::file::remove_file(socket);
    }

    pub fn command(self: Arc<Self>, req: LocalRequest, out: UnboundedSender<LocalReply>) {
        self.active.fetch_add(1, Ordering::SeqCst);
        struct Active<'a>(&'a Daemon);
        impl Drop for Active<'_> {
            fn drop(&mut self) {
                let was = self.0.active.fetch_sub(1, Ordering::SeqCst);
                assert!(was > 0, "every command that ends was counted when it began");
                self.0.touch();
            }
        }
        let _active = Active(&self);
        self.counters.commands.fetch_add(1, Ordering::Relaxed);
        let reply = match self.run(&req, &out) {
            Ok(reply) => reply,
            Err(e) => {
                if is_no_link(&e) {
                    LocalReply::Unreachable(e.to_string())
                } else if e.to_string().contains(rargo_proto::REFUSED) {
                    LocalReply::Refused(refused_text(&self.cfg.server, self.cfg.secret.is_some()))
                } else {
                    let text = format!("error: talking to the rargo server at {}: {}\n", self.cfg.server, crate::plain_cause(&e.to_string()));
                    let _ = out.send(LocalReply::Stderr(text.into_bytes()));
                    LocalReply::Exit(101)
                }
            }
        };
        let _ = out.send(reply);
    }

    /// Ends a command whose server went silent, with one line naming the server and what rargo waited for.
    fn silent(&self, out: &UnboundedSender<LocalReply>, quiet: Duration, waiting_for: &str) -> LocalReply {
        let text = rargo::link_watch::silent_text(&self.cfg.server, quiet, waiting_for);
        let _ = out.send(LocalReply::Stderr(text.into_bytes()));
        LocalReply::Exit(101)
    }

    fn fail(out: &UnboundedSender<LocalReply>, text: String) -> LocalReply {
        let _ = out.send(LocalReply::Stderr(format!("error: {text}\n").into_bytes()));
        LocalReply::Exit(101)
    }

    fn run(self: &Arc<Self>, req: &LocalRequest, out: &UnboundedSender<LocalReply>) -> std::io::Result<LocalReply> {
        if req.kind == "status" {
            return Ok(self.status(out));
        }
        if req.kind != "admin"
            && let Some(text) = free_bytes(&self.cache).and_then(|free| low_space_text(&self.cache, free))
        {
            let _ = out.send(LocalReply::Stderr(text.into_bytes()));
        }
        let cwd = PathBuf::from(&req.cwd);
        let home = PathBuf::from(var(&req.vars, "HOME").unwrap_or("/"));
        let cargo_home = var(&req.vars, "CARGO_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".cargo"));
        let base = RunRequest {
            version: PROTO_VERSION,
            args: req.args.clone(),
            root: req.cwd.clone(),
            cwd: req.cwd.clone(),
            cargo_home: cargo_home.to_string_lossy().into_owned(),
            env: Vec::new(),
            env_checked: Vec::new(),
            stdout_tty: req.stdout_tty,
            stderr_tty: req.stderr_tty,
            worktree: String::new(),
            commits: Vec::new(),
            tree: [0; 32],
            home_files: Vec::new(),
            who: format!("{}@{}", self.cfg.user, self.host),
            delivered: 0,
            lazy: false,
        };
        let env_for = |policy: &Policy| -> (Vec<(String, String)>, Vec<String>) {
            let asked = self.asked.lock().unwrap().clone();
            let env = req
                .vars
                .iter()
                .filter(|(k, _)| {
                    rargo_proto::env_allowed(k) || policy.pass.iter().any(|p| crate::pattern_matches(p, k)) || asked.contains(k)
                })
                .cloned()
                .collect();
            (env, asked)
        };
        let probe = if req.kind == "sync" || var(&req.vars, "RARGO_OFFER_WAIT").is_some() { Probe::Wait } else { Probe::Bet };
        let kind = match req.kind.as_str() {
            "admin" => CommandKind::Admin,
            "sync" => CommandKind::Sync,
            _ => CommandKind::Run,
        };
        let mut command = CommandState::new(kind, probe);
        if kind == CommandKind::Admin {
            let mut step = command.begin();
            let mut link = None;
            loop {
                step = match step {
                    Step::Link => match self.mux() {
                        Ok(mux) => {
                            link = Some(mux);
                            command.linked()
                        }
                        Err(e) => command.connect_failed().ok_or(e)?,
                    },
                    Step::Request => {
                        let mux = link.clone().expect("a link before a request");
                        let (env, env_checked) = env_for(&Policy::default());
                        let run = RunRequest { env, env_checked, ..base.clone() };
                        match self.converse(&mux, run, None, None, None, req, out, &mut command, None) {
                            Ok(Some(reply)) => return Ok(reply),
                            Ok(None) => command.again(),
                            Err(_) if mux.is_dead() => command.link_lost(mux.silent().map(|quiet| (quiet, "its answer"))),
                            Err(e) => return Err(e),
                        }
                    }
                    Step::Error(text) => return Err(std::io::Error::other(text)),
                    Step::GiveUp => return Err(lost()),
                    Step::Silent { quiet, waiting_for } => return Ok(self.silent(out, quiet, waiting_for)),
                    other => unreachable!("an admin command does not {other:?}"),
                };
            }
        }
        let Some(root) = crate::find_root(&cwd) else {
            return Ok(Self::fail(
                out,
                format!("`{}` is not inside a git worktree or a cargo package; rargo needs to know what to sync", cwd.display()),
            ));
        };
        let Some(policy_dir) = rargo_proto::workspace::command_dir(&root, &cwd, &req.args) else {
            return Ok(Self::fail(out, "the manifest must be inside the synced worktree".into()));
        };
        let wt = self.worktree(&root);
        let stats = sync_stats(&req.vars, &req.args);
        let say = |wait: Wait| {
            if stats {
                let _ = out.send(LocalReply::Stderr(waits::line(&wait).into_bytes()));
            }
        };
        let _running = match wt.running.try_lock() {
            Some(running) => running,
            None => {
                say(Wait::Worktree);
                wt.running.lock()
            }
        };
        let shown = if req.kind == "sync" {
            None
        } else {
            match self.out_for(&wt, req) {
                Ok(shown) => shown,
                Err(err) => return Ok(Self::fail(out, err.to_string())),
            }
        };
        let mut home_files = Vec::new();
        for name in ["config.toml", "config", "credentials.toml", "credentials"] {
            match rargo_net::io::file::read(cargo_home.join(name)) {
                Ok(b) if b.len() <= 1 << 20 => home_files.push((name.to_string(), b)),
                _ => {}
            }
        }
        let taken_up = io::now();
        let phase = |what: &str| {
            if stats {
                let _ = out.send(LocalReply::Stderr(format!("rargo: phase {:>8.3} s  {what}\n", io::now().saturating_duration_since(taken_up).as_secs_f64()).into_bytes()));
            }
        };
        let mut step = command.begin();
        let mut link: Option<Arc<Mux>> = None;
        let mut snap: Option<Arc<Snap>> = None;
        let mut bet = None;
        let mut failure = None;
        let mut started = io::now();
        loop {
            step = match step {
                Step::Link => {
                    if !self.link_up() {
                        say(Wait::Connecting(self.cfg.server.clone()));
                    }
                    match self.mux() {
                        Ok(mux) => {
                            link = Some(mux);
                            phase("link to the server ready");
                            started = io::now();
                            command.linked()
                        }
                        Err(e) => command.connect_failed().ok_or(e)?,
                    }
                }
                Step::Scan => {
                    let mut st = wt.state.lock();
                    if st.policy_dir != policy_dir {
                        st.policy_dir.clone_from(&policy_dir);
                        self.knowledge.lock().unwrap().changed(wt.id, true);
                    }
                    let (fresh, scanned) = self.fresh(&wt, &mut st)?;
                    if let Some(note) = st.note.take() {
                        let _ = out.send(LocalReply::Stderr(note.into_bytes()));
                    }
                    if stats {
                        let text = if scanned {
                            format!(
                                "rargo: scanned {} files in {:.1} ms ({} read, {:.1} MiB), looking at {} of {} directories\n",
                                fresh.scan.file_count,
                                io::now().saturating_duration_since(started).as_secs_f64() * 1e3,
                                fresh.scan.hashed_files,
                                fresh.scan.hashed_bytes as f64 / 1048576.0,
                                fresh.scan.listed_dirs,
                                fresh.scan.dirs.len()
                            )
                        } else {
                            format!("rargo: no scan needed, {} files watched and unchanged\n", fresh.scan.file_count)
                        };
                        let _ = out.send(LocalReply::Stderr(text.into_bytes()));
                    }
                    phase("worktree read");
                    snap = Some(fresh);
                    command.scanned()
                }
                Step::Offer(probe) => {
                    let (mux, fresh) = (link.clone().expect("a link before an offer"), snap.clone().expect("a scan before an offer"));
                    match self.offer(&mux, &wt, &fresh, probe) {
                        Ok(Offered::Sent) => {
                            phase("worktree offered and what the server lacked sent");
                            command.offered(Sent::All)
                        }
                        Ok(Offered::Bet(answer)) => {
                            phase("worktree offered by its root, answer not awaited");
                            bet = Some(answer);
                            command.offered(Sent::Bet)
                        }
                        Ok(Offered::Changed) => command.offered(Sent::Changed),
                        Err(e) if mux.is_dead() => {
                            failure = Some(e);
                            command.link_lost(mux.silent().map(|quiet| (quiet, "its answer to the worktree offer")))
                        }
                        Err(e) => return Err(e),
                    }
                }
                Step::Request => {
                    let (mux, fresh) = (link.clone().expect("a link before a request"), snap.clone().expect("a scan before a request"));
                    let (env, env_checked) = env_for(&fresh.policy);
                    let commits = match &fresh.head {
                        Some(h) => {
                            let (root, h, wt_cache) = (root.clone(), h.clone(), wt.wt_cache.clone());
                            io::cpu(move || crate::recent_commits(&root, &h, &wt_cache))
                        }
                        None => Vec::new(),
                    };
                    let delivered =
                        rargo_net::io::file::read_to_string(wt.wt_cache.join("delivered")).ok().and_then(|t| t.trim().parse().ok()).unwrap_or(0);
                    let run = RunRequest {
                        root: root.to_string_lossy().into_owned(),
                        env,
                        env_checked,
                        worktree: wt.wt_id.clone(),
                        commits,
                        tree: fresh.scan.root,
                        home_files: home_files.clone(),
                        delivered,
                        lazy: shown.is_some(),
                        ..base.clone()
                    };
                    phase("request sent");
                    match self.converse(&mux, run, Some(&fresh), Some(&wt), shown.as_ref(), req, out, &mut command, bet.take()) {
                        Ok(Some(reply)) => {
                            phase("answer complete");
                            return Ok(reply);
                        }
                        Ok(None) => {
                            phase("the server lacked the tree, or the worktree changed: offering it again");
                            command.again()
                        }
                        Err(e) if mux.is_dead() => {
                            failure = Some(e);
                            command.link_lost(mux.silent().map(|quiet| (quiet, "the build's progress")))
                        }
                        Err(e) => return Err(e),
                    }
                }
                Step::Rescan => {
                    self.knowledge.lock().unwrap().changed(wt.id, true);
                    command.rescanned()
                }
                Step::Exit(code) => return Ok(LocalReply::Exit(code)),
                Step::Fail(text) => return Ok(Self::fail(out, text.into())),
                Step::Error(text) => return Err(std::io::Error::other(text)),
                Step::GiveUp => return Err(failure.take().unwrap_or_else(lost)),
                Step::Silent { quiet, waiting_for } => return Ok(self.silent(out, quiet, waiting_for)),
            };
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn converse(
        &self,
        mux: &Arc<Mux>,
        run: RunRequest,
        snap: Option<&Arc<Snap>>,
        wt: Option<&Arc<Worktree>>,
        shown: Option<&(Arc<OutFs>, PathBuf)>,
        req: &LocalRequest,
        out: &UnboundedSender<LocalReply>,
        command: &mut CommandState,
        bet: Option<io::oneshot::Receiver<(Vec<Digest>, Vec<Digest>)>>,
    ) -> std::io::Result<Option<LocalReply>> {
        let (stream, tx, mut rx) = mux.open();
        struct Withdraw<'a>(&'a Mux, u32);
        impl Drop for Withdraw<'_> {
            fn drop(&mut self) {
                self.0.cancel(self.1);
            }
        }
        let _withdraw = Withdraw(mux, stream);
        mux.post(stream, &ClientMsg::Run(Box::new(run)));
        let placed_path = wt.map(|w| w.wt_cache.join("placed"));
        let mut placer = io::cpu(|| {
            Placer::new(&self.cache, placed_path.as_deref().map(Placed::load).unwrap_or_default())
        });
        let left_out = snap.and_then(|s| s.left_out.as_ref()).zip(wt);
        command.start(left_out.is_some());
        let mut bet_rx = None;
        if let Some(oneshot_rx) = bet {
            let (bet_tx, bet_notes) = unbounded_channel();
            io::spawn(move || {
                if let Some(missing) = oneshot_rx.recv() {
                    let _ = bet_tx.send(missing);
                }
            });
            bet_rx = Some(bet_notes);
        }
        enum Incoming {
            Bet(Option<(Vec<Digest>, Vec<Digest>)>),
            Msg(Option<ServerMsg>),
            Dead,
        }
        let final_reply = loop {
            if let Some(reply) = command.finish() {
                break reply;
            }
            let incoming = if let Some(notes) = bet_rx.as_mut() {
                io::race_biased(vec![
                    Box::new(|| Incoming::Bet(notes.recv())),
                    Box::new(|| Incoming::Msg(rx.recv())),
                    Box::new(|| { mux.until_dead(); Incoming::Dead }),
                ])
            } else {
                io::race_biased(vec![
                    Box::new(|| Incoming::Msg(rx.recv())),
                    Box::new(|| { mux.until_dead(); Incoming::Dead }),
                ])
            };
            let msg = match incoming {
                Incoming::Bet(lacking) => {
                    bet_rx = None;
                    let (trees, blobs) = lacking.ok_or_else(lost)?;
                    match command.bet_answered(!trees.is_empty(), !blobs.is_empty()) {
                        BetAnswer::Known => {
                            if let Some(s) = snap {
                                self.has_now(wt, s);
                            }
                            self.counters.probes_known.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        BetAnswer::Lacking => {
                            self.knowledge.lock().unwrap().missing(trees.iter().chain(&blobs));
                            mux.post(stream, &ClientMsg::Changed);
                            return Ok(None);
                        }
                    }
                }
                Incoming::Msg(msg) => msg,
                Incoming::Dead => None,
            };
            let Some(msg) = msg else { return Err(lost()) };
            match command.server(msg) {
                CommandEffect::Abandon => {
                    mux.post(stream, &ClientMsg::Changed);
                    return Ok(None);
                }
                CommandEffect::Relay(reply) => { let _ = out.send(reply); }
                CommandEffect::Trees(want) => {
                    self.knowledge.lock().unwrap().missing(want.iter());
                    let trees: Vec<Vec<u8>> = want.iter().filter_map(|d| snap.and_then(|s| s.scan.trees.get(d)).cloned()).collect();
                    mux.post(stream, &ClientMsg::Trees(trees));
                }
                CommandEffect::Upload(want) => {
                    self.knowledge.lock().unwrap().missing(want.iter());
                    let ok = match snap {
                        Some(s) => upload(mux, stream, s, want)?,
                        None => false,
                    };
                    if matches!(command.uploaded(ok), CommandEffect::Abandon) {
                        mux.post(stream, &ClientMsg::Changed);
                        return Ok(None);
                    }
                }
                CommandEffect::Environment(names) => {
                    let values: Vec<(String, Option<String>)> = names
                        .iter()
                        .map(|n| (n.clone(), req.vars.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone())))
                        .collect();
                    mux.post(stream, &ClientMsg::Env(values));
                    let mut asked = self.asked.lock().unwrap();
                    let before = asked.len();
                    for n in names {
                        if !asked.contains(&n) {
                            asked.push(n);
                        }
                    }
                    if asked.len() != before {
                        let _ = crate::write_atomic(&self.cache.join("env-names"), (asked.join("\n") + "\n").as_bytes());
                    }
                }
                CommandEffect::Deliver(files, writes, targets) => {
                    io::cpu(|| -> std::io::Result<()> {
                        let (files, writes) = match shown {
                            Some((fs, dir)) => show(fs, dir, files, writes)?,
                            None => (files, writes),
                        };
                        let root = wt.map(|w| w.root.as_path()).ok_or_else(|| std::io::Error::other("the server sent files for a command that has no worktree"))?;
                        placer.deliver(root, &targets, files, writes)?;
                        Ok(())
                    })?;
                }
                CommandEffect::Blobs(blobs) => io::cpu(|| -> std::io::Result<()> {
                    for b in blobs {
                        let hash = b.hash;
                        placer.blob(hash, &b.unpack()?)?;
                    }
                    Ok(())
                })?,
                CommandEffect::Part(hash, part, last) => {
                    io::cpu(|| -> std::io::Result<()> { placer.part(hash, &part.unpack()?, last) })?
                }
                CommandEffect::VerifyFetch => {
                    let complete = command.fetching.as_ref().is_some_and(|(_, hashes)| hashes.iter().all(|hash| !placer.pending(hash)));
                    command.verified_fetch(complete).map_err(std::io::Error::other)?;
                }
                CommandEffect::Report(r) => {
                    let id = match io::cpu(|| report::save(&r)) {
                        Ok(id) => id,
                        Err(e) => {
                            let text = format!("warning: failed to write on-disk future incompatible report {}: {e}\n", r.path);
                            let _ = out.send(LocalReply::Stderr(text.into_bytes()));
                            0
                        }
                    };
                    mux.post(stream, &ClientMsg::ReportId(id));
                }
                CommandEffect::ExplainExit => {
                    if let Some((ignored, wt)) = left_out
                        && let Some(note) = left_out_note(ignored, &wt.root, &command.heard)
                    {
                        let _ = out.send(LocalReply::Stderr(note.into_bytes()));
                    }
                }
                CommandEffect::None => {}
            }
            if command.ready_to_fetch() {
                let hashes = placer.wanting();
                if !hashes.is_empty() {
                    let id = mux.fetch(hashes.clone(), &tx);
                    command.request_fetch(id, hashes);
                }
            }
        };
        if let Some((fs, _)) = shown {
            io::cpu(|| fs.save());
        }
        if !placer.settled() {
            return Err(std::io::Error::other("the server did not send every file it announced"));
        }
        io::cpu(|| {
            if let Some(path) = &placed_path {
                if placer.changed {
                    let _ = placer.placed.save(path);
                    placer.prune();
                }
            }
            if let (Some(t), Some(wt)) = (command.token, wt) {
                let _ = crate::write_atomic(&wt.wt_cache.join("delivered"), format!("{t}\n").as_bytes());
            }
        });
        Ok(Some(final_reply))
    }

    fn status(&self, out: &UnboundedSender<LocalReply>) -> LocalReply {
        let c = &self.counters;
        let (kind, bytes_out, bytes_in) = match self.mux_if_up() {
            Some(m) => (m.kind(), m.bytes_out.load(Ordering::Relaxed), m.bytes_in.load(Ordering::Relaxed)),
            None => ("none", 0, 0),
        };
        let (links, held) = {
            let state = self.knowledge.lock().unwrap();
            (state.connections(), state.held.len())
        };
        let (worktrees, watched) = {
            let all = self.worktrees.lock().unwrap();
            (all.len(), self.knowledge.lock().unwrap().all_watched())
        };
        let outs = {
            let all = self.worktrees.lock().unwrap();
            let mut t = (0usize, 0usize, 0u64, 0u64);
            for w in all.values() {
                if let Some(m) = &*w.out.lock().unwrap() {
                    t.0 += 1;
                    t.1 += m.fs.virtual_count();
                    t.2 += m.fs.opened_virtual.load(Ordering::Relaxed);
                    t.3 += m.fs.made_real.load(Ordering::Relaxed);
                }
            }
            t
        };
        let text = format!(
            "server              {}\nlink                {kind}; {} made; {bytes_out} bytes sent, {bytes_in} bytes received on the current one\n\
             commands            {}\nworktrees           {worktrees} known, {watched} watched completely\n\
             scans               {} of whole worktrees, {} of {} directories that had changed or have no watch, {} spared (nothing had changed)\n\
             pushed              {} changes, {} bytes; {} trees the server had already (asked by root)\n\
             held                {} trees and contents the server is believed to hold (kept across daemons)\n\
             outputs             {} worktrees with a virtual output directory showing {} files; {} opened, {} made real; {} contents fetched on first read, {} bytes fetched, {} bytes reused from the cache\n",
            self.cfg.server,
            links,
            c.commands.load(Ordering::Relaxed),
            c.scans.load(Ordering::Relaxed),
            c.rescans.load(Ordering::Relaxed),
            c.rescanned_dirs.load(Ordering::Relaxed),
            c.scans_spared.load(Ordering::Relaxed),
            c.offers.load(Ordering::Relaxed),
            c.offered_bytes.load(Ordering::Relaxed),
            c.probes_known.load(Ordering::Relaxed),
            held,
            outs.0,
            outs.1,
            outs.2,
            outs.3,
            c.fetched_contents.load(Ordering::Relaxed),
            c.fetched_bytes.load(Ordering::Relaxed),
            c.reused_bytes.load(Ordering::Relaxed),
        );
        let mut text = text;
        for w in self.worktrees.lock().unwrap().values() {
            if let Some(m) = &*w.out.lock().unwrap() {
                text.push_str(&format!("operations          {}: {}\n", m.dir.display(), m.fs.op_report()));
            }
        }
        let _ = out.send(LocalReply::Stdout(text.into_bytes()));
        LocalReply::Exit(0)
    }
}

struct Contents {
    d: std::sync::Weak<Daemon>,
}

impl Contents {
    fn daemon(&self) -> std::io::Result<Arc<Daemon>> {
        self.d.upgrade().ok_or_else(|| std::io::Error::other("the daemon is stopping"))
    }
}

impl crate::outfs::Source for Contents {
    fn cached(&self, hash: &Digest, size: u64) -> Option<PathBuf> {
        crate::place::cached(&self.d.upgrade()?.cache, hash, size)
    }

    fn whole(&self, hash: &Digest, size: u64) -> std::io::Result<PathBuf> {
        let d = self.daemon()?;
        let reading = d.partials.lock().unwrap().get(hash).map(|(_, p)| p.clone());
        match reading {
            Some(p) => p.whole(),
            None => {
                let hash = *hash;
                io::call(move || d.fetch_content(hash, size))
            }
        }
    }

    fn reader(&self, hash: &Digest, size: u64) -> std::io::Result<(Reader, Digest)> {
        let d = self.daemon()?;
        if let Some(path) = crate::place::cached(&d.cache, hash, size) {
            return Ok((Reader::Whole(rargo_net::io::file::File::open(path)?), *hash));
        }
        if size >= rargo_proto::CHUNKED_FROM {
            let d2 = d.clone();
            let hash2 = *hash;
            if let Some(p) = io::call(move || d2.partial(hash2, size))? {
                let proven = *p.hash();
                return Ok((Reader::Ranged(p), proven));
            }
        }
        let hash = *hash;
        let path = io::call(move || d.fetch_content(hash, size))?;
        Ok((Reader::Whole(rargo_net::io::file::File::open(path)?), hash))
    }
}

struct ChunkFetcher {
    d: std::sync::Weak<Daemon>,
}

impl crate::partial::Chunks for ChunkFetcher {
    fn request(&self, p: &Arc<Partial>, idx: Vec<u32>) {
        let Some(d) = self.d.upgrade() else {
            p.failed(&idx);
            return;
        };
        let p = p.clone();
        io::spawn_off(move || {
            let need = io::cpu(|| d.reuse_chunks(&p, idx));
            if !need.is_empty() {
                let left = d.fetch_chunks(&p, need);
                if !left.is_empty() {
                    p.failed(&left);
                }
            }
            if p.is_done() {
                d.read_through(&p);
            }
        });
    }
}

enum LinkWait {
    Msg(Option<ServerMsg>),
    Dead,
}

fn next_msg(mux: &Arc<Mux>, rx: &mut UnboundedReceiver<ServerMsg>) -> std::io::Result<ServerMsg> {
    loop {
        match io::race_biased(vec![
            Box::new(|| LinkWait::Msg(rx.recv())),
            Box::new(|| { mux.until_dead(); LinkWait::Dead }),
        ]) {
            LinkWait::Msg(Some(msg)) => return Ok(msg),
            LinkWait::Msg(None) | LinkWait::Dead => return Err(lost()),
        }
    }
}

fn mountinfo() -> String {
    rargo_net::io::file::read_to_string("/proc/self/mountinfo").unwrap_or_default()
}

fn mounts_at<'a>(info: &'a str, dir: &Path) -> impl Iterator<Item = &'a str> {
    let here = dir.to_string_lossy().into_owned();
    info.lines().filter(move |l| l.split(' ').nth(4) == Some(here.as_str()))
}

fn dev_at(info: &str, dir: &Path) -> Option<u64> {
    let (major, minor) = mounts_at(info, dir).last()?.split(' ').nth(2)?.split_once(':')?;
    Some(device_number(major.parse().ok()?, minor.parse().ok()?))
}

/// The device number Linux makes of a major and a minor number.
fn device_number(major: u32, minor: u32) -> u64 {
    let (major, minor) = (u64::from(major), u64::from(minor));
    ((major & 0xffff_f000) << 32) | ((major & 0xfff) << 8) | ((minor & 0xffff_ff00) << 12) | (minor & 0xff)
}

fn release_mounts(dir: &Path) {
    let info = mountinfo();
    let mounts = mounts_at(&info, dir).count();
    let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else { return };
    for _ in 0..mounts {
        if crate::kernel::unmount(&c, libc::MNT_DETACH).is_err() {
            fusewire::unmount_with_helper(dir);
        }
    }
}

fn show(
    fs: &Arc<OutFs>,
    dir: &Path,
    files: Vec<rargo_proto::Delivery>,
    writes: Vec<rargo_proto::Write>,
) -> std::io::Result<(Vec<rargo_proto::Delivery>, Vec<rargo_proto::Write>)> {
    use rargo_proto::Write;
    let inside = |path: &str| target_relative(dir, Path::new(path));
    let now = io::system_now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as i64).unwrap_or(0);
    let mut delivery = fs.begin_delivery();
    let mut rest_writes = Vec::new();
    for w in writes {
        let path = match &w {
            Write::Dir { path } | Write::Remove { path } | Write::Symlink { path, .. } | Write::File { path, .. } | Write::Text { path, .. } => path,
        };
        let Some(rel) = inside(path) else {
            rest_writes.push(w);
            continue;
        };
        match w {
            Write::Dir { .. } => {
                rargo_net::io::file::create_dir_all(fs.real_for_write(&join_rel(&rel))?)?;
                delivery.wrote(&rel);
            }
            Write::Remove { .. } | Write::Text { text: None, .. } => delivery.remove(&rel),
            Write::Symlink { target, .. } => {
                let real = fs.real_for_write(&rel)?;
                let _ = rargo_net::io::file::remove_file(&real);
                rargo_net::io::file::symlink(std::ffi::OsStr::from_bytes(&target), &real)?;
                delivery.wrote(&rel);
            }
            Write::Text { text: Some(text), .. } => {
                let real = fs.real_for_write(&rel)?;
                if rargo_net::io::file::read_to_string(&real).ok().as_deref() != Some(text.as_str()) {
                    crate::write_atomic(&real, text.as_bytes())?;
                    delivery.wrote(&rel);
                }
            }
            Write::File { hash, size, exec, .. } => {
                delivery.put(&rel, VFile { hash, size, exec, mtime_ns: now });
            }
        }
    }
    let mut rest_files = Vec::new();
    for f in files {
        match inside(&f.path) {
            Some(rel) => {
                delivery.put(&rel, VFile { hash: f.hash, size: f.size, exec: f.exec, mtime_ns: now });
            }
            None => rest_files.push(f),
        }
    }
    Ok((rest_files, rest_writes))
}

fn target_relative(dir: &Path, path: &Path) -> Option<Vec<u8>> {
    path.strip_prefix(dir).ok().filter(|r| !r.as_os_str().is_empty()).map(|r| r.as_os_str().as_bytes().to_vec())
}

fn join_rel(rel: &[u8]) -> Vec<u8> {
    assert!(!rel.ends_with(b"/"));
    let mut p = rel.to_vec();
    p.extend_from_slice(b"/.");
    p
}

const UPLOAD_OVERHEAD: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
struct UploadFile {
    hash: Digest,
    path: PathBuf,
    size: u64,
}

struct UploadPlan {
    batches: Vec<Vec<UploadFile>>,
    multipart: Vec<UploadFile>,
    part_size: usize,
    budget: usize,
}

impl UploadPlan {
    fn new(want: Vec<Digest>, files: &std::collections::BTreeMap<Digest, (PathBuf, u64)>, budget: usize) -> Option<Self> {
        let part_size = rargo_proto::PART_SIZE.min(budget.checked_sub(UPLOAD_OVERHEAD)?);
        if part_size == 0 {
            return None;
        }
        let mut plan = Self { batches: Vec::new(), multipart: Vec::new(), part_size, budget };
        let mut batch = Vec::new();
        let mut used = 0;
        let mut seen = std::collections::BTreeSet::new();
        for hash in want {
            if !seen.insert(hash) {
                continue;
            }
            let (path, size) = files.get(&hash)?;
            let file = UploadFile { hash, path: path.clone(), size: *size };
            if *size > part_size as u64 {
                plan.multipart.push(file);
                continue;
            }
            let cost = *size as usize + UPLOAD_OVERHEAD;
            if cost > budget - used {
                plan.batches.push(std::mem::take(&mut batch));
                used = 0;
            }
            used += cost;
            batch.push(file);
        }
        if !batch.is_empty() {
            plan.batches.push(batch);
        }
        Some(plan)
    }
}

impl StateMachine for UploadPlan {
    fn invariants(&self) {
        let mut seen = std::collections::BTreeSet::new();
        for batch in &self.batches {
            assert!(!batch.is_empty(), "no batch is empty");
            let cost: usize = batch.iter().map(|file| file.size as usize + UPLOAD_OVERHEAD).sum();
            assert!(cost <= self.budget, "a batch costs {cost}, over the budget of {}", self.budget);
            for file in batch {
                assert!(file.size <= self.part_size as u64, "a batched file of {} bytes fits in one part", file.size);
                assert!(seen.insert(file.hash), "a file is planned once");
            }
        }
        for file in &self.multipart {
            assert!(file.size > self.part_size as u64, "a multipart file of {} bytes is over one part", file.size);
            assert!(seen.insert(file.hash), "a file is planned once");
        }
    }
}

struct Multipart {
    hash: Digest,
    size: u64,
    part_size: usize,
    bytes: u64,
    hasher: blake3::Hasher,
    held: Option<Vec<u8>>,
}

impl Multipart {
    fn frame(&self, data: Vec<u8>, last: bool) -> (ClientMsg, usize) {
        let part = rargo_proto::Part::pack(data);
        let bytes = part.data.len();
        (ClientMsg::BlobPart { hash: self.hash, part, last }, bytes)
    }

    fn read(&mut self, data: Vec<u8>) -> Result<Option<(ClientMsg, usize)>, ()> {
        if data.len() > self.part_size || data.len() as u64 > self.size - self.bytes {
            return Err(());
        }
        self.bytes += data.len() as u64;
        self.hasher.update(&data);
        let previous = self.held.replace(data);
        Ok(previous.map(|part| self.frame(part, false)))
    }

    fn finish(mut self) -> Option<(ClientMsg, usize)> {
        if self.bytes != self.size || *self.hasher.finalize().as_bytes() != self.hash {
            return None;
        }
        let last = self.held.take()?;
        Some(self.frame(last, true))
    }
}

#[cfg(test)]
impl StateMachine for Multipart {
    fn invariants(&self) {
        assert!(self.bytes <= self.size, "{} bytes of a {}-byte file are sent", self.bytes, self.size);
        assert!(self.held.as_ref().is_none_or(|buf| buf.len() <= self.part_size), "a held buffer fits in one part");
    }
}

fn upload(mux: &Arc<Mux>, stream: u32, snap: &Arc<Snap>, want: Vec<Digest>) -> std::io::Result<bool> {
    let Some(plan) = UploadPlan::new(want, &snap.scan.files, rargo_proto::BATCH_SIZE) else { return Ok(false) };
    let UploadPlan { batches, multipart, part_size, .. } = plan;
    for window in batches.chunks(8) {
        let window = window.to_vec();
        let packed: Vec<Option<Vec<Blob>>> = io::cpu(move || {
            window
                .par_iter()
                .map(|batch| {
                    batch
                        .par_iter()
                        .map(|file| {
                            let bytes = rargo_net::io::file::read(&file.path).ok()?;
                            (bytes.len() as u64 == file.size && *blake3::hash(&bytes).as_bytes() == file.hash)
                                .then(|| Blob::pack(file.hash, &bytes))
                        })
                        .collect::<Option<Vec<Blob>>>()
                })
                .collect()
        });
        for blobs in packed {
            let Some(blobs) = blobs else { return Ok(false) };
            let n = blobs.iter().map(|b| b.data.len()).sum();
            mux.post_bulk(stream, &ClientMsg::Blobs(blobs), n)?;
        }
    }
    for UploadFile { hash, path, size } in multipart {
        let Ok(mut f) = rargo_net::io::file::File::open(&path) else { return Ok(false) };
        let mut content = Multipart { hash, size, part_size, bytes: 0, hasher: blake3::Hasher::new(), held: None };
        loop {
            let mut buf = vec![0u8; part_size];
            let mut n = 0;
            while n < buf.len() {
                let got = std::io::Read::read(&mut f, &mut buf[n..])?;
                if got == 0 {
                    break;
                }
                n += got;
            }
            buf.truncate(n);
            if buf.is_empty() {
                break;
            }
            match content.read(buf) {
                Ok(Some((frame, bytes))) => mux.post_bulk(stream, &frame, bytes)?,
                Ok(None) => {}
                Err(()) => return Ok(false),
            }
        }
        let Some((frame, bytes)) = content.finish() else { return Ok(false) };
        mux.post_bulk(stream, &frame, bytes)?;
    }
    Ok(true)
}


#[cfg(test)]
mod upload_tests {
    use super::*;
    use expect_test::expect;

    /// Plans the upload of `want`, each file named by its first byte and `size` bytes long, and lists the plan.
    fn plan(want: &[u8], sizes: &[(u8, u64)], budget: usize) -> String {
        let files = sizes.iter().map(|&(i, size)| ([i; 32], (PathBuf::from(i.to_string()), size))).collect();
        let Some(plan) = UploadPlan::new(want.iter().map(|&i| [i; 32]).collect(), &files, budget) else {
            return "no plan\n".to_string();
        };
        plan.invariants();
        let name = |file: &UploadFile| format!("{}:{}", file.hash[0], file.size);
        let mut out = format!("part size {}\n", plan.part_size);
        for batch in &plan.batches {
            out += &format!("batch {}\n", batch.iter().map(name).collect::<Vec<_>>().join(" "));
        }
        for file in &plan.multipart {
            out += &format!("multipart {}\n", name(file));
        }
        out
    }

    #[test]
    fn upload_plans_keep_first_occurrence_order_within_both_frame_types() {
        let sizes = [(1, 0), (2, 100), (3, 168), (4, 169), (5, 40), (6, 1_000), (7, 60), (8, 104)];
        let want = [3, 1, 4, 1, 5, 2, 6, 5, 7, 8, 3];
        let found = [128, 232, 4096].map(|budget| format!("budget {budget}\n{}", plan(&want, &sizes, budget))).concat();
        expect![[r#""#]].assert_eq(&found);
        expect![[r#""#]].assert_eq(&plan(&[1, 9], &sizes, 4096));
        expect![[r#""#]].assert_eq(&plan(&[1], &sizes, UPLOAD_OVERHEAD));
    }

    #[test]
    fn generated_upload_plans_keep_first_occurrence_order_within_both_frame_types() {
        for budget in [128usize, 232, 4096] {
            let mut want = Vec::new();
            let mut files = std::collections::BTreeMap::new();
            let mut small = Vec::new();
            let mut large = Vec::new();
            for i in 0..96u8 {
                let hash = [i; 32];
                let size = u64::from(i) * 21;
                files.insert(hash, (PathBuf::from(i.to_string()), size));
                want.extend([hash, hash]);
                if size <= (budget - UPLOAD_OVERHEAD) as u64 { small.push(hash) } else { large.push(hash) }
                let plan = UploadPlan::new(want.clone(), &files, budget).unwrap();
                plan.invariants();
                assert_eq!(plan.batches.iter().flatten().map(|entry| entry.hash).collect::<Vec<_>>(), small);
                assert_eq!(plan.multipart.iter().map(|entry| entry.hash).collect::<Vec<_>>(), large);
            }
        }
    }

    #[test]
    fn a_target_relative_directory_is_named_by_its_dot_entry() {
        assert_eq!(join_rel(b"debug"), b"debug/.");
    }

    /// Feeds `input` to a multipart upload of `content` in reads of `part_size` bytes, checking it after each,
    /// and lists the frames it sends.
    fn multipart(content: &[u8], input: &[u8], part_size: usize) -> String {
        let hash = *blake3::hash(content).as_bytes();
        let mut upload =
            Multipart { hash, size: content.len() as u64, part_size, bytes: 0, hasher: blake3::Hasher::new(), held: None };
        upload.invariants();
        let frame = |(msg, bytes): (ClientMsg, usize)| match msg {
            ClientMsg::BlobPart { hash: got, part, last } => {
                assert_eq!(got, hash, "a part names its content");
                let data = part.unpack().unwrap();
                format!("part {:?} last={last} ({bytes} bytes on the wire)", String::from_utf8_lossy(&data))
            }
            other => format!("unexpected {other:?}"),
        };
        let mut out = String::new();
        for (i, chunk) in input.chunks(part_size).enumerate() {
            let sent = match upload.read(chunk.to_vec()) {
                Ok(Some(sent)) => frame(sent),
                Ok(None) => "nothing yet".to_string(),
                Err(()) => "refused".to_string(),
            };
            upload.invariants();
            out += &format!("read {i} {:?} => {sent}\n", String::from_utf8_lossy(chunk));
        }
        out + &format!("finish => {}\n", upload.finish().map_or("nothing".to_string(), frame))
    }

    #[test]
    fn only_verified_multipart_content_gets_a_terminal_frame() {
        expect![[r#""#]].assert_eq(&multipart(b"one two three", b"one two three", 4));
        expect![[r#""#]].assert_eq(&multipart(b"one two three", b"one TWO three", 4));
        expect![[r#""#]].assert_eq(&multipart(b"one two", b"one two three", 4));
        expect![[r#""#]].assert_eq(&multipart(b"one two three", b"one two", 4));
    }
}

fn read_frame(rd: &mut io::ReadHalf) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    rd.read_exact(&mut len)?;
    let n = u32::from_le_bytes(len) as usize;
    if n > 64 << 20 {
        return Err(std::io::Error::other("oversized request"));
    }
    let mut buf = vec![0u8; n];
    rd.read_exact(&mut buf)?;
    Ok(buf)
}

fn serve_command(d: Arc<Daemon>, stream: io::UnixStream, socket: PathBuf) {
    let (mut rd, mut wr) = stream.into_split();
    let Ok(raw) = read_frame(&mut rd) else { return };
    let Ok(req) = rargo_proto::decode::<LocalRequest>(&raw) else { return };
    if req.kind == "release" {
        d.release(Path::new(&req.cwd));
        let done = rargo_proto::encode(&LocalReply::Exit(0));
        let _ = wr.write_all(&(done.len() as u32).to_le_bytes());
        let _ = wr.write_all(&done);
        return;
    }
    if req.kind == "stop" {
        d.close_down(&socket);
        let done = rargo_proto::encode(&LocalReply::Exit(0));
        let _ = wr.write_all(&(done.len() as u32).to_le_bytes());
        let _ = wr.write_all(&done);
        let _ = wr.shutdown();
        io::process::exit(0);
    }
    let (tx, mut rx) = unbounded_channel::<LocalReply>();
    let work = io::spawn(move || d.command(req, tx));
    let mut probe = [0u8; 1];
    enum ClientWait {
        Reply(Option<LocalReply>),
        Gone,
    }
    loop {
        match io::race_biased(vec![
            Box::new(|| ClientWait::Reply(rx.recv())),
            Box::new(|| {
                let _ = rd.read(&mut probe);
                ClientWait::Gone
            }),
        ]) {
            ClientWait::Reply(Some(reply)) => {
                let last = matches!(reply, LocalReply::Exit(_) | LocalReply::Exec(_) | LocalReply::Unreachable(_) | LocalReply::Refused(_));
                let bytes = rargo_proto::encode(&reply);
                if wr.write_all(&(bytes.len() as u32).to_le_bytes()).is_err() || wr.write_all(&bytes).is_err() {
                    break;
                }
                if last {
                    let _ = wr.shutdown();
                    return;
                }
            }
            ClientWait::Reply(None) | ClientWait::Gone => break,
        }
    }
    work.abort();
}

pub fn serve(socket: &Path) -> ! {
    let cfg = match crate::load_config() {
        Ok(c) => c,
        Err(e) => crate::die(&e),
    };
    let cache = crate::cache_dir();
    let socket = socket.to_path_buf();
    io::drive(4, move || {
        let lock_path = socket.with_extension("lock");
        let lock = match rargo_net::io::file::OpenOptions::new().create(true).truncate(false).write(true).open(&lock_path) {
            Ok(f) => f,
            Err(e) => crate::die(&format!("{}: {e}", lock_path.display())),
        };
        {
            use std::os::fd::AsRawFd;
            let started = io::now();
            while crate::kernel::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB).is_err() {
                if io::now().saturating_duration_since(started) > Duration::from_secs(3) {
                    io::process::exit(0);
                }
                io::sleep(Duration::from_millis(20));
            }
        }
        let _ = rargo_net::io::file::remove_file(&socket);
        let listener = match io::UnixListener::bind(&socket) {
            Ok(l) => l,
            Err(e) => crate::die(&format!("{}: {e}", socket.display())),
        };
        let d = Daemon::new(cfg, cache, true);
        let idle = Duration::from_secs(
            io::env::var("RARGO_DAEMON_IDLE_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(1800),
        );
        eprintln!("rargo daemon: {} serving {} on {}", io::process::id(), d.cfg.server, socket.display());
        let mut term = io::signal(io::SignalKind::terminate()).expect("signal handler");
        let mut int = io::signal(io::SignalKind::interrupt()).expect("signal handler");
        enum Gate {
            Term,
            Int,
            Conn(std::io::Result<(io::UnixStream, ())>),
            Idle,
        }
        loop {
            match io::race_biased(vec![
                Box::new(|| {
                    let _ = term.recv();
                    Gate::Term
                }),
                Box::new(|| {
                    let _ = int.recv();
                    Gate::Int
                }),
                Box::new(|| Gate::Conn(listener.accept())),
                Box::new(|| {
                    io::sleep(Duration::from_secs(5));
                    Gate::Idle
                }),
            ]) {
                Gate::Term | Gate::Int => {
                    d.close_down(&socket);
                    break;
                }
                Gate::Conn(Ok((stream, ()))) => {
                    d.touch();
                    let (d, socket) = (d.clone(), socket.clone());
                    io::spawn(move || serve_command(d, stream, socket));
                }
                Gate::Conn(Err(_)) => {}
                Gate::Idle => {
                    let showing = d.worktrees.lock().unwrap().values().any(|w| w.out.lock().unwrap().is_some());
                    let quiet = !showing && d.active.load(Ordering::SeqCst) == 0 && io::now().saturating_duration_since(*d.last_active.lock().unwrap()) > idle;
                    d.save_known();
                    if quiet {
                        d.close_down(&socket);
                        break;
                    }
                    if !socket.ring_exists() {
                        d.unmount_all();
                        break;
                    }
                    let stale: Vec<Arc<Worktree>> = {
                        let mut all = d.worktrees.lock().unwrap();
                        let old: Vec<PathBuf> = all
                            .iter()
                            .filter(|(_, w)| w.out.lock().unwrap().is_none() && io::now().saturating_duration_since(*w.last_used.lock().unwrap()) > Duration::from_secs(4 * 3600))
                            .map(|(p, _)| p.clone())
                            .collect();
                        old.iter().filter_map(|p| all.remove(p)).collect()
                    };
                    for wt in stale {
                        d.knowledge.lock().unwrap().expired(wt.id);
                        if let Some(w) = &d.watcher {
                            w.forget(wt.id);
                        }
                        wt.changed.notify_one();
                    }
                }
            }
        }
        drop(lock);
        io::process::exit(0);
    });
    unreachable!("the daemon leaves through exit")
}

pub fn inline(cfg: Config, cache: PathBuf, req: LocalRequest, mut each: impl FnMut(LocalReply)) {
    io::run(2, move || {
        let d = Daemon::new(cfg, cache, false);
        let (reply_tx, mut reply_rx) = unbounded_channel();
        let d2 = d.clone();
        let mut command = io::spawn(move || d2.command(req, reply_tx));
        while let Some(reply) = reply_rx.recv() {
            each(reply);
        }
        let _ = command.join();
        d.save_known();
    })
}

const HEARD_MAX: usize = 1 << 20;

fn left_out_note(ignored: &scan::Ignored, root: &Path, heard: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(heard);
    let mut named: Vec<String> = Vec::new();
    for rel in paths_in(&text, &root.to_string_lossy()) {
        if named.len() == 3 {
            break;
        }
        if ignored.left_out(rel.as_bytes()) && !named.contains(&rel) {
            named.push(rel);
        }
    }
    if named.is_empty() {
        return None;
    }
    let one = named.len() == 1;
    let list = named.iter().map(|p| format!("`{p}`")).collect::<Vec<_>>().join(", ");
    let (is, it) = if one { ("is", "it") } else { ("are", "them") };
    Some(format!(
        "note: {list} {is} not on the build server: git ignores {it}, and the project's `metadata.rargo.sync` says \
         `gitignored = false`.\n      To send {it}, name {it} in `include` there.\n"
    ))
}

fn paths_in<'t>(text: &'t str, root: &'t str) -> impl Iterator<Item = String> + 't {
    let path_char = |c: char| c.is_alphanumeric() || "._-/+@".contains(c);
    let rooted = format!("{}/", root.trim_end_matches('/'));
    text.split(move |c: char| !path_char(c)).filter_map(move |token| {
        let token = token.trim_end_matches('.');
        let rel = match token.strip_prefix(rooted.as_str()) {
            Some(rel) => rel,
            None if token.starts_with('/') || !token.contains(['/', '.']) => return None,
            None => token,
        };
        let mut parts: Vec<&str> = Vec::new();
        for part in rel.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop()?;
                }
                p => parts.push(p),
            }
        }
        (!parts.is_empty()).then(|| parts.join("/"))
    })
}

#[cfg(test)]
mod left_out_tests {
    use expect_test::expect;
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
    }

    enum Ended {
        Reply,
        Again,
        Lost,
    }

    struct Model {
        sent: Vec<(bool, Vec<u8>)>,
        relayed: Vec<(bool, Vec<u8>)>,
        retried: bool,
        changes: u8,
        explained: usize,
    }

    fn converse(c: &mut CommandState, rng: &mut Rng, m: &mut Model, capture: bool, seed: u64) -> Ended {
        c.start(capture);
        c.invariants();
        let mut fetch = None;
        for i in 0..rng.below(6) {
            if c.bet && rng.below(3) == 0 {
                let (trees, blobs) = (rng.below(3) == 0, rng.below(3) == 0);
                let answer = c.bet_answered(trees, blobs);
                c.invariants();
                assert_eq!(answer == BetAnswer::Known, !trees && !blobs, "seed {seed}");
                if answer == BetAnswer::Lacking {
                    return Ended::Again;
                }
            }
            match rng.below(9) {
                0 => return Ended::Lost,
                1 => {
                    let was_bet = c.bet;
                    let effect = c.server(ServerMsg::WantBlobs(vec![[i as u8; 32]]));
                    c.invariants();
                    if was_bet {
                        assert!(matches!(effect, CommandEffect::Abandon), "seed {seed}: a request for files during a bet ends it");
                        return Ended::Again;
                    }
                    assert!(matches!(effect, CommandEffect::Upload(_)));
                    let ok = rng.below(4) != 0;
                    let effect = c.uploaded(ok);
                    c.invariants();
                    assert_eq!(matches!(effect, CommandEffect::Abandon), !ok, "seed {seed}: only a failed upload ends the request");
                    if !ok {
                        return Ended::Again;
                    }
                }
                2 => {
                    c.server(ServerMsg::Delivered { id: 1 });
                    c.invariants();
                    if c.ready_to_fetch() {
                        c.request_fetch(7 + i, vec![[9; 32]]);
                        fetch = Some(7 + i);
                    }
                    c.invariants();
                }
                _ => {
                    let out = rng.below(2) == 0;
                    let bytes = format!("{seed}-{i}").into_bytes();
                    let msg = if out { ServerMsg::Stdout(bytes.clone()) } else { ServerMsg::Stderr(bytes.clone()) };
                    m.sent.push((out, bytes));
                    match c.server(msg) {
                        CommandEffect::Relay(LocalReply::Stdout(b)) => m.relayed.push((true, b)),
                        CommandEffect::Relay(LocalReply::Stderr(b)) => m.relayed.push((false, b)),
                        CommandEffect::Abandon => return Ended::Again,
                        _ => panic!("seed {seed}: output is relayed"),
                    }
                    c.invariants();
                }
            }
        }
        let code = rng.below(3) as i32;
        let effect = c.server(ServerMsg::Exit(code));
        c.invariants();
        if matches!(effect, CommandEffect::ExplainExit) {
            assert!(code != 0 && capture, "seed {seed}: only a failure of a worktree with left-out files is explained");
            m.explained += 1;
        }
        if let Some(id) = fetch {
            assert!(c.finish().is_none(), "seed {seed}: the exit waits for the files it was promised");
            if rng.below(5) == 0 {
                return Ended::Lost;
            }
            assert!(matches!(c.server(ServerMsg::Fetched { id }), CommandEffect::VerifyFetch));
            c.verified_fetch(true).unwrap();
            c.invariants();
        }
        let reply = c.finish().expect("an exit ends the command");
        c.invariants();
        assert!(matches!(reply, LocalReply::Exit(x) if x == code), "seed {seed}: the exit code is the build's");
        Ended::Reply
    }

    #[derive(Clone, Copy)]
    enum Ev {
        Begin,
        Linked,
        ConnectFailed,
        Scanned,
        Offered(Sent),
        Lost,
        LostSilent(&'static str),
    }

    use Ev::*;

    /// Steps a command of `kind` through the events, checking it after each, and lists the step each one gave.
    fn run(kind: CommandKind, events: &[Ev]) -> String {
        let mut c = CommandState::new(kind, Probe::Wait);
        c.invariants();
        let mut out = String::new();
        for &ev in events {
            let (name, step) = match ev {
                Begin => ("begin".to_string(), Some(c.begin())),
                Linked => ("linked".to_string(), Some(c.linked())),
                ConnectFailed => ("connect failed".to_string(), c.connect_failed()),
                Scanned => ("scanned".to_string(), Some(c.scanned())),
                Offered(sent) => (format!("offered {sent:?}"), Some(c.offered(sent))),
                Lost => ("link lost".to_string(), Some(c.link_lost(None))),
                LostSilent(awaited) => {
                    (format!("link silent 45 s awaiting {awaited}"), Some(c.link_lost(Some((Duration::from_secs(45), awaited)))))
                }
            };
            c.invariants();
            out += &format!("{name} => {step:?}\n");
        }
        out
    }

    const TO_REQUEST: [Ev; 5] = [Begin, ConnectFailed, Linked, Scanned, Offered(Sent::All)];
    const RELINK: [Ev; 3] = [Linked, Scanned, Offered(Sent::All)];

    #[test]
    fn a_server_gone_silent_ends_the_command_with_that_silence_once_its_retry_cannot_connect_or_is_spent() {
        let progress = "the build's progress";
        let retry_cannot_connect = [&TO_REQUEST[..], &[LostSilent(progress), ConnectFailed]].concat();
        expect![[r#""#]].assert_eq(&run(CommandKind::Run, &retry_cannot_connect));
        let retry_spent = [&TO_REQUEST[..], &[LostSilent(progress)], &RELINK, &[Lost]].concat();
        expect![[r#""#]].assert_eq(&run(CommandKind::Run, &retry_spent));
        expect![[r#""#]].assert_eq(&run(CommandKind::Admin, &[Linked, LostSilent("its answer"), ConnectFailed]));
    }

    #[test]
    fn a_plain_drop_is_retried_once_and_a_failed_connect_after_it_is_its_own_error() {
        expect![[r#""#]].assert_eq(&run(CommandKind::Run, &[&TO_REQUEST[..], &[Lost, ConnectFailed]].concat()));
        expect![[r#""#]].assert_eq(&run(CommandKind::Run, &[&TO_REQUEST[..], &[Lost], &RELINK, &[Lost]].concat()));
    }

    #[test]
    fn generated_commands_relay_in_order_exit_with_the_builds_code_and_retry_a_lost_link_once() {
        for seed in 1..=300u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let kind = [CommandKind::Run, CommandKind::Run, CommandKind::Sync, CommandKind::Admin][rng.below(4) as usize];
            let probe = if rng.below(2) == 0 { Probe::Bet } else { Probe::Wait };
            let mut c = CommandState::new(kind, probe);
            c.invariants();
            let mut m = Model { sent: Vec::new(), relayed: Vec::new(), retried: false, changes: 0, explained: 0 };
            let capture = rng.below(2) == 0;
            let mut step = c.begin();
            let mut probes = vec![probe];
            let mut ended = false;
            for _ in 0..200 {
                let lost = |c: &mut CommandState, m: &mut Model, visible: bool| {
                    let next = c.link_lost(None);
                    c.invariants();
                    if visible || m.retried {
                        assert_eq!(next, Step::GiveUp, "seed {seed}: a lost link fails clearly after output or a retry");
                    } else {
                        assert_eq!(next, Step::Link, "seed {seed}: a lost link is retried once");
                        m.retried = true;
                    }
                    next
                };
                step = match step {
                    Step::Link => c.linked(),
                    Step::Scan => {
                        assert_ne!(kind, CommandKind::Admin, "seed {seed}");
                        c.scanned()
                    }
                    Step::Offer(p) => {
                        assert_eq!(Some(&p), probes.last(), "seed {seed}: the offer is made as the command last asked");
                        match rng.below(6) {
                            0 => lost(&mut c, &mut m, false),
                            1 => {
                                m.changes += 1;
                                let next = c.offered(Sent::Changed);
                                if m.changes >= 5 {
                                    assert_eq!(next, Step::Fail(KEPT_CHANGING), "seed {seed}");
                                } else {
                                    assert_eq!(next, Step::Link, "seed {seed}");
                                }
                                next
                            }
                            2 if p == Probe::Bet => c.offered(Sent::Bet),
                            _ => c.offered(Sent::All),
                        }
                    }
                    Step::Request => {
                        let before = m.relayed.len();
                        match converse(&mut c, &mut rng, &mut m, capture, seed) {
                            Ended::Reply => {
                                ended = true;
                                break;
                            }
                            Ended::Lost => {
                                let visible = m.relayed.len() > before;
                                lost(&mut c, &mut m, visible)
                            }
                            Ended::Again => {
                                let why = c.after;
                                let next = c.again();
                                c.invariants();
                                match (kind, why) {
                                    (CommandKind::Admin, _) => assert_eq!(next, Step::Error(UNEXPECTED), "seed {seed}"),
                                    (_, Some(After::NotThere(p))) => {
                                        assert_eq!(next, Step::Link, "seed {seed}");
                                        probes.push(p);
                                    }
                                    (_, Some(After::Changed)) => {
                                        m.changes += 1;
                                        let want = if m.changes >= 5 { Step::Fail(KEPT_CHANGING) } else { Step::Rescan };
                                        assert_eq!(next, want, "seed {seed}");
                                    }
                                    (_, None) => panic!("seed {seed}: a request ends without an answer only for a reason"),
                                }
                                next
                            }
                        }
                    }
                    Step::Rescan => c.rescanned(),
                    Step::Exit(code) => {
                        assert_eq!((kind, code), (CommandKind::Sync, 0), "seed {seed}: only a sync ends after its offer");
                        ended = true;
                        break;
                    }
                    Step::Fail(_) | Step::Error(_) | Step::GiveUp | Step::Silent { .. } => {
                        ended = true;
                        break;
                    }
                };
                c.invariants();
            }
            assert!(ended, "seed {seed}: every command ends");
            assert_eq!(m.relayed, m.sent, "seed {seed}: output is relayed in order");
            assert!(m.explained <= 1, "seed {seed}");
        }
    }

    #[test]
    fn paths_in_failure_text_are_found_relative_to_the_root() {
        let root = "/home/ann/proj";
        let rows: &[(&str, &[&str])] = &[
            ("at /home/ann/proj/src/main.rs:12:5", &["src/main.rs"]),
            ("open data/big.bin: not found.", &["data/big.bin"]),
            ("error: couldn't read `/home/ann/proj/crates/util/src/../local-note.txt`", &["crates/util/local-note.txt"]),
            ("couldn't read `/elsewhere/data/big.bin`", &[]),
            ("see /home/ann/proj/../x and /home/ann/proj/a/../../y", &[]),
            ("wrote out.txt... then a/b.rs.", &["out.txt", "a/b.rs"]),
            ("./src/lib.rs and src/./mod.rs", &["src/lib.rs", "src/mod.rs"]),
            ("a/b.rs a/b.rs /home/ann/proj/a/b.rs", &["a/b.rs", "a/b.rs", "a/b.rs"]),
            ("one/a.rs two/b.rs three/c.rs four/d.rs", &["one/a.rs", "two/b.rs", "three/c.rs", "four/d.rs"]),
            ("error[E0308]: mismatched types", &[]),
            ("plain words without paths", &[]),
            ("/home/ann/proj/ and /home/ann/proj/.", &[]),
        ];
        for (text, want) in rows {
            assert_eq!(paths_in(text, root).collect::<Vec<_>>(), *want, "{text}");
            assert_eq!(paths_in(text, "/home/ann/proj/").collect::<Vec<_>>(), *want, "a root with a trailing slash: {text}");
        }
    }

    #[test]
    fn a_failure_naming_left_out_files_gets_one_note_for_at_most_three() {
        let root = Path::new("/home/ann/proj");
        let ignored = scan::Ignored::for_test(&["junk/", "crates/util/local-note.txt", "a.bin", "b.bin", "c.bin", "d.bin"], &[]);
        let one = |p: &str| format!("note: `{p}` is not on the build server: git ignores it");
        let rows: &[(&str, Option<String>)] = &[
            ("error: couldn't read `/home/ann/proj/crates/util/src/../local-note.txt`\n", Some(one("crates/util/local-note.txt"))),
            ("cannot open /home/ann/proj/junk/blob\n", Some(one("junk/blob"))),
            ("a.bin a.bin /home/ann/proj/a.bin\n", Some(one("a.bin"))),
            ("a.bin b.bin\n", Some("note: `a.bin`, `b.bin` are not on the build server: git ignores them".into())),
            ("a.bin b.bin c.bin d.bin\n", Some("note: `a.bin`, `b.bin`, `c.bin` are not".into())),
            ("src/main.rs and /elsewhere/a.bin and ../a.bin\n", None),
            ("error[E0308]: mismatched types\n  --> src/main.rs:3:5\n", None),
            ("", None),
        ];
        for (heard, want) in rows {
            let got = left_out_note(&ignored, root, heard.as_bytes());
            match (want, &got) {
                (None, None) => {}
                (Some(start), Some(note)) => {
                    assert!(note.starts_with(start.as_str()), "{heard:?}: {note}");
                    assert!(note.contains("gitignored = false") && note.contains("`include`"), "{note}");
                    assert!(!note.contains("d.bin"), "at most three are named: {note}");
                }
                _ => panic!("{heard:?}: {got:?}"),
            }
        }
    }

    #[test]
    fn mounted_output_manifest_paths_are_relative_to_the_mount() {
        for (path, expected) in [
            ("/work/target/debug/app", Some("debug/app")),
            ("/work/target", None),
            ("/work/target-other/debug/app", None),
            ("/another/target/debug/app", None),
        ] {
            assert_eq!(target_relative(Path::new("/work/target"), Path::new(path)), expected.map(|s| s.as_bytes().to_vec()));
        }
    }

    use super::*;

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_newest_mount_on_a_directory_gives_its_device() {
        let info = "22 1 0:21 / /proc rw - proc proc rw\n\
                    80 30 0:61 / /w/target rw - fuse.rargo rargo rw\n\
                    81 80 0:62 / /w/target rw - fuse.rargo rargo rw\n\
                    82 30 0:63 / /w/target2 rw - fuse.rargo rargo rw\n";
        assert_eq!(super::dev_at(info, Path::new("/w/target")), Some(62));
        assert_eq!(super::dev_at(info, Path::new("/w/target2")), Some(63));
        assert_eq!(super::dev_at(info, Path::new("/w")), None);
        assert_eq!(super::mounts_at(info, Path::new("/w/target")).count(), 2);
    }

    #[test]
    fn a_refused_token_says_whether_there_was_one_and_names_rargo_auth() {
        let none = refused_text("build.example:7459", false);
        let revoked = refused_text("build.example:7459", true);
        assert_eq!(none, "error: this machine is not authenticated to the rargo server at build.example:7459: run `rargo auth`, which also says how to get access if the server has not let you in yet\n");
        assert!(revoked.starts_with("error: the rargo server at build.example:7459 does not accept this machine's token: it does not know it, or it was revoked or has expired. Run `rargo auth`"), "{revoked}");
        for text in [none, revoked] {
            assert!(!text.contains(rargo_proto::REFUSED), "{text}");
        }
    }

    #[test]
    fn a_nearly_full_cache_disk_is_warned_of_with_what_to_do() {
        let cache = Path::new("/home/ann/.cache/rargo");
        for (free, warned) in [(0, true), (1 << 20, true), (LOW_SPACE - 1, true), (LOW_SPACE, false), (1 << 40, false)] {
            let text = low_space_text(cache, free);
            assert_eq!(text.is_some(), warned, "{free}");
            if let Some(text) = text {
                assert!(text.contains("/home/ann/.cache/rargo") && text.contains(&format!("{} MiB free", free >> 20)), "{text}");
                assert!(text.contains("Free space on that disk") && text.contains("`rargo --rargo-daemon-stop`"), "{text}");
            }
        }
    }

    #[test]
    fn only_a_link_that_cannot_be_made_is_an_unreachable_server() {
        use std::io::{Error, ErrorKind};
        let refused = no_link(Error::new(ErrorKind::ConnectionRefused, "connection refused"));
        assert!(is_no_link(&refused));
        assert_eq!(refused.kind(), ErrorKind::ConnectionRefused);
        assert_eq!(refused.to_string(), "connection refused");
        assert!(!is_no_link(&Error::from_raw_os_error(libc::ENOENT)));
        assert!(!is_no_link(&no_link(Error::new(ErrorKind::PermissionDenied, rargo_proto::REFUSED))));
        let passed = (|| -> std::io::Result<()> { Err(no_link(Error::new(ErrorKind::TimedOut, "timed out")))? })();
        assert!(is_no_link(&passed.unwrap_err()));
    }
}
