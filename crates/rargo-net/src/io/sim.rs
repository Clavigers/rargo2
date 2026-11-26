//! A simulated backend for the fiber scheduler.
//!
//! The scheduler hands every fiber's `Op` to a backend and resumes the fiber
//! with the backend's `Event`. The real backend is the io_uring loop in
//! `ring.rs`. This one answers from memory: a virtual clock, a network of
//! datagram sockets with seeded latency, loss and duplication, and a
//! filesystem held in maps. Nothing here makes a system call, so a run is a
//! function of its seed, and a second of simulated time costs no real time.
//!
//! UDP sockets, timers, pool work and plain files are simulated. TCP,
//! Unix sockets, processes, fd passing, FUSE and signals are not; those ops
//! fail with `Unsupported`.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::CString;
use std::io;
use std::net::SocketAddr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use super::fiber::{self, Backend};
use super::ops::{Event, Op};

/// Simulated descriptors start here, far above any descriptor the process
/// really holds, so a stray `libc` call on one fails with EBADF instead of
/// touching a real file.
const FIRST_FD: i32 = 1 << 28;

/// The user and group every simulated process runs as.
const UID: u32 = 1000;

/// The process id every simulated process reports.
const PID: u32 = 4242;

/// What the simulated network does to each datagram.
#[derive(Clone, Copy, Debug)]
pub struct Net {
    /// The chance in a thousand that a datagram is dropped.
    pub loss_per_mille: u32,
    /// The chance in a thousand that a datagram arrives twice.
    pub duplicate_per_mille: u32,
    /// Each datagram takes between these two delays, so datagrams reorder
    /// whenever the range is wider than the gap between sends.
    pub delay_ms: (u64, u64),
}

impl Default for Net {
    fn default() -> Net {
        Net { loss_per_mille: 0, duplicate_per_mille: 0, delay_ms: (1, 1) }
    }
}

/// What a finished run looked like.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Report {
    /// Simulated milliseconds from start to the last fiber finishing.
    pub elapsed_ms: u64,
    /// Operations the backend answered.
    pub ops: u64,
    pub datagrams_sent: u64,
    pub datagrams_lost: u64,
    pub datagrams_duplicated: u64,
    /// A hash of every completion in order (which op, when, and the bytes of
    /// each datagram). Two runs of one seed have the same trace.
    pub trace: u64,
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }

    fn chance(&mut self, per_mille: u32) -> bool {
        per_mille > 0 && self.below(1000) < u64::from(per_mille)
    }
}

struct Sock {
    local: Option<SocketAddr>,
    peer: Option<SocketAddr>,
    queue: VecDeque<(SocketAddr, Vec<u8>)>,
    /// The parked receive: its token, its buffer size, and whether it wants the sender's address.
    waiter: Option<(u64, usize, bool)>,
}

struct Inode {
    data: Vec<u8>,
    mode: u32,
    /// Nanoseconds since the Unix epoch.
    mtime_ns: i64,
    links: u32,
}

enum Node {
    /// A directory, with when it was made in nanoseconds since the Unix epoch.
    Dir { mode: u32, mtime_ns: i64 },
    File(u64),
    Link(PathBuf),
}

/// The bytes written to a pipe and not yet read.
#[derive(Default)]
struct Pipe {
    data: VecDeque<u8>,
    /// The parked read: its token and its buffer size.
    waiter: Option<(u64, usize)>,
}

enum Handle {
    Sock(Sock),
    File { ino: u64, path: PathBuf },
    Dir { path: PathBuf },
    Pipe { id: u64, writes: bool },
}

/// What a simulated command prints and how it exits.
pub struct Ran {
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// A command as the simulation sees it.
pub struct Spawned<'a> {
    pub program: &'a std::ffi::OsStr,
    pub args: &'a [std::ffi::OsString],
    pub cwd: Option<&'a Path>,
}

type Commands = Box<dyn FnMut(&Spawned) -> io::Result<Ran>>;

enum Due {
    Complete(u64, Event),
    Deliver { to: SocketAddr, from: SocketAddr, bytes: Vec<u8> },
}

pub struct SimLoop {
    rng: Rng,
    net: Net,
    /// Whether an operation that needs no waiting is answered where it is asked, without the fiber giving way.
    in_place: bool,
    /// The descriptors opened for appending: each write through one lands at the end of its file.
    appending: std::collections::BTreeSet<i32>,
    now_ms: u64,
    base: Instant,
    next_token: u64,
    next_seq: u64,
    next_fd: i32,
    next_port: u16,
    next_ino: u64,
    due: BTreeMap<(u64, u64), Due>,
    handles: BTreeMap<i32, Handle>,
    bound: BTreeMap<SocketAddr, i32>,
    files: BTreeMap<PathBuf, Node>,
    inodes: BTreeMap<u64, Inode>,
    pipes: BTreeMap<u64, Pipe>,
    env: BTreeMap<std::ffi::OsString, std::ffi::OsString>,
    commands: Option<Commands>,
    /// The latest modification time handed out, so that each change to a file is later than the one before.
    last_stamp: i64,
    /// The descriptor that holds the exclusive lock on each locked inode.
    locks: BTreeMap<u64, i32>,
    report: Report,
}

/// The wall clock reads this many seconds since the Unix epoch when a simulation starts.
const EPOCH_S: u64 = 1_700_000_000;

fn unsupported(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, format!("the simulated backend does not do {what} yet"))
}

fn errno(code: i32) -> io::Error {
    io::Error::from_raw_os_error(code)
}

fn normal(path: &Path) -> PathBuf {
    let mut out = PathBuf::from("/");
    for part in path.components() {
        match part {
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) => out.push(name),
        }
    }
    out
}

fn path_of(c: &CString) -> PathBuf {
    PathBuf::from(std::ffi::OsStr::from_bytes(c.as_bytes()))
}

impl SimLoop {
    pub fn new(seed: u64, net: Net) -> SimLoop {
        let mut files = BTreeMap::new();
        files.insert(PathBuf::from("/"), Node::Dir { mode: 0o755, mtime_ns: (EPOCH_S * 1_000_000_000) as i64 });
        SimLoop {
            rng: Rng(seed),
            in_place: false,
            appending: std::collections::BTreeSet::new(),
            net,
            now_ms: 0,
            base: Instant::now(),
            next_token: 1,
            next_seq: 0,
            next_fd: FIRST_FD,
            next_port: 40_000,
            next_ino: 2,
            due: BTreeMap::new(),
            handles: BTreeMap::new(),
            bound: BTreeMap::new(),
            files,
            inodes: BTreeMap::new(),
            pipes: BTreeMap::new(),
            env: BTreeMap::new(),
            commands: None,
            last_stamp: 0,
            locks: BTreeMap::new(),
            report: Report { elapsed_ms: 0, ops: 0, datagrams_sent: 0, datagrams_lost: 0, datagrams_duplicated: 0, trace: 0xcbf2_9ce4_8422_2325 },
        }
    }

    pub(super) fn report(&self) -> Report {
        Report { elapsed_ms: self.now_ms, ..self.report }
    }

    fn mix(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.report.trace = (self.report.trace ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn at(&mut self, delay_ms: u64, due: Due) {
        let key = (self.now_ms + delay_ms, self.next_seq);
        self.next_seq += 1;
        self.due.insert(key, due);
    }

    fn done(&mut self, token: u64, event: Event) {
        self.at(0, Due::Complete(token, event));
    }

    fn fail(&mut self, token: u64, err: io::Error) {
        self.done(token, Event::Failed(err));
    }

    fn fd(&mut self, handle: Handle) -> i32 {
        let fd = self.next_fd;
        self.next_fd += 1;
        self.handles.insert(fd, handle);
        fd
    }

    fn sock(&mut self, fd: i32) -> io::Result<&mut Sock> {
        match self.handles.get_mut(&fd) {
            Some(Handle::Sock(sock)) => Ok(sock),
            Some(_) => Err(errno(libc::ENOTSOCK)),
            None => Err(errno(libc::EBADF)),
        }
    }

    fn resolve(&self, dir: i32, path: &CString) -> io::Result<PathBuf> {
        let path = path_of(path);
        if path.is_absolute() {
            return Ok(normal(&path));
        }
        if dir == libc::AT_FDCWD {
            return Ok(normal(&Path::new("/").join(path)));
        }
        match self.handles.get(&dir) {
            Some(Handle::Dir { path: base }) | Some(Handle::File { path: base, .. }) => Ok(normal(&base.join(path))),
            Some(Handle::Sock(_)) | Some(Handle::Pipe { .. }) => Err(errno(libc::ENOTDIR)),
            None => Err(errno(libc::EBADF)),
        }
    }

    fn follow(&self, mut path: PathBuf) -> io::Result<PathBuf> {
        for _ in 0..40 {
            match self.files.get(&path) {
                Some(Node::Link(target)) => {
                    let base = path.parent().unwrap_or(Path::new("/")).to_path_buf();
                    path = normal(&base.join(target));
                }
                _ => return Ok(path),
            }
        }
        Err(errno(libc::ELOOP))
    }

    fn parent_is_dir(&self, path: &Path) -> io::Result<()> {
        match path.parent().map(|p| self.files.get(p)) {
            None | Some(Some(Node::Dir { .. })) => Ok(()),
            Some(Some(_)) => Err(errno(libc::ENOTDIR)),
            Some(None) => Err(errno(libc::ENOENT)),
        }
    }

    fn send(&mut self, fd: i32, bytes: Vec<u8>, to: Option<SocketAddr>) -> io::Result<usize> {
        let ephemeral = self.next_port;
        let sock = self.sock(fd)?;
        let to = to.or(sock.peer).ok_or_else(|| errno(libc::EDESTADDRREQ))?;
        let from = match sock.local {
            Some(local) => local,
            None => {
                let local = SocketAddr::from(([127, 0, 0, 1], ephemeral));
                sock.local = Some(local);
                self.next_port += 1;
                self.bound.insert(local, fd);
                local
            }
        };
        let n = bytes.len();
        self.report.datagrams_sent += 1;
        if self.rng.chance(self.net.loss_per_mille) {
            self.report.datagrams_lost += 1;
            return Ok(n);
        }
        let copies = if self.rng.chance(self.net.duplicate_per_mille) {
            self.report.datagrams_duplicated += 1;
            2
        } else {
            1
        };
        for _ in 0..copies {
            let (low, high) = self.net.delay_ms;
            let delay = low + self.rng.below(high.saturating_sub(low) + 1);
            self.at(delay, Due::Deliver { to, from, bytes: bytes.clone() });
        }
        Ok(n)
    }

    fn receive(&mut self, token: u64, fd: i32, len: usize, with_addr: bool) {
        let sock = match self.sock(fd) {
            Ok(sock) => sock,
            Err(err) => return self.fail(token, err),
        };
        match sock.queue.pop_front() {
            Some((from, bytes)) => {
                let event = datagram_event(from, bytes, len, with_addr);
                self.done(token, event);
            }
            None => sock.waiter = Some((token, len, with_addr)),
        }
    }

    fn deliver(&mut self, to: SocketAddr, from: SocketAddr, bytes: Vec<u8>) -> Option<(u64, Event)> {
        let fd = self.bound.get(&to).or_else(|| self.bound.get(&SocketAddr::new([0, 0, 0, 0].into(), to.port()))).copied()?;
        let Some(Handle::Sock(sock)) = self.handles.get_mut(&fd) else { return None };
        if sock.peer.is_some_and(|peer| peer != from) {
            return None;
        }
        match sock.waiter.take() {
            Some((token, len, with_addr)) => Some((token, datagram_event(from, bytes, len, with_addr))),
            None => {
                sock.queue.push_back((from, bytes));
                None
            }
        }
    }

    fn wall_ns(&self) -> i64 {
        (EPOCH_S * 1_000_000_000 + self.now_ms * 1_000_000) as i64
    }

    /// The time a change to a file is stamped with: the wall clock, moved on
    /// by a nanosecond when the clock has not moved since the last change, so
    /// a file that was written again never keeps its modification time.
    fn stamp(&mut self) -> i64 {
        self.last_stamp = self.wall_ns().max(self.last_stamp + 1);
        self.last_stamp
    }

    /// Follows symbolic links in every component, as the kernel does when it opens a path.
    fn real(&self, path: &Path) -> io::Result<PathBuf> {
        let mut out = PathBuf::from("/");
        for part in normal(path).components() {
            if let Component::Normal(name) = part {
                out.push(name);
                out = self.follow(out)?;
            }
        }
        Ok(out)
    }

    fn unlink_inode(&mut self, ino: u64) {
        if let Some(inode) = self.inodes.get_mut(&ino) {
            inode.links = inode.links.saturating_sub(1);
        }
        self.reap(ino);
    }

    /// Frees an inode nothing names and nothing holds open.
    fn reap(&mut self, ino: u64) {
        let open = self.handles.values().any(|h| matches!(h, Handle::File { ino: held, .. } if *held == ino));
        if !open && self.inodes.get(&ino).is_some_and(|inode| inode.links == 0) {
            self.inodes.remove(&ino);
        }
    }

    fn open(&mut self, dir: i32, path: &CString, flags: i32, mode: u32) -> io::Result<i32> {
        let path = self.resolve(dir, path)?;
        let path = match path.parent() {
            Some(parent) => self.real(parent)?.join(path.file_name().unwrap_or_default()),
            None => path,
        };
        let path = if flags & libc::O_NOFOLLOW != 0 { path } else { self.follow(path)? };
        match self.files.get(&path) {
            Some(Node::Dir { .. }) => {
                if flags & libc::O_ACCMODE != libc::O_RDONLY {
                    return Err(errno(libc::EISDIR));
                }
                Ok(self.fd(Handle::Dir { path }))
            }
            Some(Node::Link(_)) => Err(errno(libc::ELOOP)),
            Some(Node::File(ino)) => {
                let ino = *ino;
                if flags & libc::O_CREAT != 0 && flags & libc::O_EXCL != 0 {
                    return Err(errno(libc::EEXIST));
                }
                if flags & libc::O_DIRECTORY != 0 {
                    return Err(errno(libc::ENOTDIR));
                }
                let held = self.inodes[&ino].mode;
                let (reads, writes) = match flags & libc::O_ACCMODE {
                    libc::O_RDONLY => (true, flags & libc::O_TRUNC != 0),
                    libc::O_WRONLY => (false, true),
                    _ => (true, true),
                };
                if (reads && held & 0o400 == 0) || (writes && held & 0o200 == 0) {
                    return Err(errno(libc::EACCES));
                }
                if flags & libc::O_TRUNC != 0 {
                    let now = self.stamp();
                    let inode = self.inodes.get_mut(&ino).expect("a named file has its inode");
                    inode.data.clear();
                    inode.mtime_ns = now;
                }
                Ok(self.fd(Handle::File { ino, path }))
            }
            None => {
                if flags & libc::O_CREAT == 0 {
                    return Err(errno(libc::ENOENT));
                }
                self.parent_is_dir(&path)?;
                let ino = self.next_ino;
                self.next_ino += 1;
                let now = self.stamp();
                self.inodes.insert(ino, Inode { data: Vec::new(), mode: mode & 0o7777 & !0o022, mtime_ns: now, links: 1 });
                self.files.insert(path.clone(), Node::File(ino));
                Ok(self.fd(Handle::File { ino, path }))
            }
        }
    }

    fn file(&mut self, fd: i32) -> io::Result<&mut Inode> {
        let ino = match self.handles.get(&fd) {
            Some(Handle::File { ino, .. }) => *ino,
            Some(Handle::Dir { .. }) => return Err(errno(libc::EISDIR)),
            Some(Handle::Sock(_)) | Some(Handle::Pipe { .. }) => return Err(errno(libc::ESPIPE)),
            None => return Err(errno(libc::EBADF)),
        };
        Ok(self.inodes.get_mut(&ino).expect("an open file keeps its inode"))
    }

    /// The pipe `fd` is an end of, and whether it is the writing end.
    fn pipe_end(&self, fd: i32) -> Option<(u64, bool)> {
        match self.handles.get(&fd) {
            Some(Handle::Pipe { id, writes }) => Some((*id, *writes)),
            _ => None,
        }
    }

    /// Takes up to `len` bytes out of a pipe, or nothing when it is empty.
    fn pipe_take(&mut self, id: u64, len: usize) -> Option<Vec<u8>> {
        let pipe = self.pipes.get_mut(&id).expect("an open end keeps its pipe");
        if pipe.data.is_empty() {
            return None;
        }
        let n = len.min(pipe.data.len());
        Some(pipe.data.drain(..n).collect())
    }

    /// Puts bytes into a pipe and hands them to the read parked on it.
    fn pipe_put(&mut self, id: u64, bytes: &[u8]) {
        let pipe = self.pipes.get_mut(&id).expect("an open end keeps its pipe");
        pipe.data.extend(bytes);
        if let Some((token, len)) = pipe.waiter.take() {
            let buf = self.pipe_take(id, len).expect("the pipe was just written");
            self.done(token, Event::Read { n: buf.len(), buf });
        }
    }

    /// Forgets a closed end of a pipe: a read parked on a pipe with no writer left sees its end, and a pipe with no end left is freed.
    fn pipe_closed(&mut self, id: u64) {
        let open = |sim: &SimLoop, writing: bool| sim.handles.values().any(|h| matches!(h, Handle::Pipe { id: held, writes } if *held == id && *writes == writing));
        let (writers, readers) = (open(self, true), open(self, false));
        if !writers {
            if let Some((token, _)) = self.pipes.get_mut(&id).and_then(|pipe| pipe.waiter.take()) {
                self.done(token, Event::Read { n: 0, buf: Vec::new() });
            }
        }
        if !writers && !readers {
            self.pipes.remove(&id);
        }
    }

    fn stat_node(&self, path: &Path) -> io::Result<Box<libc::statx>> {
        let mut st: libc::statx = unsafe { std::mem::zeroed() };
        st.stx_nlink = 1;
        st.stx_blksize = 4096;
        st.stx_uid = UID;
        st.stx_gid = UID;
        match self.files.get(path) {
            None => return Err(errno(libc::ENOENT)),
            Some(Node::Dir { mode, mtime_ns }) => {
                st.stx_mode = (libc::S_IFDIR | *mode) as u16;
                st.stx_ino = 1;
                st.stx_mtime.tv_sec = mtime_ns.div_euclid(1_000_000_000);
                st.stx_mtime.tv_nsec = mtime_ns.rem_euclid(1_000_000_000) as u32;
                st.stx_ctime = st.stx_mtime;
                st.stx_atime = st.stx_mtime;
            }
            Some(Node::Link(target)) => {
                st.stx_mode = (libc::S_IFLNK | 0o777) as u16;
                st.stx_size = target.as_os_str().len() as u64;
                st.stx_ino = 1;
            }
            Some(Node::File(ino)) => self.stat_inode(*ino, &mut st),
        }
        Ok(Box::new(st))
    }

    fn stat_inode(&self, ino: u64, st: &mut libc::statx) {
        let inode = &self.inodes[&ino];
        st.stx_mode = (libc::S_IFREG | inode.mode) as u16;
        st.stx_size = inode.data.len() as u64;
        st.stx_blocks = (inode.data.len() as u64).div_ceil(512);
        st.stx_ino = ino;
        st.stx_nlink = inode.links;
        st.stx_mtime.tv_sec = inode.mtime_ns.div_euclid(1_000_000_000);
        st.stx_mtime.tv_nsec = inode.mtime_ns.rem_euclid(1_000_000_000) as u32;
        st.stx_ctime = st.stx_mtime;
        st.stx_atime = st.stx_mtime;
    }

    fn stat(&mut self, dir: i32, path: &CString, flags: i32) -> io::Result<Box<libc::statx>> {
        if path.as_bytes().is_empty() && flags & libc::AT_EMPTY_PATH != 0 {
            return match self.handles.get(&dir) {
                Some(Handle::File { ino, .. }) => {
                    let mut st: libc::statx = unsafe { std::mem::zeroed() };
                    st.stx_blksize = 4096;
                    st.stx_uid = UID;
                    st.stx_gid = UID;
                    self.stat_inode(*ino, &mut st);
                    Ok(Box::new(st))
                }
                Some(Handle::Dir { path }) => self.stat_node(&path.clone()),
                Some(Handle::Sock(_)) | Some(Handle::Pipe { .. }) => Err(errno(libc::EINVAL)),
                None => Err(errno(libc::EBADF)),
            };
        }
        let path = self.resolve(dir, path)?;
        let path = match path.parent() {
            Some(parent) => self.real(parent)?.join(path.file_name().unwrap_or_default()),
            None => path,
        };
        let path = if flags & libc::AT_SYMLINK_NOFOLLOW != 0 { path } else { self.follow(path)? };
        self.stat_node(&path)
    }

    fn under(&self, dir: &Path) -> Vec<PathBuf> {
        self.files.range(dir.to_path_buf()..).map(|(p, _)| p.clone()).take_while(|p| p.starts_with(dir)).collect()
    }

    fn remove_node(&mut self, path: &Path) {
        if let Some(Node::File(ino)) = self.files.remove(path) {
            self.unlink_inode(ino);
        }
    }

    fn rename(&mut self, from: PathBuf, to: PathBuf) -> io::Result<()> {
        self.parent_is_dir(&to)?;
        if !self.files.contains_key(&from) {
            return Err(errno(libc::ENOENT));
        }
        if from == to {
            return Ok(());
        }
        let moved = self.under(&from);
        for path in self.under(&to) {
            self.remove_node(&path);
        }
        for path in moved {
            let node = self.files.remove(&path).expect("listed above");
            let rest = path.strip_prefix(&from).expect("under the source");
            let dest = if rest.as_os_str().is_empty() { to.clone() } else { to.join(rest) };
            for handle in self.handles.values_mut() {
                if let Handle::File { path: open, .. } | Handle::Dir { path: open } = handle {
                    if *open == path {
                        *open = dest.clone();
                    }
                }
            }
            self.files.insert(dest, node);
        }
        Ok(())
    }

    fn answer(&mut self, token: u64, op: Op) -> io::Result<Option<Event>> {
        Ok(Some(match op {
            Op::Cpu(run) | Op::Blocking(run) => {
                run();
                Event::Cpu
            }
            Op::Timeout { ms } => {
                self.at(ms, Due::Complete(token, Event::Timeout));
                return Ok(None);
            }
            Op::Socket { ty, .. } => {
                if ty & 0xf != libc::SOCK_DGRAM {
                    return Err(unsupported("stream sockets"));
                }
                let fd = self.fd(Handle::Sock(Sock { local: None, peer: None, queue: VecDeque::new(), waiter: None }));
                Event::Socket { fd }
            }
            Op::Bind { fd, addr } => {
                let addr = if addr.port() == 0 {
                    self.next_port += 1;
                    SocketAddr::new(addr.ip(), self.next_port - 1)
                } else {
                    addr
                };
                if self.bound.contains_key(&addr) {
                    return Err(errno(libc::EADDRINUSE));
                }
                self.sock(fd)?.local = Some(addr);
                self.bound.insert(addr, fd);
                Event::Bound
            }
            Op::Connect { fd, addr } => {
                self.sock(fd)?.peer = Some(addr);
                Event::Connected
            }
            Op::SendTo { fd, buf, addr } => Event::SendTo { n: self.send(fd, buf, Some(addr))? },
            Op::Send { fd, buf } => Event::Send { n: self.send(fd, buf, None)? },
            Op::RecvFrom { fd, len } => {
                self.receive(token, fd, len, true);
                return Ok(None);
            }
            Op::Recv { fd, len, .. } => {
                self.receive(token, fd, len, false);
                return Ok(None);
            }
            Op::Shutdown { fd, .. } => {
                self.sock(fd)?;
                Event::Shut
            }
            Op::Close { fd } => {
                match self.handles.remove(&fd) {
                    Some(Handle::Sock(sock)) => {
                        if let Some(local) = sock.local {
                            self.bound.remove(&local);
                        }
                        if let Some((waiter, _, _)) = sock.waiter {
                            self.fail(waiter, errno(libc::EBADF));
                        }
                    }
                    Some(Handle::File { ino, .. }) => {
                        self.locks.retain(|_, holder| *holder != fd);
                        self.reap(ino)
                    }
                    Some(Handle::Dir { .. }) => {}
                    Some(Handle::Pipe { id, .. }) => self.pipe_closed(id),
                    None => return Err(errno(libc::EBADF)),
                }
                Event::Closed
            }
            Op::OpenAt { dir, path, flags, mode } => {
                let fd = self.open(dir, &path, flags, mode)?;
                if flags & libc::O_APPEND != 0 {
                    self.appending.insert(fd);
                }
                Event::Open { fd }
            }
            Op::Read { fd, len, .. } if self.pipe_end(fd).is_some() => {
                let (id, writes) = self.pipe_end(fd).expect("checked above");
                if writes {
                    return Err(errno(libc::EBADF));
                }
                match self.pipe_take(id, len) {
                    Some(buf) => Event::Read { n: buf.len(), buf },
                    None => {
                        let pipe = self.pipes.get_mut(&id).expect("an open end keeps its pipe");
                        if pipe.waiter.is_some() {
                            return Err(unsupported("two reads parked on one pipe"));
                        }
                        pipe.waiter = Some((token, len));
                        return Ok(None);
                    }
                }
            }
            Op::Write { fd, buf, .. } if self.pipe_end(fd).is_some() => {
                let (id, writes) = self.pipe_end(fd).expect("checked above");
                if !writes {
                    return Err(errno(libc::EBADF));
                }
                self.pipe_put(id, &buf);
                Event::Write { n: buf.len() }
            }
            Op::Read { fd, off, len } => {
                let data = &self.file(fd)?.data;
                let start = (off as usize).min(data.len());
                let end = start.saturating_add(len).min(data.len());
                let buf = data[start..end].to_vec();
                Event::Read { n: buf.len(), buf }
            }
            Op::Write { fd, off, buf } => {
                let now = self.stamp();
                let appending = self.appending.contains(&fd);
                let inode = self.file(fd)?;
                let off = if appending { inode.data.len() } else { off as usize };
                let end = off + buf.len();
                if inode.data.len() < end {
                    inode.data.resize(end, 0);
                }
                inode.data[off..end].copy_from_slice(&buf);
                inode.mtime_ns = now;
                Event::Write { n: buf.len() }
            }
            Op::Ftruncate { fd, len } => {
                let now = self.stamp();
                let inode = self.file(fd)?;
                inode.data.resize(len as usize, 0);
                inode.mtime_ns = now;
                Event::Done
            }
            Op::Fsync { fd } => {
                if !self.handles.contains_key(&fd) {
                    return Err(errno(libc::EBADF));
                }
                Event::Synced
            }
            Op::Statx { dir, path, flags } => Event::Stat { buf: self.stat(dir, &path, flags)? },
            Op::MkdirAt { dir, path, mode } => {
                let path = self.resolve(dir, &path)?;
                if self.files.contains_key(&path) {
                    return Err(errno(libc::EEXIST));
                }
                self.parent_is_dir(&path)?;
                let mtime_ns = self.wall_ns();
                self.files.insert(path, Node::Dir { mode: mode & 0o7777 & !0o022, mtime_ns });
                Event::Done
            }
            Op::UnlinkAt { dir, path, flags } => {
                let path = self.resolve(dir, &path)?;
                let wants_dir = flags & libc::AT_REMOVEDIR != 0;
                match self.files.get(&path) {
                    None => return Err(errno(libc::ENOENT)),
                    Some(Node::Dir { .. }) if !wants_dir => return Err(errno(libc::EISDIR)),
                    Some(Node::Dir { .. }) => {
                        if self.under(&path).len() > 1 {
                            return Err(errno(libc::ENOTEMPTY));
                        }
                    }
                    Some(_) if wants_dir => return Err(errno(libc::ENOTDIR)),
                    Some(_) => {}
                }
                self.remove_node(&path);
                Event::Done
            }
            Op::RenameAt { old_dir, old, new_dir, new } => {
                let (from, to) = (self.resolve(old_dir, &old)?, self.resolve(new_dir, &new)?);
                self.rename(from, to)?;
                Event::Renamed
            }
            Op::SymlinkAt { target, new_dir, new } => {
                let path = self.resolve(new_dir, &new)?;
                if self.files.contains_key(&path) {
                    return Err(errno(libc::EEXIST));
                }
                self.parent_is_dir(&path)?;
                self.files.insert(path, Node::Link(path_of(&target)));
                Event::Done
            }
            Op::LinkAt { old_dir, old, new_dir, new } => {
                let (from, to) = (self.resolve(old_dir, &old)?, self.resolve(new_dir, &new)?);
                let Some(Node::File(ino)) = self.files.get(&from) else {
                    return Err(errno(if self.files.contains_key(&from) { libc::EPERM } else { libc::ENOENT }));
                };
                let ino = *ino;
                if self.files.contains_key(&to) {
                    return Err(errno(libc::EEXIST));
                }
                self.parent_is_dir(&to)?;
                self.inodes.get_mut(&ino).expect("a named file has its inode").links += 1;
                self.files.insert(to, Node::File(ino));
                Event::Done
            }
            Op::Accept { .. } | Op::Listen { .. } => return Err(unsupported("stream sockets")),
            Op::BindUnix { .. } | Op::ConnectUnix { .. } => return Err(unsupported("Unix sockets")),
            Op::SendFds { .. } | Op::RecvFds { .. } => return Err(unsupported("descriptor passing")),
            Op::WaitPid { .. } => return Err(unsupported("processes")),
            Op::Readable { .. } => return Err(unsupported("readiness polling")),
            Op::Fuse { .. } => return Err(unsupported("FUSE")),
        }))
    }
}

fn datagram_event(from: SocketAddr, mut bytes: Vec<u8>, len: usize, with_addr: bool) -> Event {
    bytes.truncate(len);
    let n = bytes.len();
    if with_addr { Event::RecvFrom { n, buf: bytes, addr: Some(from) } } else { Event::Recv { n, buf: bytes } }
}

impl SimLoop {
    pub(super) fn answers_in_place(&self) -> bool {
        self.in_place
    }

    /// Answers an operation that needs no waiting at once. One that must wait is held, and its token comes back.
    pub(super) fn answer_in_place(&mut self, op: Op) -> Result<Event, u64> {
        let token = self.next_token;
        self.next_token += 1;
        self.report.ops += 1;
        match self.answer(token, op) {
            Ok(Some(event)) => Ok(event),
            Ok(None) => Err(token),
            Err(err) => Ok(Event::Failed(err)),
        }
    }

    pub(super) fn submit(&mut self, op: Op) -> io::Result<u64> {
        let token = self.next_token;
        self.next_token += 1;
        self.report.ops += 1;
        match self.answer(token, op) {
            Ok(Some(event)) => self.done(token, event),
            Ok(None) => {}
            Err(err) => self.fail(token, err),
        }
        Ok(token)
    }

    /// The next completion, with the clock moved to when it happens. With
    /// nothing left to happen, every fiber is parked for good: a deadlock.
    pub(super) fn wait(&mut self) -> io::Result<Vec<(u64, Event)>> {
        loop {
            let Some(((at, _), due)) = self.due.pop_first() else {
                panic!("deadlock at {} ms of simulated time: fibers are parked and nothing is pending", self.now_ms);
            };
            self.now_ms = at;
            let (token, event) = match due {
                Due::Complete(token, event) => (token, event),
                Due::Deliver { to, from, bytes } => {
                    self.mix(&bytes);
                    match self.deliver(to, from, bytes) {
                        Some(done) => done,
                        None => continue,
                    }
                }
            };
            self.mix(&token.to_le_bytes());
            self.mix(&at.to_le_bytes());
            return Ok(vec![(token, event)]);
        }
    }

    pub(super) fn cancel(&mut self, token: u64) {
        self.due.retain(|_, due| !matches!(due, Due::Complete(t, _) if *t == token));
        for handle in self.handles.values_mut() {
            if let Handle::Sock(sock) = handle {
                if sock.waiter.is_some_and(|(t, _, _)| t == token) {
                    sock.waiter = None;
                }
            }
        }
        for pipe in self.pipes.values_mut() {
            if pipe.waiter.is_some_and(|(t, _)| t == token) {
                pipe.waiter = None;
            }
        }
    }
}

/// Runs `f` on the simulated backend of the loop this thread is driving, if it is driving one.
fn with<R>(f: impl FnOnce(&mut SimLoop) -> R) -> Option<R> {
    match fiber::backend()? {
        Backend::Sim(sim) => Some(f(sim)),
        Backend::Real(_) => None,
    }
}

/// Whether this thread's loop is simulated.
pub fn active() -> bool {
    with(|_| ()).is_some()
}

/// The time. In a simulation it moves only when the loop has nothing to run.
pub fn now() -> Instant {
    with(|sim| sim.base + Duration::from_millis(sim.now_ms)).unwrap_or_else(Instant::now)
}

/// Random bytes: the kernel's, or in a simulation the seed's.
pub fn random(out: &mut [u8]) {
    let seeded = with(|sim| {
        for chunk in out.chunks_mut(8) {
            let word = sim.rng.next().to_le_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
    });
    if seeded.is_none() {
        let n = unsafe { libc::getrandom(out.as_mut_ptr().cast(), out.len(), 0) };
        assert_eq!(n, out.len() as isize, "the kernel fills a short random buffer in one call");
    }
}

pub(super) fn local_addr(fd: i32) -> Option<io::Result<SocketAddr>> {
    with(|sim| sim.sock(fd).and_then(|sock| sock.local.ok_or_else(|| errno(libc::EINVAL))))
}

pub(super) fn try_recv_from(fd: i32, len: usize) -> Option<io::Result<(Vec<u8>, SocketAddr)>> {
    with(|sim| {
        let sock = sim.sock(fd)?;
        let (from, mut bytes) = sock.queue.pop_front().ok_or_else(|| errno(libc::EAGAIN))?;
        bytes.truncate(len);
        Ok((bytes, from))
    })
}

/// Closes a simulated descriptor from a destructor, which cannot park.
pub(super) fn close_now(fd: i32) -> bool {
    if fd < FIRST_FD {
        return false;
    }
    with(|sim| match sim.handles.remove(&fd) {
        Some(Handle::Sock(sock)) => {
            if let Some(local) = sock.local {
                sim.bound.remove(&local);
            }
            if let Some((waiter, _, _)) = sock.waiter {
                sim.fail(waiter, errno(libc::EBADF));
            }
        }
        Some(Handle::File { ino, .. }) => {
            sim.locks.retain(|_, holder| *holder != fd);
            sim.reap(ino)
        }
        Some(Handle::Pipe { id, .. }) => sim.pipe_closed(id),
        Some(Handle::Dir { .. }) | None => {}
    })
    .is_some()
}

/// A pipe held in memory: its reading end and its writing end.
pub(super) fn pipe() -> Option<(i32, i32)> {
    with(|sim| {
        let id = sim.next_ino;
        sim.next_ino += 1;
        sim.pipes.insert(id, Pipe::default());
        (sim.fd(Handle::Pipe { id, writes: false }), sim.fd(Handle::Pipe { id, writes: true }))
    })
}

/// A second reading end of the pipe `fd` reads.
pub(super) fn pipe_reader(fd: i32) -> Option<io::Result<i32>> {
    if fd < FIRST_FD {
        return None;
    }
    with(|sim| match sim.pipe_end(fd) {
        Some((id, _)) => Ok(sim.fd(Handle::Pipe { id, writes: false })),
        None => Err(errno(libc::EBADF)),
    })
}

/// What a read of a simulated pipe gets without waiting: nothing when the pipe is empty.
pub(super) fn pipe_try_read(fd: i32, len: usize) -> Option<io::Result<Option<Vec<u8>>>> {
    if fd < FIRST_FD {
        return None;
    }
    with(|sim| match sim.pipe_end(fd) {
        Some((id, false)) => Ok(sim.pipe_take(id, len)),
        _ => Err(errno(libc::EBADF)),
    })
}

/// Writes to a simulated pipe, which is never full.
pub(super) fn pipe_write(fd: i32, bytes: &[u8]) -> Option<io::Result<usize>> {
    if fd < FIRST_FD {
        return None;
    }
    with(|sim| match sim.pipe_end(fd) {
        Some((id, true)) => {
            sim.pipe_put(id, bytes);
            Ok(bytes.len())
        }
        _ => Err(errno(libc::EBADF)),
    })
}

/// How many bytes wait in a simulated pipe.
pub(super) fn pipe_queued(fd: i32) -> Option<io::Result<usize>> {
    if fd < FIRST_FD {
        return None;
    }
    with(|sim| match sim.pipe_end(fd) {
        Some((id, _)) => Ok(sim.pipes[&id].data.len()),
        None => Err(errno(libc::EBADF)),
    })
}

/// The wall clock. In a simulation it starts at a fixed date and moves with the simulated clock.
pub fn system_now() -> std::time::SystemTime {
    with(|sim| std::time::UNIX_EPOCH + Duration::from_nanos(sim.wall_ns() as u64)).unwrap_or_else(std::time::SystemTime::now)
}

/// The names in a simulated directory with whether each is a directory, a file or a link (`'d'`, `'f'`, `'l'`).
pub(super) fn read_dir(path: &Path) -> Option<io::Result<Vec<(std::ffi::OsString, char)>>> {
    with(|sim| {
        let dir = sim.real(path)?;
        match sim.files.get(&dir) {
            Some(Node::Dir { .. }) => {}
            Some(_) => return Err(errno(libc::ENOTDIR)),
            None => return Err(errno(libc::ENOENT)),
        }
        Ok(sim
            .under(&dir)
            .into_iter()
            .filter(|p| p.parent() == Some(dir.as_path()))
            .map(|p| {
                let kind = match sim.files[&p] {
                    Node::Dir { .. } => 'd',
                    Node::File(_) => 'f',
                    Node::Link(_) => 'l',
                };
                (p.file_name().expect("a child has a name").to_os_string(), kind)
            })
            .collect())
    })
}

pub(super) fn remove_dir_all(path: &Path) -> Option<io::Result<()>> {
    with(|sim| {
        let dir = normal(path);
        match sim.files.get(&dir) {
            Some(Node::Dir { .. }) => {}
            Some(_) => return Err(errno(libc::ENOTDIR)),
            None => return Err(errno(libc::ENOENT)),
        }
        for p in sim.under(&dir) {
            sim.remove_node(&p);
        }
        Ok(())
    })
}

pub(super) fn set_mode(path: &Path, mode: u32) -> Option<io::Result<()>> {
    with(|sim| {
        let path = sim.real(path)?;
        match sim.files.get_mut(&path) {
            Some(Node::Dir { mode: held, .. }) => *held = mode & 0o7777,
            Some(Node::File(ino)) => {
                let ino = *ino;
                sim.inodes.get_mut(&ino).expect("a named file has its inode").mode = mode & 0o7777;
            }
            Some(Node::Link(_)) | None => return Err(errno(libc::ENOENT)),
        }
        Ok(())
    })
}

pub(super) fn set_fd_mode(fd: i32, mode: u32) -> Option<io::Result<()>> {
    if fd < FIRST_FD {
        return None;
    }
    with(|sim| sim.file(fd).map(|inode| inode.mode = mode & 0o7777))
}

/// Sets a simulated file's modification time through its descriptor; `None` leaves it alone.
pub(super) fn set_fd_modified(fd: i32, mtime_ns: Option<i64>) -> Option<io::Result<()>> {
    if fd < FIRST_FD {
        return None;
    }
    with(|sim| {
        sim.file(fd).map(|inode| {
            if let Some(ns) = mtime_ns {
                inode.mtime_ns = ns;
            }
        })
    })
}

/// `fallocate` on a simulated file: it grows to cover the range unless told to keep its size, and a punched range reads as zeros.
pub(super) fn allocate(fd: i32, mode: i32, offset: u64, len: u64) -> Option<io::Result<()>> {
    if fd < FIRST_FD {
        return None;
    }
    with(|sim| {
        let inode = sim.file(fd)?;
        let end = (offset + len) as usize;
        if mode & !(libc::FALLOC_FL_KEEP_SIZE | libc::FALLOC_FL_PUNCH_HOLE) != 0 {
            return Err(errno(libc::EOPNOTSUPP));
        }
        if mode & libc::FALLOC_FL_PUNCH_HOLE != 0 {
            let end = end.min(inode.data.len());
            let start = (offset as usize).min(end);
            inode.data[start..end].fill(0);
        } else if mode & libc::FALLOC_FL_KEEP_SIZE == 0 && inode.data.len() < end {
            inode.data.resize(end, 0);
        }
        Ok(())
    })
}

pub(super) fn set_modified(path: &Path, at: std::time::SystemTime) -> Option<io::Result<()>> {
    with(|sim| {
        let path = sim.real(path)?;
        let Some(Node::File(ino)) = sim.files.get(&path) else { return Err(errno(libc::ENOENT)) };
        let ns = at.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as i64).unwrap_or(0);
        sim.inodes.get_mut(ino).expect("a named file has its inode").mtime_ns = ns;
        Ok(())
    })
}

pub(super) fn read_link(path: &Path) -> Option<io::Result<PathBuf>> {
    with(|sim| match sim.files.get(&normal(path)) {
        Some(Node::Link(target)) => Ok(target.clone()),
        Some(_) => Err(errno(libc::EINVAL)),
        None => Err(errno(libc::ENOENT)),
    })
}

pub(super) fn canonicalize(path: &Path) -> Option<io::Result<PathBuf>> {
    with(|sim| {
        let real = sim.real(path)?;
        if sim.files.contains_key(&real) { Ok(real) } else { Err(errno(libc::ENOENT)) }
    })
}

/// Whether `fd` is a descriptor of this thread's simulation.
pub(super) fn owns(fd: i32) -> bool {
    fd >= FIRST_FD && active()
}

/// The first `len` bytes of a simulated open file, which is what mapping it shows.
pub(super) fn contents(fd: i32, len: usize) -> Option<io::Result<Vec<u8>>> {
    if fd < FIRST_FD {
        return None;
    }
    with(|sim| {
        let data = &sim.file(fd)?.data;
        if len == 0 || len > data.len() {
            return Err(errno(libc::EINVAL));
        }
        Ok(data[..len].to_vec())
    })
}

/// Takes the exclusive lock on a simulated open file for `fd`, or says that another descriptor holds it.
pub(super) fn try_lock(fd: i32) -> Option<io::Result<bool>> {
    if fd < FIRST_FD {
        return None;
    }
    with(|sim| {
        let ino = match sim.handles.get(&fd) {
            Some(Handle::File { ino, .. }) => *ino,
            Some(_) => return Err(errno(libc::EINVAL)),
            None => return Err(errno(libc::EBADF)),
        };
        let holder = *sim.locks.entry(ino).or_insert(fd);
        Ok(holder == fd)
    })
}

/// Changes who owns a simulated path. Every path belongs to the simulated
/// user, who is not root, so only a change to that same owner is allowed.
pub(super) fn lchown(path: &Path, uid: u32, gid: u32) -> Option<io::Result<()>> {
    with(|sim| {
        if !sim.files.contains_key(&normal(path)) {
            return Err(errno(libc::ENOENT));
        }
        if uid == UID && gid == UID { Ok(()) } else { Err(errno(libc::EPERM)) }
    })
}

/// A second descriptor for the same open file.
pub(super) fn dup(fd: i32) -> Option<io::Result<i32>> {
    if fd < FIRST_FD {
        return None;
    }
    with(|sim| match sim.handles.get(&fd) {
        Some(Handle::File { ino, path }) => {
            let handle = Handle::File { ino: *ino, path: path.clone() };
            Ok(sim.fd(handle))
        }
        Some(Handle::Dir { path }) => {
            let handle = Handle::Dir { path: path.clone() };
            Ok(sim.fd(handle))
        }
        Some(Handle::Sock(_)) => Err(unsupported("duplicating a socket")),
        Some(Handle::Pipe { id, writes }) => {
            let handle = Handle::Pipe { id: *id, writes: *writes };
            Ok(sim.fd(handle))
        }
        None => Err(errno(libc::EBADF)),
    })
}

pub(super) fn env(name: &std::ffi::OsStr) -> Option<Option<std::ffi::OsString>> {
    with(|sim| sim.env.get(name).cloned())
}

pub(super) fn envs() -> Option<Vec<(std::ffi::OsString, std::ffi::OsString)>> {
    with(|sim| sim.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
}

/// Sets a variable in the simulation's environment, which starts empty.
pub fn set_env(name: impl Into<std::ffi::OsString>, value: impl Into<std::ffi::OsString>) {
    with(|sim| sim.env.insert(name.into(), value.into())).expect("a simulation is running");
}

/// Says what commands do in this simulation. Without it, running a command fails with `Unsupported`.
pub fn on_command(run: impl FnMut(&Spawned) -> io::Result<Ran> + 'static) {
    with(|sim| sim.commands = Some(Box::new(run))).expect("a simulation is running");
}

pub(super) fn command(spawned: &Spawned) -> Option<io::Result<Ran>> {
    let mut run = with(|sim| sim.commands.take())?;
    let Some(run_it) = run.as_mut() else { return Some(Err(unsupported("commands, unless the test says what they do with sim::on_command"))) };
    let ran = run_it(spawned);
    with(|sim| {
        if sim.commands.is_none() {
            sim.commands = run;
        }
    });
    Some(ran)
}

pub(super) fn pid() -> Option<u32> {
    with(|_| PID)
}

pub(super) fn alive(pid: u32) -> Option<bool> {
    with(|_| pid == PID || pid == 1)
}

pub(super) fn uid() -> Option<u32> {
    with(|_| UID)
}

/// Runs `body` in a simulation with seed 0 and a clean network: the way a
/// unit test gets files, a clock and an environment that live in memory.
///
/// An operation that needs no waiting (a file read, a write, a stat) is
/// answered where it is asked, so a fiber gives way to others only when it
/// sleeps, waits on a socket or pipe, or yields. That makes a test of one
/// fiber several times cheaper. A test of how fibers interleave at every
/// operation uses `run`, where each operation is a point at which another
/// fiber may go first, as on the real loop.
pub fn test<R: 'static>(body: impl FnOnce() -> R + 'static) -> R {
    let mut sim = SimLoop::new(0, Net::default());
    sim.in_place = true;
    run_on(sim, body).0
}

/// A fresh empty directory in the simulated filesystem.
pub fn tempdir() -> PathBuf {
    let n = with(|sim| {
        sim.next_ino += 1;
        sim.next_ino
    })
    .expect("a simulation is running");
    let dir = PathBuf::from(format!("/tmp/sim-{n}"));
    with(|sim| {
        let mtime_ns = sim.wall_ns();
        sim.files.entry(PathBuf::from("/tmp")).or_insert(Node::Dir { mode: 0o777, mtime_ns });
        sim.files.insert(dir.clone(), Node::Dir { mode: 0o755, mtime_ns });
    });
    dir
}

/// Runs `main` as the first fiber of a simulated loop and returns its result
/// with what the run looked like.
pub fn run<R: 'static>(seed: u64, net: Net, main: impl FnOnce() -> R + 'static) -> (R, Report) {
    run_on(SimLoop::new(seed, net), main)
}

fn run_on<R: 'static>(sim: SimLoop, main: impl FnOnce() -> R + 'static) -> (R, Report) {
    let out = std::rc::Rc::new(std::cell::RefCell::new(None));
    let slot = std::rc::Rc::clone(&out);
    let report = fiber::drive_sim(sim, move || {
        *slot.borrow_mut() = Some(main());
        fiber::rest_in_background();
    });
    let value = out.borrow_mut().take().expect("the loop returns after main has finished");
    (value, report)
}
