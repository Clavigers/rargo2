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

/// Starts a thread called `name` that runs `body`.
pub fn spawn(name: &str, body: impl FnOnce() + Send + 'static) -> io::Result<()> {
    std::thread::Builder::new().name(name.into()).spawn(body).map(drop)
}
