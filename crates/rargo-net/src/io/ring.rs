use std::collections::{BTreeMap, VecDeque};
use std::ffi::CString;
use std::io::{self, ErrorKind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex as StdMutex};
use io_uring::{opcode, types, IoUring};

use super::ops::{prepare, event_of, Event, Op};
use super::pool::{Job, Jobs, Pool, worker};

pub(super) const EVENTFD: u64 = 0;
pub(super) const CANCEL: u64 = u64::MAX;
pub(super) const RING_ENTRIES: u32 = 4096;

#[derive(Clone, Copy)]
pub(super) enum Kind {
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
    Path,
    Fsync,
    Close,
    Timeout,
    WaitPid,
    SendFds,
    RecvFds,
    Readable,
}

pub(super) struct Slot {
    pub(super) kind: Kind,
    pub(super) buf: Option<Vec<u8>>,
    pub(super) stat: Option<Box<libc::statx>>,
    pub(super) addr: Option<Box<libc::sockaddr_storage>>,
    pub(super) addrlen: Option<Box<libc::socklen_t>>,
    pub(super) ts: Option<Box<types::Timespec>>,
    pub(super) info: Option<Box<libc::siginfo_t>>,
    pub(super) path: Option<CString>,
    pub(super) path2: Option<CString>,
    pub(super) iov: Option<Box<libc::iovec>>,
    pub(super) msg: Option<Box<libc::msghdr>>,
    pub(super) cbuf: Option<Vec<u8>>,
}

pub struct Loop {
    pub(super) ring: IoUring,
    pub(super) next: u64,
    pub(super) inflight: BTreeMap<u64, Slot>,
    pub(super) pool: Pool,
    pub(super) blocking: Pool,
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
        let blocking = Pool {
            jobs: Arc::new(Jobs { q: StdMutex::new(VecDeque::new()), cv: Condvar::new(), closed: AtomicBool::new(false) }),
            done: Arc::clone(&done),
            joins: Vec::new(),
            efd,
        };
        let mut this = Self {
            ring,
            next: 1,
            inflight: BTreeMap::new(),
            pool: Pool { jobs, done, joins, efd },
            blocking,
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
        let (pool, run) = match op {
            Op::Cpu(run) => (&mut self.pool, run),
            Op::Blocking(run) => {
                if self.blocking.joins.is_empty() {
                    let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8).clamp(8, 32);
                    for _ in 0..n {
                        let (jobs, done, efd) = (Arc::clone(&self.blocking.jobs), Arc::clone(&self.blocking.done), self.blocking.efd);
                        self.blocking.joins.push(std::thread::Builder::new().name("rargo-fs".into()).spawn(move || worker(jobs, done, efd))?);
                    }
                }
                (&mut self.blocking, run)
            }
            op => {
                self.enqueue(token, op)?;
                return Ok(token);
            }
        };
        if pool.jobs.closed.load(Ordering::Relaxed) {
            return Err(io::Error::other("worker pool stopped"));
        }
        pool.jobs.q.lock().unwrap_or_else(|e| e.into_inner()).push_back(Job { token, run });
        pool.jobs.cv.notify_one();
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
        q.drain(..).map(|token| (token, Event::Cpu)).collect()
    }
}

impl Drop for Loop {
    fn drop(&mut self) {
        for pool in [&mut self.pool, &mut self.blocking] {
            pool.jobs.closed.store(true, Ordering::Relaxed);
            pool.jobs.cv.notify_all();
            for join in pool.joins.drain(..) {
                let _ = join.join();
            }
        }
        if self.pool.efd >= 0 {
            unsafe { libc::close(self.pool.efd) };
        }
    }
}

impl Slot {
    pub(super) fn empty(kind: Kind) -> Self {
        Self { kind, buf: None, stat: None, addr: None, addrlen: None, ts: None, info: None, path: None, path2: None, iov: None, msg: None, cbuf: None }
    }
    pub(super) fn buf(kind: Kind, buf: Vec<u8>) -> Self {
        Self { buf: Some(buf), ..Self::empty(kind) }
    }
}

