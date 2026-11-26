use rargo_machine::StateMachine;
use crate::model::{self, Project};
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
/// The first retransmission timeout of a lost packet, as Linux's TCP_RTO_MIN; it doubles on each loss.
const RTO_MS: u64 = 200;
/// Losses in a row before a packet gets through; at the rates a seed picks, more never happens.
const MAX_RETRIES: u32 = 5;

/// The receiving end of one connection: what TCP does under the messages, so that the network may lose,
/// duplicate and reorder packets while each connection still hands its messages over once and in order.
#[derive(Default)]
struct Rx {
    next: u64,
    held: BTreeMap<u64, Msg>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bug {
    EvictPinned,
    StaleDelivery,
    NeverAnswer,
    EvictClaimed,
    StaleKnowledge,
    DeafToSilence,
    TrustAfterCrash,
}

impl Bug {
    pub const ALL: [Bug; 7] = [
        Bug::EvictPinned,
        Bug::StaleDelivery,
        Bug::NeverAnswer,
        Bug::EvictClaimed,
        Bug::StaleKnowledge,
        Bug::DeafToSilence,
        Bug::TrustAfterCrash,
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
        }
    }

    pub fn parse(s: &str) -> Option<Bug> {
        Bug::ALL.into_iter().find(|b| b.name() == s)
    }
}

/// A fault the simulated network makes on its own, which `rargo-sim --off` turns off.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Fault {
    Loss,
    Duplicate,
    Reorder,
}

impl Fault {
    pub const ALL: [Fault; 3] = [Fault::Loss, Fault::Duplicate, Fault::Reorder];

    pub fn name(self) -> &'static str {
        match self {
            Fault::Loss => "loss",
            Fault::Duplicate => "duplicate",
            Fault::Reorder => "reorder",
        }
    }

    pub fn parse(s: &str) -> Option<Fault> {
        Fault::ALL.into_iter().find(|f| f.name() == s)
    }
}

/// Which network faults a run leaves out. Each fault draws from its own stream, so turning one off
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
    pub cache_hits: u64,
    pub tests_passed: u64,
    pub tests_failed: u64,
    pub tests_timed_out: u64,
    pub cuts: u64,
    pub worker_crashes: u64,
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
}

#[derive(Clone, Debug)]
pub struct Config {
    pub workers: u64,
    pub clients: u64,
    pub sources: usize,
    pub actions: usize,
    pub rounds: u64,
    pub fault_end_ms: u64,
    pub cut_per_mille: u64,
    pub max_latency_ms: u64,
    pub client_cuts: u64,
    pub crashes: u64,
    pub net: Net,
}

/// How often, per mille of messages, the network loses, duplicates and reorders until `fault_end_ms`.
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
    pub fn of(rng: &mut Rng, net: Net) -> Config {
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
            sources: rng.range(3, 10) as usize,
            actions: rng.range(4, 24) as usize,
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

#[derive(Clone, Debug)]
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

#[derive(Clone, Debug)]
enum Up {
    Hello { cores: usize },
    Result { job: u64, output: Vec<u8> },
    Join,
    Offer { round: u64, root: Digest, contents: Vec<Digest>, sending: Vec<Digest> },
    Session(ClientMsg),
    Request { round: u64, action: usize, inputs: Vec<Hash> },
    Ping,
}

#[derive(Clone, Debug)]
enum Down {
    Session(ServerMsg),
    Synced { round: u64, root: Digest },
    Pong,
}

#[derive(Debug)]
enum Msg {
    Up(Up),
    Work(FromServer<Sim>),
    Down(Down),
}

impl Msg {
    /// The copy a duplicating network delivers; the worker's messages are not `Clone` in `rargo-core`.
    fn copy(&self) -> Msg {
        match self {
            Msg::Up(u) => Msg::Up(u.clone()),
            Msg::Down(d) => Msg::Down(d.clone()),
            Msg::Work(w) => Msg::Work(match w {
                FromServer::Welcome => FromServer::Welcome,
                FromServer::Ask { rid, serve } => FromServer::Ask { rid: *rid, serve: *serve },
                FromServer::Push { blob, first, of, part } => {
                    FromServer::Push { blob: *blob, first: *first, of: *of, part: part.clone() }
                }
                FromServer::Run(r) => FromServer::Run(worker_state::Run {
                    action: r.action,
                    incoming: r.incoming.clone(),
                    prefill: r.prefill.clone(),
                    listings: r.listings.clone(),
                    closed: r.closed.clone(),
                    run: r.run.clone(),
                }),
                FromServer::Cancel { action } => FromServer::Cancel { action: *action },
                FromServer::Other => FromServer::Other,
            }),
        }
    }
}

#[derive(Debug)]
enum Ev {
    Deliver { from: Node, to: Node, epoch: u64, n: u64, msg: Msg },
    Connect { worker: u64, epoch: u64 },
    Finish { worker: u64, epoch: u64, token: u64 },
    Crash { worker: u64 },
    Cut { worker: u64 },
    Evict,
    Round { client: u64 },
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
}

struct Server {
    pool: WorkerPool,
    trees: TreesState,
    sessions: BTreeMap<u64, Sess>,
    sched: SchedState,
    pipe: usize,
    store: BTreeMap<Hash, Vec<u8>>,
    pins: BTreeMap<Hash, usize>,
    held: BTreeMap<u64, (u64, BTreeSet<Hash>)>,
    cache: BTreeMap<Hash, Hash>,
    jobs: BTreeMap<u64, Job>,
    ready: VecDeque<u64>,
    next_job: u64,
}

struct Client {
    round: u64,
    rounds_left: u64,
    versions: Vec<u64>,
    outputs: BTreeMap<usize, Vec<u8>>,
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

pub struct World {
    seed: u64,
    bug: Option<Bug>,
    off: Off,
    cfg: Config,
    rng: Rng,
    net: [Rng; 3],
    tx: BTreeMap<(Node, Node, u64), u64>,
    rx: BTreeMap<(Node, Node, u64), Rx>,
    now: u64,
    seq: u64,
    queue: BTreeMap<(u64, u64), Ev>,
    busy: usize,
    last: BTreeMap<(Node, Node), u64>,
    project: Project,
    server: Server,
    workers: BTreeMap<u64, WorkerNode>,
    clients: BTreeMap<u64, Client>,
    report: Report,
    trace: Vec<u8>,
    silence: Option<(u64, u64)>,
}

type Step = Result<(), Kind>;

impl World {
    pub fn new(seed: u64, bug: Option<Bug>, off: Off) -> World {
        let mut rng = Rng::new(seed);
        let mut net = Fault::ALL.map(|f| Rng::new(seed ^ NET_STREAM ^ f as u64));
        let cfg = Config::of(&mut rng, Net::of(&mut net, &off));
        let project = Project::generate(&mut rng, cfg.sources, cfg.actions);
        let mut w = World {
            seed,
            bug,
            off,
            rng,
            net,
            tx: BTreeMap::new(),
            rx: BTreeMap::new(),
            now: 0,
            seq: 0,
            queue: BTreeMap::new(),
            busy: 0,
            last: BTreeMap::new(),
            server: Server {
                pool: WorkerPool::new(),
                trees: TreesState::new(),
                sessions: BTreeMap::new(),
                sched: SchedState::new(SERVER_TOKENS, SERVER_MEM_KB),
                pipe: SERVER_TOKENS,
                store: BTreeMap::new(),
                pins: BTreeMap::new(),
                held: BTreeMap::new(),
                cache: BTreeMap::new(),
                jobs: BTreeMap::new(),
                ready: VecDeque::new(),
                next_job: 1,
            },
            workers: BTreeMap::new(),
            clients: BTreeMap::new(),
            report: Report {
                events: 0,
                end_ms: 0,
                trace: Hash::of(b""),
                runs: 0,
                cache_hits: 0,
                tests_passed: 0,
                tests_failed: 0,
                tests_timed_out: 0,
                cuts: 0,
                worker_crashes: 0,
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
            },
            trace: Vec::new(),
            silence: None,
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
            let versions = vec![0; w.cfg.sources];
            w.clients.insert(
                id,
                Client {
                    round: 0,
                    rounds_left: w.cfg.rounds,
                    versions,
                    outputs: BTreeMap::new(),
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
                    watch: Watch::new(Duration::ZERO, link_watch::SILENT_AFTER),
                    up_since: 0,
                    dead: false,
                    saved: Vec::new(),
                },
            );
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

    fn send(&mut self, from: Node, to: Node, msg: Msg) {
        let epoch = match (from, to) {
            (Node::Worker(w), _) | (_, Node::Worker(w)) => self.workers[&w].epoch,
            (Node::Client(c), _) | (_, Node::Client(c)) => self.clients[&c].epoch,
            _ => 0,
        };
        let latency = self.rng.range(1, self.cfg.max_latency_ms);
        let in_order = (self.now + latency).max(self.last.get(&(from, to)).copied().unwrap_or(0));
        let seq = self.tx.entry((from, to, epoch)).or_default();
        let n = *seq;
        *seq += 1;
        let net = self.cfg.net.clone();
        let faulty = self.now < self.cfg.fault_end_ms;
        let max_latency = self.cfg.max_latency_ms;
        let [loss, dup, reorder] = &mut self.net;
        let mut stall = 0;
        let mut rto = RTO_MS;
        for _ in 0..MAX_RETRIES {
            if !(loss.chance(net.loss_per_mille) && faulty) {
                break;
            }
            self.report.lost += 1;
            stall += rto;
            rto *= 2;
        }
        let copy_after = dup.range(1, 4 * max_latency);
        let duplicate = dup.chance(net.duplicate_per_mille) && faulty;
        let early = reorder.range(1, max_latency);
        let reordered = reorder.chance(net.reorder_per_mille) && faulty;
        let at = if reordered {
            self.report.reordered += 1;
            self.now + early + stall
        } else if stall > 0 {
            in_order + stall
        } else {
            self.last.insert((from, to), in_order);
            in_order
        };
        if duplicate {
            self.report.duplicated += 1;
            let copy = msg.copy();
            self.busy += 1;
            self.seq += 1;
            self.queue.insert((at + copy_after, self.seq), Ev::Deliver { from, to, epoch, n, msg: copy });
        }
        self.busy += 1;
        self.seq += 1;
        self.queue.insert((at, self.seq), Ev::Deliver { from, to, epoch, n, msg });
    }

    fn finished(&self) -> bool {
        self.clients.values().all(|c| c.finished)
    }

    pub fn run(mut self) -> Result<Report, Failure> {
        let seed = self.seed;
        let bug = self.bug;
        let off = self.off.clone();
        loop {
            if self.finished() {
                self.report.end_ms = self.now;
                self.report.trace = Hash::of(&self.trace);
                return Ok(self.report);
            }
            let fail = |w: &World, kind: Kind| Failure { seed, bug, off: off.clone(), at_ms: w.now, kind };
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
        for (client, sess) in &self.server.sessions {
            let Some(o) = &sess.offer else { continue };
            for h in &o.arrived {
                if self.server.trees.holder(h) == Some(o.up) && !self.server.store.contains_key(h) {
                    return Err(Kind::LostPinned(format!(
                        "blob {} that client {client} uploaded for round {} is claimed and not in the store",
                        h.hex(),
                        o.round
                    )));
                }
            }
        }
        for (h, n) in &self.server.pins {
            if *n > 0 && !self.server.store.contains_key(h) {
                return Err(Kind::LostPinned(format!("blob {} is pinned {n} times and not in the store", h.hex())));
            }
        }
        for (job, j) in &self.server.jobs {
            if let Some(h) = j.inputs.iter().find(|h| !self.server.store.contains_key(h)) {
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
            Ev::Deliver { from, to, epoch, n, msg } => self.arrive(from, to, epoch, n, msg),
            Ev::Connect { worker, epoch } => self.connect(worker, epoch),
            Ev::Finish { worker, epoch, token } => self.finish(worker, epoch, token),
            Ev::Crash { worker } => self.crash(worker),
            Ev::Cut { worker } => self.cut(worker),
            Ev::Evict => {
                self.evict();
                Ok(())
            }
            Ev::Round { client } => self.round(client),
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

    fn arrive(&mut self, from: Node, to: Node, epoch: u64, n: u64, msg: Msg) -> Step {
        if !self.open(from, to, epoch) {
            self.rx.remove(&(from, to, epoch));
            return Ok(());
        }
        if self.silent_at(self.now).is_some() && matches!((from, to), (Node::Client(_), _) | (_, Node::Client(_))) {
            let (_, end) = self.silent_at(self.now).expect("checked");
            self.schedule(end - self.now, Ev::Deliver { from, to, epoch, n, msg });
            return Ok(());
        }
        let rx = self.rx.entry((from, to, epoch)).or_default();
        if n < rx.next || rx.held.contains_key(&n) {
            return Ok(());
        }
        rx.held.insert(n, msg);
        loop {
            let rx = self.rx.entry((from, to, epoch)).or_default();
            let next = rx.next;
            let Some(msg) = rx.held.remove(&next) else { return Ok(()) };
            rx.next += 1;
            if !self.open(from, to, epoch) {
                return Ok(());
            }
            self.deliver(from, to, msg)?;
        }
    }

    fn open(&self, from: Node, to: Node, epoch: u64) -> bool {
        match (from, to) {
            (Node::Worker(w), _) | (_, Node::Worker(w)) => self.workers[&w].up && self.workers[&w].epoch == epoch,
            (Node::Client(c), _) | (_, Node::Client(c)) => self.clients[&c].up && self.clients[&c].epoch == epoch,
            _ => true,
        }
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
                let fx = node.sm.server(m, &|h| store.contains_key(h), now);
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
            node.sm.connected()
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
        self.worker_effects(worker, fx)?;
        let lost = self.server.pool.left(worker);
        self.pool_effects(lost);
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
        let output = model::compile(self.project.actions[run.action].id, &inputs);
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
        if self.server.store.is_empty() {
            return;
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
            .store
            .keys()
            .filter(|h| ignore_pins || (!self.server.pins.contains_key(h) && !job_inputs.contains(h)))
            .filter(|h| {
                if self.server.trees.holder(h).is_some() {
                    ignore_claims
                } else {
                    !offered.contains(h)
                }
            })
            .copied()
            .collect();
        if candidates.is_empty() {
            return;
        }
        let h = candidates[self.rng.below(candidates.len() as u64) as usize];
        self.server.store.remove(&h);
        self.report.evictions += 1;
        let active_clients = self
            .clients
            .values()
            .filter(|c| c.round > 0 && c.outputs.len() < self.project.actions.len())
            .count();
        self.report.clients_at_eviction = self.report.clients_at_eviction.max(active_clients);
        let worker = self.rng.range(1, self.cfg.workers);
        let busy: BTreeSet<Hash> = self
            .server
            .jobs
            .values()
            .filter(|j| j.worker == Some(worker))
            .flat_map(|j| j.inputs.iter().copied())
            .collect();
        let node = self.workers.get_mut(&worker).expect("known");
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
        match u {
            Up::Ping => self.send(Node::Server, Node::Client(client), Msg::Down(Down::Pong)),
            Up::Join => {
                self.drop_session(client);
                let mut sess = Sess { state: SessionState::new(), has: ClientSet::default(), offer: None };
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
                let store = &server.store;
                let claimed = server.trees.claim(up, &contents, &|h| store.contains_key(h));
                let want: Vec<Digest> = claimed.mine.iter().filter(|h| !sending.contains(h)).map(|h| h.0).collect();
                sess.offer = Some(Offer { round, root, contents, up, arrived: BTreeSet::new() });
                if !want.is_empty() {
                    self.report.wants += 1;
                    self.send(Node::Server, Node::Client(client), Msg::Down(Down::Session(ServerMsg::WantBlobs(want))));
                }
                self.try_finish_offer(client);
            }
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
                            server.store.insert(h, bytes);
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
                if self.bug == Some(Bug::NeverAnswer) && self.rng.chance(40) {
                    return Ok(());
                }
                if let Some(h) = inputs.iter().find(|h| !self.server.store.contains_key(h)) {
                    return Err(Kind::LostPinned(format!("input {} of a request is gone", h.hex())));
                }
                let id = self.project.actions[action].id;
                let key = if self.bug == Some(Bug::StaleDelivery) && inputs.len() > 1 {
                    model::key(id, &inputs[..inputs.len() - 1])
                } else {
                    model::key(id, &inputs)
                };
                if let Some(out) = self.server.cache.get(&key).copied()
                    && let Some(bytes) = self.server.store.get(&out).cloned()
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

    fn pending_offers(&self) -> Vec<u64> {
        self.server.sessions.iter().filter(|(_, s)| s.offer.is_some()).map(|(c, _)| *c).collect()
    }

    fn rewant(&mut self, client: u64) {
        let server = &mut self.server;
        let Some(o) = server.sessions.get(&client).and_then(|s| s.offer.as_ref()) else { return };
        let store = &server.store;
        let orphans: Vec<Hash> =
            o.contents.iter().filter(|h| !store.contains_key(h) && server.trees.holder(h).is_none()).copied().collect();
        if orphans.is_empty() {
            return;
        }
        let claimed = server.trees.claim(o.up, &orphans, &|h| store.contains_key(h));
        let want: Vec<Digest> = claimed.mine.iter().map(|h| h.0).collect();
        self.report.wants += 1;
        self.send(Node::Server, Node::Client(client), Msg::Down(Down::Session(ServerMsg::WantBlobs(want))));
    }

    fn try_finish_offer(&mut self, client: u64) {
        let server = &mut self.server;
        let Some(sess) = server.sessions.get_mut(&client) else { return };
        let Some(o) = &sess.offer else { return };
        if !o.contents.iter().all(|h| server.store.contains_key(h)) {
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
        self.server.trees.release(o.up);
        self.send(Node::Server, Node::Client(client), Msg::Down(Down::Synced { round: o.round, root: o.root }));
    }

    fn deliver_output(&mut self, client: u64, round: u64, action: usize, out: Hash, bytes: Vec<u8>) -> Step {
        if self.server.held.get(&client).is_some_and(|(r, _)| *r == round) {
            self.pin(client, out);
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
                    let read = server.store.get(&Hash(hash)).map(|v| {
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
                let out = Hash::of(&output);
                self.server.store.insert(out, output.clone());
                self.server.cache.insert(j.key, out);
                self.deliver_output(j.client, j.round, j.action, out, output)?;
            }
            Up::Join | Up::Offer { .. } | Up::Session(_) | Up::Request { .. } | Up::Ping => unreachable!("only clients say this"),
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
                pool_state::Effect::Cancel { .. } => {}
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
            let run = SimRun { action: j.action, inputs: j.inputs.clone(), ms: self.project.actions[j.action].ms };
            let pushes: Vec<(Hash, Vec<u8>)> = j.inputs.iter().map(|h| (*h, self.server.store[h].clone())).collect();
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

    fn inputs_of(&self, client: &Client, action: usize) -> Vec<Vec<u8>> {
        let a = &self.project.actions[action];
        let mut inputs: Vec<Vec<u8>> = a.sources.iter().map(|&s| model::source(s, client.versions[s])).collect();
        inputs.extend(a.deps.iter().map(|d| client.outputs[d].clone()));
        inputs
    }

    fn round(&mut self, client: u64) -> Step {
        let c = self.clients.get_mut(&client).expect("known");
        c.round += 1;
        c.rounds_left -= 1;
        c.test = TestRun::generate(self.seed, client, c.round);
        if c.round > 1 {
            for _ in 0..self.rng.range(1, 3) {
                let s = self.rng.below(self.cfg.sources as u64) as usize;
                c.versions[s] = self.rng.range(0, 2);
            }
        }
        c.outputs.clear();
        c.requested.clear();
        c.pending.clear();
        c.daemon.changed(1, false);
        self.sync(client)
    }

    fn sync(&mut self, client: u64) -> Step {
        let now = self.now;
        let c = self.clients.get_mut(&client).expect("known");
        if c.finished || c.dead {
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
                c.watch = Watch::new(Duration::from_millis(now), link_watch::SILENT_AFTER);
                c.up_since = now;
                let outputs = &c.outputs;
                c.requested.retain(|a| outputs.contains_key(a));
                c.pending.clear();
                c.parts.clear();
                self.send(Node::Client(client), Node::Server, Msg::Up(Up::Join));
            }
            Link::Skip => unreachable!("a sync needs the link"),
        }
        let sources = self.cfg.sources;
        let c = self.clients.get_mut(&client).expect("known");
        let _ = c.daemon.plan_scan(1, None, true);
        c.daemon.scanned(1, None);
        let blobs: Vec<Vec<u8>> = (0..sources).map(|s| model::source(s, c.versions[s])).collect();
        let contents: Vec<Digest> = blobs.iter().map(|b| Hash::of(b).0).collect();
        for (d, b) in contents.iter().zip(blobs) {
            c.cache.insert(*d, b);
        }
        let root = Hash::of(&contents.concat()).0;
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
        self.report.crashes += 1;
        if self.bug != Some(Bug::TrustAfterCrash) {
            c.up = false;
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
            Tick::Wait => {}
            Tick::Ping(_) => self.send(Node::Client(client), Node::Server, Msg::Up(Up::Ping)),
            Tick::Silent(_) if bug == Some(Bug::DeafToSilence) => {}
            Tick::Silent(quiet) => {
                let heard = now - quiet.as_millis() as u64;
                let slack = link_watch::PING_EVERY.as_millis() as u64 + 2 * max_latency + 2 * WATCH_STEP_MS;
                let silent_for = silence.map_or(0, |(start, end)| end.min(now).saturating_sub(start.max(heard)));
                if silent_for + slack < link_watch::SILENT_AFTER.as_millis() as u64 {
                    return Err(Kind::Silence(format!(
                        "client {client} gave up on its server at {now} ms after {} ms without a word, but the server was silent for only {silent_for} ms of it",
                        quiet.as_millis()
                    )));
                }
                c.up = false;
                self.report.silences += 1;
                self.drop_session(client);
                self.schedule(1_000, Ev::Sync { client });
                return Ok(());
            }
        }
        let c = &self.clients[&client];
        if let Some((start, end)) = silence {
            let from = start.max(c.up_since);
            let bound = from + link_watch::SILENT_AFTER.as_millis() as u64 + max_latency + 2 * WATCH_STEP_MS;
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
        if c.synced_round != c.round {
            return;
        }
        let ready = self.project.ready(&|a| c.outputs.contains_key(&a), &|a| c.requested.contains(&a));
        for action in ready {
            let c = &self.clients[&client];
            let inputs = self.inputs_of(c, action).iter().map(|b| Hash::of(b)).collect();
            let round = c.round;
            self.clients.get_mut(&client).expect("known").requested.insert(action);
            self.send(Node::Client(client), Node::Server, Msg::Up(Up::Request { round, action, inputs }));
        }
    }

    fn client_hears(&mut self, client: u64, d: Down) -> Step {
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
            Down::Session(m) => m,
            Down::Pong => return Ok(()),
        };
        let c = self.clients.get_mut(&client).expect("known");
        match m {
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
        if round != c.round || c.outputs.contains_key(&action) || !c.requested.contains(&action) {
            return Ok(());
        }
        let inputs = self.inputs_of(c, action);
        let refs: Vec<&[u8]> = inputs.iter().map(|b| b.as_slice()).collect();
        let want = model::compile(self.project.actions[action].id, &refs);
        if output != want {
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
        Ev::Deliver { from, to, epoch, n, msg } => {
            let what = match msg {
                Msg::Up(Up::Hello { .. }) => "hello",
                Msg::Up(Up::Result { .. }) => "result",
                Msg::Up(Up::Join) => "join",
                Msg::Up(Up::Offer { .. }) => "offer",
                Msg::Up(Up::Session(_)) => "upload",
                Msg::Up(Up::Request { .. }) => "request",
                Msg::Up(Up::Ping) => "ping",
                Msg::Work(_) => "work",
                Msg::Down(Down::Synced { .. }) => "synced",
                Msg::Down(Down::Session(_)) => "deliver",
                Msg::Down(Down::Pong) => "pong",
            };
            format!("{from:?}>{to:?}@{epoch}#{n}:{what};")
        }
        Ev::Connect { worker, epoch } => format!("connect {worker}@{epoch};"),
        Ev::Finish { worker, epoch, token } => format!("finish {worker}@{epoch}/{token};"),
        Ev::Crash { worker } => format!("crash {worker};"),
        Ev::Cut { worker } => format!("cut {worker};"),
        Ev::Evict => "evict;".to_string(),
        Ev::Round { client } => format!("round {client};"),
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
