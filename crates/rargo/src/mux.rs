use rargo_net::link::Link;
use rargo_proto::{CONTROL, ClientMsg, Digest, Frame, Reorder, ServerMsg};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::{Semaphore, oneshot};

const WINDOW: usize = 32 << 20;
const PING_EVERY: Duration = Duration::from_secs(10);
const DEAD_AFTER: Duration = Duration::from_secs(45);

const _: () = assert!(WINDOW <= u32::MAX as usize, "a window's permits fit acquire_many");
const _: () = assert!(DEAD_AFTER.as_secs() > 2 * PING_EVERY.as_secs());

fn dead_limit(kind: &str) -> Duration {
    let base = std::env::var("RARGO_LINK_DEAD_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|n| *n >= 1)
        .map(Duration::from_secs)
        .unwrap_or(DEAD_AFTER);
    if kind == "udp" { base } else { base * 8 }
}

struct Out {
    seq: u64,
    tx: UnboundedSender<(Vec<u8>, usize)>,
    urgent: UnboundedSender<Vec<u8>>,
}

pub struct Mux {
    link: Arc<Link>,
    out: Mutex<Out>,
    window: Arc<Semaphore>,
    streams: Mutex<HashMap<u32, UnboundedSender<ServerMsg>>>,
    next_stream: AtomicU32,
    next_id: AtomicU64,
    missing: Mutex<HashMap<u64, oneshot::Sender<(Vec<Digest>, Vec<Digest>)>>>,
    dead: AtomicBool,
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
            streams: Mutex::new(HashMap::new()),
            next_stream: AtomicU32::new(1),
            next_id: AtomicU64::new(1),
            missing: Mutex::new(HashMap::new()),
            dead: AtomicBool::new(false),
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
            let mut order: Reorder<ServerMsg> = Reorder::new(true);
            while let Some(raw) = m.link.recv().await {
                m.bytes_in.fetch_add(raw.len() as u64, Ordering::Relaxed);
                *m.last_heard.lock().unwrap() = Instant::now();
                let Ok(frame) = rargo_proto::decode::<Frame<ServerMsg>>(&raw) else { break };
                if frame.stream == rargo_proto::KEEPALIVE {
                    continue;
                }
                for (stream, msg) in order.push(frame) {
                    let msg = match msg {
                        ServerMsg::Packed(z) => match rargo_proto::unpack(&z).and_then(|b| rargo_proto::decode(&b)) {
                            Ok(m) => m,
                            Err(_) => continue,
                        },
                        m => m,
                    };
                    if stream == CONTROL {
                        m.control(msg);
                        continue;
                    }
                    let last = matches!(msg, ServerMsg::Exit(_) | ServerMsg::Exec(_) | ServerMsg::Fetched { .. });
                    let tx = m.streams.lock().unwrap().get(&stream).cloned();
                    if let Some(tx) = tx {
                        let _ = tx.send(msg);
                    }
                    if last {
                        order.end(stream);
                        m.streams.lock().unwrap().remove(&stream);
                    }
                }
            }
            m.die();
        });
        let m = mux.clone();
        tokio::spawn(async move {
            let limit = dead_limit(m.link.kind());
            // A live server answers a ping well inside the dead limit, so silence means it is gone.
            let ping_every = PING_EVERY.min(limit / 3).max(Duration::from_millis(500));
            let step = Duration::from_millis(200);
            let mut since_ping = Duration::ZERO;
            let mut n = 0u64;
            loop {
                tokio::time::sleep(step).await;
                if m.is_dead() {
                    break;
                }
                let frames = m.last_heard.lock().unwrap().elapsed();
                let quiet = if m.link.kind() == "udp" { frames.min(m.link.idle()) } else { frames };
                if quiet > limit {
                    m.die();
                    break;
                }
                since_ping += step;
                if since_ping >= ping_every {
                    since_ping = Duration::ZERO;
                    n += 1;
                    let ping = rargo_proto::encode(&Frame { stream: rargo_proto::KEEPALIVE, seq: 0, msg: ClientMsg::Ping(n) });
                    let _ = m.out.lock().unwrap().urgent.send(ping);
                }
            }
        });
        mux
    }

    pub fn kind(&self) -> &'static str {
        self.link.kind()
    }

    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::SeqCst) || self.link.is_closed()
    }

    /// Resolves when the link is closed. A missed wake is caught on the next check.
    pub async fn until_dead(&self) {
        loop {
            if self.is_dead() {
                return;
            }
            tokio::select! {
                _ = self.gone.notified() => return,
                _ = tokio::time::sleep(Duration::from_millis(200)) => {}
            }
        }
    }

    pub fn die(&self) {
        if self.dead.swap(true, Ordering::SeqCst) {
            return;
        }
        self.link.close();
        self.streams.lock().unwrap().clear();
        self.missing.lock().unwrap().clear();
        self.window.close();
        self.gone.notify_waiters();
    }

    fn control(&self, msg: ServerMsg) {
        match msg {
            ServerMsg::Missing { id, trees, blobs } => {
                if let Some(tx) = self.missing.lock().unwrap().remove(&id) {
                    let _ = tx.send((trees, blobs));
                }
            }
            _ => {}
        }
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
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "the server went away"))?;
        permit.forget();
        self.queue(stream, msg, n);
        Ok(())
    }

    pub fn open(&self) -> (u32, UnboundedSender<ServerMsg>, UnboundedReceiver<ServerMsg>) {
        let id = self.next_stream.fetch_add(1, Ordering::Relaxed);
        assert_ne!(id, CONTROL);
        assert_ne!(id, rargo_proto::KEEPALIVE);
        let (tx, rx) = unbounded_channel();
        assert!(self.streams.lock().unwrap().insert(id, tx.clone()).is_none());
        (id, tx, rx)
    }

    pub fn cancel(&self, stream: u32) {
        if self.streams.lock().unwrap().remove(&stream).is_some() {
            self.post(stream, &ClientMsg::Cancel);
        }
    }

    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn expect_missing(&self, id: u64) -> oneshot::Receiver<(Vec<Digest>, Vec<Digest>)> {
        let (tx, rx) = oneshot::channel();
        assert!(self.missing.lock().unwrap().insert(id, tx).is_none(), "offer {id} awaited twice");
        rx
    }

    pub fn fetch(&self, hashes: Vec<Digest>, to: &UnboundedSender<ServerMsg>) -> u64 {
        self.ask(to, |id| ClientMsg::Fetch { id, hashes })
    }

    pub fn ask(&self, to: &UnboundedSender<ServerMsg>, question: impl FnOnce(u64) -> ClientMsg) -> u64 {
        let stream = self.next_stream.fetch_add(1, Ordering::Relaxed);
        assert_ne!(stream, CONTROL);
        assert_ne!(stream, rargo_proto::KEEPALIVE);
        assert!(self.streams.lock().unwrap().insert(stream, to.clone()).is_none());
        let id = stream as u64;
        self.post(stream, &question(id));
        id
    }
}
