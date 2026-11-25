
use io_uring::{opcode, types, IoUring};
mod drive;
mod fuse;

pub use fuse::FuseDrive;

pub use drive::{
    AbortHandle, call, close_fd, cpu, drive, fsync_fd, interval, lookup_host, open_at, race_biased, read_at, read_ready, rename_at,
    signal, sleep, spawn, spawn_off, stat_at, timeout, unbounded_channel, wait_pid, write_at, yield_now, Elapsed, Interval,
    JoinError, JoinHandle, Mutex, MutexGuard, Notify, Permit, ReadHalf, RecvError, Semaphore, Signal, SignalKind,
    TcpListener, TcpStream, UdpSocket, UnboundedReceiver, UnboundedSender, UnixListener, UnixStream, WriteHalf,
    try_read,
};
pub use drive::oneshot;
pub use drive::watch;

use std::collections::{BTreeMap, VecDeque};
use std::ffi::{CStr, CString};
use std::io::{self, ErrorKind};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::os::unix::ffi::OsStrExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex as StdMutex};
use std::thread::JoinHandle as ThreadJoin;

pub(super) const EVENTFD: u64 = 0;
const CANCEL: u64 = u64::MAX;
const RING_ENTRIES: u32 = 4096;

pub fn refusal(err: &io::Error) -> Option<&'static str> {
    match err.raw_os_error() {
        Some(libc::EPERM) => Some(
            "io_uring is unavailable (io_uring_setup returned EPERM). Docker's default seccomp profile blocks it: start the container with --security-opt seccomp=unconfined. If kernel.io_uring_disabled is set, set it to 0.",
        ),
        Some(libc::ENOSYS) => Some(
            "io_uring is unavailable (io_uring_setup returned ENOSYS). The kernel has io_uring disabled or does not provide it: set kernel.io_uring_disabled to 0 on a kernel that includes io_uring.",
        ),
        _ => None,
    }
}

pub fn run<R>(workers: usize, main: impl FnOnce() -> R) -> R {
    let mut out = None;
    {
        let slot = &mut out;
        let body: Box<dyn FnOnce() + '_> = Box::new(move || *slot = Some(main()));
        let body: Box<dyn FnOnce() + 'static> = unsafe { std::mem::transmute(body) };
        drive(workers, body);
    }
    out.expect("drive returns after main has finished")
}

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
    WaitPid { pid: i32 },
    Readable { fd: i32 },
    Cpu(Box<dyn FnOnce() -> CpuOut + Send>),
}

pub type CpuOut = Result<Box<dyn std::any::Any + Send>, String>;

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
    Readable,
    Cpu(CpuOut),
    Failed(io::Error),
}

impl Event {
    pub(super) fn release(self) {
        if let Event::Accept { fd, .. } | Event::Socket { fd } | Event::Open { fd } = self {
            unsafe { libc::close(fd) };
        }
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Recv,
    Send,
    RecvFrom,
    SendTo,
    Accept,
    Connect,
    Bind,
    Listen,
    Socket,
    Shutdown,
    Read,
    Write,
    Open,
    Stat,
    Rename,
    Fsync,
    Close,
    Timeout,
    WaitPid,
    Readable,
}

struct Slot {
    kind: Kind,
    buf: Option<Vec<u8>>,
    stat: Option<Box<libc::statx>>,
    addr: Option<Box<libc::sockaddr_storage>>,
    addrlen: Option<Box<libc::socklen_t>>,
    ts: Option<Box<types::Timespec>>,
    info: Option<Box<libc::siginfo_t>>,
    path: Option<CString>,
    path2: Option<CString>,
    iov: Option<Box<libc::iovec>>,
    msg: Option<Box<libc::msghdr>>,
}

struct Job {
    token: u64,
    run: Box<dyn FnOnce() -> CpuOut + Send>,
}

struct Jobs {
    q: StdMutex<VecDeque<Job>>,
    cv: Condvar,
    closed: AtomicBool,
}

struct Pool {
    jobs: Arc<Jobs>,
    done: Arc<StdMutex<VecDeque<(u64, CpuOut)>>>,
    joins: Vec<ThreadJoin<()>>,
    efd: i32,
}

pub struct Loop {
    ring: IoUring,
    next: u64,
    inflight: BTreeMap<u64, Slot>,
    pool: Pool,
}

impl Loop {
    pub fn new(workers: usize) -> io::Result<Self> {
        assert!(workers >= 1, "the worker pool has at least one thread");
        let ring = IoUring::new(RING_ENTRIES)?;
        let efd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if efd < 0 {
            return Err(io::Error::last_os_error());
        }
        let jobs = Arc::new(Jobs { q: StdMutex::new(VecDeque::new()), cv: Condvar::new(), closed: AtomicBool::new(false) });
        let done = Arc::new(StdMutex::new(VecDeque::new()));
        let mut joins = Vec::with_capacity(workers);
        for _ in 0..workers {
            let jobs = Arc::clone(&jobs);
            let done = Arc::clone(&done);
            joins.push(std::thread::Builder::new().name("rargo-cpu".into()).spawn(move || worker(jobs, done, efd)).expect("a worker thread starts"));
        }
        let mut this = Self {
            ring,
            next: 1,
            inflight: BTreeMap::new(),
            pool: Pool { jobs, done, joins, efd },
        };
        this.arm_eventfd()?;
        this.ring.submit()?;
        Ok(this)
    }

    pub(crate) fn wake_fd(&self) -> i32 {
        self.pool.efd
    }

    pub fn submit(&mut self, op: Op) -> io::Result<u64> {
        let token = self.next;
        self.next += 1;
        if let Op::Cpu(run) = op {
            if self.pool.jobs.closed.load(Ordering::Relaxed) {
                return Err(io::Error::other("worker pool stopped"));
            }
            self.pool.jobs.q.lock().unwrap_or_else(|e| e.into_inner()).push_back(Job { token, run });
            self.pool.jobs.cv.notify_one();
            return Ok(token);
        }
        self.enqueue(token, op)?;
        Ok(token)
    }

    pub fn wait(&mut self) -> io::Result<Vec<(u64, Event)>> {
        loop {
            match self.ring.submit_and_wait(1) {
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
                Ok(_) => break,
            }
        }
        self.harvest()
    }

    pub(super) fn cancel(&mut self, token: u64) {
        if !self.inflight.contains_key(&token) {
            return;
        }
        let entry = opcode::AsyncCancel::new(token).build().user_data(CANCEL);
        unsafe {
            if self.ring.submission().push(&entry).is_err() {
                let _ = self.ring.submit();
                let _ = self.ring.submission().push(&entry);
            }
        }
    }

    fn enqueue(&mut self, token: u64, op: Op) -> io::Result<()> {
        let entry = prepare(token, op, &mut self.inflight)?;
        unsafe {
            if self.ring.submission().push(&entry).is_err() {
                if let Err(e) = self.ring.submit() {
                    self.inflight.remove(&token);
                    return Err(e);
                }
                if self.ring.submission().push(&entry).is_err() {
                    self.inflight.remove(&token);
                    return Err(io::Error::other("submission queue is full"));
                }
            }
        }
        Ok(())
    }

    fn arm_eventfd(&mut self) -> io::Result<()> {
        let mut buf = vec![0u8; 8];
        let entry = opcode::Read::new(types::Fd(self.pool.efd), buf.as_mut_ptr(), buf.len() as u32).build().user_data(EVENTFD);
        self.inflight.insert(EVENTFD, Slot::buf(Kind::Read, buf));
        unsafe {
            self.ring.submission().push(&entry).map_err(|_| io::Error::other("submission queue is full"))?;
        }
        Ok(())
    }

    fn harvest(&mut self) -> io::Result<Vec<(u64, Event)>> {
        let ready: Vec<(u64, i32)> = self.ring.completion().map(|c| (c.user_data(), c.result())).collect();
        let mut out = Vec::with_capacity(ready.len());
        let mut rearm = false;
        for (id, res) in ready {
            if id == CANCEL {
                continue;
            }
            if id == EVENTFD {
                self.inflight.remove(&EVENTFD);
                rearm = true;
                out.extend(self.take_cpu());
                continue;
            }
            let slot = self.inflight.remove(&id).expect("a completion matches a submission");
            out.push((id, event_of(slot, res)));
        }
        if rearm {
            self.arm_eventfd()?;
            self.ring.submit()?;
        }
        Ok(out)
    }

    fn take_cpu(&self) -> Vec<(u64, Event)> {
        let mut q = self.pool.done.lock().unwrap_or_else(|e| e.into_inner());
        q.drain(..).map(|(token, out)| (token, Event::Cpu(out))).collect()
    }
}

impl Drop for Loop {
    fn drop(&mut self) {
        self.pool.jobs.closed.store(true, Ordering::Relaxed);
        self.pool.jobs.cv.notify_all();
        for join in self.pool.joins.drain(..) {
            let _ = join.join();
        }
        if self.pool.efd >= 0 {
            unsafe { libc::close(self.pool.efd) };
        }
    }
}

fn worker(jobs: Arc<Jobs>, done: Arc<StdMutex<VecDeque<(u64, CpuOut)>>>, efd: i32) {
    loop {
        let job = {
            let mut q = jobs.q.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(job) = q.pop_front() {
                    break job;
                }
                if jobs.closed.load(Ordering::Relaxed) {
                    return;
                }
                q = jobs.cv.wait(q).unwrap_or_else(|e| e.into_inner());
            }
        };
        let out = match catch_unwind(AssertUnwindSafe(job.run)) {
            Ok(out) => out,
            Err(_) => Err("cpu job panicked".into()),
        };
        done.lock().unwrap_or_else(|e| e.into_inner()).push_back((job.token, out));
        let one = 1u64;
        unsafe { libc::write(efd, &one as *const u64 as *const libc::c_void, 8) };
    }
}

fn prepare(token: u64, op: Op, inflight: &mut BTreeMap<u64, Slot>) -> io::Result<io_uring::squeue::Entry> {
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
        Op::WaitPid { pid } => {
            let info = Box::new(unsafe { std::mem::zeroed::<libc::siginfo_t>() });
            let entry = opcode::WaitId::new(libc::P_PID, pid as libc::id_t, libc::WEXITED)
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

fn store_addr(addr: SocketAddr) -> (Box<libc::sockaddr_storage>, libc::socklen_t) {
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

fn unix_storage(path: &CStr) -> io::Result<(Box<libc::sockaddr_storage>, libc::socklen_t)> {
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

fn sock_addr_op(
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

fn unix_op(
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

fn datagram_recv(token: u64, fd: i32, len: usize, inflight: &mut BTreeMap<u64, Slot>) -> io::Result<io_uring::squeue::Entry> {
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

fn datagram_send(token: u64, fd: i32, buf: Vec<u8>, addr: SocketAddr, inflight: &mut BTreeMap<u64, Slot>) -> io::Result<io_uring::squeue::Entry> {
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

fn event_of(slot: Slot, res: i32) -> Event {
    let Slot { kind, buf, stat, addr, info, path, path2, addrlen, ts, iov, msg } = slot;
    drop((path, path2, addrlen, ts, iov, msg));
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
        Kind::WaitPid => {
            let status = info.map(|info| unsafe { info.si_status() }).unwrap_or(res);
            Event::Exited { status }
        }
        Kind::Readable => Event::Readable,
    }
}

impl Slot {
    fn empty(kind: Kind) -> Self {
        Self { kind, buf: None, stat: None, addr: None, addrlen: None, ts: None, info: None, path: None, path2: None, iov: None, msg: None }
    }
    fn buf(kind: Kind, buf: Vec<u8>) -> Self {
        Self { buf: Some(buf), ..Self::empty(kind) }
    }
}

pub fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::new(ErrorKind::InvalidInput, "path contains a nul"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_ring_says_what_to_change() {
        let perm = io::Error::from_raw_os_error(libc::EPERM);
        let nosys = io::Error::from_raw_os_error(libc::ENOSYS);
        let mem = io::Error::from_raw_os_error(libc::ENOMEM);
        let perm_msg = refusal(&perm).expect("EPERM");
        let nosys_msg = refusal(&nosys).expect("ENOSYS");
        assert!(perm_msg.contains("EPERM") && perm_msg.contains("seccomp") && perm_msg.contains("io_uring_disabled"));
        assert!(nosys_msg.contains("ENOSYS") && nosys_msg.contains("io_uring_disabled"));
        assert!(refusal(&mem).is_none());
        assert_ne!(perm_msg, nosys_msg);
    }
}
