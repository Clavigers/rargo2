
use std::io::{self, ErrorKind};
use std::mem;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::Path;


use super::fiber::*;
use super::ops::{c_path, parse_addr, Event, Op};
use super::task::*;


pub(super) fn sys_socket(domain: i32, ty: i32, proto: i32) -> io::Result<i32> {
    match op_event(Op::Socket { domain, ty: ty | libc::SOCK_CLOEXEC, proto })? {
        Event::Socket { fd } => Ok(fd),
        _ => Err(io::Error::other("socket")),
    }
}

pub(super) fn sys_bind(fd: i32, addr: SocketAddr) -> io::Result<()> {
    match op_event(Op::Bind { fd, addr })? {
        Event::Bound => Ok(()),
        _ => Err(io::Error::other("bind")),
    }
}

pub(super) fn sys_listen(fd: i32, backlog: i32) -> io::Result<()> {
    match op_event(Op::Listen { fd, backlog })? {
        Event::Listening => Ok(()),
        _ => Err(io::Error::other("listen")),
    }
}

pub(super) fn reuseaddr(fd: i32) {
    let one = 1i32;
    unsafe {
        libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR, (&one as *const i32).cast(), 4);
    }
}

pub(super) fn local_of(fd: i32) -> io::Result<SocketAddr> {
    unsafe {
        let mut storage = mem::zeroed::<libc::sockaddr_storage>();
        let mut len = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        if libc::getsockname(fd, (&mut storage as *mut libc::sockaddr_storage).cast(), &mut len) < 0 {
            return Err(io::Error::last_os_error());
        }
        parse_addr(&storage).ok_or_else(|| io::Error::other("unnamed socket"))
    }
}

pub(super) fn read_fd(fd: i32, buf: &mut [u8]) -> io::Result<usize> {
    match op_event(Op::Recv { fd, len: buf.len(), flags: 0 })? {
        Event::Recv { n, buf: got } => {
            let n = n.min(buf.len()).min(got.len());
            buf[..n].copy_from_slice(&got[..n]);
            Ok(n)
        }
        _ => Err(io::Error::other("recv")),
    }
}

pub(super) fn write_all_fd(fd: i32, mut buf: &[u8]) -> io::Result<()> {
    while !buf.is_empty() {
        let n = match op_event(Op::Send { fd, buf: buf.to_vec() })? {
            Event::Send { n } if n == 0 => return Err(io::Error::new(ErrorKind::WriteZero, "write closed")),
            Event::Send { n } => n,
            _ => return Err(io::Error::other("send")),
        };
        buf = &buf[n..];
    }
    Ok(())
}

pub(super) fn shutdown_fd(fd: i32) -> io::Result<()> {
    shutdown_how(fd, libc::SHUT_WR)
}

pub(super) fn shutdown_how(fd: i32, how: i32) -> io::Result<()> {
    match op_event(Op::Shutdown { fd, how })? {
        Event::Shut => Ok(()),
        _ => Err(io::Error::other("shutdown")),
    }
}

pub(super) struct OwnedFd {
    fd: i32,
}

impl Drop for OwnedFd {
    fn drop(&mut self) {
        if self.fd >= 0 {
            unsafe { libc::close(self.fd) };
            self.fd = -1;
        }
    }
}

impl OwnedFd {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        read_fd(self.fd, buf)
    }
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        write_all_fd(self.fd, buf)
    }
    fn shutdown(&mut self) -> io::Result<()> {
        shutdown_fd(self.fd)
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        local_of(self.fd)
    }
    fn into_raw(mut self) -> i32 {
        let fd = self.fd;
        self.fd = -1;
        fd
    }
}

pub struct ReadHalf {
    fd: i32,
}
pub struct WriteHalf {
    fd: i32,
}

impl Drop for ReadHalf {
    fn drop(&mut self) {
        if self.fd >= 0 {
            unsafe { libc::close(self.fd) };
            self.fd = -1;
        }
    }
}
impl Drop for WriteHalf {
    fn drop(&mut self) {
        if self.fd >= 0 {
            unsafe { libc::close(self.fd) };
            self.fd = -1;
        }
    }
}
impl ReadHalf {
    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        read_fd(self.fd, buf)
    }
    pub fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
        let mut off = 0;
        while off < buf.len() {
            let n = self.read(&mut buf[off..])?;
            if n == 0 {
                return Err(io::Error::new(ErrorKind::UnexpectedEof, "early end"));
            }
            off += n;
        }
        Ok(())
    }
}
impl WriteHalf {
    pub fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        write_all_fd(self.fd, buf)
    }
    pub fn shutdown(&mut self) -> io::Result<()> {
        shutdown_fd(self.fd)
    }
    pub fn halt(&mut self) -> io::Result<()> {
        shutdown_how(self.fd, libc::SHUT_RDWR)
    }
}

pub(super) fn split_fd(fd: i32) -> (ReadHalf, WriteHalf) {
    let other = unsafe { libc::dup(fd) };
    if other < 0 {
        unsafe { libc::close(fd) };
        panic!("dup: {}", io::Error::last_os_error());
    }
    (ReadHalf { fd }, WriteHalf { fd: other })
}

pub struct TcpListener {
    fd: OwnedFd,
}
pub struct TcpStream {
    fd: OwnedFd,
}
pub struct UdpSocket {
    fd: OwnedFd,
    late: Option<(std::time::Duration, std::sync::Arc<OwnedFd>)>,
}

fn link_rtt() -> Option<std::time::Duration> {
    static RTT: std::sync::OnceLock<Option<std::time::Duration>> = std::sync::OnceLock::new();
    *RTT.get_or_init(|| {
        let ms: u64 = std::env::var("RARGO_LINK_RTT_MS").ok()?.parse().ok()?;
        (ms > 0).then(|| std::time::Duration::from_millis(ms))
    })
}
pub struct UnixListener {
    fd: OwnedFd,
}
pub struct UnixStream {
    fd: OwnedFd,
}

impl TcpListener {
    pub fn bind(addr: SocketAddr) -> io::Result<Self> {
        let domain = domain_of(addr);
        let fd = sys_socket(domain, libc::SOCK_STREAM, 0)?;
        reuseaddr(fd);
        if let Err(err) = sys_bind(fd, addr) {
            unsafe { libc::close(fd) };
            return Err(err);
        }
        if let Err(err) = sys_listen(fd, 1024) {
            unsafe { libc::close(fd) };
            return Err(err);
        }
        Ok(Self { fd: OwnedFd { fd } })
    }

    pub fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        match op_event(Op::Accept { fd: self.fd.fd })? {
            Event::Accept { fd, peer } => {
                let peer = peer.unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)));
                Ok((TcpStream { fd: OwnedFd { fd } }, peer))
            }
            _ => Err(io::Error::other("accept")),
        }
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.fd.local_addr()
    }
}

impl TcpStream {
    pub fn connect(addr: SocketAddr) -> io::Result<Self> {
        let fd = sys_socket(domain_of(addr), libc::SOCK_STREAM, 0)?;
        match op_event(Op::Connect { fd, addr }) {
            Ok(Event::Connected) => Ok(Self { fd: OwnedFd { fd } }),
            Ok(_) => {
                unsafe { libc::close(fd) };
                Err(io::Error::other("connect"))
            }
            Err(err) => {
                unsafe { libc::close(fd) };
                Err(err)
            }
        }
    }

    pub fn set_nodelay(&self, on: bool) -> io::Result<()> {
        let v = i32::from(on);
        let rc = unsafe {
            libc::setsockopt(self.fd.fd, libc::IPPROTO_TCP, libc::TCP_NODELAY, (&v as *const i32).cast(), 4)
        };
        if rc < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }
    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.fd.read(buf)
    }
    pub fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        match op_event(Op::Recv { fd: self.fd.fd, len: buf.len(), flags: libc::MSG_PEEK })? {
            Event::Recv { n, buf: got } => {
                let n = n.min(buf.len()).min(got.len());
                buf[..n].copy_from_slice(&got[..n]);
                Ok(n)
            }
            _ => Err(io::Error::other("peek")),
        }
    }
    pub fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
        let mut off = 0;
        while off < buf.len() {
            let n = self.read(&mut buf[off..])?;
            if n == 0 {
                return Err(io::Error::new(ErrorKind::UnexpectedEof, "early end"));
            }
            off += n;
        }
        Ok(())
    }
    pub fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.fd.write_all(buf)
    }
    pub fn shutdown(&mut self) -> io::Result<()> {
        self.fd.shutdown()
    }
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.fd.local_addr()
    }
    pub fn into_split(self) -> (ReadHalf, WriteHalf) {
        split_fd(self.fd.into_raw())
    }
}

impl UdpSocket {
    pub fn bind(addr: SocketAddr) -> io::Result<Self> {
        let fd = sys_socket(domain_of(addr), libc::SOCK_DGRAM, 0)?;
        if let Err(err) = sys_bind(fd, addr) {
            unsafe { libc::close(fd) };
            return Err(err);
        }
        let late = match link_rtt() {
            Some(rtt) => {
                let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
                if copy < 0 {
                    let err = io::Error::last_os_error();
                    unsafe { libc::close(fd) };
                    return Err(err);
                }
                Some((rtt, std::sync::Arc::new(OwnedFd { fd: copy })))
            }
            None => None,
        };
        Ok(Self { fd: OwnedFd { fd }, late })
    }

    fn send_late(&self, buf: &[u8], addr: Option<SocketAddr>) -> bool {
        let Some((rtt, fd)) = &self.late else { return false };
        let (rtt, fd, buf) = (*rtt, std::sync::Arc::clone(fd), buf.to_vec());
        background(move || {
            sleep(rtt);
            let op = match addr {
                Some(addr) => Op::SendTo { fd: fd.fd, buf, addr },
                None => Op::Send { fd: fd.fd, buf },
            };
            let _ = op_event(op);
        });
        true
    }

    pub fn set_buffers(&self, bytes: i32) {
        unsafe {
            for (force, soft) in [
                (libc::SO_RCVBUFFORCE, libc::SO_RCVBUF),
                (libc::SO_SNDBUFFORCE, libc::SO_SNDBUF),
            ] {
                libc::setsockopt(self.fd.fd, libc::SOL_SOCKET, force, (&bytes as *const i32).cast(), 4);
                libc::setsockopt(self.fd.fd, libc::SOL_SOCKET, soft, (&bytes as *const i32).cast(), 4);
            }
        }
    }

    pub fn connect(&self, addr: SocketAddr) -> io::Result<()> {
        match op_event(Op::Connect { fd: self.fd.fd, addr })? {
            Event::Connected => Ok(()),
            _ => Err(io::Error::other("connect")),
        }
    }

    pub fn send(&self, buf: &[u8]) -> io::Result<usize> {
        if self.send_late(buf, None) {
            return Ok(buf.len());
        }
        match op_event(Op::Send { fd: self.fd.fd, buf: buf.to_vec() })? {
            Event::Send { n } => Ok(n),
            _ => Err(io::Error::other("send")),
        }
    }

    pub fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        read_fd(self.fd.fd, buf)
    }

    pub fn send_to(&self, buf: &[u8], addr: SocketAddr) -> io::Result<usize> {
        if self.send_late(buf, Some(addr)) {
            return Ok(buf.len());
        }
        match op_event(Op::SendTo { fd: self.fd.fd, buf: buf.to_vec(), addr })? {
            Event::SendTo { n } => Ok(n),
            _ => Err(io::Error::other("send")),
        }
    }

    pub fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        match op_event(Op::RecvFrom { fd: self.fd.fd, len: buf.len() })? {
            Event::RecvFrom { n, buf: got, addr } => {
                let n = n.min(buf.len()).min(got.len());
                buf[..n].copy_from_slice(&got[..n]);
                let addr = addr.unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)));
                Ok((n, addr))
            }
            _ => Err(io::Error::other("recv")),
        }
    }

    pub fn try_recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        unsafe {
            let mut storage = mem::zeroed::<libc::sockaddr_storage>();
            let mut len = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
            let n = libc::recvfrom(
                self.fd.fd,
                buf.as_mut_ptr().cast(),
                buf.len(),
                libc::MSG_DONTWAIT,
                (&mut storage as *mut libc::sockaddr_storage).cast(),
                &mut len,
            );
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            let addr = parse_addr(&storage).unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)));
            Ok((n as usize, addr))
        }
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.fd.local_addr()
    }
}

impl UnixListener {
    pub fn bind(path: &Path) -> io::Result<Self> {
        let fd = sys_socket(libc::AF_UNIX, libc::SOCK_STREAM, 0)?;
        let name = c_path(path)?;
        match op_event(Op::BindUnix { fd, path: name }) {
            Ok(Event::Bound) => {}
            Ok(_) => {
                unsafe { libc::close(fd) };
                return Err(io::Error::other("bind"));
            }
            Err(err) => {
                unsafe { libc::close(fd) };
                return Err(err);
            }
        }
        if let Err(err) = sys_listen(fd, 1024) {
            unsafe { libc::close(fd) };
            return Err(err);
        }
        Ok(Self { fd: OwnedFd { fd } })
    }

    pub fn accept(&self) -> io::Result<(UnixStream, ())> {
        match op_event(Op::Accept { fd: self.fd.fd })? {
            Event::Accept { fd, .. } => Ok((UnixStream { fd: OwnedFd { fd } }, ())),
            _ => Err(io::Error::other("accept")),
        }
    }
}

impl UnixStream {
    pub fn connect(path: &Path) -> io::Result<Self> {
        let fd = sys_socket(libc::AF_UNIX, libc::SOCK_STREAM, 0)?;
        let name = c_path(path)?;
        match op_event(Op::ConnectUnix { fd, path: name }) {
            Ok(Event::Connected) => Ok(Self { fd: OwnedFd { fd } }),
            Ok(_) => {
                unsafe { libc::close(fd) };
                Err(io::Error::other("connect"))
            }
            Err(err) => {
                unsafe { libc::close(fd) };
                Err(err)
            }
        }
    }

    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.fd.read(buf)
    }
    pub fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.fd.write_all(buf)
    }
    pub fn shutdown(&mut self) -> io::Result<()> {
        self.fd.shutdown()
    }
    pub fn into_split(self) -> (ReadHalf, WriteHalf) {
        split_fd(self.fd.into_raw())
    }
}

pub(super) fn domain_of(addr: SocketAddr) -> i32 {
    match addr {
        SocketAddr::V4(_) => libc::AF_INET,
        SocketAddr::V6(_) => libc::AF_INET6,
    }
}

pub fn lookup_host(host: &str) -> io::Result<Vec<SocketAddr>> {
    let host = host.to_owned();
    cpu(move || host.to_socket_addrs().map(|iter| iter.collect::<Vec<_>>()).map_err(|err| err.to_string()))
        .map_err(|err| io::Error::other(err))
}

pub fn send_fds(fd: i32, buf: &[u8], fds: &[i32]) -> io::Result<usize> {
    match op_event(Op::SendFds { fd, buf: buf.to_vec(), fds: fds.to_vec() })? {
        Event::Send { n } => Ok(n),
        _ => Err(io::Error::other("sendmsg")),
    }
}

pub fn recv_fds(fd: i32, len: usize) -> io::Result<(Vec<u8>, Vec<i32>)> {
    match op_event(Op::RecvFds { fd, len })? {
        Event::RecvFds { n, mut buf, fds } => {
            buf.truncate(n);
            Ok((buf, fds))
        }
        _ => Err(io::Error::other("recvmsg")),
    }
}
