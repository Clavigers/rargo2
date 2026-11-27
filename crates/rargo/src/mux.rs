#[cfg(test)]
use rargo_machine::StateMachine;
use rargo_net::io::{self, UnboundedReceiver, UnboundedSender, unbounded_channel};
use rargo::link_watch::{self, Silence, Tick, Watch};
use rargo_net::link::Link;
use rargo_proto::{CONTROL, ClientMsg, Digest, Frame, Reorder, ServerMsg};
use std::collections::BTreeMap;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

const WINDOW: usize = 32 << 20;
const WATCH_STEP: Duration = Duration::from_millis(200);

const _: () = assert!(WINDOW <= u32::MAX as usize, "a window's permits fit acquire_many");

fn dead_limit(kind: &str) -> Duration {
    let set = io::env::var("RARGO_LINK_DEAD_SECS").ok().and_then(|v| v.parse::<u64>().ok()).filter(|n| *n >= 1).map(Duration::from_secs);
    match (set, kind) {
        (Some(base), "udp") => base,
        (Some(base), _) => base * 8,
        (None, "udp") => link_watch::SILENT_AFTER,
        (None, _) => link_watch::SILENT_AFTER_TCP,
    }
}

struct Out {
    seq: u64,
    tx: UnboundedSender<(Vec<u8>, usize)>,
    urgent: UnboundedSender<Vec<u8>>,
}

type Missing = (Vec<Digest>, Vec<Digest>);

struct MuxState<S, M> {
    streams: BTreeMap<u32, S>,
    missing: BTreeMap<u64, M>,
    order: Reorder<ServerMsg>,
    next_stream: u32,
    next_id: u64,
    dead: bool,
}

enum Route<S, M> {
    Stream(S, ServerMsg),
    Missing(M, Missing),
    Drop,
}

impl<S: Clone, M> MuxState<S, M> {
    fn new() -> Self {
        Self { streams: BTreeMap::new(), missing: BTreeMap::new(), order: Reorder::new(true), next_stream: 1, next_id: 1, dead: false }
    }

    fn open(&mut self, dest: S) -> (u32, Option<S>) {
        let stream = self.next_stream;
        assert_ne!(stream, CONTROL);
        assert_ne!(stream, rargo_proto::KEEPALIVE);
        self.next_stream += 1;
        if self.dead {
            return (stream, Some(dest));
        }
        assert!(self.streams.insert(stream, dest).is_none());
        (stream, None)
    }

    fn close(&mut self, stream: u32) -> Option<S> {
        self.order.end(stream);
        self.streams.remove(&stream)
    }

    fn incoming(&mut self, frame: Frame<ServerMsg>) -> Vec<(u32, ServerMsg)> {
        if self.dead || (frame.stream != CONTROL && !self.streams.contains_key(&frame.stream)) {
            Vec::new()
        } else {
            self.order.push(frame)
        }
    }

    fn route(&mut self, stream: u32, msg: ServerMsg) -> Route<S, M> {
        if stream == CONTROL {
            if let ServerMsg::Missing { id, trees, blobs } = msg
                && let Some(dest) = self.missing.remove(&id)
            {
                return Route::Missing(dest, (trees, blobs));
            }
            return Route::Drop;
        }
        let dest = if matches!(msg, ServerMsg::Exit(_) | ServerMsg::Exec(_) | ServerMsg::Fetched { .. }) {
            self.close(stream)
        } else {
            self.streams.get(&stream).cloned()
        };
        match dest {
            Some(dest) => Route::Stream(dest, msg),
            None => Route::Drop,
        }
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn expect_missing(&mut self, id: u64, dest: M) -> Option<M> {
        if self.dead {
            return Some(dest);
        }
        assert!(self.missing.insert(id, dest).is_none(), "offer {id} awaited twice");
        None
    }

    fn fail(&mut self) -> Option<(BTreeMap<u32, S>, BTreeMap<u64, M>)> {
        if self.dead {
            return None;
        }
        self.dead = true;
        self.order = Reorder::new(true);
        Some((std::mem::take(&mut self.streams), std::mem::take(&mut self.missing)))
    }
}

#[cfg(test)]
impl<S: Clone, M> StateMachine for MuxState<S, M> {
    fn invariants(&self) {
        for &id in self.streams.keys() {
            assert!(id > CONTROL && id != rargo_proto::KEEPALIVE, "stream {id} is not a reserved id");
            assert!(id < self.next_stream, "stream {id} was handed out, below {}", self.next_stream);
        }
        if self.dead {
            assert!(self.streams.is_empty(), "a dead mux has no streams");
            assert!(self.missing.is_empty(), "a dead mux waits for nothing");
        }
    }
}

type StreamSender = UnboundedSender<ServerMsg>;
type WireState = MuxState<StreamSender, io::oneshot::Sender<Missing>>;

enum Outgoing {
    Urgent(Option<Vec<u8>>),
    Bulk(Option<(Vec<u8>, usize)>),
}

enum DeadWait {
    Gone,
    Tick,
}

/// Dials the server afresh and says whether it answered: how a silent link tells a restarted server from a gone one.
pub type Probe = Rc<dyn Fn() -> bool>;

pub struct Mux {
    link: Rc<Link>,
    out: RefCell<Out>,
    window: io::Semaphore,
    state: RefCell<WireState>,
    pub gone: io::Notify,
    started: Instant,
    watch: RefCell<Watch>,
    pub bytes_out: Cell<u64>,
    pub bytes_in: Cell<u64>,
}

impl Mux {
    /// A link over UDP that goes quiet is probed with `probe`; over TCP a quiet link may just be queued behind its data.
    pub fn new(link: Link, probe: Option<Probe>) -> Rc<Mux> {
        let probe = probe.filter(|_| link.kind() == "udp");
        let link = Rc::new(link);
        let (tx, mut rx) = unbounded_channel::<(Vec<u8>, usize)>();
        let (urgent, mut urgent_rx) = unbounded_channel::<Vec<u8>>();
        let mux = Rc::new(Mux {
            link: link.clone(),
            out: RefCell::new(Out { seq: 0, tx, urgent }),
            window: io::Semaphore::new(WINDOW as u32),
            state: RefCell::new(MuxState::new()),
            gone: io::Notify::new(),
            started: io::now(),
            watch: RefCell::new(Watch::new(Duration::ZERO, dead_limit(link.kind()))),
            bytes_out: Cell::new(0),
            bytes_in: Cell::new(0),
        });
        let m = mux.clone();
        io::spawn(move || {
            loop {
                let got = io::race_biased(vec![
                    Box::new(|| Outgoing::Urgent(urgent_rx.recv())),
                    Box::new(|| Outgoing::Bulk(rx.recv())),
                ]);
                let (frame, bulk) = match got {
                    Outgoing::Urgent(Some(frame)) => (frame, 0),
                    Outgoing::Bulk(Some(frame)) => frame,
                    Outgoing::Urgent(None) | Outgoing::Bulk(None) => break,
                };
                m.bytes_out.set(m.bytes_out.get() + frame.len() as u64);
                let sent = m.link.send(&frame);
                if bulk > 0 {
                    m.window.add_permits(bulk as u32);
                }
                if sent.is_err() {
                    break;
                }
            }
            m.die();
        });
        let m = mux.clone();
        io::spawn(move || {
            while let Some(raw) = m.link.recv() {
                m.bytes_in.set(m.bytes_in.get() + raw.len() as u64);
                let now = io::now().saturating_duration_since(m.started);
                m.watch.borrow_mut().heard(now);
                let Ok(frame) = rargo_proto::decode::<Frame<ServerMsg>>(&raw) else { break };
                let ordered = m.state.borrow_mut().incoming(frame);
                for (stream, msg) in ordered {
                    let msg = match msg {
                        ServerMsg::Packed(z) => match rargo_proto::unpack(&z).and_then(|b| rargo_proto::decode(&b)) {
                            Ok(m) => m,
                            Err(_) => continue,
                        },
                        m => m,
                    };
                    let action = m.state.borrow_mut().route(stream, msg);
                    match action {
                        Route::Stream(dest, msg) => { let _ = dest.send(msg); }
                        Route::Missing(dest, missing) => { let _ = dest.send(missing); }
                        Route::Drop => {}
                    }
                }
            }
            m.die();
        });
        let m = mux.clone();
        io::spawn(move || {
            loop {
                io::sleep(WATCH_STEP);
                if m.is_dead() {
                    break;
                }
                let now = io::now().saturating_duration_since(m.started);
                let idle = (m.link.kind() == "udp").then(|| m.link.idle());
                let tick = {
                    let mut watch = m.watch.borrow_mut();
                    if let Some(idle) = idle {
                        watch.heard(now.saturating_sub(idle));
                    }
                    watch.tick(now)
                };
                match tick {
                    Tick::Wait => {}
                    Tick::Probe => {
                        if let Some(probe) = probe.clone() {
                            let p = m.clone();
                            io::spawn(move || {
                                let answered = probe();
                                let now = io::now().saturating_duration_since(p.started);
                                if p.watch.borrow_mut().probed(now, answered).is_some() {
                                    p.die();
                                }
                            });
                        }
                    }
                    Tick::Ping(n) => {
                        let ping = rargo_proto::encode(&Frame { stream: rargo_proto::KEEPALIVE, seq: 0, msg: ClientMsg::Ping(n) });
                        let urgent = m.out.borrow().urgent.clone();
                        let _ = urgent.send(ping);
                    }
                    Tick::Silent(_) => {
                        m.die();
                        break;
                    }
                }
            }
        });
        mux
    }

    pub fn kind(&self) -> &'static str {
        self.link.kind()
    }

    /// Why this link gave up on its server, and after how long a silence, if that is why it closed.
    pub fn silent(&self) -> Option<Silence> {
        self.watch.borrow().silent()
    }

    pub fn is_dead(&self) -> bool {
        let dead = self.state.borrow().dead;
        dead || self.link.is_closed()
    }

    /// Resolves when the link is closed. A missed wake is caught on the next check.
    pub fn until_dead(&self) {
        loop {
            if self.is_dead() {
                return;
            }
            match io::race_biased(vec![
                Box::new(|| { self.gone.notified(); DeadWait::Gone }),
                Box::new(|| { io::sleep(Duration::from_millis(200)); DeadWait::Tick }),
            ]) {
                DeadWait::Gone => return,
                DeadWait::Tick => {}
            }
        }
    }

    pub fn die(&self) {
        let drained = self.state.borrow_mut().fail();
        let Some(endpoints) = drained else { return };
        self.link.close();
        drop(endpoints);
        self.window.close();
        self.gone.notify_waiters();
    }

    fn queue(&self, stream: u32, msg: &ClientMsg, bulk: usize) {
        let mut encoded_msg = None;
        if matches!(msg, ClientMsg::Trees(_) | ClientMsg::Env(_) | ClientMsg::Run(_) | ClientMsg::Fetch { .. }) {
            if let Some(z) = rargo_proto::pack(&rargo_proto::encode(msg)) {
                encoded_msg = Some(ClientMsg::Packed(z));
            }
        }
        let mut out = self.out.borrow_mut();
        let frame = match &encoded_msg {
            Some(packed) => rargo_proto::encode(&Frame { stream, seq: out.seq, msg: packed }),
            None => rargo_proto::encode(&Frame { stream, seq: out.seq, msg }),
        };
        out.seq += 1;
        let _ = out.tx.send((frame, bulk));
    }

    pub fn post(&self, stream: u32, msg: &ClientMsg) {
        self.queue(stream, msg, 0);
    }

    pub fn post_bulk(&self, stream: u32, msg: &ClientMsg, bytes: usize) -> std::io::Result<()> {
        let n = bytes.clamp(1, WINDOW);
        assert!(n >= 1);
        assert!(n <= WINDOW);
        let permit = self.window.acquire_many(n as u32)?;
        permit.forget();
        self.queue(stream, msg, n);
        Ok(())
    }

    pub fn open(&self) -> (u32, UnboundedSender<ServerMsg>, UnboundedReceiver<ServerMsg>) {
        let (tx, rx) = unbounded_channel();
        let (id, rejected) = self.state.borrow_mut().open(tx.clone());
        drop(rejected);
        (id, tx, rx)
    }

    pub fn cancel(&self, stream: u32) {
        let closed = self.state.borrow_mut().close(stream);
        if closed.is_some() {
            self.post(stream, &ClientMsg::Cancel);
        }
    }

    pub fn next_id(&self) -> u64 {
        self.state.borrow_mut().next_id()
    }

    pub fn expect_missing(&self, id: u64) -> io::oneshot::Receiver<Missing> {
        let (tx, rx) = io::oneshot::channel();
        let rejected = self.state.borrow_mut().expect_missing(id, tx);
        drop(rejected);
        rx
    }

    pub fn fetch(&self, hashes: Vec<Digest>, to: &UnboundedSender<ServerMsg>) -> u64 {
        self.ask(to, |id| ClientMsg::Fetch { id, hashes })
    }

    pub fn ask(&self, to: &UnboundedSender<ServerMsg>, question: impl FnOnce(u64) -> ClientMsg) -> u64 {
        let (stream, rejected) = self.state.borrow_mut().open(to.clone());
        drop(rejected);
        let id = stream as u64;
        self.post(stream, &question(id));
        id
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use expect_test::expect;

    enum Ev {
        Open(&'static str),
        Close(u32),
        Frame(u32, u64, ServerMsg),
        Route(u32, ServerMsg),
        NextId,
        ExpectMissing(u64, &'static str),
        Fail,
    }

    type State = MuxState<&'static str, &'static str>;

    fn routed(state: &mut State, stream: u32, msg: ServerMsg) -> String {
        match state.route(stream, msg) {
            Route::Stream(dest, msg) => format!("{dest} <- {msg:?}"),
            Route::Missing(dest, missing) => format!("{dest} <- missing {missing:?}"),
            Route::Drop => "dropped".to_string(),
        }
    }

    /// Steps a mux through the events, checking its invariants after each, and lists what each step gave back.
    fn run(events: impl IntoIterator<Item = Ev>) -> String {
        let mut state = State::new();
        state.invariants();
        let mut out = String::new();
        for ev in events {
            let line = match ev {
                Ev::Open(dest) => format!("open {dest} => {:?}", state.open(dest)),
                Ev::Close(stream) => format!("close {stream} => {:?}", state.close(stream)),
                Ev::Frame(stream, seq, msg) => {
                    let ready = state.incoming(Frame { stream, seq, msg });
                    let routes: Vec<String> = ready.into_iter().map(|(s, m)| routed(&mut state, s, m)).collect();
                    format!("frame {stream}#{seq} => {routes:?}")
                }
                Ev::Route(stream, msg) => format!("route {stream} => {}", routed(&mut state, stream, msg)),
                Ev::NextId => format!("next id => {}", state.next_id()),
                Ev::ExpectMissing(id, dest) => format!("expect missing {id} for {dest} => {:?}", state.expect_missing(id, dest)),
                Ev::Fail => format!("fail => {:?}", state.fail()),
            };
            state.invariants();
            out += &line;
            out.push('\n');
        }
        out
    }

    fn stdout(byte: u8) -> ServerMsg {
        ServerMsg::Stdout(vec![byte])
    }

    #[test]
    fn reorder_and_terminal_frames_stay_with_their_own_stream() {
        let events = [
            Ev::Open("first"),
            Ev::Open("second"),
            Ev::Frame(1, 1, ServerMsg::Exit(0)),
            Ev::Frame(2, 0, stdout(2)),
            Ev::Frame(1, 0, stdout(1)),
            Ev::Frame(1, 2, stdout(9)),
            Ev::Route(1, stdout(9)),
        ];
        expect![[r#"
            open first => (1, None)
            open second => (2, None)
            frame 1#1 => []
            frame 2#0 => ["second <- Stdout([2])"]
            frame 1#0 => ["first <- Stdout([1])", "first <- Exit(0)"]
            frame 1#2 => []
            route 1 => dropped
        "#]].assert_eq(&run(events));
    }

    #[test]
    fn failure_drains_every_open_registration_once() {
        let events = [
            Ev::Open("cancelled"),
            Ev::Open("first"),
            Ev::Open("second"),
            Ev::Close(1),
            Ev::NextId,
            Ev::ExpectMissing(1, "offer"),
            Ev::Fail,
            Ev::Fail,
            Ev::Open("late"),
            Ev::ExpectMissing(1, "late offer"),
        ];
        expect![[r#"
            open cancelled => (1, None)
            open first => (2, None)
            open second => (3, None)
            close 1 => Some("cancelled")
            next id => 1
            expect missing 1 for offer => None
            fail => Some(({2: "first", 3: "second"}, {1: "offer"}))
            fail => None
            open late => (4, Some("late"))
            expect missing 1 for late offer => Some("late offer")
        "#]].assert_eq(&run(events));
    }
}
