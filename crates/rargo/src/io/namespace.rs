//! The system calls that build a mount namespace, one function each.

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

fn cstr(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a nul"))
}

fn done(r: libc::c_long) -> io::Result<()> {
    if r != 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

pub(crate) fn unshare(flags: libc::c_int) -> io::Result<()> {
    done(unsafe { libc::unshare(flags) }.into())
}

pub(crate) fn mount(src: Option<&Path>, tgt: &Path, fstype: Option<&str>, flags: libc::c_ulong, data: Option<&str>) -> io::Result<()> {
    let text = |s: &str| CString::new(s).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "text contains a nul"));
    let src_c = src.map(cstr).transpose()?;
    let tgt_c = cstr(tgt)?;
    let fs_c = fstype.map(text).transpose()?;
    let data_c = data.map(text).transpose()?;
    done(unsafe {
        libc::mount(
            src_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            tgt_c.as_ptr(),
            fs_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            flags,
            data_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr() as *const libc::c_void),
        )
    }
    .into())
}

pub(crate) fn chdir(dir: &Path) -> io::Result<()> {
    let dir = cstr(dir)?;
    done(unsafe { libc::chdir(dir.as_ptr()) }.into())
}

pub(crate) fn pivot_root(new_root: &Path, put_old: &Path) -> io::Result<()> {
    let (new_root, put_old) = (cstr(new_root)?, cstr(put_old)?);
    done(unsafe { libc::syscall(libc::SYS_pivot_root, new_root.as_ptr(), put_old.as_ptr()) })
}

pub(crate) fn umount(target: &Path, flags: libc::c_int) -> io::Result<()> {
    let target = cstr(target)?;
    done(unsafe { libc::umount2(target.as_ptr(), flags) }.into())
}

/// Makes `attempt` in a forked child, which then exits, and gives its outcome: what a change
/// that cannot be undone would do is learned without making it here. A failure comes back as
/// the child's system error.
pub(crate) fn in_child(attempt: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
    /// The exit status of a failure that was no system error, or one too large for a status.
    const OTHER: i32 = 255;
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        let status = match attempt() {
            Ok(()) => 0,
            Err(e) => e.raw_os_error().filter(|n| (1..OTHER).contains(n)).unwrap_or(OTHER),
        };
        unsafe { libc::_exit(status) }
    }
    let mut status = 0;
    while unsafe { libc::waitpid(pid, &mut status, 0) } < 0 {
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
    match libc::WIFEXITED(status).then(|| libc::WEXITSTATUS(status)) {
        Some(0) => Ok(()),
        Some(n) if n != OTHER => Err(io::Error::from_raw_os_error(n)),
        _ => Err(io::Error::other("the attempt failed in a child process")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_attempt_in_a_child_gives_its_outcome_and_changes_nothing_here() {
        let here = std::cell::Cell::new(0);
        assert!(in_child(|| {
            here.set(1);
            Ok(())
        })
        .is_ok());
        let denied = in_child(|| {
            here.set(2);
            Err(io::Error::from_raw_os_error(libc::EACCES))
        });
        assert_eq!(denied.unwrap_err().raw_os_error(), Some(libc::EACCES));
        let other = in_child(|| Err(io::Error::other("no directory to build the namespace on")));
        assert_eq!(other.unwrap_err().kind(), io::ErrorKind::Other);
        assert_eq!(here.get(), 0, "the child's changes are its own");
    }
}
