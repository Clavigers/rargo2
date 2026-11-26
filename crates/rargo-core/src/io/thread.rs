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
