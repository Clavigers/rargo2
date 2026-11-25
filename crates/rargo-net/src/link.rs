use crate::{Auth, Conn, Endpoint};
use rargo_proto::secure::{self, Opener, Sealer};
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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

    pub fn invariants(&self) {
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

struct TcpReader {
    stream: TcpStream,
    opener: Opener,
    dec: FrameDecoder,
    pending: VecDeque<Vec<u8>>,
}

pub struct TcpLink {
    wr: Mutex<(TcpStream, Sealer)>,
    rd: Mutex<TcpReader>,
    stopper: TcpStream,
    closed: AtomicBool,
}

pub enum Link {
    Udp(Arc<Conn>, Arc<Endpoint>),
    Tcp(TcpLink),
}

const UDP_WAIT: Duration = Duration::from_millis(2500);

impl Link {
    pub fn tcp(stream: TcpStream, sealer: Sealer, opener: Opener) -> Link {
        let _ = stream.set_nodelay(true);
        let _ = stream.set_nonblocking(false);
        let rd = stream.try_clone().expect("a tcp stream clones for reading");
        let stopper = stream.try_clone().expect("a tcp stream clones for closing");
        Link::Tcp(TcpLink {
            wr: Mutex::new((stream, sealer)),
            rd: Mutex::new(TcpReader {
                stream: rd,
                opener,
                dec: FrameDecoder::new(FRAME_LIMIT),
                pending: VecDeque::new(),
            }),
            stopper,
            closed: AtomicBool::new(false),
        })
    }

    pub async fn connect(server: SocketAddr, user: &str, psk: &[u8; 32], transport: &str) -> io::Result<Link> {
        let mut udp_error = None;
        if transport != "tcp" {
            let bind: SocketAddr = if server.is_ipv4() { "0.0.0.0:0".parse().unwrap() } else { "[::]:0".parse().unwrap() };
            let endpoint = Endpoint::bind_auth(bind, false, Auth::User { name: user.to_string(), psk: *psk }).await?;
            let wait = if transport == "udp" { Duration::from_secs(20) } else { UDP_WAIT };
            match crate::connect_within(wait, &endpoint, server).await {
                Ok(conn) => return Ok(Link::Udp(conn, endpoint)),
                Err(e) if e.kind() == io::ErrorKind::PermissionDenied => return Err(e),
                Err(e) => udp_error = Some(e),
            }
            if transport == "udp" {
                return Err(udp_error.unwrap());
            }
        }
        let mut stream = match TcpStream::connect(server) {
            Ok(s) => s,
            Err(e) => return Err(if e.kind() == io::ErrorKind::ConnectionRefused { e } else { udp_error.unwrap_or(e) }),
        };
        let _ = stream.set_nodelay(true);
        let (hs, first) = secure::client_start(user, psk, &[])?;
        stream.write_all(&first)?;
        let mut head = [0u8; 3];
        stream.read_exact(&mut head)?;
        let mut body = vec![0u8; u16::from_le_bytes([head[1], head[2]]) as usize];
        stream.read_exact(&mut body)?;
        if head[0] != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("the server refused the connection: {}", String::from_utf8_lossy(&body).trim()),
            ));
        }
        let (sealer, opener) = secure::client_finish(hs, &body)?;
        Ok(Link::tcp(stream, sealer, opener))
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Link::Udp(..) => "udp",
            Link::Tcp(_) => "tcp",
        }
    }

    pub async fn send(&self, msg: &[u8]) -> io::Result<()> {
        match self {
            Link::Udp(c, _) => c.send(msg).await,
            Link::Tcp(t) => {
                if t.closed.load(Ordering::SeqCst) {
                    return Err(io::Error::new(io::ErrorKind::BrokenPipe, "connection closed"));
                }
                let mut g = t.wr.lock().unwrap_or_else(|e| e.into_inner());
                let (wr, sealer) = &mut *g;
                let records = sealer.seal_records(msg)?;
                assert!(!records.is_empty(), "every message is at least one sealed record");
                let mut buf = Vec::new();
                for rec in &records {
                    buf.extend(encode_frame(rec, FRAME_LIMIT).map_err(|e| io::Error::other(format!("frame: {e:?}")))?);
                }
                assert!(buf.len() > msg.len(), "every message is at least one sealed record");
                wr.write_all(&buf)?;
                wr.flush()
            }
        }
    }

    pub async fn recv(&self) -> Option<Vec<u8>> {
        match self {
            Link::Udp(c, _) => c.recv().await,
            Link::Tcp(t) => recv_tcp(t),
        }
    }

    pub fn close(&self) {
        match self {
            Link::Udp(c, e) => e.forget(c.id),
            Link::Tcp(t) => {
                t.closed.store(true, Ordering::SeqCst);
                let _ = t.stopper.shutdown(Shutdown::Both);
                assert!(t.closed.load(Ordering::SeqCst));
            }
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

fn recv_tcp(t: &TcpLink) -> Option<Vec<u8>> {
    let mut g = t.rd.lock().unwrap_or_else(|e| e.into_inner());
    let TcpReader { stream, opener, dec, pending } = &mut *g;
    loop {
        if t.closed.load(Ordering::SeqCst) {
            return None;
        }
        while let Some(rec) = pending.pop_front() {
            match opener.open(&rec) {
                Ok(Some(m)) => return Some(m),
                Ok(None) => continue,
                Err(_) => return None,
            }
        }
        let mut buf = [0u8; 256 << 10];
        let n = match stream.read(&mut buf) {
            Ok(0) => {
                let _ = dec.end();
                return None;
            }
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        };
        match dec.push(&buf[..n]) {
            Ok(frames) => pending.extend(frames),
            Err(_) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rargo_proto::secure::{client_finish, client_start, psk_from_secret, server_accept};

    fn round_trip(limit: usize, payloads: &[Vec<u8>]) -> Vec<Vec<u8>> {
        let mut wire = Vec::new();
        for p in payloads {
            wire.extend(encode_frame(p, limit).unwrap());
        }
        let mut d = FrameDecoder::new(limit);
        let got = d.push(&wire).unwrap();
        d.invariants();
        assert_eq!(d.end(), Ok(()));
        assert_eq!(d.missing(), 0);
        got
    }

    #[test]
    fn frames_round_trip_and_an_empty_frame_is_a_frame() {
        let payloads = [vec![], vec![1, 2, 3], vec![9; 20]];
        assert_eq!(round_trip(FRAME_LIMIT, &payloads), payloads);
        let exact = vec![7u8; 4];
        assert_eq!(round_trip(4, &[exact.clone()]), vec![exact]);
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
        let mut raw = 5u32.to_le_bytes().to_vec();
        raw.extend_from_slice(&[9; 5]);
        let mut d = FrameDecoder::new(4);
        assert!(d.push(&raw[..2]).unwrap().is_empty());
        d.invariants();
        assert_eq!(d.missing(), 2);
        assert_eq!(d.push(&raw[2..6]), Err(FrameError::Oversize { len: 5, limit: 4 }));
        d.invariants();
        assert_eq!(d.push(&[0]), Err(FrameError::Oversize { len: 5, limit: 4 }));
        let huge = (FRAME_LIMIT as u32 + 1).to_le_bytes().to_vec();
        let mut d = FrameDecoder::new(FRAME_LIMIT);
        assert_eq!(d.push(&huge), Err(FrameError::Oversize { len: FRAME_LIMIT + 1, limit: FRAME_LIMIT }));
        d.invariants();
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
