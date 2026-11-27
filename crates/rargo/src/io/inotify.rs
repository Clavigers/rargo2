//! The inotify system calls.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

pub(crate) fn open(flags: libc::c_int) -> io::Result<OwnedFd> {
    let fd = unsafe { libc::inotify_init1(flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Watches `dir` for the events in `mask` and returns the watch descriptor.
pub(crate) fn add(fd: &OwnedFd, dir: &Path, mask: u32) -> io::Result<i32> {
    let c = CString::new(dir.as_os_str().as_bytes()).map_err(io::Error::other)?;
    let wd = unsafe { libc::inotify_add_watch(fd.as_raw_fd(), c.as_ptr(), mask) };
    if wd < 0 { Err(io::Error::last_os_error()) } else { Ok(wd) }
}

pub(crate) fn remove(fd: &OwnedFd, wd: i32) {
    unsafe { libc::inotify_rm_watch(fd.as_raw_fd(), wd) };
}
