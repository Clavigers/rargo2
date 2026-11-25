use crate::model::{self, Project};
use crate::rng::Rng;
use rargo_core::hash::Hash;
use rargo_core::pool_state::{self, WorkerPool};
use rargo_core::sched::Priority;
use rargo_core::sched_state::{Broker, Effects, SchedState};
use rargo_core::worker_state::{self, Carried, FromServer, Worker};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

pub const LIVENESS_MS: u64 = 120_000;
const SERVER_TOKENS: usize = 6;
const SERVER_MEM_KB: u64 = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bug {
    EvictPinned,
    StaleDelivery,
    NeverAnswer,
}

impl Bug {
    pub const ALL: [Bug; 3] = [Bug::EvictPinned, Bug::StaleDelivery, Bug::NeverAnswer];

    pub fn name(self) -> &'static str {
        match self {
            Bug::EvictPinned => "evict-pinned",
            Bug::StaleDelivery => "stale-delivery",
            Bug::NeverAnswer => "never-answer",
        }
    }

    pub fn parse(s: &str) -> Option<Bug> {
        Bug::ALL.into_iter().find(|b| b.name() == s)
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
        }
    }

    fn detail(&self) -> &str {
        match self {
            Kind::StateMachine(d)
            | Kind::LostPinned(d)
            | Kind::Drift(d)
            | Kind::Liveness(d)
            | Kind::Deadlock(d)
            | Kind::StartedWithoutInputs(d) => d,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub seed: u64,
    pub bug: Option<Bug>,
    pub at_ms: u64,
    pub kind: Kind,
}

impl Failure {
    pub fn replay(&self) -> String {
        match self.bug {
            Some(b) => format!("rargo-sim --seed {} --bug {}", self.seed, b.name()),
            None => format!("rargo-sim --seed {}", self.seed),
        }
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
    pub cuts: u64,
    pub evictions: u64,
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
}

impl Config {
    pub fn of(rng: &mut Rng) -> Config {
        Config {
            workers: rng.range(1, 4),
            clients: rng.range(1, 3),
            sources: rng.range(3, 10) as usize,
            actions: rng.range(4, 24) as usize,
            rounds: rng.range(1, 3),
            fault_end_ms: rng.range(0, 8_000),
            cut_per_mille: rng.range(0, 30),
            max_latency_ms: rng.range(1, 60),
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

#[derive(Debug)]
enum Up {
    Hello { cores: usize },
    Result { job: u64, output: Vec<u8> },
    Upload { blob: Vec<u8> },
    Hold { round: u64 },
    Request { round: u64, action: usize, inputs: Vec<Hash> },
}

#[derive(Debug)]
enum Down {
    Done { round: u64, action: usize, output: Vec<u8> },
}

#[derive(Debug)]
enum Msg {
    Up(Up),
    Work(FromServer<Sim>),
    Down(Down),
}

#[derive(Debug)]
enum Ev {
    Deliver { from: Node, to: Node, epoch: u64, msg: Msg },
    Connect { worker: u64 },
    Finish { worker: u64, token: u64 },
    Cut { worker: u64 },
    Evict,
    Round { client: u64 },
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

struct Server {
    pool: WorkerPool,
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
}

pub struct World {
    seed: u64,
    bug: Option<Bug>,
    cfg: Config,
    rng: Rng,
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
}

type Step = Result<(), Kind>;

impl World {
    pub fn new(seed: u64, bug: Option<Bug>) -> World {
        let mut rng = Rng::new(seed);
        let cfg = Config::of(&mut rng);
        let project = Project::generate(&mut rng, cfg.sources, cfg.actions);
        let mut w = World {
            seed,
            bug,
            rng,
            now: 0,
            seq: 0,
            queue: BTreeMap::new(),
            busy: 0,
            last: BTreeMap::new(),
            server: Server {
                pool: WorkerPool::new(),
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
                cuts: 0,
                evictions: 0,
            },
            trace: Vec::new(),
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
                },
            );
            let at = w.rng.range(0, 500);
            w.schedule(at, Ev::Round { client: id });
        }
        if w.cfg.fault_end_ms > 0 {
            for id in 1..=w.cfg.workers {
                let at = w.rng.range(0, w.cfg.fault_end_ms);
                w.schedule(at, Ev::Cut { worker: id });
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
            _ => 0,
        };
        let latency = self.rng.range(1, self.cfg.max_latency_ms);
        let at = (self.now + latency).max(self.last.get(&(from, to)).copied().unwrap_or(0));
        self.last.insert((from, to), at);
        self.busy += 1;
        self.seq += 1;
        self.queue.insert((at, self.seq), Ev::Deliver { from, to, epoch, msg });
    }

    fn finished(&self) -> bool {
        self.clients.values().all(|c| c.finished)
    }

    pub fn run(mut self) -> Result<Report, Failure> {
        let seed = self.seed;
        let bug = self.bug;
        loop {
            if self.finished() {
                self.report.end_ms = self.now;
                self.report.trace = Hash::of(&self.trace);
                return Ok(self.report);
            }
            let fail = |w: &World, kind: Kind| Failure { seed, bug, at_ms: w.now, kind };
            if self.busy == 0 {
                let open: Vec<String> = self
                    .clients
                    .iter()
                    .filter(|(_, c)| !c.finished)
                    .map(|(id, c)| format!("client {id} round {} waits on {:?}", c.round, self.waiting(c)))
                    .collect();
                return Err(fail(&self, Kind::Deadlock(format!("nothing left to happen: {}", open.join("; ")))));
            }
            if self.now > self.cfg.fault_end_ms + LIVENESS_MS {
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
                        self.cfg.fault_end_ms,
                        open.join("; ")
                    )),
                ));
            }
            let ((at, _), ev) = self.queue.pop_first().expect("busy events are queued");
            if !matches!(ev, Ev::Evict) {
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

    fn waiting(&self, c: &Client) -> Vec<u64> {
        c.requested.iter().filter(|a| !c.outputs.contains_key(a)).map(|&a| self.project.actions[a].id).collect()
    }

    fn check(&self) -> Step {
        self.server.pool.invariants();
        self.server.sched.invariants();
        for w in self.workers.values() {
            w.sm.invariants();
        }
        for (h, n) in &self.server.pins {
            if *n > 0 && !self.server.store.contains_key(h) {
                return Err(Kind::LostPinned(format!("blob {} is pinned {n} times and not in the store", h.hex())));
            }
        }
        for (job, j) in &self.server.jobs {
            if let Some(w) = j.worker {
                assert!(self.server.pool.route(w, *job), "job {job} sent to worker {w} is routed there");
            }
        }
        Ok(())
    }

    fn step(&mut self, ev: Ev) -> Step {
        match ev {
            Ev::Deliver { from, to, epoch, msg } => self.deliver(from, to, epoch, msg),
            Ev::Connect { worker } => self.connect(worker),
            Ev::Finish { worker, token } => self.finish(worker, token),
            Ev::Cut { worker } => self.cut(worker),
            Ev::Evict => {
                self.evict();
                Ok(())
            }
            Ev::Round { client } => {
                self.round(client);
                Ok(())
            }
        }
    }

    fn deliver(&mut self, from: Node, to: Node, epoch: u64, msg: Msg) -> Step {
        let w = match (from, to) {
            (Node::Worker(w), _) | (_, Node::Worker(w)) => Some(w),
            _ => None,
        };
        if let Some(w) = w {
            let node = &self.workers[&w];
            if !node.up || node.epoch != epoch {
                return Ok(());
            }
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
                self.client_done(c, d)
            }
            (Node::Worker(w), Msg::Up(u)) => self.server_from_worker(w, u),
            (Node::Client(c), Msg::Up(u)) => self.server_from_client(c, u),
            (Node::Server, Msg::Up(_)) => unreachable!("the server does not write to itself"),
        }
    }

    fn connect(&mut self, worker: u64) -> Step {
        let now = self.now;
        let node = self.workers.get_mut(&worker).expect("known");
        if node.up {
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

    fn worker_effects(&mut self, worker: u64, fx: Vec<worker_state::Effect<Sim>>) -> Step {
        let mut queue: VecDeque<worker_state::Effect<Sim>> = fx.into();
        while let Some(e) = queue.pop_front() {
            match e {
                worker_state::Effect::Connect { after_ms } => {
                    let jitter = self.rng.range(0, 50);
                    self.schedule(after_ms + jitter, Ev::Connect { worker });
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
                    self.schedule(ms, Ev::Finish { worker, token });
                }
                worker_state::Effect::Abort { token } => {
                    self.workers.get_mut(&worker).expect("known").running.remove(&token);
                }
                worker_state::Effect::Report { .. } => {}
            }
        }
        Ok(())
    }

    fn finish(&mut self, worker: u64, token: u64) -> Step {
        let node = self.workers.get_mut(&worker).expect("known");
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
        let candidates: Vec<Hash> = self
            .server
            .store
            .keys()
            .filter(|h| ignore_pins || self.server.pins.get(h).copied().unwrap_or(0) == 0)
            .copied()
            .collect();
        if candidates.is_empty() {
            return;
        }
        let h = candidates[self.rng.below(candidates.len() as u64) as usize];
        self.server.store.remove(&h);
        self.report.evictions += 1;
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
            Up::Upload { blob } => {
                let h = Hash::of(&blob);
                self.server.store.insert(h, blob);
                self.pin(client, h);
            }
            Up::Hold { round } => {
                let old = self.server.held.insert(client, (round, BTreeSet::new()));
                for h in old.map(|(_, s)| s).unwrap_or_default() {
                    let n = self.server.pins.get_mut(&h).expect("a held blob is pinned");
                    *n -= 1;
                    if *n == 0 {
                        self.server.pins.remove(&h);
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
                    self.deliver_output(client, round, action, out, bytes);
                    return Ok(());
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

    fn deliver_output(&mut self, client: u64, round: u64, action: usize, out: Hash, bytes: Vec<u8>) {
        if self.server.held.get(&client).is_some_and(|(r, _)| *r == round) {
            self.pin(client, out);
        }
        self.send(Node::Server, Node::Client(client), Msg::Down(Down::Done { round, action, output: bytes }));
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
                self.deliver_output(j.client, j.round, j.action, out, output);
            }
            Up::Upload { .. } | Up::Hold { .. } | Up::Request { .. } => unreachable!("only clients say this"),
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

    fn round(&mut self, client: u64) {
        let c = self.clients.get_mut(&client).expect("known");
        c.round += 1;
        c.rounds_left -= 1;
        if c.round > 1 {
            for _ in 0..self.rng.range(1, 3) {
                let s = self.rng.below(self.cfg.sources as u64) as usize;
                c.versions[s] = self.rng.range(0, 2);
            }
        }
        c.outputs.clear();
        c.requested.clear();
        let round = c.round;
        let sources: Vec<Vec<u8>> = (0..self.cfg.sources).map(|s| model::source(s, c.versions[s])).collect();
        self.send(Node::Client(client), Node::Server, Msg::Up(Up::Hold { round }));
        for blob in sources {
            self.send(Node::Client(client), Node::Server, Msg::Up(Up::Upload { blob }));
        }
        self.request_ready(client);
    }

    fn request_ready(&mut self, client: u64) {
        let c = &self.clients[&client];
        let ready = self.project.ready(&|a| c.outputs.contains_key(&a), &|a| c.requested.contains(&a));
        for action in ready {
            let c = &self.clients[&client];
            let inputs = self.inputs_of(c, action).iter().map(|b| Hash::of(b)).collect();
            let round = c.round;
            self.clients.get_mut(&client).expect("known").requested.insert(action);
            self.send(Node::Client(client), Node::Server, Msg::Up(Up::Request { round, action, inputs }));
        }
    }

    fn client_done(&mut self, client: u64, d: Down) -> Step {
        let Down::Done { round, action, output } = d;
        let c = &self.clients[&client];
        if round != c.round || c.outputs.contains_key(&action) {
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
            if c.rounds_left == 0 {
                c.finished = true;
            } else {
                let at = self.rng.range(0, 1_000);
                self.schedule(at, Ev::Round { client });
            }
            return Ok(());
        }
        self.request_ready(client);
        Ok(())
    }
}

fn label(ev: &Ev) -> String {
    match ev {
        Ev::Deliver { from, to, epoch, msg } => {
            let what = match msg {
                Msg::Up(Up::Hello { .. }) => "hello",
                Msg::Up(Up::Result { .. }) => "result",
                Msg::Up(Up::Upload { .. }) => "upload",
                Msg::Up(Up::Hold { .. }) => "hold",
                Msg::Up(Up::Request { .. }) => "request",
                Msg::Work(_) => "work",
                Msg::Down(_) => "done",
            };
            format!("{from:?}>{to:?}@{epoch}:{what};")
        }
        Ev::Connect { worker } => format!("connect {worker};"),
        Ev::Finish { worker, token } => format!("finish {worker}/{token};"),
        Ev::Cut { worker } => format!("cut {worker};"),
        Ev::Evict => "evict;".to_string(),
        Ev::Round { client } => format!("round {client};"),
    }
}

pub fn run(seed: u64, bug: Option<Bug>) -> Result<Report, Failure> {
    World::new(seed, bug).run()
}
