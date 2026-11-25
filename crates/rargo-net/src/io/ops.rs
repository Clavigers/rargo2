use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::io::{self, ErrorKind};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use io_uring::{opcode, types};

use super::ring::{Kind, Slot};

pub enum Op {
    Recv { fd: i32, len: usize, flags: i32 },
    Send { fd: i32, buf: Vec<u8> },
    RecvFrom { fd: i32, len: usize },
    SendTo { fd: i32, buf: Vec<u8>, addr: SocketAddr },
    Accept { fd: i32 },
    Connect { fd: i32, addr: SocketAddr },
    Bind { fd: i32, addr: SocketAddr },
    BindUnix { fd: i32, path: CString },
    ConnectUnix { fd: i32, path: CString },
    Listen { fd: i32, backlog: i32 },
    Socket { domain: i32, ty: i32, proto: i32 },
    Shutdown { fd: i32, how: i32 },
    Read { fd: i32, off: u64, len: usize },
    Write { fd: i32, off: u64, buf: Vec<u8> },
    OpenAt { dir: i32, path: CString, flags: i32, mode: u32 },
    Statx { dir: i32, path: CString },
    RenameAt { old_dir: i32, old: CString, new_dir: i32, new: CString },
    Fsync { fd: i32 },
    Close { fd: i32 },
    Timeout { ms: u64 },
    WaitPid { pid: i32, reap: bool },
    SendFds { fd: i32, buf: Vec<u8>, fds: Vec<i32> },
    RecvFds { fd: i32, len: usize },
    Readable { fd: i32 },
    Cpu(Box<dyn FnOnce() + Send>),
}


pub enum Event {
    Recv { n: usize, buf: Vec<u8> },
    Send { n: usize },
    RecvFrom { n: usize, buf: Vec<u8>, addr: Option<SocketAddr> },
    SendTo { n: usize },
    Accept { fd: i32, peer: Option<SocketAddr> },
    Connected,
    Bound,
    Listening,
    Socket { fd: i32 },
    Shut,
    Read { n: usize, buf: Vec<u8> },
    Write { n: usize },
    Open { fd: i32 },
    Stat { buf: Box<libc::statx> },
    Renamed,
    Synced,
    Closed,
    Timeout,
    Exited { status: i32 },
    RecvFds { n: usize, buf: Vec<u8>, fds: Vec<i32> },
    Readable,
    Cpu,
    Failed(io::Error),
}

impl Event {
    pub(super) fn release(self) {
        match self {
            Event::Accept { fd, .. } | Event::Socket { fd } | Event::Open { fd } => {
                unsafe { libc::close(fd) };
            }
            Event::RecvFds { fds, .. } => {
                for fd in fds {
                    unsafe { libc::close(fd) };
                }
            }
            _ => {}
        }
    }
}

pub(super) fn prepare(token: u64, op: Op, inflight: &mut BTreeMap<u64, Slot>) -> io::Result<io_uring::squeue::Entry> {
    let entry = match op {
        Op::Cpu(_) => unreachable!("cpu work does not enter the ring"),
        Op::Recv { fd, len, flags } => {
            let mut buf = vec![0u8; len];
            let entry = opcode::Recv::new(types::Fd(fd), buf.as_mut_ptr(), buf.len() as u32).flags(flags).build().user_data(token);
            inflight.insert(token, Slot::buf(Kind::Recv, buf));
            entry
        }
        Op::Send { fd, buf } => {
            let entry = opcode::Send::new(types::Fd(fd), buf.as_ptr(), buf.len() as u32).build().user_data(token);
            inflight.insert(token, Slot::buf(Kind::Send, buf));
            entry
        }
        Op::RecvFrom { fd, len } => datagram_recv(token, fd, len, inflight)?,
        Op::SendTo { fd, buf, addr } => datagram_send(token, fd, buf, addr, inflight)?,
        Op::Connect { fd, addr } => sock_addr_op(token, Kind::Connect, fd, addr, inflight, |fd, ptr, len| {
            opcode::Connect::new(types::Fd(fd), ptr, len).build()
        })?,
        Op::Bind { fd, addr } => sock_addr_op(token, Kind::Bind, fd, addr, inflight, |fd, ptr, len| {
            opcode::Bind::new(types::Fd(fd), ptr, len).build()
        })?,
        Op::BindUnix { fd, path } => unix_op(token, Kind::Bind, fd, path, inflight, |fd, ptr, len| {
            opcode::Bind::new(types::Fd(fd), ptr, len).build()
        })?,
        Op::ConnectUnix { fd, path } => unix_op(token, Kind::Connect, fd, path, inflight, |fd, ptr, len| {
            opcode::Connect::new(types::Fd(fd), ptr, len).build()
        })?,
        Op::Listen { fd, backlog } => {
            let entry = opcode::Listen::new(types::Fd(fd), backlog).build().user_data(token);
            inflight.insert(token, Slot::empty(Kind::Listen));
            entry
        }
        Op::Socket { domain, ty, proto } => {
            let entry = opcode::Socket::new(domain, ty, proto).build().user_data(token);
            inflight.insert(token, Slot::empty(Kind::Socket));
            entry
        }
        Op::Shutdown { fd, how } => {
            let entry = opcode::Shutdown::new(types::Fd(fd), how).build().user_data(token);
            inflight.insert(token, Slot::empty(Kind::Shutdown));
            entry
        }
        Op::Accept { fd } => {
            let addr = Box::new(unsafe { std::mem::zeroed::<libc::sockaddr_storage>() });
            let addrlen = Box::new(std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t);
            let entry = opcode::Accept::new(types::Fd(fd), (&*addr as *const libc::sockaddr_storage).cast_mut().cast(), (&*addrlen as *const libc::socklen_t).cast_mut())
                .flags(libc::SOCK_CLOEXEC)
                .build()
                .user_data(token);
            inflight.insert(token, Slot { kind: Kind::Accept, addr: Some(addr), addrlen: Some(addrlen), ..Slot::empty(Kind::Accept) });
            entry
        }
        Op::Read { fd, off, len } => {
            let mut buf = vec![0u8; len];
            let entry = opcode::Read::new(types::Fd(fd), buf.as_mut_ptr(), buf.len() as u32).offset(off).build().user_data(token);
            inflight.insert(token, Slot::buf(Kind::Read, buf));
            entry
        }
        Op::Write { fd, off, buf } => {
            let entry = opcode::Write::new(types::Fd(fd), buf.as_ptr(), buf.len() as u32).offset(off).build().user_data(token);
            inflight.insert(token, Slot::buf(Kind::Write, buf));
            entry
        }
        Op::OpenAt { dir, path, flags, mode } => {
            let entry = opcode::OpenAt::new(types::Fd(dir), path.as_ptr()).flags(flags).mode(mode).build().user_data(token);
            inflight.insert(token, Slot { kind: Kind::Open, path: Some(path), ..Slot::empty(Kind::Open) });
            entry
        }
        Op::Statx { dir, path } => {
            let stat = Box::new(unsafe { std::mem::zeroed::<libc::statx>() });
            let entry = opcode::Statx::new(types::Fd(dir), path.as_ptr(), (&*stat as *const libc::statx).cast::<types::statx>().cast_mut())
                .flags(libc::AT_STATX_SYNC_AS_STAT)
                .mask(libc::STATX_BASIC_STATS)
                .build()
                .user_data(token);
            inflight.insert(token, Slot { kind: Kind::Stat, stat: Some(stat), path: Some(path), ..Slot::empty(Kind::Stat) });
            entry
        }
        Op::RenameAt { old_dir, old, new_dir, new } => {
            let entry = opcode::RenameAt::new(types::Fd(old_dir), old.as_ptr(), types::Fd(new_dir), new.as_ptr()).build().user_data(token);
            inflight.insert(token, Slot { kind: Kind::Rename, path: Some(old), path2: Some(new), ..Slot::empty(Kind::Rename) });
            entry
        }
        Op::Fsync { fd } => {
            let entry = opcode::Fsync::new(types::Fd(fd)).build().user_data(token);
            inflight.insert(token, Slot::empty(Kind::Fsync));
            entry
        }
        Op::Close { fd } => {
            let entry = opcode::Close::new(types::Fd(fd)).build().user_data(token);
            inflight.insert(token, Slot::empty(Kind::Close));
            entry
        }
        Op::Timeout { ms } => {
            let ts = Box::new(types::Timespec::new().sec(ms / 1000).nsec(((ms % 1000) * 1_000_000) as u32));
            let entry = opcode::Timeout::new(&*ts).build().user_data(token);
            inflight.insert(token, Slot { kind: Kind::Timeout, ts: Some(ts), ..Slot::empty(Kind::Timeout) });
            entry
        }
        Op::SendFds { fd, buf, fds } => fds_send(token, fd, buf, &fds, inflight),
        Op::RecvFds { fd, len } => fds_recv(token, fd, len, inflight),
        Op::WaitPid { pid, reap } => {
            let info = Box::new(unsafe { std::mem::zeroed::<libc::siginfo_t>() });
            let options = if reap { libc::WEXITED } else { libc::WEXITED | libc::WNOWAIT };
            let entry = opcode::WaitId::new(libc::P_PID, pid as libc::id_t, options)
                .infop(&*info)
                .build()
                .user_data(token);
            inflight.insert(token, Slot { kind: Kind::WaitPid, info: Some(info), ..Slot::empty(Kind::WaitPid) });
            entry
        }
        Op::Readable { fd } => {
            let entry = opcode::PollAdd::new(types::Fd(fd), libc::POLLIN as u32).build().user_data(token);
            inflight.insert(token, Slot::empty(Kind::Readable));
            entry
        }
    };
    Ok(entry)
}

pub(super) fn store_addr(addr: SocketAddr) -> (Box<libc::sockaddr_storage>, libc::socklen_t) {
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let len = match addr {
        SocketAddr::V4(v4) => {
            let s = libc::sockaddr_in {
                sin_family: libc::AF_INET as _,
                sin_port: v4.port().to_be(),
                sin_addr: libc::in_addr { s_addr: u32::from_ne_bytes(v4.ip().octets()) },
                sin_zero: [0; 8],
            };
            unsafe { std::ptr::copy_nonoverlapping(&s, &mut storage as *mut _ as *mut libc::sockaddr_in, 1) };
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t
        }
        SocketAddr::V6(v6) => {
            let s = libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as _,
                sin6_port: v6.port().to_be(),
                sin6_flowinfo: v6.flowinfo(),
                sin6_addr: libc::in6_addr { s6_addr: v6.ip().octets() },
                sin6_scope_id: v6.scope_id(),
            };
            unsafe { std::ptr::copy_nonoverlapping(&s, &mut storage as *mut _ as *mut libc::sockaddr_in6, 1) };
            std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t
        }
    };
    (Box::new(storage), len)
}

pub(super) fn parse_addr(storage: &libc::sockaddr_storage) -> Option<SocketAddr> {
    if storage.ss_family == libc::AF_INET as libc::sa_family_t {
        let s = unsafe { &*(storage as *const _ as *const libc::sockaddr_in) };
        let ip = Ipv4Addr::from(s.sin_addr.s_addr.to_ne_bytes());
        Some(SocketAddr::V4(SocketAddrV4::new(ip, u16::from_be(s.sin_port))))
    } else if storage.ss_family == libc::AF_INET6 as libc::sa_family_t {
        let s = unsafe { &*(storage as *const _ as *const libc::sockaddr_in6) };
        let ip = Ipv6Addr::from(s.sin6_addr.s6_addr);
        Some(SocketAddr::V6(SocketAddrV6::new(ip, u16::from_be(s.sin6_port), s.sin6_flowinfo, s.sin6_scope_id)))
    } else {
        None
    }
}

pub(super) fn unix_storage(path: &CStr) -> io::Result<(Box<libc::sockaddr_storage>, libc::socklen_t)> {
    let bytes = path.to_bytes();
    if bytes.len() >= 108 {
        return Err(io::Error::new(ErrorKind::InvalidInput, "unix path is too long"));
    }
    let mut un: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    un.sun_family = libc::AF_UNIX as _;
    for (i, b) in bytes.iter().enumerate() {
        un.sun_path[i] = *b as libc::c_char;
    }
    let len = (std::mem::size_of::<libc::sa_family_t>() + bytes.len() + 1) as libc::socklen_t;
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    unsafe { std::ptr::copy_nonoverlapping(&un as *const _ as *const u8, &mut storage as *mut _ as *mut u8, len as usize) };
    Ok((Box::new(storage), len))
}

pub(super) fn sock_addr_op(
    token: u64,
    kind: Kind,
    fd: i32,
    addr: SocketAddr,
    inflight: &mut BTreeMap<u64, Slot>,
    build: impl FnOnce(i32, *const libc::sockaddr, libc::socklen_t) -> io_uring::squeue::Entry,
) -> io::Result<io_uring::squeue::Entry> {
    let (boxed, len) = store_addr(addr);
    let ptr = &*boxed as *const libc::sockaddr_storage as *const libc::sockaddr;
    let entry = build(fd, ptr, len).user_data(token);
    inflight.insert(token, Slot { kind, addr: Some(boxed), ..Slot::empty(kind) });
    Ok(entry)
}

pub(super) fn unix_op(
    token: u64,
    kind: Kind,
    fd: i32,
    path: CString,
    inflight: &mut BTreeMap<u64, Slot>,
    build: impl FnOnce(i32, *const libc::sockaddr, libc::socklen_t) -> io_uring::squeue::Entry,
) -> io::Result<io_uring::squeue::Entry> {
    let (boxed, len) = unix_storage(&path)?;
    let ptr = &*boxed as *const libc::sockaddr_storage as *const libc::sockaddr;
    let entry = build(fd, ptr, len).user_data(token);
    inflight.insert(token, Slot { kind, addr: Some(boxed), path: Some(path), ..Slot::empty(kind) });
    Ok(entry)
}

pub(super) fn datagram_recv(token: u64, fd: i32, len: usize, inflight: &mut BTreeMap<u64, Slot>) -> io::Result<io_uring::squeue::Entry> {
    let mut buf = vec![0u8; len];
    let addr = Box::new(unsafe { std::mem::zeroed::<libc::sockaddr_storage>() });
    let addrlen = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    let iov = Box::new(libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() });
    let msg = Box::new(unsafe { std::mem::zeroed::<libc::msghdr>() });
    let msg = {
        let mut msg = msg;
        msg.msg_name = (&*addr as *const libc::sockaddr_storage).cast_mut().cast();
        msg.msg_namelen = addrlen;
        msg.msg_iov = (&*iov as *const libc::iovec).cast_mut();
        msg.msg_iovlen = 1;
        msg
    };
    let entry = opcode::RecvMsg::new(types::Fd(fd), (&*msg as *const libc::msghdr).cast_mut()).build().user_data(token);
    inflight.insert(token, Slot { kind: Kind::RecvFrom, buf: Some(buf), addr: Some(addr), iov: Some(iov), msg: Some(msg), ..Slot::empty(Kind::RecvFrom) });
    Ok(entry)
}

pub(super) fn datagram_send(token: u64, fd: i32, buf: Vec<u8>, addr: SocketAddr, inflight: &mut BTreeMap<u64, Slot>) -> io::Result<io_uring::squeue::Entry> {
    let (addr, len) = store_addr(addr);
    let iov = Box::new(libc::iovec { iov_base: buf.as_ptr().cast_mut().cast(), iov_len: buf.len() });
    let mut msg = Box::new(unsafe { std::mem::zeroed::<libc::msghdr>() });
    msg.msg_name = (&*addr as *const libc::sockaddr_storage).cast_mut().cast();
    msg.msg_namelen = len;
    msg.msg_iov = (&*iov as *const libc::iovec).cast_mut();
    msg.msg_iovlen = 1;
    let entry = opcode::SendMsg::new(types::Fd(fd), &*msg).build().user_data(token);
    inflight.insert(token, Slot { kind: Kind::SendTo, buf: Some(buf), addr: Some(addr), iov: Some(iov), msg: Some(msg), ..Slot::empty(Kind::SendTo) });
    Ok(entry)
}

const CONTROL: usize = 256;
const MOST_FDS: usize = 32;

fn fds_send(token: u64, fd: i32, buf: Vec<u8>, fds: &[i32], inflight: &mut BTreeMap<u64, Slot>) -> io_uring::squeue::Entry {
    assert!(fds.len() <= MOST_FDS, "descriptors per message fit the control buffer");
    let iov = Box::new(libc::iovec { iov_base: buf.as_ptr().cast_mut().cast(), iov_len: buf.len() });
    let mut cbuf = vec![0u8; CONTROL];
    let mut msg = Box::new(unsafe { std::mem::zeroed::<libc::msghdr>() });
    msg.msg_iov = (&*iov as *const libc::iovec).cast_mut();
    msg.msg_iovlen = 1;
    if !fds.is_empty() {
        let bytes = std::mem::size_of_val(fds) as u32;
        msg.msg_control = cbuf.as_mut_ptr().cast();
        msg.msg_controllen = unsafe { libc::CMSG_SPACE(bytes) } as _;
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&*msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(bytes) as _;
            std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(c).cast::<i32>(), fds.len());
        }
    }
    let entry = opcode::SendMsg::new(types::Fd(fd), &*msg).flags(libc::MSG_NOSIGNAL as u32).build().user_data(token);
    inflight.insert(token, Slot { buf: Some(buf), iov: Some(iov), msg: Some(msg), cbuf: Some(cbuf), ..Slot::empty(Kind::SendFds) });
    entry
}

fn fds_recv(token: u64, fd: i32, len: usize, inflight: &mut BTreeMap<u64, Slot>) -> io_uring::squeue::Entry {
    let mut buf = vec![0u8; len];
    let mut cbuf = vec![0u8; CONTROL];
    let iov = Box::new(libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() });
    let mut msg = Box::new(unsafe { std::mem::zeroed::<libc::msghdr>() });
    msg.msg_iov = (&*iov as *const libc::iovec).cast_mut();
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr().cast();
    msg.msg_controllen = cbuf.len() as _;
    let entry = opcode::RecvMsg::new(types::Fd(fd), (&*msg as *const libc::msghdr).cast_mut())
        .flags(libc::MSG_CMSG_CLOEXEC as u32)
        .build()
        .user_data(token);
    inflight.insert(token, Slot { buf: Some(buf), iov: Some(iov), msg: Some(msg), cbuf: Some(cbuf), ..Slot::empty(Kind::RecvFds) });
    entry
}

fn received_fds(msg: &libc::msghdr) -> Vec<i32> {
    let mut fds = Vec::new();
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let n = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / std::mem::size_of::<i32>();
                let data = libc::CMSG_DATA(c).cast::<i32>();
                for i in 0..n {
                    fds.push(std::ptr::read_unaligned(data.add(i)));
                }
            }
            c = libc::CMSG_NXTHDR(msg, c);
        }
    }
    fds
}

fn wait_status(info: &libc::siginfo_t) -> i32 {
    let status = unsafe { info.si_status() };
    match info.si_code {
        libc::CLD_EXITED => (status & 0xff) << 8,
        libc::CLD_DUMPED => (status & 0x7f) | 0x80,
        _ => status & 0x7f,
    }
}

pub(super) fn event_of(slot: Slot, res: i32) -> Event {
    let Slot { kind, buf, stat, addr, info, path, path2, addrlen, ts, iov, msg, cbuf } = slot;
    let fds = match (&kind, &msg) {
        (Kind::RecvFds, Some(msg)) if res >= 0 => received_fds(msg),
        _ => Vec::new(),
    };
    drop((path, path2, addrlen, ts, iov, msg, cbuf));
    if res < 0 {
        if matches!(kind, Kind::Timeout) && res == -libc::ETIME {
            return Event::Timeout;
        }
        return Event::Failed(io::Error::from_raw_os_error(-res));
    }
    let peer = addr.as_deref().and_then(parse_addr);
    match kind {
        Kind::Recv => Event::Recv { n: res as usize, buf: buf.unwrap_or_default() },
        Kind::Send => Event::Send { n: res as usize },
        Kind::RecvFrom => Event::RecvFrom { n: res as usize, buf: buf.unwrap_or_default(), addr: peer },
        Kind::SendTo => Event::SendTo { n: res as usize },
        Kind::Accept => Event::Accept { fd: res, peer },
        Kind::Connect => Event::Connected,
        Kind::Bind => Event::Bound,
        Kind::Listen => Event::Listening,
        Kind::Socket => Event::Socket { fd: res },
        Kind::Shutdown => Event::Shut,
        Kind::Read => Event::Read { n: res as usize, buf: buf.unwrap_or_default() },
        Kind::Write => Event::Write { n: res as usize },
        Kind::Open => Event::Open { fd: res },
        Kind::Stat => Event::Stat { buf: stat.unwrap_or_else(|| Box::new(unsafe { std::mem::zeroed() })) },
        Kind::Rename => Event::Renamed,
        Kind::Fsync => Event::Synced,
        Kind::Close => Event::Closed,
        Kind::Timeout => Event::Timeout,
        Kind::WaitPid => Event::Exited { status: info.map(|info| wait_status(&info)).unwrap_or(res) },
        Kind::SendFds => Event::Send { n: res as usize },
        Kind::RecvFds => Event::RecvFds { n: res as usize, buf: buf.unwrap_or_default(), fds },
        Kind::Readable => Event::Readable,
    }
}

pub fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::new(ErrorKind::InvalidInput, "path contains a nul"))
}

