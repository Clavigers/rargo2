use crate::sched_state::{Broker, Effects, Job, SchedState};
use parking_lot::Mutex;
use std::collections::BTreeMap;
use crate::io::PipeEnd;
use std::os::fd::{AsRawFd, RawFd};
use crate::io::oneshot;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    Idle = 0,
    Background = 1,
    Agent = 2,
    Human = 3,
}

struct Inner {
    state: SchedState,
    senders: BTreeMap<Job, oneshot::Sender<Permit>>,
    next_job: Job,
    shutdown: bool,
}

pub struct Scheduler {
    inner: Mutex<Inner>,
    wake_broker: std::sync::OnceLock<crate::io::Notify>,
    born: Instant,
    pipe_r: PipeEnd,
    pipe_w: PipeEnd,
    pipe_try: PipeEnd,
}

pub struct Permit {
    sched: Arc<Scheduler>,
    job: Job,
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.sched.release(self.job);
    }
}

fn write_token(fd: RawFd) {
    let _ = crate::io::write_now(fd, b"|");
}

impl Scheduler {
    pub fn new(tokens: usize, mem_budget_kb: u64) -> std::io::Result<Arc<Scheduler>> {
        let (pipe_r, pipe_w) = crate::io::pipe()?;
        for _ in 0..tokens {
            write_token(pipe_w.as_raw_fd());
        }
        let pipe_try = pipe_r.nonblocking_reader()?;
        let s = Arc::new(Scheduler {
            inner: Mutex::new(Inner {
                state: SchedState::new(tokens, mem_budget_kb),
                senders: BTreeMap::new(),
                next_job: 0,
                shutdown: false,
            }),
            wake_broker: std::sync::OnceLock::new(),
            born: crate::io::now(),
            pipe_r,
            pipe_w,
            pipe_try,
        });
        Ok(s)
    }

    pub fn tokens(&self) -> usize {
        self.inner.lock().state.tokens()
    }

    pub fn jobserver_fds(&self) -> [RawFd; 2] {
        [self.pipe_r.as_raw_fd(), self.pipe_w.as_raw_fd()]
    }

    pub fn snapshot(&self) -> (usize, usize, usize, u64) {
        let i = self.inner.lock();
        let (running, _, peak, granted) = i.state.snapshot();
        let waiting =
            i.state.wanted_jobs().into_iter().filter(|j| i.senders.get(j).is_some_and(|t| !t.is_closed())).count();
        (running, waiting, peak, granted)
    }

    pub fn free_tokens(&self) -> usize {
        self.inner.lock().state.spare() + self.in_pipe()
    }

    fn kick(&self) {
        if let Some(wake) = self.wake_broker.get() {
            wake.notify_one();
        }
    }

    fn start_broker(self: &Arc<Self>) {
        self.wake_broker.get_or_init(|| {
            let weak = Arc::downgrade(self);
            crate::io::background(move || broker(weak));
            crate::io::Notify::new()
        });
    }

    pub fn acquire(self: &Arc<Self>, prio: Priority, mem_kb: u64) -> Permit {
        self.acquire_saying(prio, mem_kb, |_, _| {})
    }

    /// Like `acquire`; when no slot is free as the job asks, `queued` is told how many jobs run and how many wait.
    pub fn acquire_saying(self: &Arc<Self>, prio: Priority, mem_kb: u64, queued: impl FnOnce(usize, usize)) -> Permit {
        self.start_broker();
        let (rx, waits) = {
            let mut i = self.inner.lock();
            self.prune(&mut i);
            let (tx, rx) = oneshot::channel();
            let job = i.next_job;
            i.next_job += 1;
            i.senders.insert(job, tx);
            let free = i.state.spare() + self.in_pipe();
            let fx = i.state.request(job, prio, mem_kb);
            self.apply(&mut i, fx);
            self.kick();
            let (running, waiting, _, _) = i.state.snapshot();
            (rx, (free == 0).then_some((running, waiting)))
        };
        if let Some((running, waiting)) = waits {
            queued(running, waiting);
        }
        match rx.recv() {
            Some(permit) => permit,
            None => {
                let hang = crate::io::Notify::new();
                loop {
                    hang.notified();
                }
            }
        }
    }

    fn prune(self: &Arc<Self>, i: &mut Inner) {
        let gone: Vec<Job> = i.senders.iter().filter(|(_, t)| t.is_closed()).map(|(j, _)| *j).collect();
        for job in gone {
            i.senders.remove(&job);
            let fx = i.state.cancel(job);
            self.apply(i, fx);
        }
    }

    fn apply(self: &Arc<Self>, i: &mut Inner, fx: Effects) {
        let mut todo = vec![fx];
        while let Some(fx) = todo.pop() {
            for job in fx.grant {
                let tx = i.senders.remove(&job).expect("a granted job has a sender");
                if let Err(p) = tx.send(Permit { sched: self.clone(), job }) {
                    std::mem::forget(p);
                    todo.push(i.state.release(job));
                }
            }
            for _ in 0..fx.put_tokens {
                write_token(self.pipe_w.as_raw_fd());
            }
            if fx.reconcile {
                let r = i.state.reconcile(self.in_pipe());
                for _ in 0..r.put {
                    write_token(self.pipe_w.as_raw_fd());
                }
                for _ in 0..r.take {
                    if !self.try_take() {
                        break;
                    }
                }
            }
        }
    }

    fn release(self: &Arc<Self>, job: Job) {
        let mut i = self.inner.lock();
        let fx = i.state.release(job);
        self.apply(&mut i, fx);
        self.prune(&mut i);
        self.kick();
    }

    pub fn restore(self: &Arc<Self>, tokens: usize) {
        let mut i = self.inner.lock();
        let fx = i.state.token_in(tokens);
        self.apply(&mut i, fx);
    }

    fn in_pipe(&self) -> usize {
        crate::io::queued(self.pipe_r.as_raw_fd()).unwrap_or(0)
    }

    fn try_take(&self) -> bool {
        matches!(crate::io::try_read(self.pipe_try.as_raw_fd(), 1), Ok(Some(token)) if token.len() == 1)
    }

    fn now_ms(&self) -> u64 {
        crate::io::now().saturating_duration_since(self.born).as_millis() as u64
    }

    pub fn shutdown(&self) {
        self.inner.lock().shutdown = true;
        self.kick();
        write_token(self.pipe_w.as_raw_fd());
    }
}

fn broker(weak: std::sync::Weak<Scheduler>) {
    loop {
        let Some(s) = weak.upgrade() else { return };
        let step = {
            let mut i = s.inner.lock();
            if i.shutdown {
                return;
            }
            s.prune(&mut i);
            match i.state.tick(s.now_ms(), s.in_pipe()) {
                Broker::TakeForIdle => {
                    if s.try_take() {
                        let fx = i.state.idle_token();
                        s.apply(&mut i, fx);
                    }
                    continue;
                }
                step => step,
            }
        };
        match step {
            Broker::Wait(ms) => {
                let wake = s.wake_broker.get().expect("the broker starts with its wake");
                let _ = crate::io::timeout(Duration::from_millis(ms), || wake.notified());
                if Arc::strong_count(&s) == 1 {
                    return;
                }
            }
            Broker::Read => {
                let Ok(token) = crate::io::read_at(s.pipe_r.as_raw_fd(), 0, 1) else { continue };
                if token.len() != 1 {
                    continue;
                }
                let mut i = s.inner.lock();
                if i.shutdown {
                    return;
                }
                let fx = i.state.token_in(1);
                s.apply(&mut i, fx);
            }
            Broker::TakeForIdle => unreachable!("taken above"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_shared_with_jobserver_clients() {
        crate::io::sim::test(|| {
            let s = Scheduler::new(2, u64::MAX).unwrap();
            assert_eq!(s.free_tokens(), 2);
            let [r, w] = s.jobserver_fds();
            assert_eq!(crate::io::try_read(r, 1).unwrap().map(|token| token.len()), Some(1));
            assert_eq!(s.free_tokens(), 1);
            let _a = s.acquire(Priority::Agent, 0);
            assert_eq!(s.free_tokens(), 0);
            let s2 = s.clone();
            let mut second = crate::io::spawn(move || s2.acquire(Priority::Agent, 0));
            crate::io::sleep(std::time::Duration::from_millis(50));
            assert!(!second.is_finished());
            write_token(w);
            crate::io::timeout(std::time::Duration::from_secs(5), || second.join()).unwrap().unwrap();
        });
    }

    #[test]
    fn a_job_that_must_wait_for_a_slot_is_told_how_many_run_and_wait_and_one_granted_at_once_is_not() {
        crate::io::sim::test(|| {
            let s = Scheduler::new(1, u64::MAX).unwrap();
            let mut said = None;
            let first = s.acquire_saying(Priority::Human, 0, |running, waiting| said = Some((running, waiting)));
            assert_eq!(said, None);
            let s2 = s.clone();
            let mut second = crate::io::spawn(move || {
                let mut said = None;
                drop(s2.acquire_saying(Priority::Human, 0, |running, waiting| said = Some((running, waiting))));
                said
            });
            crate::io::yield_now();
            drop(first);
            let said = crate::io::timeout(std::time::Duration::from_secs(5), || second.join()).unwrap().unwrap();
            assert_eq!(said, Some((1, 1)));
        });
    }
}
