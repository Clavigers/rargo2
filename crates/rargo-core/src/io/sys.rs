//! Single system calls on the host that a simulation has no answer for:
//! signalling a process and reading an extended attribute.

use std::ffi::CStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Sends SIGKILL to a process.
pub fn kill(pid: i32) -> io::Result<()> {
    if unsafe { libc::kill(pid, libc::SIGKILL) } == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

/// Reads an extended attribute of `path` into `buf`: how many bytes it has.
pub fn xattr(path: &Path, name: &CStr, buf: &mut [u8]) -> io::Result<usize> {
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    let n = unsafe { libc::getxattr(path.as_ptr(), name.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
}

/// A new kernel pipe for handing to a child process, as its read end and its write end, both closed on exec.
pub fn pipe() -> std::io::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { (<std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fds[0]), <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fds[1])) })
}
