use crate::disk;
use rargo_core::db::Table;
use rargo_core::store::Store;
use rargo_net::io::sim;
use crate::transport::{self, Side};
use rargo_net::sans_io::{EndpointState, LinkKey, RouteEvent};
use serde::{Deserialize, Serialize};
use core::net::SocketAddr;
use rargo_machine::StateMachine;
use crate::model::{self, Project};
use crate::workspace::Workspace;
use crate::worktree::{self, Worktree};
use rargo::out_tree::{Edit, OutputTree, VFile};
use rargo::scan::{Index, Scan};
use crate::rng::Rng;
use crate::nextest::{Outcome, TestRun};
use rargo_core::build::nextest_run::Phase;
use rargo::daemon_state::{DaemonState, Link, Need};
use rargo::known_digests::KnownDigests;
use rargo::link_watch::{self, Tick, Watch};
use rargo_core::hash::Hash;
use rargo_core::pool_state::{self, WorkerPool};
use rargo_core::sched::Priority;
use rargo_core::sched_state::{Broker, Effects, SchedState};
use rargo_core::session_state::{self, ClientSet, Next, Outgoing, SessionState};
use rargo_core::trees_state::{TreesState, Upload};
use rargo_core::worker_state::{self, Carried, FromServer, Worker};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use rargo_proto::{Blob, ClientMsg, Delivery, Digest, ServerMsg};
use std::fmt;
use std::time::Duration;

pub const LIVENESS_MS: u64 = 120_000;
const SERVER_TOKENS: usize = 6;
const SERVER_MEM_KB: u64 = 16 * 1024;
const WATCH_STEP_MS: u64 = 200;
/// Seeds the network faults' streams, apart from the one that drives the rest of the world.
const NET_STREAM: u64 = 0x6e65_7477_6f72_6b21;
const WORKSPACE_STREAM: u64 = 0x776f_726b_7370_6163;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bug {
    EvictPinned,
    StaleDelivery,
    NeverAnswer,
    EvictClaimed,
    StaleKnowledge,
    DeafToSilence,
    TrustAfterCrash,
    /// The daemon's watch misses the writes in the worktree, so a rescan reads none of them.
    DeafToEdits,
    /// The daemon brings the outputs back without showing them in `target/`.
    UnshownOutputs,
}

impl Bug {
    pub const ALL: [Bug; 9] = [
        Bug::EvictPinned,
        Bug::StaleDelivery,
        Bug::NeverAnswer,
        Bug::EvictClaimed,
        Bug::StaleKnowledge,
        Bug::DeafToSilence,
        Bug::TrustAfterCrash,
        Bug::DeafToEdits,
        Bug::UnshownOutputs,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Bug::EvictPinned => "evict-pinned",
            Bug::StaleDelivery => "stale-delivery",
            Bug::NeverAnswer => "never-answer",
            Bug::EvictClaimed => "evict-claimed",
            Bug::StaleKnowledge => "stale-knowledge",
            Bug::DeafToSilence => "deaf-to-silence",
            Bug::TrustAfterCrash => "trust-after-crash",
            Bug::DeafToEdits => "deaf-to-edits",
            Bug::UnshownOutputs => "unshown-outputs",
        }
    }

    pub fn parse(s: &str) -> Option<Bug> {
        Bug::ALL.into_iter().find(|b| b.name() == s)
    }
}

/// A fault the simulator makes on its own, which `rargo-sim --off` turns off.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Fault {
    Loss,
    Duplicate,
    Reorder,
    ServerCrash,
    Partition,
}

impl Fault {
    pub const ALL: [Fault; 5] = [Fault::Loss, Fault::Duplicate, Fault::Reorder, Fault::ServerCrash, Fault::Partition];
    const NETWORK: [Fault; 3] = [Fault::Loss, Fault::Duplicate, Fault::Reorder];

    pub fn name(self) -> &'static str {
        match self {
            Fault::Loss => "loss",
            Fault::Duplicate => "duplicate",
            Fault::Reorder => "reorder",
            Fault::ServerCrash => "server-crash",
            Fault::Partition => "partition",
        }
    }

    pub fn parse(s: &str) -> Option<Fault> {
        Fault::ALL.into_iter().find(|f| f.name() == s)
    }
}

/// Which optional faults a run leaves out. Each fault draws from its own stream, so turning one off
/// leaves the others' rates and draws as the seed picked them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Off(BTreeSet<Fault>);

impl Off {
    pub fn none() -> Off {
        Off::default()
    }

    pub fn with(mut self, f: Fault) -> Off {
        self.0.insert(f);
        self
    }

    pub fn has(&self, f: Fault) -> bool {
        self.0.contains(&f)
    }

    fn flags(&self) -> String {
        self.0.iter().map(|f| format!(" --off {}", f.name())).collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    StateMachine(String),
    LostPinned(String),
    Drift(String),
    Liveness(String),
    Deadlock(String),
    StartedWithoutInputs(String),
    Silence(String),
}

impl Kind {
    pub fn name(&self) -> &'static str {
        match self {
            Kind::StateMachine(_) => "a state machine's invariants",
            Kind::LostPinned(_) => "no lost pinned content",
            Kind::Drift(_) => "no drift",
            Kind::Liveness(_) => "liveness once faults stop",
            Kind::Deadlock(_) => "no deadlock",
            Kind::StartedWithoutInputs(_) => "a run starts with its inputs",
            Kind::Silence(_) => "a silent server ends the command, and only a silent one",
        }
    }

    fn detail(&self) -> &str {
        match self {
            Kind::StateMachine(d)
            | Kind::LostPinned(d)
            | Kind::Drift(d)
            | Kind::Liveness(d)
            | Kind::Deadlock(d)
            | Kind::StartedWithoutInputs(d)
            | Kind::Silence(d) => d,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub seed: u64,
    pub bug: Option<Bug>,
    pub off: Off,
    pub at_ms: u64,
    pub kind: Kind,
}

impl Failure {
    pub fn replay(&self) -> String {
        let bug = self.bug.map(|b| format!(" --bug {}", b.name())).unwrap_or_default();
        format!("rargo-sim --seed {}{bug}{}", self.seed, self.off.flags())
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "seed {}: {} broken at {} ms: {}\nreplay: {}",
            self.seed,
            self.kind.name(),
            self.at_ms,
            self.kind.detail(),
            self.replay()
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub events: u64,
    pub end_ms: u64,
    pub trace: Hash,
    pub runs: u64,
    pub long_runs: u64,
    pub failed_builds: u64,
    pub cancelled_builds: u64,
    pub cancelled_with_outputs: u64,
    pub aborted_runs: u64,
    pub cache_hits: u64,
    pub tests_passed: u64,
    pub tests_failed: u64,
    pub tests_timed_out: u64,
    pub cuts: u64,
    pub worker_crashes: u64,
    pub server_restarts: u64,
    pub jobs_lost_to_crashes: u64,
    pub evictions: u64,
    /// Most client rounds in progress when the server evicted a blob.
    pub clients_at_eviction: usize,
    pub uploads: u64,
    pub wants: u64,
    pub client_cuts: u64,
    pub silences: u64,
    pub crashes: u64,
    pub lost: u64,
    pub duplicated: u64,
    pub reordered: u64,
    pub partitioned: u64,
    pub retransmits: u64,
    pub disk_full_errors: u64,
    pub disk_crashes: u64,
    pub torn_writes: u64,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub workers: u64,
    pub clients: u64,
    pub sources: usize,
    pub rounds: u64,
    pub fault_end_ms: u64,
    pub cut_per_mille: u64,
    pub max_latency_ms: u64,
    pub client_cuts: u64,
    pub crashes: u64,
    pub net: Net,
}

/// How often, per mille of datagrams, the network loses, duplicates and reorders until `fault_end_ms`.
#[derive(Clone, Debug)]
pub struct Net {
    pub loss_per_mille: u64,
    pub duplicate_per_mille: u64,
    pub reorder_per_mille: u64,
}

impl Net {
    fn of(rngs: &mut [Rng; 3], off: &Off) -> Net {
        let mut rate = |f: Fault| {
            let rng = &mut rngs[f as usize];
            let r = if rng.chance(500) { rng.range(1, 40) } else { 0 };
            if off.has(f) { 0 } else { r }
        };
        Net { loss_per_mille: rate(Fault::Loss), duplicate_per_mille: rate(Fault::Duplicate), reorder_per_mille: rate(Fault::Reorder) }
    }
}

impl Config {
    pub fn of(rng: &mut Rng, net: Net, sources: usize) -> Config {
        let workers = rng.range(1, 4);
        let client_roll = rng.next_u64();
        // One seed in 128 exercises a crowd. Use the same draw for small worlds
        // so their remaining configuration and workload still replay.
        let clients = if client_roll >> 57 == 0 {
            20 + client_roll % 13
        } else {
            1 + client_roll % 3
        };
        Config {
            workers,
            clients,
            sources,
            rounds: rng.range(1, 3),
            fault_end_ms: rng.range(0, 8_000),
            cut_per_mille: rng.range(0, 30),
            max_latency_ms: rng.range(1, 60),
            client_cuts: rng.range(0, 3),
            crashes: rng.range(0, 2),
            net,
        }
    }
}

#[derive(Debug)]
pub struct Sim;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SimRun {
    action: usize,
    inputs: Vec<Hash>,
    ms: u64,
}

impl Carried for Sim {
    type Run = SimRun;
    type Part = Vec<u8>;
    type Serve = ();
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Node {
    Server,
    Worker(u64),
    Client(u64),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum Up {
    Hello { cores: usize },
    Result { job: u64, output: Result<Vec<u8>, Vec<u8>> },
    Join,
    Offer { round: u64, root: Digest, contents: Vec<Digest>, sending: Vec<Digest> },
    Session(ClientMsg),
    Request { round: u64, action: usize, inputs: Vec<Hash> },
    Cancel { round: u64 },
    Ping,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum Down {
    Session(ServerMsg),
    RoundTerminal { round: u64, msg: ServerMsg },
    Synced { round: u64, root: Digest },
    Pong,
    DiskError { round: u64, message: String },
}

#[derive(Debug, Serialize, Deserialize)]
enum Msg {
    Up(Up),
    Work(FromServer<Sim>),
    Down(Down),
}

struct Connection {
    id: u64,
    endpoint: EndpointState,
    sides: [Option<Side>; 2],
    wakes: [Option<u64>; 2],
}

fn addr(node: Node) -> SocketAddr {
    let (kind, id) = match node {
        Node::Server => (0, 0),
        Node::Worker(id) => (1, id),
        Node::Client(id) => (2, id),
    };
    SocketAddr::from((core::net::Ipv6Addr::from((kind << 64) | u128::from(id)), 1))
}

#[derive(Debug)]
enum Ev {
    Datagram { peer: Node, to_server: bool, id: u64, bytes: Vec<u8> },
    LinkTimer { peer: Node, server: bool, id: u64 },
    Connect { worker: u64, epoch: u64 },
    Finish { worker: u64, epoch: u64, token: u64 },
    Crash { worker: u64 },
    ServerCrash,
    Cut { worker: u64 },
    Cancel { worker: u64, epoch: u64, token: u64 },
    Evict,
    Round { client: u64 },
    Edit { client: u64 },
    Checkout { client: u64, round: u64 },
    TestComplete { client: u64, key: String, started_ms: u64 },
    Sync { client: u64 },
    ClientCut { client: u64 },
    Watch { client: u64 },
    ClientCrash { client: u64 },
    Restart { client: u64 },
}

struct WorkerNode {
    sm: Worker<Sim>,
    cores: usize,
    store: BTreeMap<Hash, Vec<u8>>,
    running: BTreeMap<u64, (u64, SimRun)>,
    up: bool,
    epoch: u64,
    cut_until: u64,
}

struct Job {
    client: u64,
    round: u64,
    action: usize,
    inputs: Vec<Hash>,
    key: Hash,
    worker: Option<u64>,
}

struct Offer {
    round: u64,
    root: Digest,
    contents: Vec<Hash>,
    up: Upload,
    arrived: BTreeSet<Hash>,
}

struct Sess {
    state: SessionState,
    has: ClientSet,
    offer: Option<Offer>,
    ended: Option<u64>,
}

struct Server {
    pool: WorkerPool,
    trees: TreesState,
    sessions: BTreeMap<u64, Sess>,
    sched: SchedState,
    pipe: usize,
    store: Option<Store>,
    pins: BTreeMap<Hash, usize>,
    held: BTreeMap<u64, (u64, BTreeSet<Hash>)>,
    jobs: BTreeMap<u64, Job>,
    ready: VecDeque<u64>,
    next_job: u64,
    disk_errors: BTreeMap<u64, (u64, String)>,
}

fn held_key(client: u64) -> [u8; 8] {
    client.to_le_bytes()
}

impl Server {
    fn new() -> Self {
        Self {
            pool: WorkerPool::new(),
            trees: TreesState::new(),
            sessions: BTreeMap::new(),
            sched: SchedState::new(SERVER_TOKENS, SERVER_MEM_KB),
            pipe: SERVER_TOKENS,
            store: None,
            pins: BTreeMap::new(),
            held: BTreeMap::new(),
            jobs: BTreeMap::new(),
            ready: VecDeque::new(),
            next_job: 1,
            disk_errors: BTreeMap::new(),
        }
    }

    fn save_held(&self, client: u64) {
        self.store().db().put(Table::Meta, &held_key(client), &rargo_proto::encode(&self.held[&client]));
    }

    fn reopen(&mut self, clients: u64) -> Step {
        let store = self.store();
        let root = store.root().parent().expect("the server disk root").to_owned();
        // Event boundaries are durability points for the process-crash fault.
        store.db().flush_checked().map_err(storage_error)?;
        *self = Self::new();
        self.store = Some(disk::open(&root));
        for client in 1..=clients {
            let Some(bytes) = self.store().db().get(Table::Meta, &held_key(client)) else { continue };
            let (round, held): (u64, BTreeSet<Hash>) = rargo_proto::decode(&bytes).map_err(storage_error)?;
            for hash in &held {
                *self.pins.entry(*hash).or_default() += 1;
            }
            self.held.insert(client, (round, held));
        }
        Ok(())
    }

    fn store(&self) -> &Store {
        self.store.as_ref().expect("the server has started")
    }
}

struct Client {
    round: u64,
    rounds_left: u64,
    tree: Vec<Option<Vec<u8>>>,
    // Reconnects and later actions in this round still read the original tree.
    snapshot: Vec<Option<Vec<u8>>>,
    reference: Vec<Vec<u8>>,
    edits: Rng,
    edit_scheduled: bool,
    checkout_seed: Option<u64>,
    /// The files `tree` names, which the daemon's scan reads.
    worktree: Worktree,
    /// The daemon's last scan, which it keeps in memory to rescan only what changed.
    last_scan: Option<Scan>,
    /// The scan's index as the daemon last saved it, which outlives a crash.
    index: Index,
    scan_root: Digest,
    /// The output filesystem's table of what the client's `target/` holds.
    target: OutputTree,
    // Dependency outputs are staged until the build succeeds.
    outputs: BTreeMap<usize, Vec<u8>>,
    round_ended: bool,
    diagnostic: Option<Vec<u8>>,
    requested: BTreeSet<usize>,
    finished: bool,
    test: Option<TestRun>,
    daemon: DaemonState,
    up: bool,
    epoch: u64,
    cut_until: u64,
    cache: BTreeMap<Digest, Vec<u8>>,
    parts: BTreeMap<Digest, Vec<u8>>,
    pending: BTreeMap<u64, Vec<Delivery>>,
    contents: Vec<Digest>,
    synced_round: u64,
    watch: Watch,
    up_since: u64,
    dead: bool,
    saved: Vec<u8>,
}

impl Client {
    fn edit(&mut self, now_ns: i64) {
        let index = self.edits.below(self.tree.len() as u64) as usize;
        let slot = &mut self.tree[index];
        if slot.is_some() && self.edits.chance(500) {
            *slot = None;
        } else {
            // Revisiting earlier contents exercises cache hits and re-uploads after eviction.
            let original = model::source(index, 0);
            let version = slot.as_ref().map_or_else(|| self.edits.below(2), |bytes| u64::from(*bytes == original));
            *slot = Some(if version == 0 { original } else { model::source(index, version) });
        }
        self.put(index, now_ns);
    }

    /// Writes source `index` as `tree` has it into the worktree.
    fn put(&mut self, index: usize, now_ns: i64) {
        match &self.tree[index] {
            Some(bytes) => self.worktree.write(&worktree::path(index), bytes.clone(), now_ns),
            None => self.worktree.remove(&worktree::path(index)),
        }
    }
}

fn ns(ms: u64) -> i64 {
    ms as i64 * 1_000_000
}

pub struct World {
    seed: u64,
    bug: Option<Bug>,
    off: Off,
    cfg: Config,
    rng: Rng,
    net: [Rng; 3],
    connections: BTreeMap<Node, Connection>,
    server_endpoint: EndpointState,
    link_key: LinkKey,
    next_conn: u64,
    now: u64,
    seq: u64,
    queue: BTreeMap<(u64, u64), Ev>,
    busy: usize,
    last: BTreeMap<(Node, bool), u64>,
    project: Project,
    server: Server,
    workers: BTreeMap<u64, WorkerNode>,
    clients: BTreeMap<u64, Client>,
    report: Report,
    trace: Vec<u8>,
    silence: Option<(u64, u64)>,
    watch_limit: Duration,
    disk_fault: Option<disk::Fault>,
    partitions: BTreeMap<(Node, bool), std::ops::Range<u64>>,
}

type Step = Result<(), Kind>;

fn storage_error(error: impl fmt::Display) -> Kind {
    Kind::StateMachine(format!("server disk: {error}"))
}

fn link_error(error: std::io::Error) -> Kind {
    Kind::StateMachine(format!("UDP link: {error}"))
}

impl World {
    pub fn new(seed: u64, bug: Option<Bug>, off: Off) -> World {
        Self::configured(seed, bug, off, |_, _| {})
    }

    fn configured(seed: u64, bug: Option<Bug>, off: Off, setup: impl FnOnce(&mut Config, &mut Project)) -> World {
        let mut rng = Rng::new(seed);
        let mut net = Fault::NETWORK.map(|f| Rng::new(seed ^ NET_STREAM ^ f as u64));
        let mut plan_rng = Rng::new(seed ^ WORKSPACE_STREAM);
        let workspace = Workspace::generate(&mut plan_rng);
        let mut project = Project::plan(&mut plan_rng, &workspace);
        let mut cfg = Config::of(&mut rng, Net::of(&mut net, &off), project.sources);
        setup(&mut cfg, &mut project);
        let link_key = transport::key();
        let mut w = World {
            seed,
            bug,
            off,
            rng,
            net,
            connections: BTreeMap::new(),
            partitions: BTreeMap::new(),
            server_endpoint: EndpointState::new(true, false, link_key.clone()),
            link_key,
            next_conn: 1,
            now: 0,
            seq: 0,
            queue: BTreeMap::new(),
            busy: 0,
            last: BTreeMap::new(),
            server: Server::new(),
            workers: BTreeMap::new(),
            clients: BTreeMap::new(),
            report: Report {
                events: 0,
                end_ms: 0,
                trace: Hash::of(b""),
                runs: 0,
                long_runs: 0,
                failed_builds: 0,
                cancelled_builds: 0,
                cancelled_with_outputs: 0,
                aborted_runs: 0,
                cache_hits: 0,
                tests_passed: 0,
                tests_failed: 0,
                tests_timed_out: 0,
                cuts: 0,
                worker_crashes: 0,
                server_restarts: 0,
                jobs_lost_to_crashes: 0,
                evictions: 0,
                clients_at_eviction: 0,
                uploads: 0,
                wants: 0,
                client_cuts: 0,
                silences: 0,
                crashes: 0,
                lost: 0,
                duplicated: 0,
                reordered: 0,
                partitioned: 0,
                retransmits: 0,
                disk_full_errors: 0,
                disk_crashes: 0,
                torn_writes: 0,
            },
            trace: Vec::new(),
            silence: None,
            watch_limit: link_watch::SILENT_AFTER,
            disk_fault: match Rng::new(seed ^ 0x6469_736b).below(12) {
                0 => Some(disk::Fault::Full),
                1 => Some(disk::Fault::Torn),
                2 => Some(disk::Fault::Crash),
                _ => None,
            },
            project,
            cfg,
        };
        for id in 1..=w.cfg.workers {
            let (sm, fx) = Worker::new();
            let cores = w.rng.range(1, 3) as usize;
            w.workers.insert(
                id,
                WorkerNode { sm, cores, store: BTreeMap::new(), running: BTreeMap::new(), up: false, epoch: 0, cut_until: 0 },
            );
            w.worker_effects(id, fx).expect("a new worker only connects");
        }
        for id in 1..=w.cfg.clients {
            w.clients.insert(
                id,
                Client {
                    round: 0,
                    rounds_left: w.cfg.rounds,
                    tree: (0..w.cfg.sources).map(|s| Some(model::source(s, 0))).collect(),
                    snapshot: Vec::new(),
                    reference: Vec::new(),
                    edits: Rng::new(seed ^ id.wrapping_mul(0x912a_e043_5b7d_68f1)),
                    edit_scheduled: false,
                    checkout_seed: Some(seed ^ id.wrapping_mul(0x5cab_f0e9_d1b2_7431)),
                    worktree: Worktree::default(),
                    last_scan: None,
                    index: Index::default(),
                    scan_root: [0; 32],
                    target: OutputTree::new(BTreeMap::new()),
                    outputs: BTreeMap::new(),
                    round_ended: false,
                    diagnostic: None,
                    requested: BTreeSet::new(),
                    finished: false,
                    test: None,
                    daemon: new_daemon(KnownDigests::default()),
                    up: false,
                    epoch: 0,
                    cut_until: 0,
                    cache: BTreeMap::new(),
                    parts: BTreeMap::new(),
                    pending: BTreeMap::new(),
                    contents: Vec::new(),
                    synced_round: 0,
                    watch: Watch::new(Duration::ZERO, w.watch_limit),
                    up_since: 0,
                    dead: false,
                    saved: Vec::new(),
                },
            );
            let c = w.clients.get_mut(&id).expect("just made");
            for index in 0..c.tree.len() {
                c.put(index, 0);
            }
            let at = w.rng.range(0, 500);
            w.schedule(at, Ev::Round { client: id });
            w.schedule_housekeeping(WATCH_STEP_MS, Ev::Watch { client: id });
        }
        if w.cfg.fault_end_ms > 0 {
            for id in 1..=w.cfg.workers {
                let at = w.rng.range(0, w.cfg.fault_end_ms);
                w.schedule(at, Ev::Cut { worker: id });
                let at = w.rng.range(0, w.cfg.fault_end_ms);
                w.schedule(at, Ev::Crash { worker: id });
            }
            for _ in 0..w.cfg.client_cuts {
                let client = w.rng.range(1, w.cfg.clients);
                let at = w.rng.range(0, w.cfg.fault_end_ms);
                w.schedule(at, Ev::ClientCut { client });
            }
            for _ in 0..w.cfg.crashes {
                let client = w.rng.range(1, w.cfg.clients);
                let at = w.rng.range(0, w.cfg.fault_end_ms);
                w.schedule(at, Ev::ClientCrash { client });
            }
            if w.rng.chance(300) {
                let start = w.rng.range(0, w.cfg.fault_end_ms);
                w.silence = Some((start, start + w.rng.range(5_000, 100_000)));
            }
        }
        let at = w.rng.range(50, 400);
        w.schedule_housekeeping(at, Ev::Evict);
        // Keep the crash schedule independent of workload and network draws.
        let mut crashes = Rng::new(seed ^ 0x7365_7276_6372_6173);
        if !w.off.has(Fault::ServerCrash) && w.cfg.fault_end_ms > 0 && crashes.chance(500) {
            for _ in 0..crashes.range(1, 3) {
                w.schedule(crashes.range(0, w.cfg.fault_end_ms), Ev::ServerCrash);
            }
        }
        if !w.off.has(Fault::Partition) && w.cfg.fault_end_ms > 0 {
            let mut rng = Rng::new(seed ^ NET_STREAM ^ Fault::Partition as u64);
            let peer = rng.range(1, w.cfg.workers + w.cfg.clients);
            let peer = if peer <= w.cfg.workers { Node::Worker(peer) } else { Node::Client(peer - w.cfg.workers) };
            let start = rng.range(0, w.cfg.fault_end_ms - 1);
            let end = rng.range(start + 1, w.cfg.fault_end_ms);
            let directions = rng.range(1, 3);
            if directions & 1 != 0 {
                w.partitions.insert((peer, false), start..end);
            }
            if directions & 2 != 0 {
                w.partitions.insert((peer, true), start..end);
            }
        }
        w
    }

    fn schedule(&mut self, after_ms: u64, ev: Ev) {
        self.busy += 1;
        self.seq += 1;
        self.queue.insert((self.now + after_ms, self.seq), ev);
    }

    fn schedule_housekeeping(&mut self, after_ms: u64, ev: Ev) {
        self.seq += 1;
        self.queue.insert((self.now + after_ms, self.seq), ev);
    }

    fn open_link(&mut self, peer: Node) -> Step {
        let id = self.next_conn;
        self.next_conn += 1;
        let mut endpoint = EndpointState::new(false, false, self.link_key.clone());
        assert!(endpoint.connect(id, addr(Node::Server), self.now));
        let initiator = Side::new(id, true, self.now).map_err(link_error)?;
        let connection = Connection { id, endpoint, sides: [Some(initiator), None], wakes: [None; 2] };
        assert!(self.connections.insert(peer, connection).is_none());
        self.wake_link(peer, false, self.now);
        Ok(())
    }

    fn close_link(&mut self, peer: Node) {
        if let Some(link) = self.connections.remove(&peer) {
            self.server_endpoint.forget(link.id);
        }
        self.last.remove(&(peer, false));
        self.last.remove(&(peer, true));
    }

    fn wake_link(&mut self, peer: Node, server: bool, at: u64) {
        let link = self.connections.get_mut(&peer).expect("a live link");
        let wake = &mut link.wakes[usize::from(server)];
        if wake.is_some_and(|current| current <= at) {
            return;
        }
        *wake = Some(at);
        let id = link.id;
        self.schedule(at - self.now, Ev::LinkTimer { peer, server, id });
    }

    fn send(&mut self, from: Node, to: Node, msg: Msg) {
        let server = from == Node::Server;
        let peer = if server { to } else { from };
        let link = self.connections.get_mut(&peer).expect("a live link");
        link.sides[usize::from(server)].as_mut().expect("a local link side").send(rargo_proto::encode(&msg));
        self.wake_link(peer, server, self.now);
    }

    fn transmit(&mut self, peer: Node, to_server: bool, id: u64, bytes: Vec<u8>) {
        let latency = self.rng.range(1, self.cfg.max_latency_ms);
        let max_latency = self.cfg.max_latency_ms;
        let net = &self.cfg.net;
        let faulty = self.now < self.cfg.fault_end_ms;
        let [loss, dup, reorder] = &mut self.net;
        let lost = loss.chance(net.loss_per_mille) && faulty;
        let copy_after = dup.range(1, 4 * max_latency);
        let duplicate = dup.chance(net.duplicate_per_mille) && faulty;
        let early = reorder.range(1, max_latency);
        let reordered = reorder.chance(net.reorder_per_mille) && faulty;
        if lost {
            self.report.lost += 1;
            return;
        }
        let after = if reordered {
            self.report.reordered += 1;
            early
        } else {
            let last = self.last.entry((peer, to_server)).or_default();
            *last = (*last).max(self.now + latency);
            *last - self.now
        };
        if duplicate {
            self.report.duplicated += 1;
            self.schedule(after + copy_after, Ev::Datagram { peer, to_server, id, bytes: bytes.clone() });
        }
        self.schedule(after, Ev::Datagram { peer, to_server, id, bytes });
    }

    fn link_timer(&mut self, peer: Node, server: bool, id: u64) -> Step {
        let Some(link) = self.connections.get_mut(&peer).filter(|link| link.id == id) else { return Ok(()) };
        let side = usize::from(server);
        if link.wakes[side] != Some(self.now) {
            return Ok(());
        }
        link.wakes[side] = None;
        let mut effects = transport::Effects::default();
        link.sides[side].as_mut().expect("a local link side")
            .drive(&self.link_key, self.now, &mut effects).map_err(link_error)?;
        self.report.retransmits += effects.retransmits;
        for packet in effects.packets {
            self.transmit(peer, !server, id, packet);
        }
        if let Some(at) = effects.wake {
            self.wake_link(peer, server, at);
        }
        Ok(())
    }

    fn finished(&self) -> bool {
        self.clients.values().all(|c| c.finished)
    }

    pub fn run(self) -> Result<Report, Failure> {
        let world = Box::new(self);
        sim::test(move || world.run_with_disk())
    }

    fn run_with_disk(mut self) -> Result<Report, Failure> {
        self.server.store = Some(disk::open(&sim::tempdir()));
        let seed = self.seed;
        let bug = self.bug;
        let off = self.off.clone();
        loop {
            let fail = |w: &World, kind: Kind| Failure { seed, bug, off: off.clone(), at_ms: w.now, kind };
            if self.finished() {
                self.server.store().db().flush_checked().map_err(|err| fail(&self, storage_error(err)))?;
                self.report.end_ms = self.now;
                self.report.trace = Hash::of(&self.trace);
                return Ok(self.report);
            }
            if self.busy == 0 {
                let open: Vec<String> = self
                    .clients
                    .iter()
                    .filter(|(_, c)| !c.finished)
                    .map(|(id, c)| format!("client {id} round {} waits on {:?}", c.round, self.waiting(c)))
                    .collect();
                return Err(fail(&self, Kind::Deadlock(format!("nothing left to happen: {}", open.join("; ")))));
            }
            if self.now > self.faults_end() + LIVENESS_MS {
                let open: Vec<String> = self
                    .clients
                    .iter()
                    .filter(|(_, c)| !c.finished)
                    .map(|(id, c)| format!("client {id} round {} waits on {:?}", c.round, self.waiting(c)))
                    .collect();
                return Err(fail(
                    &self,
                    Kind::Liveness(format!(
                        "faults stopped at {} ms and {}",
                        self.faults_end(),
                        open.join("; ")
                    )),
                ));
            }
            let ((at, _), ev) = self.queue.pop_first().expect("busy events are queued");
            if !matches!(ev, Ev::Evict | Ev::Watch { .. }) {
                self.busy -= 1;
            }
            self.now = at;
            self.report.events += 1;
            self.trace.extend_from_slice(&at.to_le_bytes());
            self.trace.extend_from_slice(label(&ev).as_bytes());
            let stepped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.step(ev)?;
                self.check()
            }));
            match stepped {
                Ok(Ok(())) => {}
                Ok(Err(kind)) => return Err(fail(&self, kind)),
                Err(panic) => {
                    let text = panic
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                        .unwrap_or_default();
                    return Err(fail(&self, Kind::StateMachine(text)));
                }
            }
        }
    }

    fn faults_end(&self) -> u64 {
        self.silence.map_or(self.cfg.fault_end_ms, |(_, end)| end.max(self.cfg.fault_end_ms))
    }

    fn silent_at(&self, at: u64) -> Option<(u64, u64)> {
        self.silence.filter(|(start, end)| (*start..*end).contains(&at))
    }

    fn waiting(&self, c: &Client) -> Vec<u64> {
        c.requested.iter().filter(|a| !c.outputs.contains_key(a)).map(|&a| self.project.actions[a].id).collect()
    }

    /// Every state machine in the world: the server's, each session's, each
    /// worker's and each client's.
    fn machines(&self) -> Vec<&dyn StateMachine> {
        let mut all: Vec<&dyn StateMachine> = vec![&self.server.pool, &self.server.sched, &self.server.trees];
        all.extend(self.server.sessions.values().map(|s| &s.state as &dyn StateMachine));
        all.extend(self.workers.values().map(|w| &w.sm as &dyn StateMachine));
        all.extend(self.clients.values().map(|c| &c.daemon as &dyn StateMachine));
        all.extend(self.clients.values().filter_map(|c| c.test.as_ref()).map(|t| &t.machine as &dyn StateMachine));
        all
    }

    fn check(&self) -> Step {
        rargo_machine::check_all(self.machines());
        for client in self.clients.values() {
            assert!(client.target.files.is_empty() || client.target.files.len() == self.project.actions.len());
            if client.round_ended {
                assert!(client.outputs.is_empty() && client.pending.is_empty() && client.parts.is_empty());
            }
        }
        for (client, sess) in &self.server.sessions {
            let Some(o) = &sess.offer else { continue };
            for h in &o.arrived {
                if self.server.trees.holder(h) == Some(o.up) && !self.server.store().has(h) {
                    return Err(Kind::LostPinned(format!(
                        "blob {} that client {client} uploaded for round {} is claimed and not in the store",
                        h.hex(),
                        o.round
                    )));
                }
            }
        }
        for (h, n) in &self.server.pins {
            if *n > 0 && !self.server.store().has(h) {
                return Err(Kind::LostPinned(format!("blob {} is pinned {n} times and not in the store", h.hex())));
            }
        }
        for (job, j) in &self.server.jobs {
            if let Some(h) = j.inputs.iter().find(|h| !self.server.store().has(h)) {
                return Err(Kind::LostPinned(format!("input {} of unfinished job {job} is gone", h.hex())));
            }
            if let Some(w) = j.worker {
                assert!(self.server.pool.route(w, *job), "job {job} sent to worker {w} is routed there");
            }
        }
        Ok(())
    }

    fn step(&mut self, ev: Ev) -> Step {
        match ev {
            Ev::Datagram { peer, to_server, id, bytes } => self.arrive(peer, to_server, id, bytes),
            Ev::LinkTimer { peer, server, id } => self.link_timer(peer, server, id),
            Ev::Connect { worker, epoch } => self.connect(worker, epoch),
            Ev::Finish { worker, epoch, token } => self.finish(worker, epoch, token),
            Ev::Crash { worker } => self.crash(worker),
            Ev::ServerCrash => self.server_crash(),
            Ev::Cut { worker } => self.cut(worker),
            Ev::Cancel { worker, epoch, token } => {
                if self.workers[&worker].epoch == epoch
                    && let Some((job, _)) = self.workers[&worker].running.get(&token)
                    && let Some(j) = self.server.jobs.get(job)
                    && j.round == self.clients[&j.client].round
                    && self.clients[&j.client].up
                    && !self.clients[&j.client].round_ended
                    && self.clients[&j.client].outputs.len() < self.project.actions.len()
                {
                    self.send(Node::Client(j.client), Node::Server, Msg::Up(Up::Cancel { round: j.round }));
                }
                Ok(())
            }
            Ev::Evict => {
                self.evict();
                Ok(())
            }
            Ev::Round { client } => self.round(client),
            Ev::Edit { client } => {
                self.clients.get_mut(&client).expect("known").edit(ns(self.now));
                Ok(())
            }
            Ev::Checkout { client, round } => self.checkout(client, round),
            Ev::TestComplete { client, key, started_ms } => {
                self.test_complete(client, &key, started_ms);
                Ok(())
            }
            Ev::Sync { client } => self.sync(client),
            Ev::ClientCut { client } => {
                self.client_cut(client);
                Ok(())
            }
            Ev::Watch { client } => self.watch(client),
            Ev::ClientCrash { client } => {
                self.client_crash(client);
                Ok(())
            }
            Ev::Restart { client } => self.restart(client),
        }
    }

    fn arrive(&mut self, peer: Node, to_server: bool, id: u64, bytes: Vec<u8>) -> Step {
        let silence = self.silent_at(self.now);
        let Some(link) = self.connections.get_mut(&peer).filter(|link| link.id == id) else { return Ok(()) };
        // Drop datagrams in the affected direction. The transport's clock
        // drives retransmission, including for packets already in flight.
        if self.partitions.get(&(peer, to_server)).is_some_and(|window| window.contains(&self.now)) {
            self.report.partitioned += 1;
            return Ok(());
        }
        // The silence fault stalls an established client session. Let agreement run so that
        // the application-level watch can still observe silence on newly connected sessions.
        if let Some((_, end)) = silence
            && matches!(peer, Node::Client(_))
            && link.sides.iter().all(|side| side.as_ref().is_some_and(Side::agreed))
        {
            self.schedule(end - self.now, Ev::Datagram { peer, to_server, id, bytes });
            return Ok(());
        }
        let (from, to) = if to_server { (peer, Node::Server) } else { (Node::Server, peer) };
        let from_addr = addr(from);
        let endpoint = if to_server { &mut self.server_endpoint } else { &mut link.endpoint };
        let side = &mut link.sides[usize::from(to_server)];
        let mut effects = transport::Effects::default();
        for event in endpoint.recv(from_addr, &bytes, self.now).events {
            if matches!(event, RouteEvent::Open { .. }) {
                assert!(to_server && side.is_none());
                *side = Some(Side::new(id, false, self.now).map_err(link_error)?);
            }
            side.as_mut().expect("the endpoint opened the link")
                .route(endpoint, event, from_addr, self.now, &mut effects).map_err(link_error)?;
        }
        let acks = endpoint.take_datagrams();
        for (dest, ack) in acks {
            assert_eq!(dest, from_addr);
            self.transmit(peer, !to_server, id, ack);
        }
        if self.connections[&peer].sides[usize::from(to_server)].is_some() {
            self.wake_link(peer, to_server, self.now);
        }
        for bytes in effects.messages {
            let msg = rargo_proto::decode(&bytes).map_err(link_error)?;
            self.deliver(from, to, msg)?;
        }
        Ok(())
    }

    fn deliver(&mut self, from: Node, to: Node, msg: Msg) -> Step {
        let w = match (from, to) {
            (Node::Worker(w), _) | (_, Node::Worker(w)) => Some(w),
            _ => None,
        };
        if let Node::Client(c) = to {
            if self.clients[&c].dead {
                return Ok(());
            }
            let now = Duration::from_millis(self.now);
            self.clients.get_mut(&c).expect("known").watch.heard(now);
        }
        match (from, msg) {
            (_, Msg::Work(m)) => {
                let w = w.expect("work goes to a worker");
                let now = self.now;
                let node = self.workers.get_mut(&w).expect("known");
                let store = &node.store;
                let cancel = matches!(m, FromServer::Cancel { .. });
                let fx = node.sm.server(m, &|h| store.contains_key(h), now);
                if cancel && fx.iter().any(|e| matches!(e, worker_state::Effect::Abort { .. })) {
                    self.report.aborted_runs += 1;
                }
                self.worker_effects(w, fx)
            }
            (_, Msg::Down(d)) => {
                let Node::Client(c) = to else { unreachable!("down messages go to clients") };
                self.client_hears(c, d)
            }
            (Node::Worker(w), Msg::Up(u)) => self.server_from_worker(w, u),
            (Node::Client(c), Msg::Up(u)) => self.server_from_client(c, u),
            (Node::Server, Msg::Up(_)) => unreachable!("the server does not write to itself"),
        }
    }

    fn connect(&mut self, worker: u64, epoch: u64) -> Step {
        let now = self.now;
        let node = self.workers.get_mut(&worker).expect("known");
        if node.up || node.epoch != epoch {
            return Ok(());
        }
        let fx = if now < node.cut_until {
            node.sm.connect_failed(false)
        } else {
            node.up = true;
            node.epoch += 1;
            let fx = node.sm.connected();
            self.open_link(Node::Worker(worker))?;
            fx
        };
        self.worker_effects(worker, fx)
    }

    fn cut(&mut self, worker: u64) -> Step {
        let cut_for = self.rng.range(0, 3_000);
        let next = self.rng.range(200, 4_000);
        if self.now + next < self.cfg.fault_end_ms && self.rng.chance(500 + self.cfg.cut_per_mille * 10) {
            self.schedule(next, Ev::Cut { worker });
        }
        let now = self.now;
        let node = self.workers.get_mut(&worker).expect("known");
        if !node.up {
            return Ok(());
        }
        node.cut_until = now + cut_for;
        self.report.cuts += 1;
        node.up = false;
        node.epoch += 1;
        let fx = node.sm.link_lost();
        self.close_link(Node::Worker(worker));
        self.worker_effects(worker, fx)?;
        let lost = self.server.pool.left(worker);
        self.pool_effects(lost);
        Ok(())
    }

    fn server_crash(&mut self) -> Step {
        // Capture the invariant independently of the pin metadata the new server reads.
        let expected_pins = self.server.pins.clone();
        self.server.reopen(self.cfg.clients)?;
        for hash in expected_pins.keys() {
            if !self.server.store().read(hash).is_ok_and(|bytes| Hash::of(&bytes) == *hash) {
                return Err(Kind::LostPinned(format!("blob {hash} was lost after a server restart")));
            }
        }
        if self.server.pins != expected_pins {
            return Err(Kind::LostPinned("server restart changed the pin counts".into()));
        }
        self.report.server_restarts += 1;

        // Old datagrams and timers still in the queue carry connection IDs that
        // can never match the new links.
        self.connections.clear();
        self.last.clear();
        self.server_endpoint = EndpointState::new(true, false, self.link_key.clone());
        for worker in 1..=self.cfg.workers {
            let node = self.workers.get_mut(&worker).expect("known worker");
            if node.up {
                node.up = false;
                node.epoch += 1;
                let effects = node.sm.link_lost();
                self.worker_effects(worker, effects)?;
            }
        }
        for client in 1..=self.cfg.clients {
            let c = self.clients.get_mut(&client).expect("known client");
            if c.up {
                c.up = false;
                if !c.dead && !c.finished && c.outputs.len() < self.project.actions.len() {
                    self.schedule(1, Ev::Sync { client });
                }
            }
        }
        Ok(())
    }

    fn crash(&mut self, worker: u64) -> Step {
        let downtime = self.rng.range(1, 1_000);
        let node = self.workers.get_mut(&worker).expect("known");
        let (sm, mut fx) = Worker::new();
        let [worker_state::Effect::Connect { after_ms }] = fx.as_mut_slice() else {
            unreachable!("a new worker only connects");
        };
        // A fresh worker quits if its first connection fails. Wait out any link cut.
        *after_ms = downtime.max(node.cut_until.saturating_sub(self.now));
        node.sm = sm;
        node.running.clear();
        // A process crash drops in-flight work. The simulated worker disk survives.
        node.up = false;
        node.epoch += 1;
        self.report.worker_crashes += 1;
        self.close_link(Node::Worker(worker));

        let lost = self.server.pool.left(worker);
        self.report.jobs_lost_to_crashes += lost
            .iter()
            .filter(|effect| matches!(effect, pool_state::Effect::Lost { .. }))
            .count() as u64;
        self.pool_effects(lost);
        self.worker_effects(worker, fx)
    }

    fn worker_effects(&mut self, worker: u64, fx: Vec<worker_state::Effect<Sim>>) -> Step {
        let mut queue: VecDeque<worker_state::Effect<Sim>> = fx.into();
        while let Some(e) = queue.pop_front() {
            match e {
                worker_state::Effect::Connect { after_ms } => {
                    let jitter = self.rng.range(0, 50);
                    let epoch = self.workers[&worker].epoch;
                    self.schedule(after_ms + jitter, Ev::Connect { worker, epoch });
                }
                worker_state::Effect::Quit => {}
                worker_state::Effect::Hello => {
                    let cores = self.workers[&worker].cores;
                    self.send(Node::Worker(worker), Node::Server, Msg::Up(Up::Hello { cores }));
                }
                worker_state::Effect::Serve { .. } | worker_state::Effect::Refuse { .. } => {}
                worker_state::Effect::Assemble { blob, parts } => {
                    let bytes = parts.concat();
                    assert_eq!(Hash::of(&bytes), blob, "a pushed blob assembles to its hash");
                    let node = self.workers.get_mut(&worker).expect("known");
                    node.store.insert(blob, bytes);
                    queue.extend(node.sm.assembled(blob));
                }
                worker_state::Effect::Stored { .. } => {}
                worker_state::Effect::Start { token, action, run } => {
                    let node = self.workers.get_mut(&worker).expect("known");
                    if let Some(h) = run.inputs.iter().find(|h| !node.store.contains_key(h)) {
                        return Err(Kind::StartedWithoutInputs(format!(
                            "worker {worker} started job {action} without {}",
                            h.hex()
                        )));
                    }
                    let ms = run.ms;
                    node.running.insert(token, (action, run));
                    let epoch = node.epoch;
                    self.schedule(ms, Ev::Finish { worker, epoch, token });
                    if ms >= model::LONG_RUN_MS {
                        self.report.long_runs += 1;
                        if self.seed.is_multiple_of(3) {
                            self.schedule(ms / 2, Ev::Cancel { worker, epoch, token });
                        }
                    }
                }
                worker_state::Effect::Abort { token } => {
                    self.workers.get_mut(&worker).expect("known").running.remove(&token);
                }
                worker_state::Effect::Report { .. } => {}
            }
        }
        Ok(())
    }

    fn finish(&mut self, worker: u64, epoch: u64, token: u64) -> Step {
        let node = self.workers.get_mut(&worker).expect("known");
        if node.epoch != epoch {
            return Ok(());
        }
        let Some((job, run)) = node.running.remove(&token) else { return Ok(()) };
        let inputs: Vec<&[u8]> = run.inputs.iter().map(|h| node.store[h].as_slice()).collect();
        let output = model::compile(self.seed, self.project.actions[run.action].id, &inputs);
        let fx = node.sm.ended(token);
        self.report.runs += 1;
        for e in fx {
            if let worker_state::Effect::Report { action, .. } = e {
                assert_eq!(action, job);
                self.send(Node::Worker(worker), Node::Server, Msg::Up(Up::Result { job, output: output.clone() }));
            }
        }
        Ok(())
    }

    fn evict(&mut self) {
        if self.busy > 0 {
            let at = self.rng.range(20, 400);
            self.schedule_housekeeping(at, Ev::Evict);
        }
        let ignore_pins = self.bug == Some(Bug::EvictPinned);
        let ignore_claims = self.bug == Some(Bug::EvictClaimed);
        // Jobs can outlive the client round that pinned their inputs.
        let job_inputs: BTreeSet<Hash> = self.server.jobs.values().flat_map(|j| j.inputs.iter().copied()).collect();
        // An offer may rely on blobs already in the store without claiming them.
        let offered: BTreeSet<Hash> = self
            .server
            .sessions
            .values()
            .filter_map(|s| s.offer.as_ref())
            .flat_map(|o| o.contents.iter().copied())
            .collect();
        let candidates: Vec<Hash> = self
            .server
            .store()
            .all_hashes()
            .into_iter()
            .filter(|h| ignore_pins || (!self.server.pins.contains_key(h) && !job_inputs.contains(h)))
            .filter(|h| {
                if self.server.trees.holder(h).is_some() {
                    ignore_claims
                } else {
                    !offered.contains(h)
                }
            })
            .collect();
        if candidates.is_empty() {
            return;
        }
        let h = candidates[self.rng.below(candidates.len() as u64) as usize];
        self.server.store().remove(&h);
        self.report.evictions += 1;
        let active_clients = self
            .clients
            .values()
            .filter(|c| c.round > 0 && c.outputs.len() < self.project.actions.len())
            .count();
        self.report.clients_at_eviction = self.report.clients_at_eviction.max(active_clients);
        let worker = self.rng.range(1, self.cfg.workers);
        let mut busy: BTreeSet<Hash> = self
            .server
            .jobs
            .values()
            .filter(|j| j.worker == Some(worker))
            .flat_map(|j| j.inputs.iter().copied())
            .collect();
        let node = self.workers.get_mut(&worker).expect("known");
        busy.extend(node.running.values().flat_map(|(_, run)| run.inputs.iter().copied()));
        if let Some(h) = node.store.keys().find(|h| !busy.contains(h)).copied() {
            node.store.remove(&h);
        }
    }

    fn pin(&mut self, client: u64, h: Hash) {
        let held = &mut self.server.held.get_mut(&client).expect("a client holds a tree").1;
        if held.insert(h) {
            *self.server.pins.entry(h).or_default() += 1;
        }
    }

    fn server_from_client(&mut self, client: u64, u: Up) -> Step {
        if let Up::Offer { round, .. } | Up::Request { round, .. } = &u
            && let Some((failed, message)) = self.server.disk_errors.get(&client)
            && round == failed
        {
            self.send(Node::Server, Node::Client(client), Msg::Down(Down::DiskError { round: *round, message: message.clone() }));
            return Ok(());
        }
        match u {
            Up::Ping => self.send(Node::Server, Node::Client(client), Msg::Down(Down::Pong)),
            Up::Join => {
                self.drop_session(client);
                let mut sess = Sess { state: SessionState::new(), has: ClientSet::default(), offer: None, ended: None };
                sess.state.knows(&mut sess.has, std::iter::empty());
                self.server.sessions.insert(client, sess);
            }
            Up::Offer { round, root, contents, sending } => {
                let contents: Vec<Hash> = contents.into_iter().map(Hash).collect();
                let sending: BTreeSet<Hash> = sending.into_iter().map(Hash).collect();
                let server = &mut self.server;
                let sess = server.sessions.get_mut(&client).expect("a client joins before it offers");
                if let Some(old) = sess.offer.take() {
                    server.trees.release(old.up);
                }
                let up = server.trees.begin_upload();
                let store = server.store.as_ref().expect("the server has started");
                let claimed = server.trees.claim(up, &contents, &|h| store.has(h));
                let want: Vec<Digest> = claimed.mine.iter().filter(|h| !sending.contains(h)).map(|h| h.0).collect();
                sess.offer = Some(Offer { round, root, contents, up, arrived: BTreeSet::new() });
                if !want.is_empty() {
                    self.report.wants += 1;
                    self.send(Node::Server, Node::Client(client), Msg::Down(Down::Session(ServerMsg::WantBlobs(want))));
                }
                self.try_finish_offer(client);
            }
            Up::Cancel { round } => self.end_server_round(client, round, model::CANCELLED, None),
            Up::Session(m) => {
                let server = &mut self.server;
                let sess = server.sessions.get_mut(&client).expect("a client joins before it uploads");
                let step = sess.state.client(m);
                if let Some(refused) = step.refused {
                    return Err(Kind::StateMachine(format!("the server refused client {client}: {refused}")));
                }
                let mut wake = false;
                for effect in step.effects {
                    match effect {
                        session_state::Effect::Store { hash, bytes } => {
                            let h = Hash(hash);
                            server.store.as_ref().unwrap().put_bytes(&bytes).map_err(storage_error)?;
                            sess.has.extend([hash]);
                            if let Some(o) = &mut sess.offer
                                && o.contents.contains(&h)
                            {
                                o.arrived.insert(h);
                            }
                            self.report.uploads += 1;
                        }
                        session_state::Effect::Wake => wake = true,
                        other => return Err(Kind::StateMachine(format!("an upload of whole blobs had the effect {other:?}"))),
                    }
                }
                if wake {
                    for client in self.pending_offers() {
                        self.try_finish_offer(client);
                    }
                }
            }
            Up::Request { round, action, inputs } => {
                if self.server.sessions[&client].ended == Some(round) {
                    return Ok(());
                }
                if self.bug == Some(Bug::NeverAnswer) && self.rng.chance(40) {
                    return Ok(());
                }
                if let Some(h) = inputs.iter().find(|h| !self.server.store().has(h)) {
                    return Err(Kind::LostPinned(format!("input {} of a request is gone", h.hex())));
                }
                let id = self.project.actions[action].id;
                let key = if self.bug == Some(Bug::StaleDelivery) && inputs.len() > 1 {
                    model::key(id, &inputs[..inputs.len() - 1])
                } else {
                    model::key(id, &inputs)
                };
                if let Some(out) = disk::cached(self.server.store(), key)
                    && let Ok(bytes) = self.server.store().read(&out)
                {
                    self.report.cache_hits += 1;
                    return self.deliver_output(client, round, action, out, bytes);
                }
                let job = self.server.next_job;
                self.server.next_job += 1;
                let mem_kb = self.project.actions[action].mem_kb;
                self.server.jobs.insert(job, Job { client, round, action, inputs, key, worker: None });
                let fx = self.server.sched.request(job, Priority::Agent, mem_kb);
                self.sched_effects(fx);
            }
            Up::Hello { .. } | Up::Result { .. } => unreachable!("only workers say this"),
        }
        Ok(())
    }

    fn drop_session(&mut self, client: u64) {
        let Some(sess) = self.server.sessions.remove(&client) else { return };
        let Some(o) = sess.offer else { return };
        self.server.trees.release(o.up);
        for other in self.pending_offers() {
            self.rewant(other);
        }
    }

    fn cancel_jobs(&mut self, client: u64, round: u64) {
        let jobs: Vec<_> = self
            .server
            .jobs
            .iter()
            .filter(|(_, j)| j.client == client && j.round == round)
            .map(|(&job, _)| job)
            .collect();
        for job in jobs {
            self.server.ready.retain(|&ready| ready != job);
            let j = self.server.jobs.remove(&job).expect("selected job");
            let fx = self.server.pool.finished(job, false);
            self.pool_effects(fx);
            if let Some(worker) = j.worker {
                let fx = self.server.pool.give_back(worker);
                self.pool_effects(fx);
            }
            let fx = if self.server.sched.is_running(job) {
                self.server.sched.release(job)
            } else {
                self.server.sched.cancel(job)
            };
            self.sched_effects(fx);
        }
    }

    fn end_server_round(&mut self, client: u64, round: u64, code: i32, diagnostic: Option<Vec<u8>>) {
        if self.server.held.get(&client).is_none_or(|(held, _)| *held != round) {
            return;
        }
        let Some(sess) = self.server.sessions.get_mut(&client) else { return };
        if sess.ended == Some(round) {
            return;
        }
        sess.ended = Some(round);
        self.cancel_jobs(client, round);
        if let Some(bytes) = diagnostic {
            self.send(Node::Server, Node::Client(client), Msg::Down(Down::RoundTerminal { round, msg: ServerMsg::Stderr(bytes) }));
        }
        self.send(Node::Server, Node::Client(client), Msg::Down(Down::RoundTerminal { round, msg: ServerMsg::Exit(code) }));
    }

    fn pending_offers(&self) -> Vec<u64> {
        self.server.sessions.iter().filter(|(_, s)| s.offer.is_some()).map(|(c, _)| *c).collect()
    }

    fn rewant(&mut self, client: u64) {
        let server = &mut self.server;
        let Some(o) = server.sessions.get(&client).and_then(|s| s.offer.as_ref()) else { return };
        let store = server.store.as_ref().expect("the server has started");
        let orphans: Vec<Hash> =
            o.contents.iter().filter(|h| !store.has(h) && server.trees.holder(h).is_none()).copied().collect();
        if orphans.is_empty() {
            return;
        }
        let claimed = server.trees.claim(o.up, &orphans, &|h| store.has(h));
        let want: Vec<Digest> = claimed.mine.iter().map(|h| h.0).collect();
        self.report.wants += 1;
        self.send(Node::Server, Node::Client(client), Msg::Down(Down::Session(ServerMsg::WantBlobs(want))));
    }

    fn try_finish_offer(&mut self, client: u64) {
        let server = &mut self.server;
        let Some(sess) = server.sessions.get_mut(&client) else { return };
        let Some(o) = &sess.offer else { return };
        if !o.contents.iter().all(|h| server.store.as_ref().unwrap().has(h)) {
            return;
        }
        let o = sess.offer.take().expect("checked above");
        let same_round = server.held.get(&client).is_some_and(|(r, _)| *r == o.round);
        let old = if same_round { None } else { server.held.insert(client, (o.round, BTreeSet::new())) };
        for h in old.map(|(_, s)| s).unwrap_or_default() {
            let n = server.pins.get_mut(&h).expect("a held blob is pinned");
            *n -= 1;
            if *n == 0 {
                server.pins.remove(&h);
            }
        }
        for h in &o.contents {
            self.pin(client, *h);
        }
        self.server.save_held(client);
        self.server.trees.release(o.up);
        self.send(Node::Server, Node::Client(client), Msg::Down(Down::Synced { round: o.round, root: o.root }));
    }

    fn deliver_output(&mut self, client: u64, round: u64, action: usize, out: Hash, bytes: Vec<u8>) -> Step {
        if self.server.held.get(&client).is_some_and(|(r, _)| *r == round) {
            self.pin(client, out);
            self.server.save_held(client);
        }
        let server = &mut self.server;
        let Some(sess) = server.sessions.get_mut(&client) else { return Ok(()) };
        let file = Delivery { path: format!("{round}/{action}"), hash: out.0, size: bytes.len() as u64, exec: false, own: true };
        let Some(mut outgoing): Option<Outgoing> = sess.state.deliver(&mut sess.has, vec![file], Vec::new(), Vec::new()) else {
            return Ok(());
        };
        let mut msgs = Vec::new();
        loop {
            match outgoing.step() {
                Next::Read { hash, at, len, .. } => {
                    let read = server.store.as_ref().unwrap().read(&Hash(hash)).ok().map(|v| {
                        let at = (at as usize).min(v.len());
                        v[at..(at + len).min(v.len())].to_vec()
                    });
                    outgoing.read(read);
                }
                Next::Send(m) | Next::Bulk(m) => msgs.push(m),
                Next::Unreadable(h) => {
                    return Err(Kind::LostPinned(format!("output {} being delivered is gone", Hash(h).hex())));
                }
                Next::Gone | Next::Done => break,
            }
        }
        for m in msgs {
            self.send(Node::Server, Node::Client(client), Msg::Down(Down::Session(m)));
        }
        Ok(())
    }

    fn server_from_worker(&mut self, worker: u64, u: Up) -> Step {
        match u {
            Up::Hello { cores } => {
                let fx = self.server.pool.joined(worker, cores);
                self.pool_effects(fx);
            }
            Up::Result { job, output } => {
                if !self.server.pool.route(worker, job) {
                    return Ok(());
                }
                let fx = self.server.pool.finished(job, true);
                self.pool_effects(fx);
                let fx = self.server.pool.give_back(worker);
                self.pool_effects(fx);
                let j = self.server.jobs.remove(&job).expect("a routed job is known");
                let fx = self.server.sched.release(job);
                self.sched_effects(fx);
                let output = match output {
                    Ok(output) => output,
                    Err(diagnostic) => {
                        self.end_server_round(j.client, j.round, model::FAILED, Some(diagnostic));
                        return Ok(());
                    }
                };
                let out = Hash::of(&output);
                let fault = if !self.server.pins.is_empty()
                    && !self.server.store().has(&out)
                    && self.clients.get(&j.client).is_some_and(|c| !c.finished && c.round == j.round)
                {
                    self.disk_fault.take()
                } else {
                    None
                };
                match disk::put(&mut self.server.store, &output, fault) {
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::StorageFull => {
                        let message = rargo_core::serve::failure("keeping the build output", &error.into());
                        self.server.disk_errors.insert(j.client, (j.round, message.clone()));
                        // A job can finish after its client loses the link. The recorded
                        // error is returned when that client offers the round again.
                        if self.server.sessions.contains_key(&j.client) {
                            self.send(Node::Server, Node::Client(j.client), Msg::Down(Down::DiskError { round: j.round, message }));
                        }
                        return Ok(());
                    }
                    Err(error) => return Err(storage_error(error)),
                }
                if matches!(fault, Some(disk::Fault::Torn | disk::Fault::Crash)) {
                    self.report.disk_crashes += 1;
                    self.report.torn_writes += u64::from(fault == Some(disk::Fault::Torn));
                    // Reopen and read the pinned files, checking bytes as well as index entries.
                    for h in self.server.pins.keys() {
                        if !self.server.store().read(h).is_ok_and(|bytes| Hash::of(&bytes) == *h) {
                            return Err(Kind::LostPinned(format!("blob {h} was lost after a disk crash")));
                        }
                    }
                }
                disk::cache(self.server.store(), j.key, out);
                self.deliver_output(j.client, j.round, j.action, out, output)?;
            }
            Up::Join | Up::Offer { .. } | Up::Session(_) | Up::Request { .. } | Up::Cancel { .. } | Up::Ping => unreachable!("only clients say this"),
        }
        Ok(())
    }

    fn pool_effects(&mut self, fx: Vec<pool_state::Effect>) {
        let mut wake = false;
        for e in fx {
            match e {
                pool_state::Effect::Wake => wake = true,
                pool_state::Effect::Lost { action: job } => {
                    let j = self.server.jobs.get_mut(&job).expect("a lost job is known");
                    j.worker = None;
                    let mem_kb = self.project.actions[j.action].mem_kb;
                    let fx = self.server.sched.release(job);
                    self.sched_effects(fx);
                    let fx = self.server.sched.request(job, Priority::Agent, mem_kb);
                    self.sched_effects(fx);
                }
                pool_state::Effect::Cancel { worker, action } => {
                    self.send(Node::Server, Node::Worker(worker), Msg::Work(FromServer::Cancel { action }));
                }
            }
        }
        if wake {
            self.dispatch();
        }
    }

    fn sched_effects(&mut self, fx: Effects) {
        self.server.pipe += fx.put_tokens;
        if fx.reconcile {
            let r = self.server.sched.reconcile(self.server.pipe);
            self.server.pipe += r.put;
            self.server.pipe -= r.take.min(self.server.pipe);
        }
        let granted = !fx.grant.is_empty();
        self.server.ready.extend(fx.grant);
        if granted {
            self.dispatch();
        }
        while self.server.sched.tick(self.now, self.server.pipe) == Broker::Read && self.server.pipe > 0 {
            self.server.pipe -= 1;
            let fx = self.server.sched.token_in(1);
            self.sched_effects(fx);
        }
    }

    fn dispatch(&mut self) {
        while let Some(&job) = self.server.ready.front() {
            let Some(worker) = self.server.pool.lease(None) else { return };
            self.server.ready.pop_front();
            assert!(self.server.pool.sent(job, worker), "a leased worker is here");
            let j = self.server.jobs.get_mut(&job).expect("a granted job is known");
            j.worker = Some(worker);
            let a = &self.project.actions[j.action];
            let ms = model::duration(self.seed, model::key(a.id, &j.inputs), a.ms);
            let run = SimRun { action: j.action, inputs: j.inputs.clone(), ms };
            let pushes: Vec<_> = run.inputs.iter().map(|h| {
                (*h, self.server.store().read(h).expect("a pinned job input is readable"))
            }).collect();
            let msg = FromServer::Run(worker_state::Run {
                action: job,
                incoming: run.inputs.clone(),
                prefill: Vec::new(),
                listings: Vec::new(),
                closed: Vec::new(),
                run,
            });
            self.send(Node::Server, Node::Worker(worker), Msg::Work(msg));
            for (blob, bytes) in pushes {
                let push = FromServer::Push { blob, first: true, of: 1, part: bytes };
                self.send(Node::Server, Node::Worker(worker), Msg::Work(push));
            }
        }
    }

    fn round(&mut self, client: u64) -> Step {
        let c = self.clients.get_mut(&client).expect("known");
        c.rounds_left -= 1;
        c.test = TestRun::generate(self.seed, client, self.cfg.rounds - c.rounds_left);
        self.build_snapshot(client)
    }

    fn checkout(&mut self, client: u64, round: u64) -> Step {
        let c = self.clients.get_mut(&client).expect("known");
        if c.round_ended || c.round != round || c.outputs.len() == self.project.actions.len() {
            return Ok(());
        }
        // Versions 0 and 1 are used by single-file edits on the original branch.
        let mut rng = Rng::new(c.checkout_seed.take().expect("checkout scheduled once"));
        c.tree = (0..c.tree.len()).map(|index| Some(model::source(index, rng.range(2, 16)))).collect();
        for index in 0..c.tree.len() {
            c.put(index, ns(self.now));
        }
        self.build_snapshot(client)
    }

    fn build_snapshot(&mut self, client: u64) -> Step {
        let c = self.clients.get_mut(&client).expect("known");
        // Old jobs keep their round ID even if they finish after the checkout.
        c.round += 1;
        if self.bug == Some(Bug::DeafToEdits) {
            c.worktree.forget_changes();
        }
        let scan = c
            .worktree
            .scan(&c.target, c.last_scan.take(), &c.index, ns(self.now))
            .map_err(|e| Kind::StateMachine(format!("client {client} could not scan its worktree: {e}")))?;
        if scan.index_changed {
            c.index = scan.index.clone();
        }
        // The build reads what the scan saw, and must come out as a clean build of the tree.
        let present = c.tree.iter().flatten().count() as u64;
        if scan.file_count != present + worktree::OTHER_FILES {
            return Err(Kind::Drift(format!(
                "client {client}'s scan saw {} files where its worktree has {present} sources and {} other files",
                scan.file_count,
                worktree::OTHER_FILES
            )));
        }
        let snapshot: Vec<Option<Vec<u8>>> = (0..c.tree.len())
            .map(|index| {
                let entry = scan.index.files.get(&worktree::path(index))?;
                let bytes = c.worktree.read(&worktree::path(index)).filter(|b| Hash::of(b).0 == entry.hash);
                Some(bytes.map_or_else(|| format!("stale {}", Hash(entry.hash).hex()).into_bytes(), <[u8]>::to_vec))
            })
            .collect();
        c.snapshot = snapshot;
        c.scan_root = scan.root;
        c.last_scan = Some(scan);
        c.reference = self.project.clean_build(&c.tree);
        c.edit_scheduled = false;
        c.synced_round = 0;
        c.outputs.clear();
        c.round_ended = false;
        c.diagnostic = None;
        c.requested.clear();
        c.pending.clear();
        c.daemon.changed(1, false);
        self.sync(client)
    }

    fn sync(&mut self, client: u64) -> Step {
        let now = self.now;
        let c = self.clients.get_mut(&client).expect("known");
        if c.round_ended || c.finished || c.dead || c.round == 0 {
            return Ok(());
        }
        match c.daemon.link(c.up, Need::Must) {
            Link::Use => {}
            Link::Connect => {
                if now < c.cut_until {
                    let at = c.cut_until - now;
                    self.schedule(at, Ev::Sync { client });
                    return Ok(());
                }
                c.daemon.connected();
                c.up = true;
                c.epoch += 1;
                c.watch = Watch::new(Duration::from_millis(now), self.watch_limit);
                c.up_since = now;
                let outputs = &c.outputs;
                c.requested.retain(|a| outputs.contains_key(a));
                c.synced_round = 0;
                c.pending.clear();
                c.parts.clear();
                self.open_link(Node::Client(client))?;
                self.send(Node::Client(client), Node::Server, Msg::Up(Up::Join));
            }
            Link::Skip => unreachable!("a sync needs the link"),
        }
        let c = self.clients.get_mut(&client).expect("known");
        let _ = c.daemon.plan_scan(1, None, true);
        c.daemon.scanned(1, None);
        let blobs = c.snapshot.iter().flatten();
        let contents: Vec<Digest> = blobs.clone().map(|b| Hash::of(b).0).collect();
        for (d, b) in contents.iter().zip(blobs) {
            c.cache.insert(*d, b.clone());
        }
        let root = c.scan_root;
        let (_, lacking) = c.daemon.offer(Some(1), &root, std::iter::empty(), contents.iter().map(|d| (d, ())));
        let sending: Vec<Digest> = lacking.into_iter().map(|(d, ())| d).collect();
        let upload: Vec<Blob> = sending.iter().map(|d| Blob::pack(*d, &c.cache[d])).collect();
        c.contents = contents.clone();
        let round = c.round;
        self.send(Node::Client(client), Node::Server, Msg::Up(Up::Offer { round, root, contents, sending }));
        for b in upload {
            self.send(Node::Client(client), Node::Server, Msg::Up(Up::Session(ClientMsg::Blobs(vec![b]))));
        }
        Ok(())
    }

    fn client_cut(&mut self, client: u64) {
        let now = self.now;
        let until = now + self.rng.range(50, 2_000);
        let c = self.clients.get_mut(&client).expect("known");
        if !c.up {
            return;
        }
        c.up = false;
        c.cut_until = until;
        let finished = c.finished;
        self.report.client_cuts += 1;
        self.close_link(Node::Client(client));
        self.drop_session(client);
        if !finished {
            self.schedule(until - now, Ev::Sync { client });
        }
    }

    /// The client's daemon dies: its link closes, and everything it held in
    /// memory is gone. Only what it last saved of its knowledge survives.
    fn client_crash(&mut self, client: u64) {
        let c = self.clients.get_mut(&client).expect("known");
        if c.finished || c.dead {
            return;
        }
        c.dead = true;
        // The new daemon scans all of the worktree again, with the index the old one saved.
        c.last_scan = None;
        self.report.crashes += 1;
        if self.bug != Some(Bug::TrustAfterCrash) {
            c.up = false;
            // Its sockets die with it; the new daemon opens a link of its own.
            self.close_link(Node::Client(client));
        }
        self.drop_session(client);
        let at = self.rng.range(10, 1_500);
        self.schedule(at, Ev::Restart { client });
    }

    /// The client side finds its daemon gone, starts a new one and runs the
    /// command it was running again from the start. The new daemon rebuilds
    /// what it knows of the server: it reads the knowledge last saved, which
    /// the server may have evicted since, and makes a new link and session.
    fn restart(&mut self, client: u64) -> Step {
        let lost_save = self.rng.chance(300);
        let c = self.clients.get_mut(&client).expect("known");
        c.dead = false;
        if self.bug != Some(Bug::TrustAfterCrash) {
            let held = if lost_save { None } else { KnownDigests::decode(&c.saved) };
            c.daemon = new_daemon(held.unwrap_or_default());
            c.cache.clear();
        }
        if c.outputs.len() == self.project.actions.len() {
            // It died between commands: the next one starts the new daemon's link.
            return Ok(());
        }
        c.outputs.clear();
        c.requested.clear();
        c.pending.clear();
        c.parts.clear();
        c.synced_round = 0;
        self.sync(client)
    }

    fn watch(&mut self, client: u64) -> Step {
        let now = self.now;
        let max_latency = self.cfg.max_latency_ms;
        let silence = self.silence;
        let bug = self.bug;
        if self.clients[&client].finished {
            return Ok(());
        }
        self.schedule_housekeeping(WATCH_STEP_MS, Ev::Watch { client });
        let c = self.clients.get_mut(&client).expect("known");
        if !c.up || c.dead {
            return Ok(());
        }
        match c.watch.tick(Duration::from_millis(now)) {
            // Server restarts close the link directly. The silence fault keeps it open.
            Tick::Wait | Tick::Probe => {}
            Tick::Ping(_) => self.send(Node::Client(client), Node::Server, Msg::Up(Up::Ping)),
            Tick::Silent(_) if bug == Some(Bug::DeafToSilence) => {}
            Tick::Silent(quiet) => {
                let heard = now - quiet.as_millis() as u64;
                let slack = c.watch.ping_every().as_millis() as u64 + 2 * max_latency + 2 * WATCH_STEP_MS;
                let silent_for = silence.map_or(0, |(start, end)| end.min(now).saturating_sub(start.max(heard)));
                if silent_for + slack < self.watch_limit.as_millis() as u64 {
                    return Err(Kind::Silence(format!(
                        "client {client} gave up on its server at {now} ms after {} ms without a word, but the server was silent for only {silent_for} ms of it",
                        quiet.as_millis()
                    )));
                }
                c.up = false;
                self.report.silences += 1;
                self.close_link(Node::Client(client));
                self.drop_session(client);
                self.schedule(1_000, Ev::Sync { client });
                return Ok(());
            }
        }
        let c = &self.clients[&client];
        if let Some((start, end)) = silence {
            let from = start.max(c.up_since);
            let bound = from + self.watch_limit.as_millis() as u64 + max_latency + 2 * WATCH_STEP_MS;
            if c.up && now < end && now > bound {
                return Err(Kind::Silence(format!(
                    "client {client} still waits at {now} ms on a server silent since {start} ms, past its bound of {bound} ms"
                )));
            }
        }
        Ok(())
    }

    fn request_ready(&mut self, client: u64) {
        let c = &self.clients[&client];
        if c.round_ended || c.synced_round != c.round {
            return;
        }
        let ready = self.project.ready(&|a| c.outputs.contains_key(&a), &|a| c.requested.contains(&a));
        if !ready.is_empty() && !c.edit_scheduled {
            let round = c.round;
            let checkout = c.checkout_seed.is_some();
            self.clients.get_mut(&client).expect("known").edit_scheduled = true;
            self.schedule(1, Ev::Edit { client });
            if checkout {
                self.schedule(1, Ev::Checkout { client, round });
            }
        }
        for action in ready {
            let c = &self.clients[&client];
            let spec = &self.project.actions[action];
            let sources = spec.sources.iter().filter_map(|&s| c.snapshot[s].as_deref());
            let deps = spec.deps.iter().map(|d| c.outputs[d].as_slice());
            let inputs = sources.chain(deps).map(Hash::of).collect();
            let round = c.round;
            self.clients.get_mut(&client).expect("known").requested.insert(action);
            self.send(Node::Client(client), Node::Server, Msg::Up(Up::Request { round, action, inputs }));
        }
    }

    fn client_hears(&mut self, client: u64, d: Down) -> Step {
        if self.clients[&client].finished {
            return Ok(());
        }
        let m = match d {
            Down::Synced { round, root } => {
                let c = self.clients.get_mut(&client).expect("known");
                if round != c.round {
                    return Ok(());
                }
                let contents = c.contents.clone();
                c.daemon.synced(Some(1), root, contents.iter());
                c.save();
                c.synced_round = round;
                self.request_ready(client);
                return Ok(());
            }
            Down::RoundTerminal { round, msg } => {
                let c = &self.clients[&client];
                if round != c.round || c.outputs.len() == self.project.actions.len() {
                    return Ok(());
                }
                msg
            }
            Down::Session(m) => m,
            Down::Pong => return Ok(()),
            Down::DiskError { round, message } => {
                let c = self.clients.get_mut(&client).expect("known");
                if c.round == round && !c.finished {
                    assert_eq!(message, rargo_core::serve::OUT_OF_SPACE);
                    c.finished = true;
                    self.report.disk_full_errors += 1;
                }
                return Ok(());
            }
        };
        let c = self.clients.get_mut(&client).expect("known");
        if c.round_ended {
            return Ok(());
        }
        match m {
            ServerMsg::Stderr(bytes) => c.diagnostic = Some(bytes),
            ServerMsg::Exit(code @ (model::FAILED | model::CANCELLED)) => {
                if code == model::FAILED {
                    let c = &self.clients[&client];
                    let expected = c
                        .requested
                        .iter()
                        .filter(|a| !c.outputs.contains_key(a))
                        .filter_map(|&a| self.compiler_error(c, a))
                        .any(|diagnostic| c.diagnostic.as_ref() == Some(&diagnostic));
                    if !expected {
                        return Err(Kind::Drift(format!("client {client} received an unexpected compiler diagnostic")));
                    }
                    self.report.failed_builds += 1;
                } else {
                    self.report.cancelled_builds += 1;
                    self.report.cancelled_with_outputs += u64::from(!c.outputs.is_empty());
                }
                let c = self.clients.get_mut(&client).expect("known");
                c.round_ended = true;
                c.outputs.clear();
                c.pending.clear();
                c.parts.clear();
                c.test = None;
                self.complete_round(client);
            }
            ServerMsg::WantBlobs(want) => {
                if self.bug == Some(Bug::StaleKnowledge) {
                    return Ok(());
                }
                c.daemon.missing(want.iter());
                c.save();
                let upload: Vec<Blob> = want.iter().filter_map(|d| c.cache.get(d).map(|b| Blob::pack(*d, b))).collect();
                if upload.len() != want.len() {
                    return Err(Kind::StateMachine(format!("the server wants content client {client} never had")));
                }
                for b in upload {
                    self.send(Node::Client(client), Node::Server, Msg::Up(Up::Session(ClientMsg::Blobs(vec![b]))));
                }
            }
            ServerMsg::Deliver { id, files, .. } => {
                c.pending.insert(id, files);
            }
            ServerMsg::Blobs(blobs) => {
                for b in blobs {
                    let hash = b.hash;
                    let bytes = b.unpack().map_err(|_| Kind::Drift(format!("client {client} got a blob that does not unpack")))?;
                    if Hash::of(&bytes).0 != hash {
                        return Err(Kind::Drift(format!("client {client} got a blob that does not match its hash")));
                    }
                    c.cache.insert(hash, bytes);
                }
            }
            ServerMsg::BlobPart { hash, part, last } => {
                let data = part.unpack().map_err(|_| Kind::Drift(format!("client {client} got a part that does not unpack")))?;
                c.parts.entry(hash).or_default().extend(data);
                if last {
                    let bytes = c.parts.remove(&hash).unwrap_or_default();
                    if Hash::of(&bytes).0 != hash {
                        return Err(Kind::Drift(format!("client {client} put together a blob that does not match its hash")));
                    }
                    c.cache.insert(hash, bytes);
                }
            }
            ServerMsg::Delivered { id } => {
                let Some(files) = c.pending.remove(&id) else { return Ok(()) };
                for f in files {
                    let (round, action) = f.path.split_once('/').expect("a delivered path is round/action");
                    let (round, action): (u64, usize) = (round.parse().expect("a round"), action.parse().expect("an action"));
                    let Some(bytes) = self.clients[&client].cache.get(&f.hash).cloned() else {
                        return Err(Kind::Drift(format!("client {client} was told {} is delivered without its content", f.path)));
                    };
                    self.client_done(client, round, action, bytes)?;
                }
            }
            other => return Err(Kind::StateMachine(format!("client {client} did not expect {other:?}"))),
        }
        Ok(())
    }

    fn client_done(&mut self, client: u64, round: u64, action: usize, output: Vec<u8>) -> Step {
        let c = &self.clients[&client];
        if c.round_ended || round != c.round || c.outputs.contains_key(&action) || !c.requested.contains(&action) {
            return Ok(());
        }
        let want = &c.reference[action];
        if self.compiler_error(c, action).is_some() {
            return Err(Kind::Drift(format!(
                "client {client} round {round} got output for action {} that should fail",
                self.project.actions[action].id
            )));
        }
        if &output != want {
            return Err(Kind::Drift(format!(
                "client {client} round {round} got {:?} for action {} where its tree gives {:?}",
                String::from_utf8_lossy(&output[..output.len().min(40)]),
                self.project.actions[action].id,
                String::from_utf8_lossy(&want[..want.len().min(40)])
            )));
        }
        let c = self.clients.get_mut(&client).expect("known");
        c.outputs.insert(action, output);
        if c.outputs.len() == self.project.actions.len() {
            self.publish_target(client)?;
            let c = self.clients.get_mut(&client).expect("known");
            if let Some(test) = &mut c.test {
                let scheduled = test.built();
                self.schedule_tests(client, scheduled);
            } else {
                self.complete_round(client);
            }
            return Ok(());
        }
        self.request_ready(client);
        Ok(())
    }

    fn compiler_error(&self, c: &Client, action: usize) -> Option<Vec<u8>> {
        let spec = &self.project.actions[action];
        let sources = spec.sources.iter().filter_map(|&s| c.snapshot[s].as_deref());
        let deps = spec.deps.iter().map(|&d| c.reference[d].as_slice());
        let inputs: Vec<_> = sources.chain(deps).collect();
        model::compile(self.seed, spec.id, &inputs).err()
    }

    fn publish_target(&mut self, client: u64) -> Step {
        let c = self.clients.get_mut(&client).expect("known");
        if self.bug != Some(Bug::UnshownOutputs) {
            let edits = c.outputs.iter().map(|(&action, output)| {
                let file = VFile { hash: Hash::of(output).0, size: output.len() as u64, exec: false, mtime_ns: ns(self.now) };
                let path = worktree::target_path(self.project.actions[action].id);
                Edit::Put { path, file, replaced_real: false }
            }).collect();
            c.target.apply(c.target.generation + 1, edits);
            c.target.invariants();
        }
        self.check_target(client)
    }

    /// What the round left in the client's `target/` is a clean build of its tree, and nothing else.
    fn check_target(&self, client: u64) -> Step {
        let c = &self.clients[&client];
        let want: BTreeMap<Vec<u8>, Digest> = self
            .project
            .actions
            .iter()
            .zip(&c.reference)
            .map(|(a, out)| (worktree::target_path(a.id), Hash::of(out).0))
            .collect();
        let got: BTreeMap<Vec<u8>, Digest> = c.target.files.iter().map(|(p, f)| (p.clone(), f.hash)).collect();
        if got != want {
            let wrong = want.iter().filter(|(p, h)| got.get(*p) != Some(h)).count() + got.keys().filter(|p| !want.contains_key(*p)).count();
            return Err(Kind::Drift(format!(
                "client {client} round {}: {wrong} files in target/ differ from a clean build of its tree",
                c.round
            )));
        }
        Ok(())
    }

    fn schedule_tests(&mut self, client: u64, scheduled: Vec<(String, u64)>) {
        for (key, delay) in scheduled {
            self.schedule(delay, Ev::TestComplete { client, key, started_ms: self.now });
        }
    }

    fn test_complete(&mut self, client: u64, key: &str, started_ms: u64) {
        let test = self.clients.get_mut(&client).expect("known").test.as_mut().expect("a test command");
        let (outcome, scheduled) = test.completed(key, self.now - started_ms);
        match outcome {
            Some(Outcome::Passed) => self.report.tests_passed += 1,
            Some(Outcome::Failed) => self.report.tests_failed += 1,
            Some(Outcome::TimedOut) => self.report.tests_timed_out += 1,
            None => {}
        }
        if test.machine.phase() == Phase::Done {
            self.complete_round(client);
        } else {
            self.schedule_tests(client, scheduled);
        }
    }

    fn complete_round(&mut self, client: u64) {
        let c = self.clients.get_mut(&client).expect("known");
        if c.rounds_left == 0 {
            c.finished = true;
        } else {
            self.schedule(0, Ev::Edit { client });
            let at = self.rng.range(0, 1_000);
            self.schedule(at, Ev::Round { client });
        }
    }
}

impl Client {
    /// The daemon writes its knowledge of the server to disk, as the real one
    /// does, so a daemon started after a crash reads it back.
    fn save(&mut self) {
        if let Some(bytes) = self.daemon.held.take_changes() {
            self.saved = bytes;
        }
    }
}

fn new_daemon(held: KnownDigests) -> DaemonState {
    let mut daemon = DaemonState::new(held);
    daemon.added(1);
    daemon.watched(1, true);
    daemon
}

fn label(ev: &Ev) -> String {
    match ev {
        Ev::Datagram { peer, to_server, id, bytes } => {
            format!("datagram {peer:?}/{to_server}@{id} [{}];", bytes.len())
        }
        Ev::LinkTimer { peer, server, id } => format!("link timer {peer:?}/{server}@{id};"),
        Ev::Connect { worker, epoch } => format!("connect {worker}@{epoch};"),
        Ev::Finish { worker, epoch, token } => format!("finish {worker}@{epoch}/{token};"),
        Ev::Crash { worker } => format!("crash {worker};"),
        Ev::ServerCrash => "server crash and restart;".to_string(),
        Ev::Cut { worker } => format!("cut {worker};"),
        Ev::Cancel { worker, epoch, token } => format!("cancel {worker}@{epoch}/{token};"),
        Ev::Evict => "evict;".to_string(),
        Ev::Round { client } => format!("round {client};"),
        Ev::Edit { client } => format!("edit {client};"),
        Ev::Checkout { client, round } => format!("checkout {client} round {round};"),
        Ev::TestComplete { client, key, .. } => format!("test result {client}/{key};"),
        Ev::Sync { client } => format!("sync {client};"),
        Ev::ClientCut { client } => format!("client cut {client};"),
        Ev::Watch { client } => format!("watch {client};"),
        Ev::ClientCrash { client } => format!("crash {client};"),
        Ev::Restart { client } => format!("restart {client};"),
    }
}

pub fn run(seed: u64, bug: Option<Bug>) -> Result<Report, Failure> {
    run_off(seed, bug, Off::none())
}

pub fn run_off(seed: u64, bug: Option<Bug>, off: Off) -> Result<Report, Failure> {
    World::new(seed, bug, off).run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_crowd_of_client_rounds_overlap_during_eviction() {
        let off = [Fault::ServerCrash, Fault::Loss, Fault::Duplicate, Fault::Reorder, Fault::Partition]
            .into_iter().fold(Off::none(), Off::with);
        // One action and one round per client exercise contention without a
        // large build graph. Give the links one brief, deterministic outage.
        // Twenty clients is the smallest crowd `Config::of` draws.
        let mut world = World::configured(50, None, off, |cfg, project| {
            cfg.clients = 20;
            cfg.rounds = 1;
            cfg.max_latency_ms = 3;
            project.actions.truncate(1);
        });
        world.watch_limit = Duration::from_secs(2);
        world.silence = Some((550, 3200));
        let report = world.run().unwrap_or_else(|failure| panic!("{failure}"));
        assert!(report.clients_at_eviction >= 20, "{report:?}");
        assert!(report.cache_hits > 0, "clients reuse action outputs");
        assert!(report.silences > 0, "clients give up on a silent server");
    }

    #[test]
    fn stale_cancellation_cannot_end_the_next_round() {
        let mut world = World::new(0, None, Off::none());
        let client = world.clients.get_mut(&1).unwrap();
        client.round = 2;
        client.outputs.insert(0, b"staged output".to_vec());
        world
            .client_hears(1, Down::RoundTerminal { round: 1, msg: ServerMsg::Exit(model::CANCELLED) })
            .unwrap();
        assert!(!world.clients[&1].round_ended);
        assert_eq!(world.report.cancelled_builds, 0);
        assert_eq!(world.clients[&1].outputs.len(), 1);

        world
            .client_hears(1, Down::RoundTerminal { round: 2, msg: ServerMsg::Exit(model::CANCELLED) })
            .unwrap();
        assert!(world.clients[&1].round_ended);
        assert_eq!(world.report.cancelled_builds, 1);
        assert!(world.clients[&1].outputs.is_empty());
        assert!(world.clients[&1].target.files.is_empty());
    }

    #[test]
    fn a_server_restart_reloads_shared_pins_and_discards_old_links() {
        sim::test(|| {
            let mut world = World::new(12, None, Off::none());
            world.server.store = Some(disk::open(&sim::tempdir()));
            let hash = world.server.store().put_bytes(b"pinned").unwrap();
            let key = Hash::of(b"action");
            disk::cache(world.server.store(), key, hash);
            for client in 1..=world.cfg.clients {
                world.server.held.insert(client, (1, BTreeSet::new()));
                world.pin(client, hash);
                world.server.save_held(client);
            }
            world.round(1).unwrap();
            let old_link = world.connections[&Node::Client(1)].id;

            world.step(Ev::ServerCrash).unwrap();
            world.check().unwrap();
            assert!(world.connections.is_empty());
            assert!(world.server.sessions.is_empty());
            assert_eq!(world.server.pins[&hash], world.cfg.clients as usize);
            assert_eq!(disk::cached(world.server.store(), key), Some(hash));
            assert_eq!(world.report.server_restarts, 1);

            world.sync(1).unwrap();
            assert_ne!(world.connections[&Node::Client(1)].id, old_link);
            world.arrive(Node::Client(1), true, old_link, Vec::new()).unwrap();
        });
    }

    #[test]
    fn a_one_way_partition_heals_without_replacing_the_link() {
        let mut world = World::new(85, None, Off::none().with(Fault::Partition));
        let peer = Node::Worker(1);
        world.open_link(peer).unwrap();
        let id = world.connections[&peer].id;
        world.partitions.insert((peer, false), 0..100);
        world.arrive(peer, false, id, Vec::new()).unwrap();
        assert_eq!(world.report.partitioned, 1);
        world.arrive(peer, true, id, Vec::new()).unwrap();
        assert_eq!(world.report.partitioned, 1);
        world.now = 100;
        world.arrive(peer, false, id, Vec::new()).unwrap();
        assert_eq!(world.report.partitioned, 1);
        assert_eq!(world.connections[&peer].id, id);
    }

    #[test]
    fn partitions_cover_workers_and_clients_in_both_directions() {
        let mut seen = BTreeSet::new();
        for seed in 0..600 {
            let world = World::new(seed, None, Off::none());
            if let Some(&(peer, _)) = world.partitions.keys().next() {
                let outbound = world.partitions.contains_key(&(peer, false));
                let inbound = world.partitions.contains_key(&(peer, true));
                seen.insert((matches!(peer, Node::Worker(_)), outbound, inbound));
            }
        }
        assert_eq!(seen, BTreeSet::from([
            (false, true, false), (false, false, true), (false, true, true),
            (true, true, false), (true, false, true), (true, true, true),
        ]));
    }

    #[test]
    fn a_lost_initial_handshake_is_retransmitted_on_the_simulated_clock() {
        let mut world = World::new(85, None, Off::none());
        world.cfg.fault_end_ms = 100;
        world.cfg.net = Net { loss_per_mille: 1_000, duplicate_per_mille: 0, reorder_per_mille: 0 };
        let report = world.run().expect("the build finishes after the initial handshake is lost");
        assert!(report.lost > 0);
        assert!(report.retransmits > 0);
        assert!(report.runs > 0);
    }
}

#[cfg(test)]
mod edit_tests {
    use super::*;

    #[test]
    fn checkout_replaces_the_build_snapshot_and_ignores_old_results() {
        let mut world = World::new(12, None, Off::none());
        world.round(1).unwrap();
        let old_round = world.clients[&1].round;
        let old_tree = world.clients[&1].snapshot.clone();
        let action = world.project.ready(&|_| false, &|_| false)[0];
        let old_output = world.clients[&1].reference[action].clone();

        world.clients.get_mut(&1).unwrap().synced_round = old_round;
        world.request_ready(1);
        assert!(world.clients[&1].requested.contains(&action));
        assert!(world.queue.values().any(|event| matches!(
            event,
            Ev::Checkout { client: 1, round } if *round == old_round
        )));

        let rounds_left = world.clients[&1].rounds_left;
        world.step(Ev::Checkout { client: 1, round: old_round }).unwrap();
        let client = &world.clients[&1];
        assert_eq!(client.rounds_left, rounds_left);
        assert_eq!(client.snapshot, client.tree);
        assert!(client.snapshot.iter().zip(old_tree).all(|(new, old)| *new != old));
        assert!(client.requested.is_empty());
        let new_round = client.round;
        let clean = world.project.clean_build(&client.tree);
        assert_eq!(client.reference, clean);
        let new_output = clean[action].clone();
        assert_ne!(new_output, old_output);

        let root = Hash::of(&client.contents.concat()).0;
        world.client_hears(1, Down::Synced { round: old_round, root }).unwrap();
        assert!(world.clients[&1].requested.is_empty());
        world.client_hears(1, Down::Synced { round: new_round, root }).unwrap();
        assert!(world.clients[&1].requested.contains(&action));
        world.client_done(1, old_round, action, old_output).unwrap();
        assert!(world.clients[&1].outputs.is_empty());
        world.client_done(1, new_round, action, new_output.clone()).unwrap();
        assert_eq!(world.clients[&1].outputs[&action], new_output);
    }

    #[test]
    fn a_round_scans_only_what_changed_and_a_new_daemon_reuses_the_saved_index() {
        let mut world = World::new(12, None, Off::none());
        world.clients.get_mut(&1).unwrap().rounds_left = 3;
        world.now = 1_000;
        world.round(1).unwrap();
        let first = world.clients[&1].last_scan.clone().unwrap();
        let files = first.file_count;
        assert_eq!(first.hashed_files, files, "a first scan hashes every file");

        world.now = 2_000;
        world.step(Ev::Edit { client: 1 }).unwrap();
        world.now = 3_000;
        world.round(1).unwrap();
        let client = &world.clients[&1];
        let second = client.last_scan.clone().unwrap();
        assert!(second.hashed_files <= 1, "only the edited file is read again, not {}", second.hashed_files);
        assert!(second.listed_dirs < second.dirs.len() as u64, "a rescan lists only the directories written in");
        assert_eq!(client.snapshot, client.tree);
        assert_ne!(second.root, first.root);

        world.client_crash(1);
        world.now = 4_000;
        world.round(1).unwrap();
        let client = &world.clients[&1];
        let third = client.last_scan.as_ref().unwrap();
        assert_eq!(third.listed_dirs, third.dirs.len() as u64, "a new daemon lists all of the worktree");
        assert_eq!(third.hashed_files, 0, "but reads no file its saved index vouches for");
        assert_eq!(third.root, second.root);
        assert_eq!(client.snapshot, client.tree);
    }

    #[test]
    fn live_edits_leave_the_current_build_on_its_snapshot() {
        let mut world = World::new(12, None, Off::none());
        world.clients.get_mut(&1).unwrap().rounds_left = 2;
        world.round(1).unwrap();
        let original = world.clients[&1].snapshot.clone();
        let original_contents = world.clients[&1].contents.clone();
        let mut edited = false;
        let mut deleted = false;
        let mut added = false;
        for _ in 0..32 {
            let before = world.clients[&1].tree.clone();
            world.step(Ev::Edit { client: 1 }).unwrap();
            let client = &world.clients[&1];
            for (old, new) in before.iter().zip(&client.tree) {
                edited |= old.is_some() && new.is_some() && old != new;
                deleted |= old.is_some() && new.is_none();
                added |= old.is_none() && new.is_some();
            }
        }
        assert!(edited && deleted && added);

        world.sync(1).unwrap();
        let client = &world.clients[&1];
        assert_eq!(client.contents, original_contents);
        assert_eq!(client.snapshot, original);
        assert_eq!(client.reference, world.project.clean_build(&original));

        world.round(1).unwrap();
        let client = &world.clients[&1];
        assert_eq!(client.snapshot, client.tree);
        assert_ne!(client.snapshot, original);
        assert_eq!(client.reference, world.project.clean_build(&client.tree));
        assert_ne!(client.reference, world.project.clean_build(&original));
    }
}
