//! The link between a client daemon and the server: one long-lived,
//! encrypted message channel per daemon.
//!
//! Normally it is a connection of the reliable-UDP transport, which keeps
//! its congestion window between commands, follows the client when its
//! address changes and delivers messages independently of each other. Where
//! UDP does not get through, the same messages travel over one TCP
//! connection, sealed as records (`rargo_proto::secure`).
//!
//! Messages may overtake each other on the UDP form; whoever needs an order
//! numbers the messages (see `rargo_proto::Reorder`).

use crate::{Auth, Conn, Endpoint};
use rargo_proto::secure::{self, Opener, Sealer};
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

pub struct TcpLink {
    wr: tokio::sync::Mutex<(OwnedWriteHalf, Sealer)>,
    rd: tokio::sync::Mutex<(BufReader<OwnedReadHalf>, Opener)>,
    closed: AtomicBool,
    closing: tokio::sync::Notify,
}

pub enum Link {
    Udp(Arc<Conn>, Arc<Endpoint>),
    Tcp(TcpLink),
}

/// How long the UDP key agreement may take before TCP is tried.
const UDP_WAIT: Duration = Duration::from_millis(2500);

impl Link {
    /// Wraps a TCP connection whose handshake is done.
    pub fn tcp(stream: TcpStream, sealer: Sealer, opener: Opener) -> Link {
        let _ = stream.set_nodelay(true);
        let (rd, wr) = stream.into_split();
        Link::Tcp(TcpLink {
            wr: tokio::sync::Mutex::new((wr, sealer)),
            rd: tokio::sync::Mutex::new((BufReader::with_capacity(256 << 10, rd), opener)),
            closed: AtomicBool::new(false),
            closing: tokio::sync::Notify::new(),
        })
    }

    /// Connects to the server as `user`: over UDP, or over TCP if no answer
    /// comes back that way. `transport` is `"udp"`, `"tcp"` or anything
    /// else for "UDP first".
    pub async fn connect(server: SocketAddr, user: &str, psk: &[u8; 32], transport: &str) -> io::Result<Link> {
        let mut udp_error = None;
        if transport != "tcp" {
            let bind: SocketAddr = if server.is_ipv4() { "0.0.0.0:0".parse().unwrap() } else { "[::]:0".parse().unwrap() };
            let endpoint = Endpoint::bind_auth(bind, false, Auth::User { name: user.to_string(), psk: *psk }).await?;
            let wait = if transport == "udp" { Duration::from_secs(20) } else { UDP_WAIT };
            match tokio::time::timeout(wait, endpoint.connect(server)).await {
                Ok(Ok(conn)) => return Ok(Link::Udp(conn, endpoint)),
                // The server answered and said no: TCP would say the same.
                Ok(Err(e)) if e.kind() == io::ErrorKind::PermissionDenied => return Err(e),
                Ok(Err(e)) => udp_error = Some(e),
                Err(_) => udp_error = Some(io::Error::new(io::ErrorKind::TimedOut, "no answer over UDP")),
            }
            if transport == "udp" {
                return Err(udp_error.unwrap());
            }
        }
        let mut stream = match TcpStream::connect(server).await {
            Ok(s) => s,
            // Report the more telling of the two failures.
            Err(e) => return Err(if e.kind() == io::ErrorKind::ConnectionRefused { e } else { udp_error.unwrap_or(e) }),
        };
        let _ = stream.set_nodelay(true);
        let (hs, first) = secure::client_start(user, psk, &[])?;
        stream.write_all(&first).await?;
        let mut head = [0u8; 3];
        stream.read_exact(&mut head).await?;
        let mut body = vec![0u8; u16::from_le_bytes([head[1], head[2]]) as usize];
        stream.read_exact(&mut body).await?;
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
                let mut g = t.wr.lock().await;
                let (wr, sealer) = &mut *g;
                let mut buf = Vec::with_capacity(msg.len() + msg.len() / 1000 + 64);
                sealer.seal(msg, &mut buf)?;
                assert!(buf.len() > msg.len(), "every message is at least one sealed record");
                wr.write_all(&buf).await?;
                wr.flush().await
            }
        }
    }

    /// The next message; `None` once the link is closed or broken.
    pub async fn recv(&self) -> Option<Vec<u8>> {
        match self {
            Link::Udp(c, _) => c.recv().await,
            Link::Tcp(t) => {
                let mut g = t.rd.lock().await;
                let (rd, opener) = &mut *g;
                let read = async {
                    loop {
                        let mut len = [0u8; 4];
                        rd.read_exact(&mut len).await.ok()?;
                        let n = u32::from_le_bytes(len) as usize;
                        if n > 65535 {
                            return None;
                        }
                        let mut rec = vec![0u8; n];
                        rd.read_exact(&mut rec).await.ok()?;
                        if let Some(m) = opener.open(&rec).ok()? {
                            return Some(m);
                        }
                    }
                };
                tokio::select! {
                    m = read => m,
                    _ = t.closing.notified() => None,
                }
            }
        }
    }

    pub fn close(&self) {
        match self {
            Link::Udp(c, e) => e.forget(c.id),
            Link::Tcp(t) => {
                t.closed.store(true, Ordering::SeqCst);
                t.closing.notify_waiters();
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

    /// Time since anything arrived from the peer (UDP), for telling a dead
    /// peer from a quiet one.
    pub fn idle(&self) -> Duration {
        match self {
            Link::Udp(c, _) => c.idle(),
            Link::Tcp(_) => Duration::ZERO,
        }
    }
}
