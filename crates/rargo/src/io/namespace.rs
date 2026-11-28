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

/// Writes a namespace control file using a sequential write. These procfs
/// files reject positioned writes with ESPIPE.
pub(crate) fn write_proc(path: &str, value: &str) -> io::Result<()> {
    use std::io::Write;
    std::fs::OpenOptions::new().write(true).open(path)?.write_all(value.as_bytes())
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
