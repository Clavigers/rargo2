pub mod link;
mod conn;
mod endpoint;
mod handshake;

use parking_lot::Mutex;
use std::collections::HashMap;
pub mod io;
use std::io as sio;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use crate::io::{Mutex as FiberMutex, Notify, UdpSocket, UnboundedReceiver, UnboundedSender};

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

pub(crate) fn connect_within(wait: Duration, endpoint: &Arc<Endpoint>, server: SocketAddr) -> sio::Result<Arc<Conn>> {
    match io::timeout(wait, || endpoint.connect(server)) {
        Ok(r) => r,
        Err(_) => Err(sio::Error::new(sio::ErrorKind::TimedOut, "no answer over UDP")),
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
    epoch: Instant,
    state: Mutex<conn::ConnState>,
    wake_sender: Notify,
    pub send_settled: Notify,
    space: Notify,
    inbound_tx: UnboundedSender<Vec<u8>>,
    inbound_rx: FiberMutex<UnboundedReceiver<Vec<u8>>>,
    pub stats: NetStats,
    closed: std::sync::atomic::AtomicBool,
    last_heard: Mutex<Instant>,
    rng: AtomicU64,
    handshake_in: Mutex<Vec<Vec<u8>>>,
}

impl Conn {
    fn new(id: u64, sock: Arc<UdpSocket>, peer: SocketAddr, key: LinkKey, per_conn: bool) -> Arc<Conn> {
        let (tx, rx) = io::unbounded_channel();
        let c = Arc::new(Conn {
            id,
            key,
            per_conn,
            cipher: std::sync::OnceLock::new(),
            tag: std::sync::OnceLock::new(),
            user: std::sync::OnceLock::new(),
            sock,
            peer: Mutex::new(peer),
            epoch: Instant::now(),
            state: Mutex::new(conn::ConnState::new(id)),
            wake_sender: Notify::new(),
            send_settled: Notify::new(),
            space: Notify::new(),
            inbound_tx: tx,
            inbound_rx: FiberMutex::new(rx),
            stats: NetStats::default(),
            closed: std::sync::atomic::AtomicBool::new(false),
            last_heard: Mutex::new(Instant::now()),
            rng: AtomicU64::new(id | 1),
            handshake_in: Mutex::new(Vec::new()),
        });
        let c2 = c.clone();
        io::spawn(move || c2.sender_loop());
        c
    }

    pub fn set_loss(&self, fraction: f32) {
        self.state.lock().set_loss(fraction);
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
        self.state.lock().close();
        self.wake_sender.notify_one();
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    pub fn send_idle(&self) -> bool {
        self.state.lock().send_idle()
    }

    pub fn sender_state(&self) -> String {
        let v = self.state.lock().view();
        format!(
            "window {:.0} (slow start below {:.0}), {} in flight, {} KiB queued, round trip {:?} (shortest {:?}), {} sent, {} retransmitted",
            v.cwnd,
            v.ssthresh,
            v.inflight,
            v.queued_bytes >> 10,
            v.srtt,
            v.min_rtt,
            self.stats.sent_packets.load(Ordering::Relaxed),
            self.stats.retransmits.load(Ordering::Relaxed)
        )
    }

    pub fn rtt(&self) -> Option<Duration> {
        self.state.lock().rtt()
    }

    pub fn send(&self, msg: &[u8]) -> sio::Result<()> {
        if !self.encrypted() {
            return self.send_raw(msg);
        }
        let Some(cipher) = self.cipher.get() else {
            return Err(sio::Error::other("link: no keys agreed yet"));
        };
        let sealed = if msg.len() >= 256 << 10 { io::cpu(|| cipher.seal(msg)) } else { cipher.seal(msg) }?;
        self.send_raw(&sealed)
    }

    fn send_raw(&self, msg: &[u8]) -> sio::Result<()> {
        loop {
            if self.is_closed() {
                return Err(sio::Error::new(sio::ErrorKind::BrokenPipe, "connection closed"));
            }
            match self.state.lock().send(msg) {
                conn::Admit::Queued => {
                    self.stats.messages_out.fetch_add(1, Ordering::Relaxed);
                    self.wake_sender.notify_one();
                    return Ok(());
                }
                conn::Admit::Closed => return Err(sio::Error::new(sio::ErrorKind::BrokenPipe, "connection closed")),
                conn::Admit::Full => {}
            }
            self.space.notified();
        }
    }

    pub fn recv(&self) -> Option<Vec<u8>> {
        loop {
            let raw = self.recv_raw()?;
            if self.handshake_in.lock().iter().any(|m| m == &raw) {
                continue;
            }
            if !self.encrypted() {
                return Some(raw);
            }
            let cipher = self.cipher.get()?;
            let opened = if raw.len() >= 256 << 10 { io::cpu(|| cipher.open(&raw)) } else { cipher.open(&raw) };
            return match opened {
                Ok(m) => Some(m),
                Err(_) => {
                    self.close();
                    None
                }
            };
        }
    }

    fn recv_raw(&self) -> Option<Vec<u8>> {
        self.inbound_rx.lock().recv()
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

    fn apply(&self, hs: &mut handshake::Handshake, result: sio::Result<handshake::Action>) -> sio::Result<handshake::Action> {
        match result {
            Ok(action) => Ok(action),
            Err(e) => {
                if let Some(pkt) = hs.take_notify() {
                    let _ = self.send_raw(&pkt);
                }
                Err(e)
            }
        }
    }

    fn drive(&self, hs: &mut handshake::Handshake, keys: Option<&UserKeys>) -> sio::Result<handshake::Agreed> {
        let deadline = Instant::now() + HANDSHAKE_WAIT;
        let mut action = hs.start()?;
        loop {
            hs.invariants();
            match action {
                handshake::Action::Send(pkt) => {
                    self.send_raw(&pkt)?;
                    action = hs.sent()?;
                }
                handshake::Action::Wait => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(hs.timeout_error());
                    }
                    let slice = (left / hs.resend_share()).max(Duration::from_millis(1)).min(left);
                    match io::timeout(slice, || self.recv_raw()) {
                        Ok(Some(pkt)) => {
                            self.note_handshake(&pkt);
                            let result = hs.recv(&pkt);
                            action = self.apply(hs, result)?;
                        }
                        Ok(None) => return Err(sio::Error::new(sio::ErrorKind::BrokenPipe, "connection closed")),
                        Err(_) => {
                            if let Some(pkt) = hs.lost() {
                                self.send_raw(&pkt)?;
                                action = handshake::Action::Wait;
                            } else {
                                return Err(hs.timeout_error());
                            }
                        }
                    }
                }
                handshake::Action::NeedKey(name) => {
                    let found = keys.ok_or_else(|| sio::Error::other("no user directory"))?.as_ref()(&name);
                    let result = hs.provide_key(found);
                    action = self.apply(hs, result)?;
                }
                handshake::Action::Done(agreed) => {
                    hs.invariants();
                    return Ok(agreed);
                }
            }
        }
    }

    fn agree_as_initiator(&self, key: &[u8; 32]) -> sio::Result<()> {
        let mut hs = handshake::Handshake::shared_initiator(*key, Self::ephemeral());
        self.install(self.drive(&mut hs, None)?);
        Ok(())
    }

    fn agree_as_responder(&self, key: &[u8; 32]) -> sio::Result<()> {
        let mut hs = handshake::Handshake::shared_responder(*key, Self::ephemeral());
        self.install(self.drive(&mut hs, None)?);
        Ok(())
    }

    fn agree_as_user(&self, name: &str, psk: &[u8; 32]) -> sio::Result<()> {
        let mut hs = handshake::Handshake::user_initiator(name, *psk, Self::ephemeral());
        self.install(self.drive(&mut hs, None)?);
        Ok(())
    }

    fn agree_with_user(&self, keys: &UserKeys) -> sio::Result<()> {
        let mut hs = handshake::Handshake::user_responder(Self::ephemeral());
        self.install(self.drive(&mut hs, Some(keys))?);
        Ok(())
    }

    fn now(&self) -> Duration {
        self.epoch.elapsed()
    }

    fn jitter(rng: &AtomicU64) -> u64 {
        let mut x = rng.load(Ordering::Relaxed);
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        rng.store(x, Ordering::Relaxed);
        x
    }

    fn signals(&self, out: &conn::Output) {
        if out.space {
            self.space.notify_waiters();
        }
        if out.wake {
            self.wake_sender.notify_one();
        }
        if out.settled {
            self.send_settled.notify_waiters();
        }
    }

    fn deliver(&self, messages: Vec<Vec<u8>>) {
        if messages.is_empty() {
            return;
        }
        let seen = self.handshake_in.lock();
        for m in messages {
            if seen.iter().any(|h| h == &m) {
                continue;
            }
            self.stats.messages_in.fetch_add(1, Ordering::Relaxed);
            let _ = self.inbound_tx.send(m);
        }
    }

    fn emit(&self, packets: Vec<conn::Packet>) {
        for p in packets {
            self.stats.sent_packets.fetch_add(1, Ordering::Relaxed);
            self.stats.sent_bytes.fetch_add(p.bytes.len() as u64, Ordering::Relaxed);
            if p.dropped {
                continue;
            }
            let mut pkt = p.bytes;
            let peer = *self.peer.lock();
            seal(&self.key, &mut pkt);
            if p.msg != AGREEMENT {
                self.tag_datagram(&mut pkt);
            }
            let _ = self.sock.send_to(&pkt, peer);
        }
    }

    fn tag_datagram(&self, pkt: &mut Vec<u8>) {
        if let Some(k) = self.tag.get() {
            let tag = blake3::keyed_hash(k, pkt);
            pkt.extend_from_slice(&tag.as_bytes()[..TAG]);
        }
    }

    fn sender_loop(self: Arc<Self>) {
        loop {
            if self.is_closed() {
                return;
            }
            let now = self.now();
            let rng = &self.rng;
            let mut draw = || Self::jitter(rng);
            let out = self.state.lock().on_timer(now, &mut draw);
            if out.stopped {
                return;
            }
            self.signals(&out);
            self.stats.retransmits.fetch_add(out.retransmits, Ordering::Relaxed);
            let again = out.again;
            let deadline = out.deadline;
            self.emit(out.datagrams);
            if again {
                continue;
            }
            match deadline {
                Some(d) => {
                    let wait = d.saturating_sub(self.now()).max(Duration::from_millis(1));
                    let _ = io::timeout(wait, || self.wake_sender.notified());
                }
                None => self.wake_sender.notified(),
            }
        }
    }

    fn on_ack(&self, msg: u64, frag: u32) {
        let pkt = conn::ack_packet(self.id, &[(msg, frag)]);
        let now = self.now();
        let out = self.state.lock().on_datagram(now, &pkt);
        self.signals(&out);
    }

    fn on_data(&self, msg: u64, frag: u32, nfrags: u32, data: &[u8]) -> bool {
        let pkt = conn::data_packet(self.id, msg, frag, nfrags, data);
        let now = self.now();
        let out = self.state.lock().on_datagram(now, &pkt);
        let ack = !out.acks.is_empty();
        self.deliver(out.messages);
        ack
    }
}

pub struct Endpoint {
    sock: Arc<UdpSocket>,
    conns: Mutex<HashMap<u64, Arc<Conn>>>,
    state: Mutex<endpoint::EndpointState>,
    t0: Instant,
    accept_tx: UnboundedSender<Arc<Conn>>,
    accept_rx: FiberMutex<UnboundedReceiver<Arc<Conn>>>,
    key: LinkKey,
    auth: Auth,
}

impl Endpoint {
    pub fn bind(addr: SocketAddr, accepting: bool) -> sio::Result<Arc<Endpoint>> {
        Endpoint::bind_keyed(addr, accepting, None)
    }

    pub fn bind_keyed(addr: SocketAddr, accepting: bool, key: LinkKey) -> sio::Result<Arc<Endpoint>> {
        let auth = match key {
            Some(k) => Auth::Shared(k),
            None => Auth::None,
        };
        Endpoint::bind_auth(addr, accepting, auth)
    }

    pub fn bind_auth(addr: SocketAddr, accepting: bool, auth: Auth) -> sio::Result<Arc<Endpoint>> {
        let key: LinkKey = match &auth {
            Auth::Shared(k) => Some(k.clone()),
            _ => None,
        };
        let sock = Arc::new(UdpSocket::bind(addr)?);
        sock.set_buffers(16 << 20);
        let (tx, rx) = io::unbounded_channel();
        let per_conn = matches!(auth, Auth::Users(_) | Auth::User { .. });
        let ep = Arc::new(Endpoint {
            sock,
            conns: Mutex::new(HashMap::new()),
            state: Mutex::new(endpoint::EndpointState::new(accepting, per_conn, key.clone())),
            t0: Instant::now(),
            accept_tx: tx,
            accept_rx: FiberMutex::new(rx),
            key,
            auth,
        });
        let ep2 = ep.clone();
        io::spawn(move || ep2.recv_loop());
        Ok(ep)
    }

    pub fn local_addr(&self) -> sio::Result<SocketAddr> {
        self.sock.local_addr()
    }

    pub fn connect(&self, peer: SocketAddr) -> sio::Result<Arc<Conn>> {
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
            Auth::Shared(key) => c.agree_as_initiator(key),
            Auth::User { name, psk } => c.agree_as_user(name, psk),
            Auth::Users(_) => Err(sio::Error::other("an accepting endpoint does not connect")),
        };
        if let Err(e) = agreed {
            self.forget(id);
            return Err(e);
        }
        Ok(c)
    }

    pub fn accept(&self) -> Option<Arc<Conn>> {
        self.accept_rx.lock().recv()
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
                io::spawn(move || {
                    match c.agree_with_user(&keys) {
                        Ok(()) => {
                            let _ = tx.send(c);
                        }
                        Err(_) => {
                            io::sleep(Duration::from_secs(2));
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
                io::spawn(move || {
                    match c.agree_as_responder(&key) {
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

    fn recv_loop(self: Arc<Self>) {
        let mut buf = vec![0u8; 65536];
        loop {
            let Ok((n, from)) = self.sock.recv_from(&mut buf) else { return };
            self.ingest(from, &buf[..n]);
            while let Ok((n, from)) = self.sock.try_recv_from(&mut buf) {
                self.ingest(from, &buf[..n]);
                if self.state.lock().ack_full() {
                    break;
                }
            }
            let outbound = self.state.lock().take_datagrams();
            for (to, pkt) in outbound {
                let _ = self.sock.send_to(&pkt, to);
            }
        }
    }
}


