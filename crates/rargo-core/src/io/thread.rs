//! Threads of this process that the loop does not run.

use std::io;

/// Starts a thread called `name` that runs `body` at scheduling priority `nice`.
pub fn spawn_niced(name: &str, nice: i32, body: impl FnOnce() + Send + 'static) -> io::Result<()> {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let tid = unsafe { libc::syscall(libc::SYS_gettid) };
            if tid > 0 {
                unsafe {
                    libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, nice);
                }
            }
            body()
        })
        .map(drop)
}

/// Maps `each` over `items` on the rayon pool and collects what it returns.
/// A simulation has one thread, so there the items are mapped in turn where this is called.
pub fn par_map<T, R, C>(items: Vec<T>, each: impl Fn(T) -> R + Send + Sync) -> C
where
    T: Send,
    R: Send,
    C: rayon::iter::FromParallelIterator<R> + FromIterator<R>,
{
    if super::sim::active() {
        return items.into_iter().map(each).collect();
    }
    use rayon::prelude::*;
    items.into_par_iter().map(each).collect()
}

/// State that a thread of its own works through. What the state is and what is
/// next in it are the caller's; this guards it and wakes whoever waits on it.
pub struct Monitor<Q> {
    state: std::sync::Mutex<Q>,
    changed: std::sync::Condvar,
}

impl<Q> Monitor<Q> {
    pub fn new(state: Q) -> Monitor<Q> {
        Monitor { state: std::sync::Mutex::new(state), changed: std::sync::Condvar::new() }
    }

    /// Changes the state and wakes the threads that wait on it.
    pub fn change<R>(&self, change: impl FnOnce(&mut Q) -> R) -> R {
        let out = change(&mut self.state.lock().unwrap_or_else(|e| e.into_inner()));
        self.changed.notify_all();
        out
    }

    /// Blocks this thread until `take` finds something in the state.
    pub fn wait<T>(&self, mut take: impl FnMut(&mut Q) -> Option<T>) -> T {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            match take(&mut state) {
                Some(found) => return found,
                None => state = self.changed.wait(state).unwrap_or_else(|e| e.into_inner()),
            }
        }
    }
}
