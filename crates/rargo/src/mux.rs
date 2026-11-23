//! The daemon's end of its link to the server: every command a stream,
//! stream 0 for what the daemon does on its own.

use rargo_net::link::Link;
use rargo_proto::{CONTROL, ClientMsg, Digest, Frame, Reorder, ServerMsg};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::{Semaphore, oneshot};

/// Bytes of file contents that may wait to be sent.
const WINDOW: usize = 32 << 20;
/// A keepalive this often keeps a NAT's mapping alive and tells the server
/// where the client is now.
const PING_EVERY: Duration = Duration::from_secs(10);
/// Nothing heard for this long: the link is dead.
const DEAD_AFTER: Duration = Duration::from_secs(45);

struct Out {
    seq: u64,
    tx: UnboundedSender<(Vec<u8>, usize)>,
    /// Keepalives: sent ahead of whatever is queued.
    urgent: UnboundedSender<Vec<u8>>,
}

pub struct Mux {
    link: Arc<Link>,
    out: Mutex<Out>,
    window: Arc<Semaphore>,
    streams: Mutex<HashMap<u32, UnboundedSender<ServerMsg>>>,
    next_stream: AtomicU32,
    next_id: AtomicU64,
    /// Who waits for the answer to an offer.
    missing: Mutex<HashMap<u64, oneshot::Sender<(Vec<Digest>, Vec<Digest>)>>>,
    /// Trees and file contents the server is believed to hold: what this
    /// daemon sent it or was told it has. Only ever a hint: the server asks
    /// for whatever it turns out to lack.
    pub sent: Mutex<HashSet<Digest>>,
    dead: AtomicBool,
    /// Woken when the link dies.
    pub gone: tokio::sync::Notify,
    /// When the server was last heard from (any frame).
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
            sent: Mutex::new(HashSet::new()),
            dead: AtomicBool::new(false),
            gone: tokio::sync::Notify::new(),
            last_heard: Mutex::new(Instant::now()),
            bytes_out: AtomicU64::new(0),
            bytes_in: AtomicU64::new(0),
        });
        // Writer: frames leave in the order they were queued.
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
        // Reader.
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
        // Keepalives.
        let m = mux.clone();
        tokio::spawn(async move {
            let mut n = 0u64;
            loop {
                tokio::time::sleep(PING_EVERY).await;
                if m.is_dead() {
                    break;
                }
                if m.last_heard.lock().unwrap().elapsed() > DEAD_AFTER {
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
        self.dead.load(Ordering::SeqCst) || self.link.is_closed()
    }

    /// The link is gone: everyone waiting on it is told so (their channels
    /// close).
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
        // File contents are compressed already; manifests are not.
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

    /// Queues a message. Messages reach the server in the order they were
    /// queued, whatever their streams.
    pub fn post(&self, stream: u32, msg: &ClientMsg) {
        self.queue(stream, msg, 0);
    }

    /// Queues file contents, waiting while too much is queued already.
    pub async fn post_bulk(&self, stream: u32, msg: &ClientMsg, bytes: usize) -> std::io::Result<()> {
        let n = bytes.clamp(1, WINDOW);
        let permit = self
            .window
            .acquire_many(n as u32)
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "the connection to the server was lost"))?;
        permit.forget();
        self.queue(stream, msg, n);
        Ok(())
    }

    /// Opens a stream for a command.
    pub fn open(&self) -> (u32, UnboundedSender<ServerMsg>, UnboundedReceiver<ServerMsg>) {
        let id = self.next_stream.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = unbounded_channel();
        self.streams.lock().unwrap().insert(id, tx.clone());
        (id, tx, rx)
    }

    /// Gives a stream up before its command has finished.
    pub fn cancel(&self, stream: u32) {
        if self.streams.lock().unwrap().remove(&stream).is_some() {
            self.post(stream, &ClientMsg::Cancel);
        }
    }

    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Registers for the answer to the offer `id`.
    pub fn expect_missing(&self, id: u64) -> oneshot::Receiver<(Vec<Digest>, Vec<Digest>)> {
        let (tx, rx) = oneshot::channel();
        self.missing.lock().unwrap().insert(id, tx);
        rx
    }

    /// Asks the server for contents, on a stream of its own; they arrive
    /// on `to`, followed by [`ServerMsg::Fetched`] with the returned id.
    pub fn fetch(&self, hashes: Vec<Digest>, to: &UnboundedSender<ServerMsg>) -> u64 {
        self.ask(to, |id| ClientMsg::Fetch { id, hashes })
    }

    /// Sends a question on a stream of its own; the answer arrives on
    /// `to`, ending with [`ServerMsg::Fetched`] with the returned id.
    pub fn ask(&self, to: &UnboundedSender<ServerMsg>, question: impl FnOnce(u64) -> ClientMsg) -> u64 {
        let stream = self.next_stream.fetch_add(1, Ordering::Relaxed);
        self.streams.lock().unwrap().insert(stream, to.clone());
        let id = stream as u64;
        self.post(stream, &question(id));
        id
    }
}
