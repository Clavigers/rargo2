//! A file's bytes as memory.

use std::io;
use std::os::fd::AsRawFd;

use super::file::File;

/// Runs `read` over the first `len` bytes of `file`, mapped read-only into
/// memory for the length of the call. A simulated file has no pages to map,
/// so its bytes are read into a buffer instead.
pub fn mapped<R>(file: &File, len: usize, read: impl FnOnce(&[u8]) -> R) -> io::Result<R> {
    if super::sim::active() {
        let mut bytes = vec![0u8; len];
        file.read_exact_at(&mut bytes, 0)?;
        return Ok(read(&bytes));
    }
    unsafe {
        let p = libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_PRIVATE, file.as_raw_fd(), 0);
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let out = read(std::slice::from_raw_parts(p as *const u8, len));
        libc::munmap(p, len);
        Ok(out)
    }
}
