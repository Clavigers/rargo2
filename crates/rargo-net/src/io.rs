
mod fiber;
pub mod file;
mod fs;
pub mod env;
mod fuse;
mod net;
mod ops;
mod pool;
pub mod process;
mod ring;
mod signal;
pub mod sim;
mod sync;
mod task;

pub use fuse::FuseDrive;

pub use fiber::{drive, loop_running, on_fiber};
pub use fs::{close_fd, fsync_fd, open_at, read_at, read_ready, rename_at, stat_at, try_read, write_at};
pub use process::{output, wait_exit, wait_exited};
pub use net::{lookup_host, recv_fds, send_fds, ReadHalf, TcpListener, TcpStream, UdpSocket, UnixListener, UnixStream, WriteHalf};
pub use sim::{now, random, system_now};
pub use signal::{signal, Signal, SignalKind};
pub use sync::{oneshot, unbounded_channel, watch, Mutex, MutexGuard, Notify, Permit, RecvError, Semaphore, UnboundedReceiver, UnboundedSender};
pub use task::{background, blocking, call, spawn_off, cpu, interval, on_this_thread, race_biased, sleep, spawn, timeout, within, yield_now, AbortHandle, Elapsed, Interval, JoinError, JoinHandle};

use std::io;

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

/// Why rargo cannot run on this machine, when the kernel refuses it an io_uring.
pub fn unavailable() -> Option<String> {
    let err = ring::probe().err()?;
    Some(refusal(&err).map(str::to_owned).unwrap_or_else(|| format!("io_uring is unavailable (io_uring_setup failed: {err})")))
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

#[cfg(test)]
mod tests_loop;
