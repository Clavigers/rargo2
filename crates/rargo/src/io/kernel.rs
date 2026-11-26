//! System calls that only a real kernel answers.

use std::io;
use std::os::fd::RawFd;

fn done(result: libc::c_int) -> io::Result<()> {
    if result == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// Makes the file open as `to` share the blocks of the file open as `from`.
#[cfg(target_os = "linux")]
pub fn share_blocks(from: RawFd, to: RawFd) -> io::Result<()> {
    const FICLONE: libc::c_ulong = 0x4004_9409;
    done(unsafe { libc::ioctl(to, FICLONE as libc::Ioctl, from) })
}

/// Sends `signal` to the process `pid`; signal 0 only asks whether it could be sent.
pub fn kill(pid: libc::pid_t, signal: libc::c_int) -> io::Result<()> {
    done(unsafe { libc::kill(pid, signal) })
}

/// Sets the access and modification times of `path`, as `utimensat` reads `times` and `flags`.
pub fn set_times(path: &std::ffi::CStr, times: &[libc::timespec; 2], flags: libc::c_int) -> io::Result<()> {
    done(unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), flags) })
}

/// The space and file counts of the file system holding `path`.
pub fn statvfs(path: &std::ffi::CStr) -> io::Result<libc::statvfs> {
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    done(unsafe { libc::statvfs(path.as_ptr(), &mut s) })?;
    Ok(s)
}

/// Unmounts what is mounted at `path`, as `umount2` reads `flags`.
pub fn unmount(path: &std::ffi::CStr, flags: libc::c_int) -> io::Result<()> {
    done(unsafe { libc::umount2(path.as_ptr(), flags) })
}

/// Takes or drops an advisory lock on an open file, as `flock` reads `operation`.
pub fn flock(fd: RawFd, operation: libc::c_int) -> io::Result<()> {
    done(unsafe { libc::flock(fd, operation) })
}
