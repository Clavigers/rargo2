use rargo_machine::StateMachine;
use crate::io::{self as shell, JoinHandle, Mutex as FiberMutex, Notify, ReadHalf, TcpStream, WriteHalf};
use crate::{Auth, Conn, Endpoint};
use rargo_proto::secure::{self, Opener, Sealer};
use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

/// A TCP record is a little-endian length and then that many bytes.
pub const FRAME_LIMIT: usize = 65535;

#[derive(Debug, PartialEq, Eq)]
pub enum FrameError {
    Oversize { len: usize, limit: usize },
    Truncated { missing: usize },
}

/// Bytes of one frame, with a length prefix. The payload is at most `limit` bytes.
pub fn encode_frame(payload: &[u8], limit: usize) -> Result<Vec<u8>, FrameError> {
    if payload.len() > limit {
        return Err(FrameError::Oversize { len: payload.len(), limit });
    }
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// Incremental decoder. Feed it any split of a byte stream; it yields whole frames.
pub struct FrameDecoder {
    limit: usize,
    buf: Vec<u8>,
    body: Option<usize>,
    failed: Option<usize>,
}

impl FrameDecoder {
    pub fn new(limit: usize) -> FrameDecoder {
        FrameDecoder { limit, buf: Vec::new(), body: None, failed: None }
    }

    fn need(&self) -> usize {
        match self.body {
            None => 4 - self.buf.len(),
            Some(n) => n - self.buf.len(),
        }
    }

    /// Bytes still required to finish the frame in progress. Zero at a frame boundary.
    pub fn missing(&self) -> usize {
        if self.failed.is_some() || (self.body.is_none() && self.buf.is_empty()) {
            0
        } else {
            self.need()
        }
    }

    /// A stream that ends on a frame boundary is complete. A partial frame says how many bytes are absent.
    pub fn end(&self) -> Result<(), FrameError> {
        self.invariants();
        if let Some(len) = self.failed {
            return Err(FrameError::Oversize { len, limit: self.limit });
        }
        let missing = self.missing();
        if missing == 0 { Ok(()) } else { Err(FrameError::Truncated { missing }) }
    }

    pub fn push(&mut self, mut chunk: &[u8]) -> Result<Vec<Vec<u8>>, FrameError> {
        if let Some(len) = self.failed {
            return Err(FrameError::Oversize { len, limit: self.limit });
        }
        let mut out = Vec::new();
        while !chunk.is_empty() {
            let need = self.need();
            let take = need.min(chunk.len());
            self.buf.extend_from_slice(&chunk[..take]);
            chunk = &chunk[take..];
            if take < need {
                break;
            }
            if self.body.is_none() {
                let len = u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
                self.buf.clear();
                if len > self.limit {
                    self.failed = Some(len);
                    self.body = None;
                    self.invariants();
                    return Err(FrameError::Oversize { len, limit: self.limit });
                }
                if len == 0 {
                    out.push(Vec::new());
                } else {
                    self.body = Some(len);
                }
            } else {
                let frame = std::mem::take(&mut self.buf);
                self.body = None;
                out.push(frame);
            }
        }
        self.invariants();
        Ok(out)
    }
}

impl StateMachine for FrameDecoder {
    fn invariants(&self) {
        if self.failed.is_some() {
            assert!(self.buf.is_empty() && self.body.is_none(), "a refused frame leaves the decoder idle");
            return;
        }
        match self.body {
            None => assert!(self.buf.len() < 4, "a partial length is shorter than 4 bytes"),
            Some(n) => {
                assert!(n > 0 && n <= self.limit, "a body length is inside the limit");
                assert!(self.buf.len() < n, "a partial body is shorter than its frame");
                assert_eq!(self.missing(), n - self.buf.len(), "missing counts the body bytes not yet fed");
            }
        }
        if self.body.is_none() && !self.buf.is_empty() {
            assert_eq!(self.missing(), 4 - self.buf.len(), "missing counts the length bytes not yet fed");
        }
        if self.body.is_none() && self.buf.is_empty() {
            assert_eq!(self.missing(), 0, "a frame boundary has nothing missing");
        }
    }
}

struct Inbox {
    msgs: VecDeque<Vec<u8>>,
    stop: bool,
    eof: bool,
}

struct Shared {
    mu: Mutex<Inbox>,
    space: Notify,
    data: Notify,
}

pub struct TcpLink {
    wr: FiberMutex<Option<(WriteHalf, Sealer)>>,
    shared: Arc<Shared>,
    closed: AtomicBool,
    reader: Mutex<Option<JoinHandle<()>>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub enum Link {
    Udp(Arc<Conn>, Arc<Endpoint>),
    Tcp(TcpLink),
}

const UDP_WAIT: Duration = Duration::from_millis(2500);
/// How long connecting over TCP and the handshake after it may take: a stopped server's kernel still accepts the connection.
const CONNECT_WITHIN: Duration = Duration::from_secs(5);

/// A byte stream a TCP dial speaks its handshake over.
trait Wire {
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()>;
    fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()>;
}

impl Wire for TcpStream {
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        TcpStream::write_all(self, buf)
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
        TcpStream::read_exact(self, buf)
    }
}

/// What a TCP dial needs from the world: a connection to the server, and the clock that bounds the whole dial.
trait Dial {
    type Wire: Wire + 'static;
    fn connect(&self) -> io::Result<Self::Wire>;
    fn within<T: 'static>(&self, limit: Duration, body: impl FnOnce() -> T) -> Option<T>;
}

/// The real dial: the event loop's TCP and its timers.
struct Tcp<F>(F);

impl<F: Fn() -> io::Result<TcpStream>> Dial for Tcp<F> {
    type Wire = TcpStream;

    fn connect(&self) -> io::Result<TcpStream> {
        (self.0)()
    }

    fn within<T: 'static>(&self, limit: Duration, body: impl FnOnce() -> T) -> Option<T> {
        shell::timeout(limit, body).ok()
    }
}

/// Connects over TCP and finishes the handshake, all within `CONNECT_WITHIN` on the dial's clock.
fn dial<D: Dial>(d: &D, user: &str, psk: &[u8; 32], udp_error: Option<io::Error>) -> io::Result<(D::Wire, Sealer, Opener)> {
    let handshake = || -> io::Result<(D::Wire, Sealer, Opener)> {
        let mut wire = match d.connect() {
            Ok(w) => w,
            Err(e) => return Err(if e.kind() == io::ErrorKind::ConnectionRefused { e } else { udp_error.unwrap_or(e) }),
        };
        let (hs, first) = secure::client_start(user, psk, &[])?;
        wire.write_all(&first)?;
        let mut head = [0u8; 3];
        wire.read_exact(&mut head)?;
        let mut body = vec![0u8; u16::from_le_bytes([head[1], head[2]]) as usize];
        wire.read_exact(&mut body)?;
        if head[0] != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("the server refused the connection: {}", String::from_utf8_lossy(&body).trim()),
            ));
        }
        let (sealer, opener) = secure::client_finish(hs, &body)?;
        Ok((wire, sealer, opener))
    };
    match d.within(CONNECT_WITHIN, handshake) {
        Some(dialled) => dialled,
        None => Err(io::Error::new(io::ErrorKind::TimedOut, format!("no answer over TCP within {} s", CONNECT_WITHIN.as_secs()))),
    }
}

impl Link {
    pub fn tcp(stream: TcpStream, sealer: Sealer, opener: Opener) -> Link {
        let _ = stream.set_nodelay(true);
        let (rd, wr) = stream.into_split();
        let shared = Arc::new(Shared {
            mu: Mutex::new(Inbox { msgs: VecDeque::new(), stop: false, eof: false }),
            space: Notify::new(),
            data: Notify::new(),
        });
        let reading = Arc::clone(&shared);
        let reader = shell::spawn(move || tcp_read(rd, opener, reading));
        Link::Tcp(TcpLink {
            wr: FiberMutex::new(Some((wr, sealer))),
            shared,
            closed: AtomicBool::new(false),
            reader: Mutex::new(Some(reader)),
        })
    }

    pub fn connect(server: SocketAddr, user: &str, psk: &[u8; 32], transport: &str) -> io::Result<Link> {
        let mut udp_error = None;
        if transport != "tcp" {
            let bind: SocketAddr = if server.is_ipv4() { "0.0.0.0:0".parse().unwrap() } else { "[::]:0".parse().unwrap() };
            let wait = if transport == "udp" { Duration::from_secs(20) } else { UDP_WAIT };
            let connect = |marked| {
                let (name, psk) = (user.to_string(), *psk);
                let auth = if marked { Auth::User { name, psk } } else { Auth::UnmarkedUser { name, psk } };
                let endpoint = Endpoint::bind_auth(bind, false, auth)?;
                crate::connect_within(wait, &endpoint, server).map(|conn| Link::Udp(conn, endpoint))
            };
            // Use a fresh socket for an old server. Its failed handshake may still own the first peer address.
            let result = connect(true).or_else(|error| {
                if error.kind() == io::ErrorKind::Unsupported { connect(false) } else { Err(error) }
            });
            match result {
                Ok(link) => return Ok(link),
                Err(e) if e.kind() == io::ErrorKind::PermissionDenied => return Err(e),
                Err(e) => udp_error = Some(e),
            }
            if transport == "udp" {
                return Err(udp_error.unwrap());
            }
        }
        let tcp = Tcp(move || {
            let stream = TcpStream::connect(server)?;
            let _ = stream.set_nodelay(true);
            Ok(stream)
        });
        let (stream, sealer, opener) = dial(&tcp, user, psk, udp_error)?;
        Ok(Link::tcp(stream, sealer, opener))
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Link::Udp(..) => "udp",
            Link::Tcp(_) => "tcp",
        }
    }

    pub fn send(&self, msg: &[u8]) -> io::Result<()> {
        match self {
            Link::Udp(c, _) => c.send(msg),
            Link::Tcp(t) => t.send(msg),
        }
    }

    pub fn recv(&self) -> Option<Vec<u8>> {
        match self {
            Link::Udp(c, _) => c.recv(),
            Link::Tcp(t) => t.recv(),
        }
    }

    pub fn close(&self) {
        match self {
            Link::Udp(c, e) => e.forget(c.id),
            Link::Tcp(t) => t.halt(),
        }
    }

    pub fn is_closed(&self) -> bool {
        match self {
            Link::Udp(c, _) => c.is_closed(),
            Link::Tcp(t) => t.closed.load(Ordering::SeqCst),
        }
    }

    pub fn idle(&self) -> Duration {
        match self {
            Link::Udp(c, _) => c.idle(),
            Link::Tcp(_) => Duration::ZERO,
        }
    }
}

impl TcpLink {
    fn send(&self, msg: &[u8]) -> io::Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "connection closed"));
        }
        let mut g = self.wr.lock();
        let Some((wr, sealer)) = g.as_mut() else {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "connection closed"));
        };
        let records = sealer.seal_records(msg)?;
        assert!(!records.is_empty(), "every message is at least one sealed record");
        let mut buf = Vec::new();
        for rec in &records {
            buf.extend(encode_frame(rec, FRAME_LIMIT).map_err(|e| io::Error::other(format!("frame: {e:?}")))?);
        }
        assert!(buf.len() > msg.len(), "every message is at least one sealed record");
        wr.write_all(&buf)
    }

    fn recv(&self) -> Option<Vec<u8>> {
        loop {
            let mut g = lock(&self.shared.mu);
            if let Some(msg) = g.msgs.pop_front() {
                let drained = g.msgs.is_empty();
                drop(g);
                if drained {
                    self.shared.space.notify_one();
                }
                return Some(msg);
            }
            if g.eof {
                return None;
            }
            drop(g);
            self.shared.data.notified();
        }
    }

    fn halt(&self) {
        self.closed.store(true, Ordering::SeqCst);
        {
            let mut g = lock(&self.shared.mu);
            g.stop = true;
            g.eof = true;
        }
        self.shared.space.notify_waiters();
        self.shared.data.notify_waiters();
        if let Some((mut wr, _)) = self.wr.lock().take() {
            let _ = wr.halt();
        }
        assert!(self.closed.load(Ordering::SeqCst));
    }
}

impl Drop for TcpLink {
    fn drop(&mut self) {
        self.halt();
        if let Some(mut reader) = lock(&self.reader).take() {
            let _ = reader.join();
        }
    }
}

fn deliver(shared: &Shared, msg: Vec<u8>) -> bool {
    {
        let mut g = lock(&shared.mu);
        if g.stop {
            return false;
        }
        g.msgs.push_back(msg);
    }
    shared.data.notify_one();
    true
}

fn mark_eof(shared: &Shared) {
    {
        let mut g = lock(&shared.mu);
        g.eof = true;
        g.stop = true;
    }
    shared.space.notify_waiters();
    shared.data.notify_waiters();
}

fn tcp_read(mut stream: ReadHalf, mut opener: Opener, shared: Arc<Shared>) {
    let mut dec = FrameDecoder::new(FRAME_LIMIT);
    let mut pending: VecDeque<Vec<u8>> = VecDeque::new();
    let mut buf = vec![0u8; 256 << 10];
    let mut need_more = false;
    loop {
        if lock(&shared.mu).stop {
            return;
        }
        while let Some(rec) = pending.pop_front() {
            match opener.open(&rec) {
                Ok(Some(msg)) => {
                    need_more = false;
                    if !deliver(&shared, msg) {
                        return;
                    }
                }
                Ok(None) => need_more = true,
                Err(_) => {
                    mark_eof(&shared);
                    return;
                }
            }
        }
        // At a frame boundary, with a message already waiting, the next read waits
        // until that message is taken. A partial frame, or a record the opener has
        // not finished, is read at once.
        if !need_more && dec.missing() == 0 {
            loop {
                let waiting = {
                    let g = lock(&shared.mu);
                    if g.stop {
                        return;
                    }
                    !g.msgs.is_empty()
                };
                if !waiting {
                    break;
                }
                shared.space.notified();
            }
        }
        let n = match stream.read(&mut buf) {
            Ok(0) => {
                let _ = dec.end();
                mark_eof(&shared);
                return;
            }
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => {
                mark_eof(&shared);
                return;
            }
        };
        match dec.push(&buf[..n]) {
            Ok(frames) => pending.extend(frames),
            Err(_) => {
                mark_eof(&shared);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use expect_test::expect;
    use rargo_proto::secure::{client_finish, client_start, psk_from_secret, server_accept};

    #[test]
    fn an_empty_frame_and_a_frame_at_the_limit_are_delivered() {
        let mut decoder = FrameDecoder::new(4);
        let frames = decoder.push(&[0, 0, 0, 0, 4, 0, 0, 0, 7, 7, 7, 7]);
        expect!["Ok([[], [7, 7, 7, 7]])"].assert_eq(&format!("{frames:?}"));
        expect!["Ok(())"].assert_eq(&format!("{:?}", decoder.end()));
        decoder.invariants();
    }

    #[test]
    fn every_split_of_the_byte_stream_yields_the_same_frames() {
        let payloads = [vec![1, 2], vec![3]];
        let mut wire = Vec::new();
        for p in &payloads {
            wire.extend(encode_frame(p, FRAME_LIMIT).unwrap());
        }
        let n = wire.len();
        let ways = 1u32 << (n - 1);
        for mask in 0..ways {
            let mut parts: Vec<&[u8]> = Vec::new();
            let mut start = 0;
            for i in 1..n {
                if mask & (1 << (i - 1)) != 0 {
                    parts.push(&wire[start..i]);
                    start = i;
                }
            }
            parts.push(&wire[start..]);
            let mut d = FrameDecoder::new(FRAME_LIMIT);
            let mut got = Vec::new();
            for part in parts {
                got.extend(d.push(part).unwrap());
                d.invariants();
            }
            assert_eq!(got, payloads);
            assert_eq!(d.end(), Ok(()));
            d.invariants();
        }
        let big = vec![4u8; 300];
        let wire = encode_frame(&big, FRAME_LIMIT).unwrap();
        let mut d = FrameDecoder::new(FRAME_LIMIT);
        let mut got = Vec::new();
        for (i, b) in wire.iter().enumerate() {
            got.extend(d.push(&[*b]).unwrap());
            d.invariants();
            let fed = i + 1;
            let expect = if fed < 4 { 4 - fed } else if fed < wire.len() { big.len() - (fed - 4) } else { 0 };
            assert_eq!(d.missing(), expect);
        }
        assert_eq!(got, vec![big]);
    }

    #[test]
    fn an_oversize_frame_is_refused() {
        assert_eq!(encode_frame(&[1, 2, 3], 2), Err(FrameError::Oversize { len: 3, limit: 2 }));
        let mut decoder = FrameDecoder::new(4);
        let mut trace = Vec::new();
        for chunk in [&[5, 0][..], &[0, 0, 9, 9], &[0]] {
            trace.push(format!("{:?}, missing={}", decoder.push(chunk), decoder.missing()));
            decoder.invariants();
        }
        expect![[r#"
            Ok([]), missing=2
            Err(Oversize { len: 5, limit: 4 }), missing=0
            Err(Oversize { len: 5, limit: 4 }), missing=0"#]].assert_eq(&trace.join("\n"));
        let mut decoder = FrameDecoder::new(FRAME_LIMIT);
        let huge = (FRAME_LIMIT as u32 + 1).to_le_bytes();
        expect!["Err(Oversize { len: 65536, limit: 65535 })"]
            .assert_eq(&format!("{:?}", decoder.push(&huge)));
        decoder.invariants();
    }

    #[test]
    fn a_truncated_stream_reports_exactly_what_is_missing() {
        let frame = encode_frame(&[1, 2, 3, 4, 5], FRAME_LIMIT).unwrap();
        assert_eq!(frame.len(), 9);
        let d = FrameDecoder::new(FRAME_LIMIT);
        assert_eq!(d.end(), Ok(()));
        assert_eq!(d.missing(), 0);
        d.invariants();
        for n in 1..frame.len() {
            let mut d = FrameDecoder::new(FRAME_LIMIT);
            assert!(d.push(&frame[..n]).unwrap().is_empty());
            d.invariants();
            // A short length is missing the rest of those 4 bytes. Once the length is known, the rest of the body.
            let missing = if n < 4 { 4 - n } else { 5 - (n - 4) };
            assert_eq!(d.missing(), missing);
            assert_eq!(d.end(), Err(FrameError::Truncated { missing }));
        }
        let mut d = FrameDecoder::new(FRAME_LIMIT);
        let first = encode_frame(&[8, 8], FRAME_LIMIT).unwrap();
        let second = encode_frame(&[1, 2, 3, 4], FRAME_LIMIT).unwrap();
        let mut wire = first.clone();
        wire.extend(&second[..3]);
        assert_eq!(d.push(&wire).unwrap(), vec![vec![8, 8]]);
        d.invariants();
        assert_eq!(d.missing(), 1);
        assert_eq!(d.end(), Err(FrameError::Truncated { missing: 1 }));
        let mut d = FrameDecoder::new(FRAME_LIMIT);
        let mut wire = first;
        wire.extend(&second[..6]);
        assert_eq!(d.push(&wire).unwrap(), vec![vec![8, 8]]);
        d.invariants();
        assert_eq!(d.missing(), 2);
        assert_eq!(d.end(), Err(FrameError::Truncated { missing: 2 }));
    }

    struct Clock {
        now: std::cell::Cell<Duration>,
        deadline: std::cell::Cell<Option<Duration>>,
    }

    struct Peer {
        clock: std::rc::Rc<Clock>,
        psk: [u8; 32],
        answer_after: Option<Duration>,
        refuse: Option<io::ErrorKind>,
    }

    struct PeerWire {
        clock: std::rc::Rc<Clock>,
        psk: [u8; 32],
        answer_after: Option<Duration>,
        arrived: bool,
        reply: Vec<u8>,
    }

    impl Wire for PeerWire {
        fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
            let ulen = buf[8] as usize;
            let mlen = u16::from_le_bytes([buf[9 + ulen], buf[10 + ulen]]) as usize;
            let (msg2, ..) = server_accept(&self.psk, &buf[11 + ulen..11 + ulen + mlen])?;
            self.reply = [vec![0], (msg2.len() as u16).to_le_bytes().to_vec(), msg2].concat();
            Ok(())
        }

        fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
            let deadline = self.clock.deadline.get().expect("a dial reads within its bound");
            match self.answer_after {
                None => self.clock.now.set(deadline),
                Some(after) if !self.arrived => {
                    self.arrived = true;
                    self.clock.now.set(self.clock.now.get() + after);
                }
                Some(_) => {}
            }
            if self.clock.now.get() >= deadline {
                self.clock.now.set(deadline);
                return Err(io::Error::new(io::ErrorKind::TimedOut, "the clock reached the dial's deadline"));
            }
            let rest = self.reply.split_off(buf.len());
            buf.copy_from_slice(&self.reply);
            self.reply = rest;
            Ok(())
        }
    }

    impl Dial for Peer {
        type Wire = PeerWire;

        fn connect(&self) -> io::Result<PeerWire> {
            if let Some(kind) = self.refuse {
                return Err(io::Error::new(kind, "the peer's port is closed"));
            }
            Ok(PeerWire { clock: self.clock.clone(), psk: self.psk, answer_after: self.answer_after, arrived: false, reply: Vec::new() })
        }

        fn within<T: 'static>(&self, limit: Duration, body: impl FnOnce() -> T) -> Option<T> {
            self.clock.deadline.set(Some(self.clock.now.get() + limit));
            let got = body();
            (self.clock.now.get() < self.clock.deadline.get().unwrap()).then_some(got)
        }
    }

    fn peer(answer_after: Option<Duration>, refuse: Option<io::ErrorKind>) -> Peer {
        let clock = Clock { now: std::cell::Cell::new(Duration::ZERO), deadline: std::cell::Cell::new(None) };
        Peer { clock: std::rc::Rc::new(clock), psk: psk_from_secret(b"dial"), answer_after, refuse }
    }

    #[test]
    fn a_peer_that_accepts_and_never_answers_ends_the_dial_once_its_bound_passes_on_the_clock() {
        let silent = peer(None, None);
        let Err(e) = dial(&silent, "ann", &silent.psk, None) else { panic!("a silent peer is not dialled") };
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        assert_eq!(e.to_string(), "no answer over TCP within 5 s");
        assert_eq!(silent.clock.now.get(), CONNECT_WITHIN, "the dial gives up exactly at its bound");

        let late = peer(Some(CONNECT_WITHIN + Duration::from_secs(1)), None);
        let Err(e) = dial(&late, "ann", &late.psk, None) else { panic!("an answer after the bound is too late") };
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);

        let prompt = peer(Some(CONNECT_WITHIN - Duration::from_secs(1)), None);
        assert!(dial(&prompt, "ann", &prompt.psk, None).is_ok(), "an answer inside the bound finishes the handshake");
        assert_eq!(prompt.clock.now.get(), CONNECT_WITHIN - Duration::from_secs(1));
    }

    #[test]
    fn a_closed_port_is_said_at_once_and_another_connect_failure_keeps_the_udp_error() {
        let closed = peer(None, Some(io::ErrorKind::ConnectionRefused));
        let udp = || Some(io::Error::new(io::ErrorKind::TimedOut, "no answer over UDP"));
        let Err(e) = dial(&closed, "ann", &closed.psk, udp()) else { panic!() };
        assert_eq!(e.kind(), io::ErrorKind::ConnectionRefused);
        assert_eq!(closed.clock.now.get(), Duration::ZERO);
        let unreachable = peer(None, Some(io::ErrorKind::HostUnreachable));
        let Err(e) = dial(&unreachable, "ann", &unreachable.psk, udp()) else { panic!() };
        assert_eq!(e.to_string(), "no answer over UDP");
    }

    fn handshake() -> (Sealer, Opener, Sealer, Opener) {
        let k = psk_from_secret(b"frame");
        let (hs, first) = client_start("ann", &k, b"").unwrap();
        let ulen = first[8] as usize;
        let mlen = u16::from_le_bytes([first[9 + ulen], first[10 + ulen]]) as usize;
        let msg1 = &first[11 + ulen..11 + ulen + mlen];
        let (msg2, _, ss, so) = server_accept(&k, msg1).unwrap();
        let (cs, co) = client_finish(hs, &msg2).unwrap();
        (cs, co, ss, so)
    }

    #[test]
    fn sealed_records_travel_as_frames() {
        let (mut cs, mut co, mut ss, mut so) = handshake();
        // A client's records open on the server, and the server's open on the client.
        let msg = b"payload across records";
        let records = cs.seal_records(msg).unwrap();
        let mut wire = Vec::new();
        for rec in &records {
            wire.extend(encode_frame(rec, FRAME_LIMIT).unwrap());
        }
        let mut one = FrameDecoder::new(FRAME_LIMIT);
        let mut got_frames = Vec::new();
        let mut at = 0;
        for step in [1usize, 2, 5, 8] {
            let next = (at + step).min(wire.len());
            got_frames.extend(one.push(&wire[at..next]).unwrap());
            one.invariants();
            at = next;
            if at == wire.len() {
                break;
            }
        }
        if at < wire.len() {
            got_frames.extend(one.push(&wire[at..]).unwrap());
            one.invariants();
        }
        assert_eq!(got_frames, records);
        let mut plain = None;
        for f in got_frames {
            if let Some(m) = so.open(&f).unwrap() {
                plain = Some(m);
            }
        }
        assert_eq!(plain.unwrap(), msg);

        let mut sealed = Vec::new();
        ss.seal(msg, &mut sealed).unwrap();
        let mut d = FrameDecoder::new(FRAME_LIMIT);
        let frames = d.push(&sealed).unwrap();
        d.invariants();
        assert_eq!(d.end(), Ok(()));
        let mut plain = None;
        for f in frames {
            if let Some(m) = co.open(&f).unwrap() {
                plain = Some(m);
            }
        }
        assert_eq!(plain.unwrap(), msg);
    }
}
