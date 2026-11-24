//! The machine-wide scheduler.
//!
//! One pool of tokens represents the machine's cores. It is a real jobserver
//! pipe: every running action holds one token, and rustc's own worker
//! threads take further tokens from the same pipe, so "how many things are
//! computing right now" has a single answer. Requests are admitted in
//! priority order, and only if their expected memory fits.

use parking_lot::{Condvar, Mutex};
use std::collections::BinaryHeap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Arc;
use tokio::sync::oneshot;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    /// Only a core nothing else wants: a local copy of an action already
    /// running on a worker. Not counted as waiting work, and never served
    /// ahead of a compiler's helper threads: it gets a token only once one
    /// has lain unclaimed in the jobserver pipe.
    Idle = 0,
    /// Background work (drift checks, pre-warming).
    Background = 1,
    /// A coding agent.
    Agent = 2,
    /// A person at a terminal.
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
        // Higher priority first, then first come first served.
        self.prio.cmp(&o.prio).then(o.seq.cmp(&self.seq))
    }
}

struct Inner {
    running: usize,
    mem_kb: u64,
    waiters: BinaryHeap<Waiter>,
    seq: u64,
    /// Tokens fetched from the pipe and not yet handed to an action.
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
    /// The read end again, non-blocking, for taking a token only if one is
    /// there (the jobserver's own descriptor must stay blocking).
    pipe_try: OwnedFd,
}

/// How long a token must lie unclaimed before an idle-priority request may
/// have it. A helper thread blocked on the pipe takes a token the moment it
/// appears, so one that is still there this much later is wanted by nobody.
const IDLE_DWELL: std::time::Duration = std::time::Duration::from_millis(40);

/// The right to run one action. Dropping it returns the token.
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
        // SAFETY: writing one byte from a valid buffer.
        let r = unsafe { libc::write(fd, b"|".as_ptr() as *const _, 1) };
        if r == 1 || std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return;
        }
    }
}

impl Scheduler {
    /// `tokens` is the number of cores to use; `mem_budget_kb` the memory
    /// actions may use in total.
    pub fn new(tokens: usize, mem_budget_kb: u64) -> std::io::Result<Arc<Scheduler>> {
        let mut fds = [0; 2];
        // The pipe ends must be blocking: jobserver clients expect that.
        // SAFETY: fds is a valid two-element array.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: both fds were just created.
        let (pipe_r, pipe_w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        for _ in 0..tokens {
            write_token(pipe_w.as_raw_fd());
        }
        // A second open file description of the same pipe: O_NONBLOCK set
        // on a duplicate would apply to the jobserver's descriptor too.
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

    /// The jobserver pipe, for handing to sandboxed processes.
    pub fn jobserver_fds(&self) -> [RawFd; 2] {
        [self.pipe_r.as_raw_fd(), self.pipe_w.as_raw_fd()]
    }

    /// `(running, waiting, peak running, total granted)`.
    pub fn snapshot(&self) -> (usize, usize, usize, u64) {
        let i = self.inner.lock();
        let waiting = i.waiters.iter().filter(|w| w.prio != Priority::Idle && !w.tx.is_closed()).count();
        (i.running, waiting, i.peak_running, i.granted)
    }

    /// Cores nothing is using right now: tokens neither held by an action
    /// nor taken from the pipe by a compiler's helper threads.
    pub fn free_tokens(&self) -> usize {
        let spare = self.inner.lock().spare_tokens;
        let mut avail: libc::c_int = 0;
        // SAFETY: FIONREAD writes the number of readable bytes to `avail`.
        if unsafe { libc::ioctl(self.pipe_r.as_raw_fd(), libc::FIONREAD, &mut avail) } != 0 {
            return spare;
        }
        spare + avail.max(0) as usize
    }

    /// Waits for a core and `mem_kb` of memory budget.
    pub async fn acquire(self: &Arc<Self>, prio: Priority, mem_kb: u64) -> Permit {
        let rx = {
            let mut i = self.inner.lock();
            let (tx, rx) = oneshot::channel();
            i.seq += 1;
            let seq = i.seq;
            i.waiters.push(Waiter { prio, seq, mem_kb, tx });
            self.dispatch(&mut i, false);
            self.wake_broker.notify_one();
            rx
        };
        // The sender is only dropped at shutdown.
        match rx.await {
            Ok(p) => p,
            Err(_) => std::future::pending().await,
        }
    }

    fn fits(&self, i: &Inner, mem_kb: u64) -> bool {
        i.running == 0 || i.mem_kb + mem_kb <= self.mem_budget_kb
    }

    /// Hands spare tokens to waiters. Called with the lock held. Requests
    /// at idle priority are passed over unless `idle_too`: a token an action
    /// gives back belongs to whoever is waiting for the pipe before it
    /// belongs to them.
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
            i.spare_tokens -= 1;
            i.running += 1;
            i.mem_kb += w.mem_kb;
            i.peak_running = i.peak_running.max(i.running);
            i.granted += 1;
            let permit = Permit { sched: self.clone(), mem_kb: w.mem_kb };
            if let Err(p) = w.tx.send(permit) {
                // The requester went away; undo without re-entering the lock.
                std::mem::forget(p);
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
        i.running -= 1;
        i.mem_kb -= mem_kb;
        // The token goes straight to the next waiter if there is one that
        // fits, otherwise back to the pipe for rustc's worker threads.
        i.spare_tokens += 1;
        self.dispatch(&mut i, false);
        self.return_spares(&mut i);
        self.wake_broker.notify_one();
        if i.running == 0 {
            self.reconcile(&mut i);
        }
    }

    /// Puts back tokens that a killed process held and could not return
    /// (see `cgroup::ActionGroup::helper_tokens`).
    pub fn restore(self: &Arc<Self>, tokens: usize) {
        let mut i = self.inner.lock();
        i.spare_tokens += tokens;
        self.dispatch(&mut i, false);
        self.return_spares(&mut i);
    }

    /// Requests the broker fetches tokens for.
    fn wanted(&self, i: &Inner) -> usize {
        i.waiters.iter().filter(|w| w.prio != Priority::Idle && !w.tx.is_closed()).count()
    }

    fn idle_wanted(&self, i: &Inner) -> bool {
        i.waiters.iter().any(|w| w.prio == Priority::Idle && !w.tx.is_closed())
    }

    /// Tokens in the pipe right now.
    fn in_pipe(&self) -> usize {
        let mut avail: libc::c_int = 0;
        // SAFETY: FIONREAD writes the number of readable bytes to `avail`.
        if unsafe { libc::ioctl(self.pipe_r.as_raw_fd(), libc::FIONREAD, &mut avail) } != 0 {
            return 0;
        }
        avail.max(0) as usize
    }

    /// Takes a token if one is there, without waiting.
    fn try_take(&self) -> bool {
        let mut b = 0u8;
        // SAFETY: non-blocking read of one byte into a valid buffer.
        unsafe { libc::read(self.pipe_try.as_raw_fd(), &mut b as *mut u8 as *mut _, 1) == 1 }
    }

    fn return_spares(&self, i: &mut Inner) {
        let want = self.wanted(i).min(1);
        while i.spare_tokens > want {
            i.spare_tokens -= 1;
            write_token(self.pipe_w.as_raw_fd());
        }
    }

    /// With no action running nobody can be holding a token: the pipe and
    /// the scheduler's own spares must add up to all of them. A process
    /// that was killed while holding tokens never returned them, and what
    /// was put back on its behalf was an estimate; settle the difference
    /// either way.
    fn reconcile(&self, i: &mut Inner) {
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
        // Unblock the broker if it is waiting on the pipe.
        write_token(self.pipe_w.as_raw_fd());
    }
}

/// Fetches tokens from the pipe on behalf of waiting actions. A blocking
/// read on a pipe cannot be done from async code or under a lock, hence the
/// dedicated thread.
fn broker(weak: std::sync::Weak<Scheduler>) {
    loop {
        let Some(s) = weak.upgrade() else { return };
        {
            let mut i = s.inner.lock();
            // A token seen in the pipe on the previous round, for idle
            // requests.
            let mut lay_unclaimed = false;
            loop {
                if i.shutdown {
                    return;
                }
                if s.wanted(&i) > i.spare_tokens {
                    break;
                }
                if s.idle_wanted(&i) {
                    // Nothing urgent, but someone would use a core nobody
                    // wants: one that has stayed in the pipe for a while.
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
        // SAFETY: blocking read of one byte into a valid buffer.
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
        let s = Scheduler::new(1, u64::MAX).unwrap();
        let [r, w] = s.jobserver_fds();
        rt().block_on(async {
            let running = s.acquire(Priority::Agent, 0).await;
            let s2 = s.clone();
            let idle = tokio::spawn(async move { s2.acquire(Priority::Idle, 0).await });
            // A compiler's helper thread, blocked on the jobserver.
            let (got_tx, got_rx) = std::sync::mpsc::channel();
            let (back_tx, back_rx) = std::sync::mpsc::channel::<()>();
            let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
            let helper = std::thread::spawn(move || {
                started_tx.send(()).unwrap();
                let mut b = 0u8;
                // SAFETY: blocking read of one byte into a valid buffer.
                assert_eq!(unsafe { libc::read(r, &mut b as *mut u8 as *mut _, 1) }, 1);
                got_tx.send(()).unwrap();
                back_rx.recv().unwrap();
                write_token(w);
            });
            // The helper must be in its read before the action ends, on a
            // machine however busy: it has said that it runs, and is then
            // given time to block.
            tokio::task::spawn_blocking(move || started_rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap()).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            // The action ends: its core goes to the helper, not to the idle
            // request, however long that has been waiting.
            drop(running);
            tokio::task::spawn_blocking(move || got_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap()).await.unwrap();
            tokio::time::sleep(IDLE_DWELL * 5).await;
            assert!(!idle.is_finished());
            assert_eq!(s.snapshot().1, 0, "an idle request is not waiting work");
            // The helper is done; now nobody wants the core.
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
            // A small action is admitted past the big one.
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
            // A "rustc worker thread" takes a token straight from the pipe.
            let [r, w] = s.jobserver_fds();
            let mut b = 0u8;
            assert_eq!(unsafe { libc::read(r, &mut b as *mut u8 as *mut _, 1) }, 1);
            assert_eq!(s.free_tokens(), 1);
            let _a = s.acquire(Priority::Agent, 0).await;
            assert_eq!(s.free_tokens(), 0);
            // Both tokens are in use now; a second action must wait.
            let s2 = s.clone();
            let second = tokio::spawn(async move { s2.acquire(Priority::Agent, 0).await });
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert!(!second.is_finished());
            // The worker thread gives its token back.
            write_token(w);
            tokio::time::timeout(std::time::Duration::from_secs(5), second).await.unwrap().unwrap();
        });
    }
}
