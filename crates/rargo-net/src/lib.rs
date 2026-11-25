pub mod link;
mod endpoint;
mod handshake;

use parking_lot::Mutex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::{Notify, mpsc};

const FRAG: usize = 1200;
const T_DATA: u8 = 1;
const T_ACK: u8 = 2;
const DATA_HDR: usize = 1 + 8 + 8 + 4 + 4;
const MIN_RTO: Duration = Duration::from_millis(10);
const MAX_RTO: Duration = Duration::from_secs(2);
const INIT_CWND: f64 = 64.0;
const MIN_CWND: f64 = 8.0;
const MAX_CWND: f64 = 8192.0;
const SEND_BUFFER: usize = 8 << 20;
const _: () = assert!(MIN_CWND <= INIT_CWND);
const _: () = assert!(INIT_CWND <= MAX_CWND);
const _: () = assert!(MIN_RTO.as_nanos() < MAX_RTO.as_nanos());
const _: () = assert!(DATA_HDR == 25);
const _: () = assert!(SEND_BUFFER >= FRAG);

pub(crate) async fn connect_within(wait: Duration, endpoint: &Arc<Endpoint>, server: SocketAddr) -> io::Result<Arc<Conn>> {
    match tokio::time::timeout(wait, endpoint.connect(server)).await {
        Ok(r) => r,
        Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "no answer over UDP")),
    }
}

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
    seq: u64,
}

const REORDER: u64 = 3;
const LOSS_BETA: f64 = 0.7;
const GROWTH: f64 = 8.0;
const PACE_BURST: u32 = 16;
const _: () = assert!(LOSS_BETA > 0.0 && LOSS_BETA < 1.0);
const _: () = assert!(REORDER > 0);

struct SendState {
    next_msg: u64,
    queue: VecDeque<(u64, u32, u32, Arc<[u8]>)>,
    queued_bytes: usize,
    inflight: HashMap<(u64, u32), Inflight>,
    cwnd: f64,
    ssthresh: f64,
    srtt: Option<Duration>,
    rttvar: Duration,
    last_cut: Instant,
    min_rtt: Option<Duration>,
    next_send: Instant,
    next_seq: u64,
    high_acked: u64,
}

struct Partial {
    frags: Vec<Option<Vec<u8>>>,
    have: u32,
}

struct RecvState {
    partial: HashMap<u64, Partial>,
    done_below: u64,
    done: HashSet<u64>,
}

pub type LinkKey = Option<Arc<[u8; 32]>>;

pub type UserKeys = Arc<dyn Fn(&str) -> Option<[u8; 32]> + Send + Sync>;

#[derive(Clone)]
pub enum Auth {
    None,
    Shared(Arc<[u8; 32]>),
    Users(UserKeys),
    User { name: String, psk: [u8; 32] },
}

fn user_handshake_key(psk: &[u8; 32]) -> [u8; 32] {
    blake3::derive_key("rargo client link handshake v1", psk)
}

const AGREEMENT: u64 = 1;

fn handshake_key(key: &[u8; 32]) -> [u8; 32] {
    blake3::derive_key("rargo worker link handshake v2", key)
}
const HANDSHAKE_WAIT: Duration = Duration::from_secs(20);
const TAG: usize = 16;
const _: () = assert!(DATA_HDR + FRAG + 2 * TAG <= 1500 - 20 - 8);
const _: () = assert!(AGREEMENT >= 1, "message ids start at 1");

fn seal(key: &LinkKey, pkt: &mut Vec<u8>) {
    if let Some(k) = key {
        let before = pkt.len();
        let tag = blake3::keyed_hash(k, pkt);
        pkt.extend_from_slice(&tag.as_bytes()[..TAG]);
        assert_eq!(pkt.len(), before + TAG);
    }
}

fn open<'a>(key: &LinkKey, pkt: &'a [u8]) -> Option<&'a [u8]> {
    let Some(k) = key else { return Some(pkt) };
    if pkt.len() < TAG {
        return None;
    }
    let (body, tag) = pkt.split_at(pkt.len() - TAG);
    let want = blake3::keyed_hash(k, body);
    let diff = want.as_bytes()[..TAG].iter().zip(tag).fold(0u8, |d, (a, b)| d | (a ^ b));
    (diff == 0).then_some(body)
}

pub struct Conn {
    pub id: u64,
    key: LinkKey,
    per_conn: bool,
    cipher: std::sync::OnceLock<rargo_proto::secure::LinkCipher>,
    tag: std::sync::OnceLock<[u8; 32]>,
    user: std::sync::OnceLock<String>,
    sock: Arc<UdpSocket>,
    peer: Mutex<SocketAddr>,
    send: Mutex<SendState>,
    recv: Mutex<RecvState>,
    wake_sender: Notify,
    pub send_settled: Notify,
    space: Notify,
    inbound_tx: mpsc::UnboundedSender<Vec<u8>>,
    inbound_rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<Vec<u8>>>,
    pub stats: NetStats,
    closed: std::sync::atomic::AtomicBool,
    last_heard: Mutex<Instant>,
    loss: Mutex<f32>,
    rng: AtomicU64,
    handshake_in: Mutex<Vec<Vec<u8>>>,
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
                min_rtt: None,
                next_send: Instant::now(),
                next_seq: 1,
                high_acked: 0,
            }),
            recv: Mutex::new(RecvState { partial: HashMap::new(), done_below: 1, done: HashSet::new() }),
            wake_sender: Notify::new(),
            send_settled: Notify::new(),
            space: Notify::new(),
            inbound_tx: tx,
            inbound_rx: tokio::sync::Mutex::new(rx),
            stats: NetStats::default(),
            closed: std::sync::atomic::AtomicBool::new(false),
            last_heard: Mutex::new(Instant::now()),
            loss: Mutex::new(0.0),
            rng: AtomicU64::new(id | 1),
            handshake_in: Mutex::new(Vec::new()),
        });
        let c2 = c.clone();
        tokio::spawn(async move { c2.sender_loop().await });
        c
    }

    pub fn set_loss(&self, fraction: f32) {
        *self.loss.lock() = fraction;
    }

    pub fn user(&self) -> Option<&str> {
        self.user.get().map(|s| s.as_str())
    }

    fn encrypted(&self) -> bool {
        self.key.is_some() || self.per_conn
    }

    pub fn peer(&self) -> SocketAddr {
        *self.peer.lock()
    }

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

    pub fn send_idle(&self) -> bool {
        let s = self.send.lock();
        s.queue.is_empty() && s.inflight.is_empty()
    }

    pub fn sender_state(&self) -> String {
        let s = self.send.lock();
        format!(
            "window {:.0} (slow start below {:.0}), {} in flight, {} KiB queued, round trip {:?} (shortest {:?}), {} sent, {} retransmitted",
            s.cwnd,
            s.ssthresh,
            s.inflight.len(),
            s.queued_bytes >> 10,
            s.srtt,
            s.min_rtt,
            self.stats.sent_packets.load(Ordering::Relaxed),
            self.stats.retransmits.load(Ordering::Relaxed)
        )
    }

    pub fn rtt(&self) -> Option<Duration> {
        self.send.lock().srtt
    }

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
                    assert!(id >= 1);
                    s.next_msg += 1;
                    let n = msg.len().div_ceil(FRAG).max(1) as u32;
                    let queue_before = s.queue.len();
                    let mut cut = 0usize;
                    for i in 0..n {
                        let a = i as usize * FRAG;
                        let b = (a + FRAG).min(msg.len());
                        assert!(a <= b);
                        assert!(b - a <= FRAG);
                        cut += b - a;
                        s.queue.push_back((id, i, n, Arc::from(&msg[a..b])));
                    }
                    assert_eq!(cut, msg.len(), "fragments cover the message exactly");
                    assert_eq!(s.queue.len() - queue_before, n as usize);
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

    pub async fn recv(&self) -> Option<Vec<u8>> {
        loop {
            let raw = self.recv_raw().await?;
            if self.handshake_in.lock().iter().any(|m| m == &raw) {
                continue;
            }
            if !self.encrypted() {
                return Some(raw);
            }
            let cipher = self.cipher.get()?;
            let opened = if raw.len() >= 256 << 10 { tokio::task::block_in_place(|| cipher.open(&raw)) } else { cipher.open(&raw) };
            return match opened {
                Ok(m) => Some(m),
                Err(_) => {
                    self.close();
                    None
                }
            };
        }
    }

    async fn recv_raw(&self) -> Option<Vec<u8>> {
        self.inbound_rx.lock().await.recv().await
    }

    fn note_handshake(&self, msg: &[u8]) {
        self.handshake_in.lock().push(msg.to_vec());
    }

    fn ephemeral() -> [u8; 32] {
        let mut b = [0u8; 32];
        unsafe { libc::getrandom(b.as_mut_ptr() as *mut _, 32, 0) };
        b
    }

    fn install(&self, agreed: handshake::Agreed) {
        if let Some(binding) = agreed.binding {
            let _ = self.tag.set(binding);
        }
        if let Some(user) = agreed.user {
            let _ = self.user.set(user);
        }
        let _ = self.cipher.set(agreed.cipher);
    }

    async fn apply(&self, hs: &mut handshake::Handshake, result: io::Result<handshake::Action>) -> io::Result<handshake::Action> {
        match result {
            Ok(action) => Ok(action),
            Err(e) => {
                if let Some(pkt) = hs.take_notify() {
                    let _ = self.send_raw(&pkt).await;
                }
                Err(e)
            }
        }
    }

    async fn drive(&self, hs: &mut handshake::Handshake, keys: Option<&UserKeys>) -> io::Result<handshake::Agreed> {
        let deadline = Instant::now() + HANDSHAKE_WAIT;
        let mut action = hs.start()?;
        loop {
            hs.invariants();
            match action {
                handshake::Action::Send(pkt) => {
                    self.send_raw(&pkt).await?;
                    action = hs.sent()?;
                }
                handshake::Action::Wait => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(hs.timeout_error());
                    }
                    let slice = (left / hs.resend_share()).max(Duration::from_millis(1)).min(left);
                    match tokio::time::timeout(slice, self.recv_raw()).await {
                        Ok(Some(pkt)) => {
                            self.note_handshake(&pkt);
                            let result = hs.recv(&pkt);
                            action = self.apply(hs, result).await?;
                        }
                        Ok(None) => return Err(io::Error::new(io::ErrorKind::BrokenPipe, "connection closed")),
                        Err(_) => {
                            if let Some(pkt) = hs.lost() {
                                self.send_raw(&pkt).await?;
                                action = handshake::Action::Wait;
                            } else {
                                return Err(hs.timeout_error());
                            }
                        }
                    }
                }
                handshake::Action::NeedKey(name) => {
                    let found = keys.ok_or_else(|| io::Error::other("no user directory"))?.as_ref()(&name);
                    let result = hs.provide_key(found);
                    action = self.apply(hs, result).await?;
                }
                handshake::Action::Done(agreed) => {
                    hs.invariants();
                    return Ok(agreed);
                }
            }
        }
    }

    async fn agree_as_initiator(&self, key: &[u8; 32]) -> io::Result<()> {
        let mut hs = handshake::Handshake::shared_initiator(*key, Self::ephemeral());
        self.install(self.drive(&mut hs, None).await?);
        Ok(())
    }

    async fn agree_as_responder(&self, key: &[u8; 32]) -> io::Result<()> {
        let mut hs = handshake::Handshake::shared_responder(*key, Self::ephemeral());
        self.install(self.drive(&mut hs, None).await?);
        Ok(())
    }

    async fn agree_as_user(&self, name: &str, psk: &[u8; 32]) -> io::Result<()> {
        let mut hs = handshake::Handshake::user_initiator(name, *psk, Self::ephemeral());
        self.install(self.drive(&mut hs, None).await?);
        Ok(())
    }

    async fn agree_with_user(&self, keys: &UserKeys) -> io::Result<()> {
        let mut hs = handshake::Handshake::user_responder(Self::ephemeral());
        self.install(self.drive(&mut hs, Some(keys)).await?);
        Ok(())
    }

    fn rto(s: &SendState) -> Duration {
        let rto = match s.srtt {
            Some(srtt) => (srtt + (4 * s.rttvar).max(srtt / 2).max(Duration::from_millis(5))).clamp(MIN_RTO, MAX_RTO),
            None => Duration::from_millis(200),
        };
        assert!(rto >= MIN_RTO);
        assert!(rto <= MAX_RTO);
        rto
    }

    fn lossy(&self) -> bool {
        let p = *self.loss.lock();
        if p <= 0.0 {
            return false;
        }
        let mut x = self.rng.load(Ordering::Relaxed);
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng.store(x, Ordering::Relaxed);
        (x % 10_000) as f32 / 10_000.0 < p
    }

    async fn transmit(&self, msg: u64, frag: u32, nfrags: u32, data: &[u8]) {
        assert!(frag < nfrags);
        assert!(data.len() <= FRAG);
        assert!(msg >= 1);
        let mut pkt = Vec::with_capacity(DATA_HDR + data.len());
        pkt.push(T_DATA);
        pkt.extend_from_slice(&self.id.to_le_bytes());
        pkt.extend_from_slice(&msg.to_le_bytes());
        pkt.extend_from_slice(&frag.to_le_bytes());
        pkt.extend_from_slice(&nfrags.to_le_bytes());
        pkt.extend_from_slice(data);
        assert_eq!(pkt.len(), DATA_HDR + data.len());
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
            let mut batch: Vec<(u64, u32, u32, Arc<[u8]>)> = Vec::new();
            let mut resend: Vec<(u64, u32, u32, Arc<[u8]>)> = Vec::new();
            let next_deadline;
            let mut pace_until: Option<Instant> = None;
            {
                let mut s = self.send.lock();
                let now = Instant::now();
                let rto = Self::rto(&s);
                let gain = if s.cwnd < s.ssthresh { 2.0 } else { 1.25 };
                let per_sec = s.cwnd * gain / s.srtt.map_or(0.05, |d| d.as_secs_f64()).max(0.0005);
                let interval = Duration::from_secs_f64(1.0 / per_sec);
                let burst = ((per_sec * 0.002) as u32).max(PACE_BURST);
                let earliest = now.checked_sub(interval * burst).unwrap_or(now);
                if s.next_send < earliest {
                    s.next_send = earliest;
                }
                let budget =
                    if now >= s.next_send { (now.duration_since(s.next_send).as_secs_f64() * per_sec) as u32 + 1 } else { 0 };
                let budget = budget.min(burst);
                let mut more_due = false;
                let mut timed_out = false;
                let mut fast_lost = false;
                let mut soonest: Option<Instant> = None;
                let high_acked = s.high_acked;
                let mut seq = s.next_seq;
                for ((m, f), inf) in s.inflight.iter_mut() {
                    let due = inf.sent_at + rto * (1 << inf.retries.min(5));
                    let overtaken = high_acked >= inf.seq + REORDER;
                    if (overtaken || due <= now) && resend.len() as u32 >= budget {
                        more_due = true;
                    } else if overtaken || due <= now {
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
                if (timed_out || fast_lost) && now.duration_since(s.last_cut) > s.srtt.unwrap_or(Duration::from_millis(100)) {
                    let beta = if timed_out { 0.5 } else { LOSS_BETA };
                    s.ssthresh = (s.cwnd * beta).max(MIN_CWND);
                    s.cwnd = s.ssthresh;
                    s.last_cut = now;
                    assert!(s.cwnd >= MIN_CWND);
                }
                let budget = budget.saturating_sub(resend.len() as u32);
                let mut sent_new = 0u32;
                while (s.inflight.len() as f64) < s.cwnd {
                    if sent_new >= budget {
                        if !s.queue.is_empty() {
                            pace_until = Some(s.next_send + interval * (sent_new + resend.len() as u32 + 1));
                        }
                        break;
                    }
                    sent_new += 1;
                    let Some((m, f, n, data)) = s.queue.pop_front() else { break };
                    assert!(f < n);
                    assert!(s.queued_bytes >= data.len(), "queued_bytes counts every queued fragment");
                    s.queued_bytes -= data.len();
                    let seq = s.next_seq;
                    s.next_seq += 1;
                    let previous = s.inflight.insert((m, f), Inflight { data: data.clone(), nfrags: n, sent_at: now, retries: 0, seq });
                    assert!(previous.is_none(), "a queued fragment is not in flight already");
                    batch.push((m, f, n, data));
                }
                assert!(s.next_seq > s.high_acked);
                if s.queue.is_empty() {
                    assert_eq!(s.queued_bytes, 0);
                }
                let sent = sent_new.min(batch.len() as u32) + resend.len() as u32;
                s.next_send += interval * sent;
                if more_due {
                    pace_until = Some(s.next_send);
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
            if (!batch.is_empty() || !resend.is_empty()) && pace_until.is_none() {
                continue;
            }
            let next_deadline = match (next_deadline, pace_until) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
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
            assert!(inf.seq < s.next_seq, "acknowledged seq was handed out");
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
                let min = s.min_rtt.map_or(sample, |m| m.min(sample));
                s.min_rtt = Some(min);
                if s.cwnd < s.ssthresh && s.cwnd >= 2.0 * INIT_CWND && sample > min + (min / 8).max(Duration::from_millis(2)) {
                    s.ssthresh = s.cwnd;
                }
            }
            if s.cwnd < s.ssthresh {
                s.cwnd += 1.0;
            } else {
                s.cwnd += GROWTH.max(s.cwnd * 0.05) / s.cwnd;
            }
            s.cwnd = s.cwnd.min(MAX_CWND);
            assert!(s.cwnd >= MIN_CWND);
            assert!(s.cwnd <= MAX_CWND);
        }
        let idle = s.queue.is_empty() && s.inflight.is_empty();
        drop(s);
        self.wake_sender.notify_one();
        if idle {
            self.send_settled.notify_waiters();
        }
    }

    fn on_data(&self, msg: u64, frag: u32, nfrags: u32, data: &[u8]) -> bool {
        let mut r = self.recv.lock();
        if msg < r.done_below || r.done.contains(&msg) {
            return true;
        }
        if nfrags == 0 {
            return false;
        }
        let p = r.partial.entry(msg).or_insert_with(|| Partial { frags: vec![None; nfrags as usize], have: 0 });
        if (frag as usize) >= p.frags.len() {
            return false;
        }
        if p.frags[frag as usize].is_none() {
            p.frags[frag as usize] = Some(data.to_vec());
            p.have += 1;
        }
        assert!(p.have as usize <= p.frags.len());
        if p.have as usize == p.frags.len() {
            let p = r.partial.remove(&msg).unwrap();
            let mut whole = Vec::with_capacity(p.frags.iter().map(|f| f.as_ref().map_or(0, |v| v.len())).sum());
            for f in p.frags {
                whole.extend_from_slice(&f.unwrap());
            }
            assert!(r.done.insert(msg), "a message is delivered once");
            loop {
                let low = r.done_below;
                if !r.done.remove(&low) {
                    break;
                }
                r.done_below += 1;
            }
            assert!(msg < r.done_below || r.done.contains(&msg));
            assert!(!r.done.contains(&r.done_below));
            debug_assert!(r.done.iter().all(|&m| m > r.done_below));
            let repeat = self.handshake_in.lock().iter().any(|m| m == &whole);
            if !repeat {
                self.stats.messages_in.fetch_add(1, Ordering::Relaxed);
                let _ = self.inbound_tx.send(whole);
            }
        }
        true
    }
}

pub struct Endpoint {
    sock: Arc<UdpSocket>,
    conns: Mutex<HashMap<u64, Arc<Conn>>>,
    state: Mutex<endpoint::EndpointState>,
    t0: Instant,
    accept_tx: mpsc::UnboundedSender<Arc<Conn>>,
    accept_rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<Arc<Conn>>>,
    key: LinkKey,
    auth: Auth,
}

impl Endpoint {
    pub async fn bind(addr: SocketAddr, accepting: bool) -> io::Result<Arc<Endpoint>> {
        Endpoint::bind_keyed(addr, accepting, None).await
    }

    pub async fn bind_keyed(addr: SocketAddr, accepting: bool, key: LinkKey) -> io::Result<Arc<Endpoint>> {
        let auth = match key {
            Some(k) => Auth::Shared(k),
            None => Auth::None,
        };
        Endpoint::bind_auth(addr, accepting, auth).await
    }

    pub async fn bind_auth(addr: SocketAddr, accepting: bool, auth: Auth) -> io::Result<Arc<Endpoint>> {
        let key: LinkKey = match &auth {
            Auth::Shared(k) => Some(k.clone()),
            _ => None,
        };
        let sock = Arc::new(UdpSocket::bind(addr).await?);
        {
            use std::os::fd::AsRawFd;
            let fd = sock.as_raw_fd();
            let size: libc::c_int = 16 << 20;
            for opt in [libc::SO_RCVBUFFORCE, libc::SO_SNDBUFFORCE, libc::SO_RCVBUF, libc::SO_SNDBUF] {
                unsafe {
                    libc::setsockopt(fd, libc::SOL_SOCKET, opt, &size as *const _ as *const libc::c_void, 4);
                }
            }
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let per_conn = matches!(auth, Auth::Users(_) | Auth::User { .. });
        let ep = Arc::new(Endpoint {
            sock,
            conns: Mutex::new(HashMap::new()),
            state: Mutex::new(endpoint::EndpointState::new(accepting, per_conn, key.clone())),
            t0: Instant::now(),
            accept_tx: tx,
            accept_rx: tokio::sync::Mutex::new(rx),
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

    pub async fn connect(&self, peer: SocketAddr) -> io::Result<Arc<Conn>> {
        let id = {
            let mut b = [0u8; 8];
            unsafe { libc::getrandom(b.as_mut_ptr() as *mut _, 8, 0) };
            u64::from_le_bytes(b) | 1
        };
        let c = Conn::new(id, self.sock.clone(), peer, self.key.clone(), self.per_conn());
        let now = self.t0.elapsed().as_millis() as u64;
        assert!(self.state.lock().connect(id, peer, now), "connection ids are unique");
        assert!(self.conns.lock().insert(id, c.clone()).is_none(), "connection ids are unique");
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

    pub async fn accept(&self) -> Option<Arc<Conn>> {
        self.accept_rx.lock().await.recv().await
    }

    fn per_conn(&self) -> bool {
        matches!(self.auth, Auth::Users(_) | Auth::User { .. })
    }

    pub fn forget(&self, id: u64) {
        self.state.lock().forget(id);
        if let Some(c) = self.conns.lock().remove(&id) {
            c.close();
        }
    }

    fn sync_tag(&self, bytes: &[u8]) {
        if bytes.len() < 9 {
            return;
        }
        let id = u64::from_le_bytes(bytes[1..9].try_into().unwrap());
        let tag = self.conns.lock().get(&id).and_then(|c| c.tag.get().copied());
        if let Some(tag) = tag {
            self.state.lock().set_tag(id, tag);
        }
    }

    fn open_conn(&self, id: u64, from: SocketAddr) {
        let c = Conn::new(id, self.sock.clone(), from, self.key.clone(), self.per_conn());
        assert!(self.conns.lock().insert(id, c.clone()).is_none(), "connection ids are unique");
        match (self.key.clone(), &self.auth) {
            (_, Auth::Users(keys)) => {
                let (c, tx, keys) = (c.clone(), self.accept_tx.clone(), keys.clone());
                tokio::spawn(async move {
                    match c.agree_with_user(&keys).await {
                        Ok(()) => {
                            let _ = tx.send(c);
                        }
                        Err(_) => {
                            tokio::time::sleep(Duration::from_secs(2)).await;
                            c.close();
                        }
                    }
                });
            }
            (None, _) => {
                let _ = self.accept_tx.send(c.clone());
            }
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
    }

    fn ingest(&self, from: SocketAddr, bytes: &[u8]) {
        self.sync_tag(bytes);
        let now = self.t0.elapsed().as_millis() as u64;
        let routed = self.state.lock().recv(from, bytes, now);
        for ev in routed.events {
            match ev {
                endpoint::RouteEvent::Open { id, from } => self.open_conn(id, from),
                endpoint::RouteEvent::Peer { id, from } => {
                    if let Some(c) = self.conns.lock().get(&id) {
                        *c.last_heard.lock() = Instant::now();
                        *c.peer.lock() = from;
                    }
                }
                endpoint::RouteEvent::Data { id, msg, frag, nfrags, payload } => {
                    let conn = self.conns.lock().get(&id).cloned();
                    if let Some(c) = conn {
                        if c.on_data(msg, frag, nfrags, &payload) {
                            self.state.lock().ack(id, msg, frag, from);
                        }
                    }
                }
                endpoint::RouteEvent::Ack { id, msg, frag } => {
                    if let Some(c) = self.conns.lock().get(&id) {
                        c.on_ack(msg, frag);
                    }
                }
            }
        }
        if let Some(id) = routed.id {
            if let Some(c) = self.conns.lock().get(&id) {
                c.stats.recv_packets.fetch_add(1, Ordering::Relaxed);
                c.stats.recv_bytes.fetch_add(routed.len as u64, Ordering::Relaxed);
            }
        }
    }

    async fn recv_loop(self: Arc<Self>) {
        let mut buf = vec![0u8; 65536];
        loop {
            let Ok((n, from)) = self.sock.recv_from(&mut buf).await else { return };
            self.ingest(from, &buf[..n]);
            while let Ok((n, from)) = self.sock.try_recv_from(&mut buf) {
                self.ingest(from, &buf[..n]);
                if self.state.lock().ack_full() {
                    break;
                }
            }
            let outbound = self.state.lock().take_datagrams();
            for (to, pkt) in outbound {
                let _ = self.sock.send_to(&pkt, to).await;
            }
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
            s.send(&msgs[150]).await.unwrap();
            assert!(tokio::time::timeout(Duration::from_secs(30), c.recv()).await.unwrap().unwrap() == msgs[150]);
        });
    }
}
