//! The system calls rargod makes itself.

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Unmounts whatever is mounted at `target`.
pub fn umount(target: &Path, flags: libc::c_int) -> io::Result<()> {
    let target = CString::new(target.as_os_str().as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a nul"))?;
    if unsafe { libc::umount2(target.as_ptr(), flags) } != 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}
