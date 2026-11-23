//! A reliable message transport over UDP, used between the server and its
//! workers and between the server and the client daemons, and the
//! client-server [`link`] built on it.
//!
//! Messages of any size are cut into datagram-sized fragments, each
//! acknowledged individually and retransmitted on a timer. Messages are
//! independent: a lost fragment delays only its own message, never the ones
//! behind it, which matters because a worker has many file-system requests
//! and bulk transfers in flight at once. Delivery order between messages is
//! arrival order; the RPC layer above matches replies to requests by id.
//!
//! Congestion control is deliberately simple (slow start, additive
//! increase, halving on timeout). A connection lives as long as its two
//! ends do: its window and round-trip estimate carry over from one command
//! to the next, so nothing after the first exchange pays for a handshake
//! or a slow start. A peer is known by its connection id, not its address:
//! an authenticated datagram from a new address moves the connection there
//! (a client that changed networks, a NAT that picked a new port).

pub mod link;

use parking_lot::Mutex;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::{Notify, mpsc};

/// Payload bytes per datagram. Stays under a 1500-byte MTU with headers.
const FRAG: usize = 1200;
const T_DATA: u8 = 1;
const T_ACK: u8 = 2;
const DATA_HDR: usize = 1 + 8 + 8 + 4 + 4;
const MIN_RTO: Duration = Duration::from_millis(10);
const MAX_RTO: Duration = Duration::from_secs(2);
const INIT_CWND: f64 = 64.0;
const MIN_CWND: f64 = 8.0;
const MAX_CWND: f64 = 8192.0;
/// Unsent bytes a connection buffers before `send` waits.
const SEND_BUFFER: usize = 32 << 20;

#[derive(Default)]
pub struct NetStats {
    pub sent_packets: AtomicU64,
    pub sent_bytes: AtomicU64,
    pub retransmits: AtomicU64,
    pub recv_packets: AtomicU64,
    pub recv_bytes: AtomicU64,
    pub messages_in: AtomicU64,
    pub messages_out: AtomicU64,
}

struct Inflight {
    data: Arc<[u8]>,
    nfrags: u32,
    sent_at: Instant,
    retries: u32,
    /// Order of its latest transmission on this connection.
    seq: u64,
}

/// A fragment counts as lost once this many fragments sent after it have
/// been acknowledged (fast retransmit), without waiting for its timer.
const REORDER: u64 = 3;
/// Window after a loss, as a fraction of the window before.
const LOSS_BETA: f64 = 0.7;
/// Window growth per round trip after slow start, in fragments. Reno's 1
/// keeps a path with random loss far below its rate; this is a private
/// build network, so more aggressive growth is fine.
const GROWTH: f64 = 8.0;

struct SendState {
    next_msg: u64,
    /// Fragments not yet sent: `(msg, frag, nfrags, bytes)`.
    queue: VecDeque<(u64, u32, u32, Arc<[u8]>)>,
    queued_bytes: usize,
    inflight: HashMap<(u64, u32), Inflight>,
    cwnd: f64,
    ssthresh: f64,
    srtt: Option<Duration>,
    rttvar: Duration,
    last_cut: Instant,
    next_seq: u64,
    /// Highest `seq` acknowledged so far.
    high_acked: u64,
}

struct Partial {
    frags: Vec<Option<Vec<u8>>>,
    have: u32,
}

struct RecvState {
    partial: HashMap<u64, Partial>,
    /// Every message id below this has been delivered.
    done_below: u64,
    done: HashSet<u64>,
}

/// A key shared by server and workers. Every datagram carries a tag made
/// with it; datagrams without a valid tag are dropped before anything looks
/// at them (a worker runs whatever commands the server sends, and the
/// server trusts the results). On a keyed link every message is also
/// encrypted, with keys agreed per connection (Noise `NNpsk0` over this
/// key, so recorded traffic stays unreadable if the key leaks later); what
/// an observer still sees is message sizes and timing.
pub type LinkKey = Option<Arc<[u8; 32]>>;

/// Looks up a user's pre-shared key.
pub type UserKeys = Arc<dyn Fn(&str) -> Option<[u8; 32]> + Send + Sync>;

/// How the connections of an endpoint prove who they are.
#[derive(Clone)]
pub enum Auth {
    /// Not at all (tests).
    None,
    /// One key shared by both ends (server and workers). Every datagram
    /// carries a tag made with it.
    Shared(Arc<[u8; 32]>),
    /// The accepting end of client links: each connection names its user in
    /// the key agreement and proves that user's key. Datagrams after the
    /// agreement carry a tag made with a key of the connection.
    Users(UserKeys),
    /// The connecting end of a client link.
    User { name: String, psk: [u8; 32] },
}

fn user_handshake_key(psk: &[u8; 32]) -> [u8; 32] {
    blake3::derive_key("rargo client link handshake v1", psk)
}

/// The message id of the key agreement in each direction: the only one
/// whose datagrams are accepted without a connection tag.
const AGREEMENT: u64 = 1;

fn handshake_key(key: &[u8; 32]) -> [u8; 32] {
    blake3::derive_key("rargo worker link handshake v2", key)
}
/// How long either side waits for the other's half of the key agreement.
const HANDSHAKE_WAIT: Duration = Duration::from_secs(20);
const TAG: usize = 16;

fn seal(key: &LinkKey, pkt: &mut Vec<u8>) {
    if let Some(k) = key {
        let tag = blake3::keyed_hash(k, pkt);
        pkt.extend_from_slice(&tag.as_bytes()[..TAG]);
    }
}

fn open<'a>(key: &LinkKey, pkt: &'a [u8]) -> Option<&'a [u8]> {
    let Some(k) = key else { return Some(pkt) };
    if pkt.len() < TAG {
        return None;
    }
    let (body, tag) = pkt.split_at(pkt.len() - TAG);
    let want = blake3::keyed_hash(k, body);
    // Compared without an early exit.
    let diff = want.as_bytes()[..TAG].iter().zip(tag).fold(0u8, |d, (a, b)| d | (a ^ b));
    (diff == 0).then_some(body)
}

/// One end of a session with one peer.
pub struct Conn {
    pub id: u64,
    key: LinkKey,
    /// The connection agrees on keys of its own although the endpoint has
    /// no shared key (a client link).
    per_conn: bool,
    /// Set once the key agreement is done, on a keyed link.
    cipher: std::sync::OnceLock<rargo_proto::secure::LinkCipher>,
    /// On a client link: the key datagrams are tagged with after the
    /// agreement, and who the peer proved to be.
    tag: std::sync::OnceLock<[u8; 32]>,
    user: std::sync::OnceLock<String>,
    sock: Arc<UdpSocket>,
    peer: Mutex<SocketAddr>,
    send: Mutex<SendState>,
    recv: Mutex<RecvState>,
    wake_sender: Notify,
    space: Notify,
    inbound_tx: mpsc::UnboundedSender<Vec<u8>>,
    inbound_rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<Vec<u8>>>,
    pub stats: NetStats,
    closed: std::sync::atomic::AtomicBool,
    last_heard: Mutex<Instant>,
    /// Test hook: drop this fraction of outgoing datagrams.
    loss: Mutex<f32>,
    rng: AtomicU64,
}

impl Conn {
    fn new(id: u64, sock: Arc<UdpSocket>, peer: SocketAddr, key: LinkKey, per_conn: bool) -> Arc<Conn> {
        let (tx, rx) = mpsc::unbounded_channel();
        let c = Arc::new(Conn {
            id,
            key,
            per_conn,
            cipher: std::sync::OnceLock::new(),
            tag: std::sync::OnceLock::new(),
            user: std::sync::OnceLock::new(),
            sock,
            peer: Mutex::new(peer),
            send: Mutex::new(SendState {
                next_msg: 1,
                queue: VecDeque::new(),
                queued_bytes: 0,
                inflight: HashMap::new(),
                cwnd: INIT_CWND,
                ssthresh: MAX_CWND,
                srtt: None,
                rttvar: Duration::from_millis(50),
                last_cut: Instant::now(),
                next_seq: 1,
                high_acked: 0,
            }),
            recv: Mutex::new(RecvState { partial: HashMap::new(), done_below: 1, done: HashSet::new() }),
            wake_sender: Notify::new(),
            space: Notify::new(),
            inbound_tx: tx,
            inbound_rx: tokio::sync::Mutex::new(rx),
            stats: NetStats::default(),
            closed: std::sync::atomic::AtomicBool::new(false),
            last_heard: Mutex::new(Instant::now()),
            loss: Mutex::new(0.0),
            rng: AtomicU64::new(id | 1),
        });
        let c2 = c.clone();
        tokio::spawn(async move { c2.sender_loop().await });
        c
    }

    /// Makes this end drop a fraction of what it sends (for tests).
    pub fn set_loss(&self, fraction: f32) {
        *self.loss.lock() = fraction;
    }

    /// The user the peer proved to be (the accepting end of a client link).
    pub fn user(&self) -> Option<&str> {
        self.user.get().map(|s| s.as_str())
    }

    fn encrypted(&self) -> bool {
        self.key.is_some() || self.per_conn
    }

    pub fn peer(&self) -> SocketAddr {
        *self.peer.lock()
    }

    /// Time since anything arrived from the peer.
    pub fn idle(&self) -> Duration {
        self.last_heard.lock().elapsed()
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.wake_sender.notify_one();
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Current smoothed round-trip time, if measured.
    pub fn rtt(&self) -> Option<Duration> {
        self.send.lock().srtt
    }

    /// Queues a message for reliable delivery.
    pub async fn send(&self, msg: &[u8]) -> io::Result<()> {
        if !self.encrypted() {
            return self.send_raw(msg).await;
        }
        let Some(cipher) = self.cipher.get() else {
            return Err(io::Error::other("link: no keys agreed yet"));
        };
        let sealed = if msg.len() >= 256 << 10 { tokio::task::block_in_place(|| cipher.seal(msg)) } else { cipher.seal(msg) }?;
        self.send_raw(&sealed).await
    }

    async fn send_raw(&self, msg: &[u8]) -> io::Result<()> {
        loop {
            if self.is_closed() {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "connection closed"));
            }
            {
                let mut s = self.send.lock();
                if s.queued_bytes < SEND_BUFFER || s.queue.is_empty() {
                    let id = s.next_msg;
                    s.next_msg += 1;
                    let n = msg.len().div_ceil(FRAG).max(1) as u32;
                    for i in 0..n {
                        let a = i as usize * FRAG;
                        let b = (a + FRAG).min(msg.len());
                        s.queue.push_back((id, i, n, Arc::from(&msg[a..b])));
                    }
                    s.queued_bytes += msg.len();
                    self.stats.messages_out.fetch_add(1, Ordering::Relaxed);
                    drop(s);
                    self.wake_sender.notify_one();
                    return Ok(());
                }
            }
            self.space.notified().await;
        }
    }

    /// The next complete message from the peer.
    pub async fn recv(&self) -> Option<Vec<u8>> {
        let raw = self.recv_raw().await?;
        if !self.encrypted() {
            return Some(raw);
        }
        let cipher = self.cipher.get()?;
        let opened = if raw.len() >= 256 << 10 { tokio::task::block_in_place(|| cipher.open(&raw)) } else { cipher.open(&raw) };
        match opened {
            Ok(m) => Some(m),
            Err(_) => {
                // Only a peer holding the link key can get a message this
                // far; one that does not decrypt means a broken peer.
                self.close();
                None
            }
        }
    }

    async fn recv_raw(&self) -> Option<Vec<u8>> {
        self.inbound_rx.lock().await.recv().await
    }

    /// The connecting side of the key agreement.
    async fn agree_as_initiator(&self, key: &[u8; 32]) -> io::Result<()> {
        let (hs, m1) = rargo_proto::secure::link_start(&handshake_key(key))?;
        self.send_raw(&m1).await?;
        let m2 = tokio::time::timeout(HANDSHAKE_WAIT, self.recv_raw())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no answer from the server (is it running, and is the worker key the same?)"))?
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "connection closed"))?;
        let _ = self.cipher.set(rargo_proto::secure::link_finish(hs, &m2)?);
        Ok(())
    }

    async fn agree_as_responder(&self, key: &[u8; 32]) -> io::Result<()> {
        let m1 = tokio::time::timeout(HANDSHAKE_WAIT, self.recv_raw())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no key agreement"))?
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "connection closed"))?;
        let (m2, cipher) = rargo_proto::secure::link_accept(&handshake_key(key), &m1)?;
        let _ = self.cipher.set(cipher);
        self.send_raw(&m2).await
    }

    /// The connecting side of a client link: names the user and proves
    /// the user's key.
    async fn agree_as_user(&self, name: &str, psk: &[u8; 32]) -> io::Result<()> {
        let (hs, m1) = rargo_proto::secure::link_start(&user_handshake_key(psk))?;
        let mut first = vec![name.len().min(255) as u8];
        first.extend_from_slice(&name.as_bytes()[..name.len().min(255)]);
        first.extend_from_slice(&m1);
        self.send_raw(&first).await?;
        let m2 = tokio::time::timeout(HANDSHAKE_WAIT, self.recv_raw())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no answer from the server"))?
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "connection closed"))?;
        if m2.first() == Some(&1) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("the server refused the connection: {}", String::from_utf8_lossy(&m2[1..]).trim()),
            ));
        }
        let cipher = rargo_proto::secure::link_finish(hs, m2.get(1..).unwrap_or_default())?;
        let _ = self.tag.set(cipher.binding());
        let _ = self.cipher.set(cipher);
        Ok(())
    }

    /// The accepting side of a client link.
    async fn agree_with_user(&self, keys: &UserKeys) -> io::Result<()> {
        let first = tokio::time::timeout(HANDSHAKE_WAIT, self.recv_raw())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no key agreement"))?
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "connection closed"))?;
        let n = *first.first().ok_or_else(|| io::Error::other("empty key agreement"))? as usize;
        let name = first.get(1..1 + n).and_then(|b| std::str::from_utf8(b).ok()).ok_or_else(|| io::Error::other("malformed key agreement"))?;
        let accepted = keys(name).and_then(|psk| rargo_proto::secure::link_accept(&user_handshake_key(&psk), &first[1 + n..]).ok());
        let Some((m2, cipher)) = accepted else {
            let mut refusal = vec![1u8];
            refusal.extend_from_slice(b"the server does not know this user or key");
            let _ = self.send_raw(&refusal).await;
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("refused `{name}`: unknown user or wrong key")));
        };
        let _ = self.user.set(name.to_string());
        let _ = self.tag.set(cipher.binding());
        let _ = self.cipher.set(cipher);
        let mut reply = vec![0u8];
        reply.extend_from_slice(&m2);
        self.send_raw(&reply).await
    }

    fn rto(s: &SendState) -> Duration {
        match s.srtt {
            Some(srtt) => (srtt + 4 * s.rttvar).clamp(MIN_RTO, MAX_RTO),
            None => Duration::from_millis(200),
        }
    }

    fn lossy(&self) -> bool {
        let p = *self.loss.lock();
        if p <= 0.0 {
            return false;
        }
        // xorshift
        let mut x = self.rng.load(Ordering::Relaxed);
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng.store(x, Ordering::Relaxed);
        (x % 10_000) as f32 / 10_000.0 < p
    }

    async fn transmit(&self, msg: u64, frag: u32, nfrags: u32, data: &[u8]) {
        let mut pkt = Vec::with_capacity(DATA_HDR + data.len());
        pkt.push(T_DATA);
        pkt.extend_from_slice(&self.id.to_le_bytes());
        pkt.extend_from_slice(&msg.to_le_bytes());
        pkt.extend_from_slice(&frag.to_le_bytes());
        pkt.extend_from_slice(&nfrags.to_le_bytes());
        pkt.extend_from_slice(data);
        self.stats.sent_packets.fetch_add(1, Ordering::Relaxed);
        self.stats.sent_bytes.fetch_add(pkt.len() as u64, Ordering::Relaxed);
        if self.lossy() {
            return;
        }
        let peer = *self.peer.lock();
        seal(&self.key, &mut pkt);
        if msg != AGREEMENT {
            self.tag_datagram(&mut pkt);
        }
        let _ = self.sock.send_to(&pkt, peer).await;
    }

    /// On a client link with keys agreed: appends the connection's tag.
    fn tag_datagram(&self, pkt: &mut Vec<u8>) {
        if let Some(k) = self.tag.get() {
            let tag = blake3::keyed_hash(k, pkt);
            pkt.extend_from_slice(&tag.as_bytes()[..TAG]);
        }
    }

    async fn sender_loop(self: Arc<Self>) {
        loop {
            if self.is_closed() {
                return;
            }
            // Send new fragments while the window allows.
            let mut batch: Vec<(u64, u32, u32, Arc<[u8]>)> = Vec::new();
            let mut resend: Vec<(u64, u32, u32, Arc<[u8]>)> = Vec::new();
            let next_deadline;
            {
                let mut s = self.send.lock();
                let now = Instant::now();
                let rto = Self::rto(&s);
                // Retransmit what has timed out.
                let mut timed_out = false;
                let mut fast_lost = false;
                let mut soonest: Option<Instant> = None;
                let high_acked = s.high_acked;
                let mut seq = s.next_seq;
                for ((m, f), inf) in s.inflight.iter_mut() {
                    let due = inf.sent_at + rto * (1 << inf.retries.min(5));
                    let overtaken = high_acked >= inf.seq + REORDER;
                    if overtaken || due <= now {
                        inf.sent_at = now;
                        inf.retries += 1;
                        inf.seq = seq;
                        seq += 1;
                        resend.push((*m, *f, inf.nfrags, inf.data.clone()));
                        if overtaken {
                            fast_lost = true;
                        } else {
                            timed_out = true;
                        }
                    } else {
                        soonest = Some(soonest.map_or(due, |x| x.min(due)));
                    }
                }
                s.next_seq = seq;
                // One reduction per round trip, however many fragments of
                // that round were lost. A timeout means the path stalled and
                // is answered harder than a loss found by acknowledgements.
                if (timed_out || fast_lost) && now.duration_since(s.last_cut) > s.srtt.unwrap_or(Duration::from_millis(100)) {
                    let beta = if timed_out { 0.5 } else { LOSS_BETA };
                    s.ssthresh = (s.cwnd * beta).max(MIN_CWND);
                    s.cwnd = s.ssthresh;
                    s.last_cut = now;
                }
                while (s.inflight.len() as f64) < s.cwnd {
                    let Some((m, f, n, data)) = s.queue.pop_front() else { break };
                    s.queued_bytes = s.queued_bytes.saturating_sub(data.len());
                    let seq = s.next_seq;
                    s.next_seq += 1;
                    s.inflight.insert((m, f), Inflight { data: data.clone(), nfrags: n, sent_at: now, retries: 0, seq });
                    batch.push((m, f, n, data));
                }
                next_deadline = soonest.or_else(|| (!s.inflight.is_empty()).then(|| now + rto));
            }
            if !batch.is_empty() {
                self.space.notify_waiters();
            }
            self.stats.retransmits.fetch_add(resend.len() as u64, Ordering::Relaxed);
            for (m, f, n, data) in &resend {
                self.transmit(*m, *f, *n, data).await;
            }
            for (m, f, n, data) in &batch {
                self.transmit(*m, *f, *n, data).await;
            }
            if !batch.is_empty() || !resend.is_empty() {
                continue;
            }
            match next_deadline {
                Some(d) => {
                    let wait = d.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
                    let _ = tokio::time::timeout(wait, self.wake_sender.notified()).await;
                }
                None => self.wake_sender.notified().await,
            }
        }
    }

    fn on_ack(&self, msg: u64, frag: u32) {
        let mut s = self.send.lock();
        if let Some(inf) = s.inflight.remove(&(msg, frag)) {
            s.high_acked = s.high_acked.max(inf.seq);
            if inf.retries == 0 {
                let sample = inf.sent_at.elapsed();
                match s.srtt {
                    None => {
                        s.srtt = Some(sample);
                        s.rttvar = sample / 2;
                    }
                    Some(srtt) => {
                        let diff = if sample > srtt { sample - srtt } else { srtt - sample };
                        s.rttvar = (s.rttvar * 3 + diff) / 4;
                        s.srtt = Some((srtt * 7 + sample) / 8);
                    }
                }
            }
            if s.cwnd < s.ssthresh {
                s.cwnd += 1.0;
            } else {
                s.cwnd += GROWTH / s.cwnd;
            }
            s.cwnd = s.cwnd.min(MAX_CWND);
        }
        drop(s);
        self.wake_sender.notify_one();
    }

    /// Handles a data fragment; returns whether to acknowledge it.
    fn on_data(&self, msg: u64, frag: u32, nfrags: u32, data: &[u8]) -> bool {
        let mut r = self.recv.lock();
        if msg < r.done_below || r.done.contains(&msg) {
            return true; // duplicate of something already delivered
        }
        if nfrags == 0 {
            return false; // malformed
        }
        let p = r.partial.entry(msg).or_insert_with(|| Partial { frags: vec![None; nfrags as usize], have: 0 });
        if (frag as usize) >= p.frags.len() {
            return false;
        }
        if p.frags[frag as usize].is_none() {
            p.frags[frag as usize] = Some(data.to_vec());
            p.have += 1;
        }
        if p.have as usize == p.frags.len() {
            let p = r.partial.remove(&msg).unwrap();
            let mut whole = Vec::with_capacity(p.frags.iter().map(|f| f.as_ref().map_or(0, |v| v.len())).sum());
            for f in p.frags {
                whole.extend_from_slice(&f.unwrap());
            }
            r.done.insert(msg);
            loop {
                let low = r.done_below;
                if !r.done.remove(&low) {
                    break;
                }
                r.done_below += 1;
            }
            self.stats.messages_in.fetch_add(1, Ordering::Relaxed);
            let _ = self.inbound_tx.send(whole);
        }
        true
    }
}

/// A UDP socket carrying any number of connections.
pub struct Endpoint {
    sock: Arc<UdpSocket>,
    conns: Mutex<HashMap<u64, Arc<Conn>>>,
    accept_tx: mpsc::UnboundedSender<Arc<Conn>>,
    accept_rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<Arc<Conn>>>,
    accepting: bool,
    key: LinkKey,
    auth: Auth,
}

impl Endpoint {
    /// Binds a socket. With `accepting`, unknown connection ids from peers
    /// create new connections (the server side).
    pub async fn bind(addr: SocketAddr, accepting: bool) -> io::Result<Arc<Endpoint>> {
        Endpoint::bind_keyed(addr, accepting, None).await
    }

    /// Like [`Endpoint::bind`], with every datagram authenticated by `key`.
    pub async fn bind_keyed(addr: SocketAddr, accepting: bool, key: LinkKey) -> io::Result<Arc<Endpoint>> {
        let auth = match key {
            Some(k) => Auth::Shared(k),
            None => Auth::None,
        };
        Endpoint::bind_auth(addr, accepting, auth).await
    }

    /// Binds a socket whose connections authenticate as `auth` says.
    pub async fn bind_auth(addr: SocketAddr, accepting: bool, auth: Auth) -> io::Result<Arc<Endpoint>> {
        let key: LinkKey = match &auth {
            Auth::Shared(k) => Some(k.clone()),
            _ => None,
        };
        let sock = Arc::new(UdpSocket::bind(addr).await?);
        // Large buffers: bursts of thousands of fragments are normal.
        {
            use std::os::fd::AsRawFd;
            let fd = sock.as_raw_fd();
            let size: libc::c_int = 16 << 20;
            for opt in [libc::SO_RCVBUFFORCE, libc::SO_SNDBUFFORCE, libc::SO_RCVBUF, libc::SO_SNDBUF] {
                // SAFETY: valid fd and pointer to an int option value.
                unsafe {
                    libc::setsockopt(fd, libc::SOL_SOCKET, opt, &size as *const _ as *const libc::c_void, 4);
                }
            }
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let ep = Arc::new(Endpoint {
            sock,
            conns: Mutex::new(HashMap::new()),
            accept_tx: tx,
            accept_rx: tokio::sync::Mutex::new(rx),
            accepting,
            key,
            auth,
        });
        let ep2 = ep.clone();
        tokio::spawn(async move { ep2.recv_loop().await });
        Ok(ep)
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.sock.local_addr()
    }

    /// Opens a connection to a peer (the worker side). On a keyed link
    /// this agrees on the connection's keys before returning.
    pub async fn connect(&self, peer: SocketAddr) -> io::Result<Arc<Conn>> {
        let id = {
            let mut b = [0u8; 8];
            // SAFETY: filling a valid 8-byte buffer.
            unsafe { libc::getrandom(b.as_mut_ptr() as *mut _, 8, 0) };
            u64::from_le_bytes(b) | 1
        };
        let c = Conn::new(id, self.sock.clone(), peer, self.key.clone(), self.per_conn());
        self.conns.lock().insert(id, c.clone());
        let agreed = match &self.auth {
            Auth::None => Ok(()),
            Auth::Shared(key) => c.agree_as_initiator(key).await,
            Auth::User { name, psk } => c.agree_as_user(name, psk).await,
            Auth::Users(_) => Err(io::Error::other("an accepting endpoint does not connect")),
        };
        if let Err(e) = agreed {
            self.forget(id);
            return Err(e);
        }
        Ok(c)
    }

    /// The next connection opened by a peer.
    pub async fn accept(&self) -> Option<Arc<Conn>> {
        self.accept_rx.lock().await.recv().await
    }

    fn per_conn(&self) -> bool {
        matches!(self.auth, Auth::Users(_) | Auth::User { .. })
    }

    /// Whether a datagram is let in, and whether it is proven to come from
    /// the peer of its connection. With a shared key, only tagged datagrams
    /// get in. On client links a datagram is proven by its connection's
    /// tag; unproven ones are let in for the key agreement only.
    fn admit<'a>(&self, pkt: &'a [u8]) -> Option<(&'a [u8], bool)> {
        if !self.per_conn() {
            return open(&self.key, pkt).map(|p| (p, true));
        }
        if pkt.len() >= 9 + TAG {
            let id = u64::from_le_bytes(pkt[1..9].try_into().unwrap());
            let tag_key = self.conns.lock().get(&id).and_then(|c| c.tag.get().copied());
            if let Some(k) = tag_key {
                let (body, tag) = pkt.split_at(pkt.len() - TAG);
                let want = blake3::keyed_hash(&k, body);
                let diff = want.as_bytes()[..TAG].iter().zip(tag).fold(0u8, |d, (a, b)| d | (a ^ b));
                if diff == 0 {
                    return Some((body, true));
                }
            }
        }
        Some((pkt, false))
    }

    pub fn forget(&self, id: u64) {
        if let Some(c) = self.conns.lock().remove(&id) {
            c.close();
        }
    }

    async fn recv_loop(self: Arc<Self>) {
        let mut buf = vec![0u8; 65536];
        let mut acks: BTreeMap<u64, (SocketAddr, Vec<u8>)> = BTreeMap::new();
        loop {
            let Ok((n, from)) = self.sock.recv_from(&mut buf).await else { return };
            if let Some((pkt, proven)) = self.admit(&buf[..n]) {
                self.handle(pkt, from, proven, &mut acks);
            }
            // Drain whatever else is already queued, then flush the
            // acknowledgements in as few datagrams as possible.
            while let Ok((n, from)) = self.sock.try_recv_from(&mut buf) {
                if let Some((pkt, proven)) = self.admit(&buf[..n]) {
                    self.handle(pkt, from, proven, &mut acks);
                }
                if acks.values().any(|(_, a)| a.len() > 1100) {
                    break;
                }
            }
            for (id, (to, mut pkt)) in std::mem::take(&mut acks) {
                seal(&self.key, &mut pkt);
                if self.per_conn() {
                    let conn = self.conns.lock().get(&id).cloned();
                    if let Some(c) = conn {
                        c.tag_datagram(&mut pkt);
                    }
                }
                let _ = self.sock.send_to(&pkt, to).await;
            }
        }
    }

    fn handle(&self, pkt: &[u8], from: SocketAddr, proven: bool, acks: &mut BTreeMap<u64, (SocketAddr, Vec<u8>)>) {
        if pkt.len() < 9 {
            return;
        }
        let id = u64::from_le_bytes(pkt[1..9].try_into().unwrap());
        // Unproven datagrams may only belong to the key agreement.
        let first_msg = (pkt.len() >= 17).then(|| u64::from_le_bytes(pkt[9..17].try_into().unwrap()));
        if !proven && pkt[0] == T_DATA && first_msg != Some(AGREEMENT) {
            return;
        }
        let conn = {
            let mut conns = self.conns.lock();
            match conns.get(&id) {
                Some(c) => c.clone(),
                None if self.accepting && pkt[0] == T_DATA => {
                    let c = Conn::new(id, self.sock.clone(), from, self.key.clone(), self.per_conn());
                    conns.insert(id, c.clone());
                    match (self.key.clone(), &self.auth) {
                        (_, Auth::Users(keys)) => {
                            let (c, tx, keys) = (c.clone(), self.accept_tx.clone(), keys.clone());
                            tokio::spawn(async move {
                                match c.agree_with_user(&keys).await {
                                    Ok(()) => {
                                        let _ = tx.send(c);
                                    }
                                    Err(_) => {
                                        // Let the refusal reach the peer.
                                        tokio::time::sleep(Duration::from_secs(2)).await;
                                        c.close();
                                    }
                                }
                            });
                        }
                        (None, _) => {
                            let _ = self.accept_tx.send(c.clone());
                        }
                        // Handed to `accept` once its keys are agreed.
                        (Some(key), _) => {
                            let (c, tx) = (c.clone(), self.accept_tx.clone());
                            tokio::spawn(async move {
                                match c.agree_as_responder(&key).await {
                                    Ok(()) => {
                                        let _ = tx.send(c);
                                    }
                                    Err(_) => c.close(),
                                }
                            });
                        }
                    }
                    c
                }
                None => return,
            }
        };
        if proven {
            *conn.last_heard.lock() = Instant::now();
            // A peer behind NAT, a restarted worker or a client that
            // changed networks comes from a new address.
            *conn.peer.lock() = from;
        }
        conn.stats.recv_packets.fetch_add(1, Ordering::Relaxed);
        conn.stats.recv_bytes.fetch_add(pkt.len() as u64, Ordering::Relaxed);
        match pkt[0] {
            T_DATA if pkt.len() >= DATA_HDR => {
                let msg = u64::from_le_bytes(pkt[9..17].try_into().unwrap());
                let frag = u32::from_le_bytes(pkt[17..21].try_into().unwrap());
                let nfrags = u32::from_le_bytes(pkt[21..25].try_into().unwrap());
                if conn.on_data(msg, frag, nfrags, &pkt[DATA_HDR..]) {
                    let (_, a) = acks.entry(id).or_insert_with(|| {
                        let mut v = vec![T_ACK];
                        v.extend_from_slice(&id.to_le_bytes());
                        (from, v)
                    });
                    a.extend_from_slice(&msg.to_le_bytes());
                    a.extend_from_slice(&frag.to_le_bytes());
                }
            }
            T_ACK => {
                let mut rest = &pkt[9..];
                while rest.len() >= 12 {
                    let msg = u64::from_le_bytes(rest[..8].try_into().unwrap());
                    let frag = u32::from_le_bytes(rest[8..12].try_into().unwrap());
                    if proven || msg == AGREEMENT {
                        conn.on_ack(msg, frag);
                    }
                    rest = &rest[12..];
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap()
    }

    async fn pair() -> (Arc<Endpoint>, Arc<Conn>, Arc<Endpoint>, Arc<Conn>) {
        let server = Endpoint::bind("127.0.0.1:0".parse().unwrap(), true).await.unwrap();
        let client = Endpoint::bind("127.0.0.1:0".parse().unwrap(), false).await.unwrap();
        let c = client.connect(server.local_addr().unwrap()).await.unwrap();
        c.send(b"hello").await.unwrap();
        let s = server.accept().await.unwrap();
        assert_eq!(s.recv().await.unwrap(), b"hello");
        (server, s, client, c)
    }

    #[test]
    fn client_links_prove_their_user_and_follow_a_changed_address() {
        rt().block_on(async {
            let addr = || "127.0.0.1:0".parse().unwrap();
            let users: UserKeys = Arc::new(|name| (name == "ann").then_some([7u8; 32]));
            let server = Endpoint::bind_auth(addr(), true, Auth::Users(users)).await.unwrap();
            let at = server.local_addr().unwrap();
            let ann = Endpoint::bind_auth(addr(), false, Auth::User { name: "ann".into(), psk: [7; 32] }).await.unwrap();
            let c = ann.connect(at).await.unwrap();
            let big: Vec<u8> = (0..200_000u32).map(|i| (i * 7) as u8).collect();
            c.send(&big).await.unwrap();
            let s = server.accept().await.unwrap();
            assert_eq!(s.user(), Some("ann"));
            assert_eq!(s.recv().await.unwrap(), big);
            s.send(b"back").await.unwrap();
            assert_eq!(c.recv().await.unwrap(), b"back");

            // A wrong key and an unknown user are told no.
            for (name, psk) in [("ann", [8u8; 32]), ("eve", [7u8; 32])] {
                let bad = Endpoint::bind_auth(addr(), false, Auth::User { name: name.into(), psk }).await.unwrap();
                let e = tokio::time::timeout(Duration::from_secs(5), bad.connect(at)).await.unwrap().err().unwrap();
                assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{name}");
            }

            // A datagram that names the connection but lacks its tag (an
            // outsider who saw the id) neither delivers anything nor moves
            // the connection.
            let outsider = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let mut pkt = vec![T_DATA];
            pkt.extend_from_slice(&s.id.to_le_bytes());
            pkt.extend_from_slice(&99u64.to_le_bytes());
            pkt.extend_from_slice(&0u32.to_le_bytes());
            pkt.extend_from_slice(&1u32.to_le_bytes());
            pkt.extend_from_slice(b"forged");
            outsider.send_to(&pkt, at).await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            let before = s.peer();
            assert_ne!(before, outsider.local_addr().unwrap());

            // The client's address changes (here: its socket is replaced by
            // one on another port): the next thing it sends moves the
            // connection, and the server's answer finds it.
            let moved = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
            let relay_to = moved.clone();
            // Forward between the client's real socket and the server
            // through the new port, both ways.
            let client_addr = ann.local_addr().unwrap();
            let proxy = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
            *c.peer.lock() = proxy.local_addr().unwrap();
            let (p2, m2) = (proxy.clone(), moved.clone());
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                loop {
                    let Ok((n, _)) = p2.recv_from(&mut buf).await else { break };
                    let _ = m2.send_to(&buf[..n], at).await;
                }
            });
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                loop {
                    let Ok((n, _)) = relay_to.recv_from(&mut buf).await else { break };
                    let _ = proxy.send_to(&buf[..n], client_addr).await;
                }
            });
            c.send(b"from somewhere else").await.unwrap();
            assert_eq!(tokio::time::timeout(Duration::from_secs(5), s.recv()).await.unwrap().unwrap(), b"from somewhere else");
            assert_eq!(s.peer(), moved.local_addr().unwrap());
            s.send(b"found you").await.unwrap();
            assert_eq!(tokio::time::timeout(Duration::from_secs(5), c.recv()).await.unwrap().unwrap(), b"found you");
        });
    }

    #[test]
    fn keyed_link_carries_messages_and_rejects_other_keys() {
        rt().block_on(async {
            let key = |b: u8| -> LinkKey { Some(Arc::new([b; 32])) };
            let addr = || "127.0.0.1:0".parse().unwrap();
            let server = Endpoint::bind_keyed(addr(), true, key(1)).await.unwrap();
            let good = Endpoint::bind_keyed(addr(), false, key(1)).await.unwrap();
            let c = good.connect(server.local_addr().unwrap()).await.unwrap();
            let big: Vec<u8> = (0..300_000u32).map(|i| i as u8).collect();
            c.send(&big).await.unwrap();
            let s = server.accept().await.unwrap();
            assert_eq!(s.recv().await.unwrap(), big);
            s.send(b"back").await.unwrap();
            assert_eq!(c.recv().await.unwrap(), b"back");

            // Wrong key, and no key at all: nothing is accepted.
            for k in [key(2), None] {
                let keyed = k.is_some();
                let bad = Endpoint::bind_keyed(addr(), false, k).await.unwrap();
                let b = tokio::time::timeout(Duration::from_millis(300), bad.connect(server.local_addr().unwrap())).await;
                if keyed {
                    assert!(b.is_err(), "the key agreement must not complete with another key");
                } else {
                    let _ = b.unwrap().unwrap().send(b"let me in").await;
                }
                let got = tokio::time::timeout(Duration::from_millis(300), server.accept()).await;
                assert!(got.is_err(), "a connection with the wrong key was accepted");
            }
        });
    }

    #[test]
    fn small_and_large_messages_both_ways() {
        rt().block_on(async {
            let (_se, s, _ce, c) = pair().await;
            s.send(b"reply").await.unwrap();
            assert_eq!(c.recv().await.unwrap(), b"reply");
            let big: Vec<u8> = (0..5_000_000u32).map(|i| (i * 31 % 251) as u8).collect();
            c.send(&big).await.unwrap();
            c.send(b"after").await.unwrap();
            let mut got = Vec::new();
            for _ in 0..2 {
                got.push(s.recv().await.unwrap());
            }
            got.sort_by_key(|m| m.len());
            assert_eq!(got[0], b"after");
            assert!(got[1] == big);
            assert!(c.rtt().is_some());
        });
    }

    /// Not a pass/fail test: prints loopback throughput.
    /// `cargo test -p rargo-core --release --lib net::tests::throughput -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn throughput() {
        rt().block_on(async {
            let (_se, s, _ce, c) = pair().await;
            let big = vec![7u8; 256 << 20];
            let t = Instant::now();
            for part in big.chunks(1 << 20) {
                c.send(part).await.unwrap();
            }
            let mut got = 0usize;
            while got < big.len() {
                got += s.recv().await.unwrap().len();
            }
            let secs = t.elapsed().as_secs_f64();
            println!(
                "256 MiB in {secs:.2}s = {:.0} MiB/s; {} packets, {} retransmits, rtt {:?}",
                256.0 / secs,
                c.stats.sent_packets.load(Ordering::Relaxed),
                c.stats.retransmits.load(Ordering::Relaxed),
                c.rtt()
            );
        });
    }

    #[test]
    fn survives_packet_loss_in_both_directions() {
        rt().block_on(async {
            let (_se, s, _ce, c) = pair().await;
            c.set_loss(0.05);
            s.set_loss(0.05);
            let msgs: Vec<Vec<u8>> =
                (0..200u32).map(|i| (0..(i * 997 % 40_000 + 1)).map(|j| (i + j) as u8).collect()).collect();
            for m in &msgs {
                c.send(m).await.unwrap();
            }
            let mut got = Vec::new();
            for _ in 0..msgs.len() {
                got.push(tokio::time::timeout(Duration::from_secs(30), s.recv()).await.expect("timed out").unwrap());
            }
            let mut want = msgs.clone();
            want.sort();
            got.sort();
            assert!(got == want, "every message arrives exactly once, intact");
            assert!(c.stats.retransmits.load(Ordering::Relaxed) > 0, "loss must have forced retransmissions");
            // And the reverse direction under the same loss.
            s.send(&msgs[150]).await.unwrap();
            assert!(tokio::time::timeout(Duration::from_secs(30), c.recv()).await.unwrap().unwrap() == msgs[150]);
        });
    }
}
