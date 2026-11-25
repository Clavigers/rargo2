use rargo_net::link::Link;
use rargo_proto::{CONTROL, ClientMsg, Digest, Frame, Reorder, ServerMsg};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::{Semaphore, oneshot};

const WINDOW: usize = 32 << 20;
const PING_EVERY: Duration = Duration::from_secs(10);
const DEAD_AFTER: Duration = Duration::from_secs(45);

const _: () = assert!(WINDOW <= u32::MAX as usize, "a window's permits fit acquire_many");
const _: () = assert!(DEAD_AFTER.as_secs() > 2 * PING_EVERY.as_secs());

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

    #[cfg(test)]
    fn invariants(&self) -> bool {
        self.streams.keys().all(|&id| id > CONTROL && id < self.next_stream && id != rargo_proto::KEEPALIVE)
            && (!self.dead || (self.streams.is_empty() && self.missing.is_empty()))
    }
}

type StreamSender = UnboundedSender<ServerMsg>;
type WireState = MuxState<StreamSender, oneshot::Sender<Missing>>;

pub struct Mux {
    link: Arc<Link>,
    out: Mutex<Out>,
    window: Arc<Semaphore>,
    state: Mutex<WireState>,
    pub gone: tokio::sync::Notify,
    last_heard: Mutex<Instant>,
    pub bytes_out: AtomicU64,
    pub bytes_in: AtomicU64,
}

impl Mux {
    pub fn new(link: Link) -> Arc<Mux> {
        let link = Arc::new(link);
        let (tx, mut rx) = unbounded_channel::<(Vec<u8>, usize)>();
        let (urgent, mut urgent_rx) = unbounded_channel::<Vec<u8>>();
        let mux = Arc::new(Mux {
            link: link.clone(),
            out: Mutex::new(Out { seq: 0, tx, urgent }),
            window: Arc::new(Semaphore::new(WINDOW)),
            state: Mutex::new(MuxState::new()),
            gone: tokio::sync::Notify::new(),
            last_heard: Mutex::new(Instant::now()),
            bytes_out: AtomicU64::new(0),
            bytes_in: AtomicU64::new(0),
        });
        let m = mux.clone();
        tokio::spawn(async move {
            loop {
                let (frame, bulk) = tokio::select! {
                    biased;
                    u = urgent_rx.recv() => match u {
                        Some(f) => (f, 0),
                        None => break,
                    },
                    f = rx.recv() => match f {
                        Some(f) => f,
                        None => break,
                    },
                };
                m.bytes_out.fetch_add(frame.len() as u64, Ordering::Relaxed);
                let sent = m.link.send(&frame).await;
                if bulk > 0 {
                    m.window.add_permits(bulk);
                }
                if sent.is_err() {
                    break;
                }
            }
            m.die();
        });
        let m = mux.clone();
        tokio::spawn(async move {
            while let Some(raw) = m.link.recv().await {
                m.bytes_in.fetch_add(raw.len() as u64, Ordering::Relaxed);
                *m.last_heard.lock().unwrap() = Instant::now();
                let Ok(frame) = rargo_proto::decode::<Frame<ServerMsg>>(&raw) else { break };
                let ordered = m.state.lock().unwrap().incoming(frame);
                for (stream, msg) in ordered {
                    let msg = match msg {
                        ServerMsg::Packed(z) => match rargo_proto::unpack(&z).and_then(|b| rargo_proto::decode(&b)) {
                            Ok(m) => m,
                            Err(_) => continue,
                        },
                        m => m,
                    };
                    let action = m.state.lock().unwrap().route(stream, msg);
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
        tokio::spawn(async move {
            let mut n = 0u64;
            loop {
                tokio::time::sleep(PING_EVERY).await;
                if m.is_dead() {
                    break;
                }
                let frames = m.last_heard.lock().unwrap().elapsed();
                let quiet = if m.link.kind() == "udp" { frames.min(m.link.idle()) } else { frames };
                let limit = if m.link.kind() == "udp" { DEAD_AFTER } else { DEAD_AFTER * 8 };
                if quiet > limit {
                    m.die();
                    break;
                }
                n += 1;
                let ping = rargo_proto::encode(&Frame { stream: rargo_proto::KEEPALIVE, seq: 0, msg: ClientMsg::Ping(n) });
                let _ = m.out.lock().unwrap().urgent.send(ping);
            }
        });
        mux
    }

    pub fn kind(&self) -> &'static str {
        self.link.kind()
    }

    pub fn is_dead(&self) -> bool {
        self.state.lock().unwrap().dead || self.link.is_closed()
    }

    pub fn die(&self) {
        let drained = self.state.lock().unwrap().fail();
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
        let mut out = self.out.lock().unwrap();
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

    pub async fn post_bulk(&self, stream: u32, msg: &ClientMsg, bytes: usize) -> std::io::Result<()> {
        let n = bytes.clamp(1, WINDOW);
        assert!(n >= 1);
        assert!(n <= WINDOW);
        let permit = self
            .window
            .acquire_many(n as u32)
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "the connection to the server was lost"))?;
        permit.forget();
        self.queue(stream, msg, n);
        Ok(())
    }

    pub fn open(&self) -> (u32, UnboundedSender<ServerMsg>, UnboundedReceiver<ServerMsg>) {
        let (tx, rx) = unbounded_channel();
        let (id, rejected) = self.state.lock().unwrap().open(tx.clone());
        drop(rejected);
        (id, tx, rx)
    }

    pub fn cancel(&self, stream: u32) {
        let closed = self.state.lock().unwrap().close(stream);
        if closed.is_some() {
            self.post(stream, &ClientMsg::Cancel);
        }
    }

    pub fn next_id(&self) -> u64 {
        self.state.lock().unwrap().next_id()
    }

    pub fn expect_missing(&self, id: u64) -> oneshot::Receiver<Missing> {
        let (tx, rx) = oneshot::channel();
        let rejected = self.state.lock().unwrap().expect_missing(id, tx);
        drop(rejected);
        rx
    }

    pub fn fetch(&self, hashes: Vec<Digest>, to: &UnboundedSender<ServerMsg>) -> u64 {
        self.ask(to, |id| ClientMsg::Fetch { id, hashes })
    }

    pub fn ask(&self, to: &UnboundedSender<ServerMsg>, question: impl FnOnce(u64) -> ClientMsg) -> u64 {
        let (stream, rejected) = self.state.lock().unwrap().open(to.clone());
        drop(rejected);
        let id = stream as u64;
        self.post(stream, &question(id));
        id
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reorder_and_terminal_frames_stay_with_their_own_stream() {
        let mut state = MuxState::<_, ()>::new();
        let (first, _) = state.open("first");
        assert!(state.invariants());
        let (second, _) = state.open("second");
        assert!(state.invariants());

        let stdout = |byte| ServerMsg::Stdout(vec![byte]);
        assert!(state.incoming(Frame { stream: first, seq: 1, msg: ServerMsg::Exit(0) }).is_empty());
        assert!(state.invariants());
        let msgs = state.incoming(Frame { stream: second, seq: 0, msg: stdout(2) });
        assert!(state.invariants());
        let [(stream, msg)] = msgs.try_into().ok().unwrap();
        assert!(matches!(state.route(stream, msg), Route::Stream("second", ServerMsg::Stdout(bytes)) if bytes == [2]));
        assert!(state.invariants());

        let msgs = state.incoming(Frame { stream: first, seq: 0, msg: stdout(1) });
        assert!(state.invariants());
        let [(stream, msg), (last_stream, last)] = msgs.try_into().ok().unwrap();
        assert!(matches!(state.route(stream, msg), Route::Stream("first", ServerMsg::Stdout(bytes)) if bytes == [1]));
        assert!(state.invariants());
        assert!(matches!(state.route(last_stream, last), Route::Stream("first", ServerMsg::Exit(0))));
        assert!(state.invariants());
        assert!(state.incoming(Frame { stream: first, seq: 2, msg: stdout(9) }).is_empty());
        assert!(state.invariants());
        assert!(matches!(state.route(first, stdout(9)), Route::Drop));
        assert!(state.invariants());
    }

    #[test]
    fn failure_drains_every_open_registration_once() {
        let mut state = MuxState::new();
        let (closed, _) = state.open("cancelled");
        assert!(state.invariants());
        let (a, _) = state.open("first");
        assert!(state.invariants());
        let (b, _) = state.open("second");
        assert!(state.invariants());
        assert_eq!(state.close(closed), Some("cancelled"));
        assert!(state.invariants());
        let id = state.next_id();
        assert!(state.invariants());
        assert_eq!(state.expect_missing(id, "offer"), None);
        assert!(state.invariants());
        let (streams, offers) = state.fail().unwrap();
        assert!(state.invariants());
        assert_eq!(streams, BTreeMap::from([(a, "first"), (b, "second")]));
        assert_eq!(offers, BTreeMap::from([(id, "offer")]));
        assert!(state.fail().is_none());
        assert!(state.invariants());
        assert_eq!(state.open("late").1, Some("late"));
        assert!(state.invariants());
        assert_eq!(state.expect_missing(id, "late offer"), Some("late offer"));
        assert!(state.invariants());
    }
}
