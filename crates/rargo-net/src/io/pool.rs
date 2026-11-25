use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex as StdMutex};
use std::thread::JoinHandle as ThreadJoin;


pub(super) struct Job {
    pub(super) token: u64,
    pub(super) run: Box<dyn FnOnce() + Send>,
}

pub(super) struct Jobs {
    pub(super) q: StdMutex<VecDeque<Job>>,
    pub(super) cv: Condvar,
    pub(super) closed: AtomicBool,
}

pub(super) struct Pool {
    pub(super) jobs: Arc<Jobs>,
    pub(super) done: Arc<StdMutex<VecDeque<u64>>>,
    pub(super) joins: Vec<ThreadJoin<()>>,
    pub(super) efd: i32,
}

pub(super) fn worker(jobs: Arc<Jobs>, done: Arc<StdMutex<VecDeque<u64>>>, efd: i32) {
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
        let _ = catch_unwind(AssertUnwindSafe(job.run));
        done.lock().unwrap_or_else(|e| e.into_inner()).push_back(job.token);
        let one = 1u64;
        unsafe { libc::write(efd, &one as *const u64 as *const libc::c_void, 8) };
    }
}

