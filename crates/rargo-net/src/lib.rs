pub mod link;
mod conn;
mod endpoint;
mod handshake;

/// The UDP link's state machines and packet adapters for shells with their own clock and I/O.
pub mod sans_io {
    pub use crate::conn::{Admit, ConnState, Output, Packet, ack_packet, data_packet};
    pub use crate::endpoint::{EndpointState, RouteEvent, Routed};
    pub use crate::handshake::{Action, Agreed, Handshake};
    pub use crate::{HANDSHAKE_WAIT, LinkKey, seal};
}

use rargo_machine::StateMachine;
use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{BTreeMap, VecDeque};
pub mod io;
use std::io as sio;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use crate::io::{Mutex as FiberMutex, Notify, UdpSocket, UnboundedReceiver, UnboundedSender};

const FRAG: usize = 1200;
const T_DATA: u8 = 1;
const T_ACK: u8 = 2;
/// "No such connection": the answer to a packet for a connection id the endpoint does not have.
const T_RESET: u8 = 3;
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

pub(crate) fn connect_within(wait: Duration, endpoint: &Rc<Endpoint>, server: SocketAddr) -> sio::Result<Rc<Conn>> {
    match io::timeout(wait, || endpoint.connect(server)) {
        Ok(r) => r,
        Err(_) => Err(sio::Error::new(sio::ErrorKind::TimedOut, "no answer over UDP")),
    }
}

#[derive(Default)]
pub struct NetStats {
    pub sent_packets: Cell<u64>,
    pub sent_bytes: Cell<u64>,
    pub retransmits: Cell<u64>,
    pub recv_packets: Cell<u64>,
    pub recv_bytes: Cell<u64>,
    pub messages_in: Cell<u64>,
    pub messages_out: Cell<u64>,
}

fn bump(counter: &Cell<u64>, by: u64) {
    counter.set(counter.get().wrapping_add(by));
}

pub type LinkKey = Option<Arc<[u8; 32]>>;

/// A source of random bytes: connection ids, handshake ephemerals and the
/// seed of the retransmit jitter all come from one, passed in by the caller.
/// `io::os_random` is the real one, `io::seeded_random` the one for tests.
pub trait Random {
    fn fill(&self, out: &mut [u8]);
}

fn draw_u64(random: &dyn Random) -> u64 {
    let mut b = [0u8; 8];
    random.fill(&mut b);
    u64::from_le_bytes(b)
}

/// A new connection's id: odd, so it is never zero.
fn conn_id(random: &dyn Random) -> u64 {
    draw_u64(random) | 1
}

/// The seed of a connection's xorshift jitter, which must not be zero.
fn jitter_seed(random: &dyn Random) -> u64 {
    draw_u64(random) | 1
}

fn ephemeral(random: &dyn Random) -> [u8; 32] {
    let mut b = [0u8; 32];
    random.fill(&mut b);
    b
}

pub type UserKeys = Rc<dyn Fn(&str) -> Option<[u8; 32]>>;

#[derive(Clone)]
pub enum Auth {
    None,
    Shared(Arc<[u8; 32]>),
    Users(UserKeys),
    User { name: String, psk: [u8; 32] },
    UnmarkedUser { name: String, psk: [u8; 32] },
}

fn user_handshake_key(psk: &[u8; 32]) -> [u8; 32] {
    blake3::derive_key("rargo client link handshake v1", psk)
}

const AGREEMENT: u64 = 1;

fn handshake_key(key: &[u8; 32]) -> [u8; 32] {
    blake3::derive_key("rargo worker link handshake v2", key)
}
pub const HANDSHAKE_WAIT: Duration = Duration::from_secs(20);
const TAG: usize = 16;
const _: () = assert!(DATA_HDR + FRAG + 2 * TAG <= 1500 - 20 - 8);
const _: () = assert!(AGREEMENT >= 1, "message ids start at 1");

pub fn seal(key: &LinkKey, pkt: &mut Vec<u8>) {
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
    cipher: OnceCell<rargo_proto::secure::LinkCipher>,
    tag: OnceCell<[u8; 32]>,
    user: OnceCell<String>,
    sock: Rc<UdpSocket>,
    peer: Cell<SocketAddr>,
    epoch: Instant,
    state: RefCell<conn::ConnState>,
    wake_sender: Notify,
    pub send_settled: Notify,
    space: Notify,
    inbound_tx: UnboundedSender<Vec<u8>>,
    inbound_rx: FiberMutex<UnboundedReceiver<Vec<u8>>>,
    pub stats: NetStats,
    closed: Cell<bool>,
    last_heard: Cell<Instant>,
    rng: Cell<u64>,
    random: Rc<dyn Random>,
    handshake_in: RefCell<Vec<Vec<u8>>>,
}

impl Conn {
    fn new(id: u64, sock: Rc<UdpSocket>, peer: SocketAddr, key: LinkKey, per_conn: bool, random: Rc<dyn Random>) -> Rc<Conn> {
        let (tx, rx) = io::unbounded_channel();
        let c = Rc::new(Conn {
            id,
            key,
            per_conn,
            cipher: OnceCell::new(),
            tag: OnceCell::new(),
            user: OnceCell::new(),
            sock,
            peer: Cell::new(peer),
            epoch: io::now(),
            state: RefCell::new(conn::ConnState::new(id)),
            wake_sender: Notify::new(),
            send_settled: Notify::new(),
            space: Notify::new(),
            inbound_tx: tx,
            inbound_rx: FiberMutex::new(rx),
            stats: NetStats::default(),
            closed: Cell::new(false),
            last_heard: Cell::new(io::now()),
            rng: Cell::new(jitter_seed(random.as_ref())),
            random,
            handshake_in: RefCell::new(Vec::new()),
        });
        let c2 = c.clone();
        io::spawn(move || c2.sender_loop());
        c
    }

    pub fn set_loss(&self, fraction: f32) {
        self.state.borrow_mut().set_loss(fraction);
    }

    pub fn user(&self) -> Option<&str> {
        self.user.get().map(|s| s.as_str())
    }

    fn encrypted(&self) -> bool {
        self.key.is_some() || self.per_conn
    }

    pub fn peer(&self) -> SocketAddr {
        self.peer.get()
    }

    pub fn idle(&self) -> Duration {
        io::now().saturating_duration_since(self.last_heard.get())
    }

    pub fn close(&self) {
        self.closed.set(true);
        self.state.borrow_mut().close();
        self.wake_sender.notify_one();
    }

    pub fn is_closed(&self) -> bool {
        self.closed.get()
    }

    pub fn send_idle(&self) -> bool {
        self.state.borrow().send_idle()
    }

    pub fn sender_state(&self) -> String {
        let v = self.state.borrow().view();
        format!(
            "window {:.0} (slow start below {:.0}), {} in flight, {} KiB queued, round trip {:?} (shortest {:?}), {} sent, {} retransmitted",
            v.cwnd,
            v.ssthresh,
            v.inflight,
            v.queued_bytes >> 10,
            v.srtt,
            v.min_rtt,
            self.stats.sent_packets.get(),
            self.stats.retransmits.get()
        )
    }

    pub fn rtt(&self) -> Option<Duration> {
        self.state.borrow().rtt()
    }

    pub fn send(&self, msg: &[u8]) -> sio::Result<()> {
        if !self.encrypted() {
            return self.send_raw(msg);
        }
        let Some(cipher) = self.cipher.get() else {
            return Err(sio::Error::other("link: no keys agreed yet"));
        };
        let base = cipher.reserve(msg.len());
        let keys = cipher.keys();
        let sealed = if msg.len() >= 256 << 10 { io::cpu(|| keys.seal_from(base, msg)) } else { keys.seal_from(base, msg) }?;
        self.send_raw(&sealed)
    }

    fn send_raw(&self, msg: &[u8]) -> sio::Result<()> {
        loop {
            if self.is_closed() {
                return Err(sio::Error::new(sio::ErrorKind::BrokenPipe, "connection closed"));
            }
            let admit = self.state.borrow_mut().send(msg);
            match admit {
                conn::Admit::Queued => {
                    bump(&self.stats.messages_out, 1);
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
            if self.handshake_in.borrow().iter().any(|m| m == &raw) {
                continue;
            }
            if !self.encrypted() {
                return Some(raw);
            }
            let keys = self.cipher.get()?.keys();
            let opened = if raw.len() >= 256 << 10 { io::cpu(|| keys.open(&raw)) } else { keys.open(&raw) };
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
        self.handshake_in.borrow_mut().push(msg.to_vec());
    }

    fn ephemeral(&self) -> [u8; 32] {
        ephemeral(self.random.as_ref())
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
        let deadline = io::now() + HANDSHAKE_WAIT;
        let mut action = hs.start()?;
        loop {
            hs.invariants();
            match action {
                handshake::Action::Send(pkt) => {
                    self.send_raw(&pkt)?;
                    action = hs.sent()?;
                }
                handshake::Action::Wait => {
                    let left = deadline.saturating_duration_since(io::now());
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
        let mut hs = handshake::Handshake::shared_initiator(*key, self.ephemeral());
        self.install(self.drive(&mut hs, None)?);
        Ok(())
    }

    fn agree_as_responder(&self, key: &[u8; 32]) -> sio::Result<()> {
        let mut hs = handshake::Handshake::shared_responder(*key, self.ephemeral());
        self.install(self.drive(&mut hs, None)?);
        Ok(())
    }

    fn agree_as_user(&self, name: &str, psk: &[u8; 32], marked: bool) -> sio::Result<()> {
        let mut hs = handshake::Handshake::user_initiator(name, *psk, self.ephemeral(), marked);
        self.install(self.drive(&mut hs, None)?);
        Ok(())
    }

    fn agree_with_user(&self, keys: &UserKeys) -> sio::Result<()> {
        let mut hs = handshake::Handshake::user_responder(self.ephemeral());
        self.install(self.drive(&mut hs, Some(keys))?);
        Ok(())
    }

    fn now(&self) -> Duration {
        io::now().saturating_duration_since(self.epoch)
    }

    fn jitter(rng: &Cell<u64>) -> u64 {
        let mut x = rng.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        rng.set(x);
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
        let seen = self.handshake_in.borrow();
        for m in messages {
            if seen.iter().any(|h| h == &m) {
                continue;
            }
            bump(&self.stats.messages_in, 1);
            let _ = self.inbound_tx.send(m);
        }
    }

    fn emit(&self, packets: Vec<conn::Packet>) {
        for p in packets {
            bump(&self.stats.sent_packets, 1);
            bump(&self.stats.sent_bytes, p.bytes.len() as u64);
            if p.dropped {
                continue;
            }
            let mut pkt = p.bytes;
            let peer = self.peer.get();
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

    fn sender_loop(self: Rc<Self>) {
        loop {
            if self.is_closed() {
                return;
            }
            let now = self.now();
            let rng = &self.rng;
            let mut draw = || Self::jitter(rng);
            let out = self.state.borrow_mut().on_timer(now, &mut draw);
            if out.stopped {
                return;
            }
            self.signals(&out);
            bump(&self.stats.retransmits, out.retransmits);
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
        let out = self.state.borrow_mut().on_datagram(now, &pkt);
        self.signals(&out);
    }

    fn on_data(&self, msg: u64, frag: u32, nfrags: u32, data: &[u8]) -> bool {
        let pkt = conn::data_packet(self.id, msg, frag, nfrags, data);
        let now = self.now();
        let out = self.state.borrow_mut().on_datagram(now, &pkt);
        let ack = !out.acks.is_empty();
        self.deliver(out.messages);
        ack
    }
}

/// Connections that are ready for `Endpoint::accept`, oldest first.
#[derive(Default)]
struct Accepted {
    queue: RefCell<VecDeque<Rc<Conn>>>,
    ready: Notify,
}

impl Accepted {
    fn push(&self, conn: Rc<Conn>) {
        self.queue.borrow_mut().push_back(conn);
        self.ready.notify_one();
    }

    fn pop(&self) -> Rc<Conn> {
        loop {
            let next = self.queue.borrow_mut().pop_front();
            if let Some(conn) = next {
                return conn;
            }
            self.ready.notified();
        }
    }
}

pub struct Endpoint {
    sock: Rc<UdpSocket>,
    conns: RefCell<BTreeMap<u64, Rc<Conn>>>,
    state: RefCell<endpoint::EndpointState>,
    t0: Instant,
    accepted: Rc<Accepted>,
    key: LinkKey,
    auth: Auth,
    random: Rc<dyn Random>,
}

impl Endpoint {
    pub fn bind(addr: SocketAddr, accepting: bool) -> sio::Result<Rc<Endpoint>> {
        Endpoint::bind_keyed(addr, accepting, None)
    }

    pub fn bind_keyed(addr: SocketAddr, accepting: bool, key: LinkKey) -> sio::Result<Rc<Endpoint>> {
        let auth = match key {
            Some(k) => Auth::Shared(k),
            None => Auth::None,
        };
        Endpoint::bind_auth(addr, accepting, auth)
    }

    pub fn bind_auth(addr: SocketAddr, accepting: bool, auth: Auth) -> sio::Result<Rc<Endpoint>> {
        Endpoint::bind_random(addr, accepting, auth, io::os_random())
    }

    /// Binds an endpoint that draws every random value from `random`.
    pub fn bind_random(addr: SocketAddr, accepting: bool, auth: Auth, random: Rc<dyn Random>) -> sio::Result<Rc<Endpoint>> {
        let key: LinkKey = match &auth {
            Auth::Shared(k) => Some(k.clone()),
            _ => None,
        };
        let sock = Rc::new(UdpSocket::bind(addr)?);
        sock.set_buffers(16 << 20);
        let per_conn = matches!(auth, Auth::Users(_) | Auth::User { .. } | Auth::UnmarkedUser { .. });
        let ep = Rc::new(Endpoint {
            sock,
            conns: RefCell::new(BTreeMap::new()),
            state: RefCell::new(endpoint::EndpointState::new(accepting, per_conn, key.clone())),
            t0: io::now(),
            accepted: Rc::default(),
            key,
            auth,
            random,
        });
        let ep2 = ep.clone();
        io::spawn(move || ep2.recv_loop());
        Ok(ep)
    }

    pub fn local_addr(&self) -> sio::Result<SocketAddr> {
        self.sock.local_addr()
    }

    pub fn connect(&self, peer: SocketAddr) -> sio::Result<Rc<Conn>> {
        let id = conn_id(self.random.as_ref());
        let c = Conn::new(id, self.sock.clone(), peer, self.key.clone(), self.per_conn(), self.random.clone());
        let now = io::now().saturating_duration_since(self.t0).as_millis() as u64;
        assert!(self.state.borrow_mut().connect(id, peer, now), "connection ids are unique");
        assert!(self.conns.borrow_mut().insert(id, c.clone()).is_none(), "connection ids are unique");
        let agreed = match &self.auth {
            Auth::None => Ok(()),
            Auth::Shared(key) => c.agree_as_initiator(key),
            Auth::User { name, psk } => c.agree_as_user(name, psk, true),
            Auth::UnmarkedUser { name, psk } => c.agree_as_user(name, psk, false),
            Auth::Users(_) => Err(sio::Error::other("an accepting endpoint does not connect")),
        };
        if let Err(e) = agreed {
            self.forget(id);
            return Err(e);
        }
        Ok(c)
    }

    pub fn accept(&self) -> Option<Rc<Conn>> {
        Some(self.accepted.pop())
    }

    fn per_conn(&self) -> bool {
        matches!(self.auth, Auth::Users(_) | Auth::User { .. } | Auth::UnmarkedUser { .. })
    }

    pub fn forget(&self, id: u64) {
        self.state.borrow_mut().forget(id);
        let removed = self.conns.borrow_mut().remove(&id);
        if let Some(c) = removed {
            c.close();
        }
    }

    fn sync_tag(&self, bytes: &[u8]) {
        if bytes.len() < 9 {
            return;
        }
        let id = u64::from_le_bytes(bytes[1..9].try_into().unwrap());
        let tag = self.conns.borrow().get(&id).and_then(|c| c.tag.get().copied());
        if let Some(tag) = tag {
            self.state.borrow_mut().set_tag(id, tag);
        }
    }

    fn open_conn(&self, id: u64, from: SocketAddr) {
        let c = Conn::new(id, self.sock.clone(), from, self.key.clone(), self.per_conn(), self.random.clone());
        assert!(self.conns.borrow_mut().insert(id, c.clone()).is_none(), "connection ids are unique");
        match (self.key.clone(), &self.auth) {
            (_, Auth::Users(keys)) => {
                let (c, accepted, keys) = (c.clone(), self.accepted.clone(), keys.clone());
                io::spawn(move || {
                    match c.agree_with_user(&keys) {
                        Ok(()) => accepted.push(c),
                        Err(_) => {
                            io::sleep(Duration::from_secs(2));
                            c.close();
                        }
                    }
                });
            }
            (None, _) => {
                self.accepted.push(c.clone());
            }
            (Some(key), _) => {
                let (c, accepted) = (c.clone(), self.accepted.clone());
                io::spawn(move || {
                    match c.agree_as_responder(&key) {
                        Ok(()) => accepted.push(c),
                        Err(_) => c.close(),
                    }
                });
            }
        }
    }

    fn conn(&self, id: u64) -> Option<Rc<Conn>> {
        self.conns.borrow().get(&id).cloned()
    }

    fn ingest(&self, from: SocketAddr, bytes: &[u8]) {
        self.sync_tag(bytes);
        let now = io::now().saturating_duration_since(self.t0).as_millis() as u64;
        let routed = self.state.borrow_mut().recv(from, bytes, now);
        let mut reset = None;
        for ev in routed.events {
            match ev {
                endpoint::RouteEvent::Open { id, from } => self.open_conn(id, from),
                endpoint::RouteEvent::Peer { id, from } => {
                    if let Some(c) = self.conn(id) {
                        c.last_heard.set(io::now());
                        c.peer.set(from);
                    }
                }
                endpoint::RouteEvent::Data { id, msg, frag, nfrags, payload } => {
                    if let Some(c) = self.conn(id) {
                        if c.on_data(msg, frag, nfrags, &payload) {
                            self.state.borrow_mut().ack(id, msg, frag, from);
                        }
                    }
                }
                endpoint::RouteEvent::Ack { id, msg, frag } => {
                    if let Some(c) = self.conn(id) {
                        c.on_ack(msg, frag);
                    }
                }
                endpoint::RouteEvent::Reset { id } => reset = Some(id),
            }
        }
        if let Some(id) = routed.id {
            if let Some(c) = self.conn(id) {
                bump(&c.stats.recv_packets, 1);
                bump(&c.stats.recv_bytes, routed.len as u64);
            }
        }
        if let Some(id) = reset {
            self.forget(id);
        }
    }

    fn recv_loop(self: Rc<Self>) {
        let mut buf = vec![0u8; 65536];
        loop {
            let Ok((n, from)) = self.sock.recv_from(&mut buf) else { return };
            self.ingest(from, &buf[..n]);
            while let Ok((n, from)) = self.sock.try_recv_from(&mut buf) {
                self.ingest(from, &buf[..n]);
                if self.state.borrow().ack_full() {
                    break;
                }
            }
            let outbound = self.state.borrow_mut().take_datagrams();
            for (to, pkt) in outbound {
                let _ = self.sock.send_to(&pkt, to);
            }
        }
    }
}

#[cfg(test)]
mod tests_sim;
#[cfg(test)]
mod tests {
    use super::*;

    fn draws(seed: u64) -> (Vec<u64>, [u8; 32], u64) {
        let random = io::seeded_random(seed);
        let ids = (0..4).map(|_| conn_id(random.as_ref())).collect();
        (ids, ephemeral(random.as_ref()), jitter_seed(random.as_ref()))
    }

    #[test]
    fn the_same_seed_gives_the_same_ids() {
        let (ids, eph, jitter) = draws(7);
        assert_eq!(draws(7), (ids.clone(), eph, jitter), "a run is a function of its seed");
        assert!(ids.iter().all(|id| id & 1 == 1), "connection ids are never zero");
        assert_ne!(jitter, 0, "xorshift never leaves zero");
        let mut distinct = ids.clone();
        distinct.sort();
        distinct.dedup();
        assert_eq!(distinct.len(), ids.len(), "one run's ids differ");
        assert_ne!(draws(8).0, ids, "another seed gives other ids");
    }

    #[test]
    fn a_seeded_source_fills_any_length() {
        let random = io::seeded_random(1);
        let mut odd = [0u8; 13];
        random.fill(&mut odd);
        assert_ne!(odd, [0u8; 13]);
        let (a, b) = (io::seeded_random(1), io::seeded_random(1));
        let (mut x, mut y) = ([0u8; 13], [0u8; 13]);
        a.fill(&mut x);
        b.fill(&mut y);
        assert_eq!(x, y);
    }
}
