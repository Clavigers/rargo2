//! Pipes. A simulation holds a pipe's bytes in memory.

use std::io;

use super::sim;

/// One end of a pipe, closed when dropped.
#[derive(Debug)]
pub struct PipeEnd {
    fd: i32,
}

impl PipeEnd {
    /// A second reading end of this pipe that never waits: reading it with `try_read` gets nothing when the pipe is empty.
    pub fn nonblocking_reader(&self) -> io::Result<PipeEnd> {
        if let Some(fd) = sim::pipe_reader(self.fd) {
            return Ok(PipeEnd { fd: fd? });
        }
        let path = format!("/proc/self/fd/{}\0", self.fd);
        let fd = unsafe { libc::open(path.as_ptr().cast(), libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC) };
        if fd < 0 { Err(io::Error::last_os_error()) } else { Ok(PipeEnd { fd }) }
    }
}

impl std::os::fd::AsRawFd for PipeEnd {
    fn as_raw_fd(&self) -> i32 {
        self.fd
    }
}

impl Drop for PipeEnd {
    fn drop(&mut self) {
        if !sim::close_now(self.fd) {
            unsafe { libc::close(self.fd) };
        }
    }
}

/// A new pipe: its reading end and its writing end. Neither end is inherited across `exec`.
pub fn pipe() -> io::Result<(PipeEnd, PipeEnd)> {
    if let Some((r, w)) = sim::pipe() {
        return Ok((PipeEnd { fd: r }, PipeEnd { fd: w }));
    }
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((PipeEnd { fd: fds[0] }, PipeEnd { fd: fds[1] }))
}

/// Writes to a descriptor with one system call, without parking the fiber, so it can be called with a lock held.
pub fn write_now(fd: i32, buf: &[u8]) -> io::Result<usize> {
    if let Some(wrote) = sim::pipe_write(fd, buf) {
        return wrote;
    }
    loop {
        let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// How many bytes wait to be read from a pipe.
pub fn queued(fd: i32) -> io::Result<usize> {
    if let Some(n) = sim::pipe_queued(fd) {
        return n;
    }
    let mut n: libc::c_int = 0;
    if unsafe { libc::ioctl(fd, libc::FIONREAD, &mut n) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n.max(0) as usize)
}
