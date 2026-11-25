use parking_lot::{Condvar, Mutex};
use std::collections::BinaryHeap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Arc;
use tokio::sync::oneshot;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    Idle = 0,
    Background = 1,
    Agent = 2,
    Human = 3,
}

struct Waiter {
    prio: Priority,
    seq: u64,
    mem_kb: u64,
    tx: oneshot::Sender<Permit>,
}

impl PartialEq for Waiter {
    fn eq(&self, o: &Self) -> bool {
        self.prio == o.prio && self.seq == o.seq
    }
}
impl Eq for Waiter {}
impl PartialOrd for Waiter {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Waiter {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        self.prio.cmp(&o.prio).then(o.seq.cmp(&self.seq))
    }
}

struct Inner {
    running: usize,
    mem_kb: u64,
    waiters: BinaryHeap<Waiter>,
    seq: u64,
    spare_tokens: usize,
    shutdown: bool,
    peak_running: usize,
    granted: u64,
}

pub struct Scheduler {
    inner: Mutex<Inner>,
    wake_broker: Condvar,
    tokens: usize,
    mem_budget_kb: u64,
    pipe_r: OwnedFd,
    pipe_w: OwnedFd,
    pipe_try: OwnedFd,
}

const IDLE_DWELL: std::time::Duration = std::time::Duration::from_millis(40);

pub struct Permit {
    sched: Arc<Scheduler>,
    mem_kb: u64,
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.sched.release(self.mem_kb);
    }
}

fn write_token(fd: RawFd) {
    loop {
        let r = unsafe { libc::write(fd, b"|".as_ptr() as *const _, 1) };
        if r == 1 || std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return;
        }
    }
}

impl Scheduler {
    pub fn new(tokens: usize, mem_budget_kb: u64) -> std::io::Result<Arc<Scheduler>> {
        let mut fds = [0; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let (pipe_r, pipe_w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        for _ in 0..tokens {
            write_token(pipe_w.as_raw_fd());
        }
        let pipe_try: OwnedFd = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(format!("/proc/self/fd/{}", pipe_r.as_raw_fd()))?
            .into();
        let s = Arc::new(Scheduler {
            inner: Mutex::new(Inner {
                running: 0,
                mem_kb: 0,
                waiters: BinaryHeap::new(),
                seq: 0,
                spare_tokens: 0,
                shutdown: false,
                peak_running: 0,
                granted: 0,
            }),
            wake_broker: Condvar::new(),
            tokens,
            mem_budget_kb,
            pipe_r,
            pipe_w,
            pipe_try,
        });
        let weak = Arc::downgrade(&s);
        std::thread::Builder::new().name("jobserver".into()).spawn(move || broker(weak))?;
        Ok(s)
    }

    pub fn tokens(&self) -> usize {
        self.tokens
    }

    pub fn jobserver_fds(&self) -> [RawFd; 2] {
        [self.pipe_r.as_raw_fd(), self.pipe_w.as_raw_fd()]
    }

    pub fn snapshot(&self) -> (usize, usize, usize, u64) {
        let i = self.inner.lock();
        let waiting = i.waiters.iter().filter(|w| w.prio != Priority::Idle && !w.tx.is_closed()).count();
        (i.running, waiting, i.peak_running, i.granted)
    }

    pub fn free_tokens(&self) -> usize {
        let spare = self.inner.lock().spare_tokens;
        let mut avail: libc::c_int = 0;
        if unsafe { libc::ioctl(self.pipe_r.as_raw_fd(), libc::FIONREAD, &mut avail) } != 0 {
            return spare;
        }
        spare + avail.max(0) as usize
    }

    pub async fn acquire(self: &Arc<Self>, prio: Priority, mem_kb: u64) -> Permit {
        let rx = {
            let mut i = self.inner.lock();
            let (tx, rx) = oneshot::channel();
            i.seq += 1;
            let seq = i.seq;
            assert!(i.waiters.iter().all(|w| w.seq < seq), "sequence numbers ascend");
            i.waiters.push(Waiter { prio, seq, mem_kb, tx });
            self.dispatch(&mut i, false);
            self.wake_broker.notify_one();
            rx
        };
        match rx.await {
            Ok(p) => p,
            Err(_) => std::future::pending().await,
        }
    }

    fn fits(&self, i: &Inner, mem_kb: u64) -> bool {
        i.running == 0 || i.mem_kb + mem_kb <= self.mem_budget_kb
    }

    fn dispatch(self: &Arc<Self>, i: &mut Inner, idle_too: bool) {
        let mut skipped: Vec<Waiter> = Vec::new();
        while i.spare_tokens > 0 {
            let Some(w) = i.waiters.pop() else { break };
            if w.tx.is_closed() {
                continue;
            }
            if !self.fits(i, w.mem_kb) || (w.prio == Priority::Idle && !idle_too) {
                skipped.push(w);
                continue;
            }
            assert!(self.fits(i, w.mem_kb));
            i.spare_tokens -= 1;
            i.running += 1;
            i.mem_kb += w.mem_kb;
            i.peak_running = i.peak_running.max(i.running);
            i.granted += 1;
            let permit = Permit { sched: self.clone(), mem_kb: w.mem_kb };
            if let Err(p) = w.tx.send(permit) {
                std::mem::forget(p);
                assert!(i.running > 0);
                assert!(i.mem_kb >= w.mem_kb);
                i.running -= 1;
                i.mem_kb -= w.mem_kb;
                i.spare_tokens += 1;
            }
        }
        for w in skipped {
            i.waiters.push(w);
        }
    }

    fn release(self: &Arc<Self>, mem_kb: u64) {
        let mut i = self.inner.lock();
        assert!(i.running > 0, "release without a running action");
        assert!(i.mem_kb >= mem_kb, "memory released that was never granted");
        i.running -= 1;
        i.mem_kb -= mem_kb;
        if i.running == 0 {
            assert_eq!(i.mem_kb, 0, "memory held with nothing running");
        }
        i.spare_tokens += 1;
        self.dispatch(&mut i, false);
        self.return_spares(&mut i);
        self.wake_broker.notify_one();
        if i.running == 0 {
            self.reconcile(&mut i);
        }
    }

    pub fn restore(self: &Arc<Self>, tokens: usize) {
        let mut i = self.inner.lock();
        i.spare_tokens += tokens;
        self.dispatch(&mut i, false);
        self.return_spares(&mut i);
    }

    fn wanted(&self, i: &Inner) -> usize {
        i.waiters.iter().filter(|w| w.prio != Priority::Idle && !w.tx.is_closed()).count()
    }

    fn idle_wanted(&self, i: &Inner) -> bool {
        i.waiters.iter().any(|w| w.prio == Priority::Idle && !w.tx.is_closed())
    }

    fn in_pipe(&self) -> usize {
        let mut avail: libc::c_int = 0;
        if unsafe { libc::ioctl(self.pipe_r.as_raw_fd(), libc::FIONREAD, &mut avail) } != 0 {
            return 0;
        }
        avail.max(0) as usize
    }

    fn try_take(&self) -> bool {
        let mut b = 0u8;
        unsafe { libc::read(self.pipe_try.as_raw_fd(), &mut b as *mut u8 as *mut _, 1) == 1 }
    }

    fn return_spares(&self, i: &mut Inner) {
        let want = self.wanted(i).min(1);
        while i.spare_tokens > want {
            i.spare_tokens -= 1;
            write_token(self.pipe_w.as_raw_fd());
        }
        assert!(i.spare_tokens <= 1);
    }

    fn reconcile(&self, i: &mut Inner) {
        assert_eq!(i.running, 0);
        let have = self.in_pipe() + i.spare_tokens;
        for _ in have..self.tokens {
            write_token(self.pipe_w.as_raw_fd());
        }
        for _ in self.tokens..have {
            if !self.try_take() {
                break;
            }
        }
    }

    pub fn shutdown(&self) {
        self.inner.lock().shutdown = true;
        self.wake_broker.notify_all();
        write_token(self.pipe_w.as_raw_fd());
    }
}

fn broker(weak: std::sync::Weak<Scheduler>) {
    loop {
        let Some(s) = weak.upgrade() else { return };
        {
            let mut i = s.inner.lock();
            let mut lay_unclaimed = false;
            loop {
                if i.shutdown {
                    return;
                }
                if s.wanted(&i) > i.spare_tokens {
                    break;
                }
                if s.idle_wanted(&i) {
                    if lay_unclaimed && s.in_pipe() > 0 && s.try_take() {
                        i.spare_tokens += 1;
                        s.dispatch(&mut i, true);
                        s.return_spares(&mut i);
                        lay_unclaimed = false;
                        continue;
                    }
                    lay_unclaimed = s.in_pipe() > 0;
                    s.wake_broker.wait_for(&mut i, IDLE_DWELL);
                } else {
                    lay_unclaimed = false;
                    s.wake_broker.wait_for(&mut i, std::time::Duration::from_secs(1));
                }
                if Arc::strong_count(&s) == 1 {
                    return;
                }
            }
        }
        let fd = s.pipe_r.as_raw_fd();
        let mut b = 0u8;
        let r = unsafe { libc::read(fd, &mut b as *mut u8 as *mut _, 1) };
        if r != 1 {
            continue;
        }
        let mut i = s.inner.lock();
        if i.shutdown {
            return;
        }
        i.spare_tokens += 1;
        s.dispatch(&mut i, false);
        s.return_spares(&mut i);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap()
    }

    #[test]
    fn idle_requests_come_after_helper_threads() {
        let mut last = None;
        for _ in 0..5 {
            match std::panic::catch_unwind(helper_before_idle) {
                Ok(()) => return,
                Err(e) => last = Some(e),
            }
        }
        std::panic::resume_unwind(last.unwrap());
    }

    fn helper_before_idle() {
        let s = Scheduler::new(1, u64::MAX).unwrap();
        let [r, w] = s.jobserver_fds();
        rt().block_on(async {
            let running = s.acquire(Priority::Agent, 0).await;
            let s2 = s.clone();
            let idle = tokio::spawn(async move { s2.acquire(Priority::Idle, 0).await });
            let (got_tx, got_rx) = std::sync::mpsc::channel();
            let (back_tx, back_rx) = std::sync::mpsc::channel::<()>();
            let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
            let helper = std::thread::spawn(move || {
                started_tx.send(()).unwrap();
                let mut b = 0u8;
                assert_eq!(unsafe { libc::read(r, &mut b as *mut u8 as *mut _, 1) }, 1);
                got_tx.send(()).unwrap();
                back_rx.recv().unwrap();
                write_token(w);
            });
            tokio::task::spawn_blocking(move || started_rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap()).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            drop(running);
            tokio::task::spawn_blocking(move || got_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap()).await.unwrap();
            tokio::time::sleep(IDLE_DWELL * 5).await;
            assert!(!idle.is_finished());
            assert_eq!(s.snapshot().1, 0, "an idle request is not waiting work");
            back_tx.send(()).unwrap();
            let p = tokio::time::timeout(std::time::Duration::from_secs(5), idle).await.unwrap().unwrap();
            assert_eq!(s.snapshot().0, 1);
            drop(p);
            tokio::task::spawn_blocking(move || helper.join().unwrap()).await.unwrap();
        });
    }

    #[test]
    fn limits_concurrency_to_tokens() {
        let s = Scheduler::new(3, u64::MAX).unwrap();
        rt().block_on(async {
            let mut permits = Vec::new();
            for _ in 0..3 {
                permits.push(s.acquire(Priority::Agent, 0).await);
            }
            assert_eq!(s.snapshot().0, 3);
            let s2 = s.clone();
            let fourth = tokio::spawn(async move { s2.acquire(Priority::Agent, 0).await });
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert!(!fourth.is_finished());
            permits.pop();
            let p = tokio::time::timeout(std::time::Duration::from_secs(5), fourth).await.unwrap().unwrap();
            assert_eq!(s.snapshot().0, 3);
            drop(p);
            drop(permits);
            assert_eq!(s.snapshot().0, 0);
        });
    }

    #[test]
    fn humans_go_first() {
        let s = Scheduler::new(1, u64::MAX).unwrap();
        rt().block_on(async {
            let first = s.acquire(Priority::Agent, 0).await;
            let order = Arc::new(Mutex::new(Vec::new()));
            let mut tasks = Vec::new();
            for (name, prio) in [("agent1", Priority::Agent), ("agent2", Priority::Agent), ("human", Priority::Human)] {
                let s2 = s.clone();
                let o = order.clone();
                tasks.push(tokio::spawn(async move {
                    let p = s2.acquire(prio, 0).await;
                    o.lock().push(name);
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    drop(p);
                }));
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            drop(first);
            for t in tasks {
                t.await.unwrap();
            }
            assert_eq!(*order.lock(), vec!["human", "agent1", "agent2"]);
        });
    }

    #[test]
    fn memory_admission_holds_back_big_actions() {
        let s = Scheduler::new(8, 1000).unwrap();
        rt().block_on(async {
            let a = s.acquire(Priority::Agent, 600).await;
            let s2 = s.clone();
            let big = tokio::spawn(async move { s2.acquire(Priority::Agent, 600).await });
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert!(!big.is_finished(), "600 + 600 exceeds the 1000 budget");
            let small = tokio::time::timeout(std::time::Duration::from_secs(5), s.acquire(Priority::Agent, 300))
                .await
                .unwrap();
            drop(a);
            drop(small);
            let _p = tokio::time::timeout(std::time::Duration::from_secs(5), big).await.unwrap().unwrap();
        });
    }

    #[test]
    fn tokens_are_shared_with_jobserver_clients() {
        let s = Scheduler::new(2, u64::MAX).unwrap();
        rt().block_on(async {
            assert_eq!(s.free_tokens(), 2);
            let [r, w] = s.jobserver_fds();
            let mut b = 0u8;
            assert_eq!(unsafe { libc::read(r, &mut b as *mut u8 as *mut _, 1) }, 1);
            assert_eq!(s.free_tokens(), 1);
            let _a = s.acquire(Priority::Agent, 0).await;
            assert_eq!(s.free_tokens(), 0);
            let s2 = s.clone();
            let second = tokio::spawn(async move { s2.acquire(Priority::Agent, 0).await });
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert!(!second.is_finished());
            write_token(w);
            tokio::time::timeout(std::time::Duration::from_secs(5), second).await.unwrap().unwrap();
        });
    }
}
