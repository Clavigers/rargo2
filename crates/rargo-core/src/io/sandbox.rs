//! The system calls that start a sandboxed process: namespaces, mounts,
//! descriptors and identity. Each function makes the calls it is named for
//! and reports what the kernel said. Which calls a sandbox needs, in what
//! order and with which arguments is the plan `crate::sandbox` computes.
//!
//! Everything here calls the kernel directly and never parks a fiber, because
//! most of it runs in a freshly cloned process, which shares its parent's
//! ring and must not submit to it.

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;

fn ok(result: libc::c_int) -> io::Result<()> {
    if result == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

fn fd(result: libc::c_int) -> io::Result<RawFd> {
    if result < 0 { Err(io::Error::last_os_error()) } else { Ok(result) }
}

/// A connected pair of Unix stream sockets, neither inherited across `exec`.
pub fn socket_pair() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    ok(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0, fds.as_mut_ptr()) })?;
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Sets whether a descriptor is closed by `exec`.
pub fn set_close_on_exec(fd: RawFd, on: bool) -> io::Result<()> {
    ok(unsafe { libc::fcntl(fd, libc::F_SETFD, if on { libc::FD_CLOEXEC } else { 0 }) })
}

/// Shuts a socket down in both directions.
pub fn shutdown(fd: RawFd) -> io::Result<()> {
    ok(unsafe { libc::shutdown(fd, libc::SHUT_RDWR) })
}

/// Asks the kernel to kill this process when its parent dies.
pub fn die_with_parent() -> io::Result<()> {
    ok(unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) })
}

/// Names this process's main thread.
pub fn set_process_name(name: &CStr) -> io::Result<()> {
    ok(unsafe { libc::prctl(libc::PR_SET_NAME, name.as_ptr()) })
}

/// What `wait4` returned for a child.
pub struct Reaped {
    /// The process `wait4` reaped: 0 when the child has not exited, -1 when the call failed.
    pub pid: i32,
    pub status: i32,
    pub user_us: u64,
    pub sys_us: u64,
    pub max_rss_kb: u64,
}

/// Reaps a child without waiting for it.
pub fn reap(pid: i32) -> Reaped {
    let mut status = 0i32;
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    let reaped = unsafe { libc::wait4(pid, &mut status, libc::WNOHANG, &mut ru) };
    Reaped {
        pid: reaped,
        status,
        user_us: ru.ru_utime.tv_sec as u64 * 1_000_000 + ru.ru_utime.tv_usec as u64,
        sys_us: ru.ru_stime.tv_sec as u64 * 1_000_000 + ru.ru_stime.tv_usec as u64,
        max_rss_kb: ru.ru_maxrss as u64,
    }
}

/// Waits for any child to change state: its pid and its wait status.
pub fn wait_any() -> io::Result<(i32, i32)> {
    let mut status = 0;
    let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
    if pid < 0 { Err(io::Error::last_os_error()) } else { Ok((pid, status)) }
}

/// A new pipe: its reading end and its writing end, neither inherited across `exec`.
pub fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    ok(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) })?;
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Starts a child in the namespaces `flags` names: the child's pid in the parent, 0 in the child.
pub fn clone(flags: i32) -> io::Result<i32> {
    let pid = unsafe { libc::syscall(libc::SYS_clone, (flags | libc::SIGCHLD) as libc::c_ulong, 0usize, 0usize, 0usize, 0usize) } as i32;
    if pid < 0 { Err(io::Error::last_os_error()) } else { Ok(pid) }
}

/// Forks: the child's pid in the parent, 0 in the child.
pub fn fork() -> io::Result<i32> {
    let pid = unsafe { libc::fork() };
    if pid < 0 { Err(io::Error::last_os_error()) } else { Ok(pid) }
}

/// Ends this process at once, running no destructors and flushing nothing.
pub fn exit_now(code: i32) -> ! {
    unsafe { libc::_exit(code) }
}

/// Reads from a descriptor with one system call.
pub fn read(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
}

/// Writes to a descriptor with one system call.
pub fn write(fd: RawFd, buf: &[u8]) -> io::Result<usize> {
    let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
    if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
}

/// Makes `target` a copy of `fd`, closing what `target` was.
pub fn dup_to(fd: RawFd, target: RawFd) -> io::Result<()> {
    self::fd(unsafe { libc::dup2(fd, target) }).map(|_| ())
}

/// A copy of `fd` numbered `min` or above, closed by `exec`.
pub fn dup_above(fd: RawFd, min: RawFd) -> io::Result<RawFd> {
    self::fd(unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, min) })
}

/// Opens a path for reading.
pub fn open_read(path: &CStr) -> io::Result<OwnedFd> {
    let fd = self::fd(unsafe { libc::open(path.as_ptr(), libc::O_RDONLY) })?;
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Unblocks every signal for this thread.
pub fn unblock_signals() {
    unsafe {
        let mut mask: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        libc::sigprocmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut());
    }
}

/// One `mount` call.
pub fn mount(src: Option<&CStr>, target: &CStr, fstype: Option<&CStr>, flags: u64, data: Option<&CStr>) -> io::Result<()> {
    let ptr = |c: Option<&CStr>| c.map_or(std::ptr::null(), CStr::as_ptr);
    ok(unsafe { libc::mount(ptr(src), target.as_ptr(), ptr(fstype), flags as libc::c_ulong, ptr(data).cast()) })
}

/// Makes `root` this mount namespace's root and detaches the old one.
pub fn pivot_root(root: &CStr) -> io::Result<()> {
    unsafe {
        ok(libc::chdir(root.as_ptr()))?;
        ok(libc::syscall(libc::SYS_pivot_root, c".".as_ptr(), c".".as_ptr()) as libc::c_int)?;
        ok(libc::umount2(c".".as_ptr(), libc::MNT_DETACH))?;
        ok(libc::chdir(c"/".as_ptr()))
    }
}

/// Brings this network namespace's loopback interface up.
pub fn loopback_up() -> io::Result<()> {
    let sock = fd(unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) })?;
    let sock = unsafe { OwnedFd::from_raw_fd(sock) };
    let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() };
    ifr.ifr_name[0] = b'l' as libc::c_char;
    ifr.ifr_name[1] = b'o' as libc::c_char;
    ok(unsafe { libc::ioctl(sock.as_raw_fd(), libc::SIOCGIFFLAGS, &mut ifr) })?;
    unsafe { ifr.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short };
    ok(unsafe { libc::ioctl(sock.as_raw_fd(), libc::SIOCSIFFLAGS, &ifr) })
}

/// Sets this UTS namespace's host name.
pub fn set_hostname(name: &[u8]) -> io::Result<()> {
    ok(unsafe { libc::sethostname(name.as_ptr().cast(), name.len()) })
}

/// Drops every supplementary group.
pub fn drop_groups() -> io::Result<()> {
    ok(unsafe { libc::setgroups(0, std::ptr::null()) })
}

pub fn set_gid(gid: u32) -> io::Result<()> {
    ok(unsafe { libc::setgid(gid) })
}

pub fn set_uid(uid: u32) -> io::Result<()> {
    ok(unsafe { libc::setuid(uid) })
}

pub fn chdir(dir: &CStr) -> io::Result<()> {
    ok(unsafe { libc::chdir(dir.as_ptr()) })
}

/// Replaces this process with `program`. It returns only when the kernel refuses, with why.
pub fn execve(program: &CStr, argv: &[CString], env: &[CString]) -> io::Error {
    let pointers = |strings: &[CString]| strings.iter().map(|s| s.as_ptr()).chain([std::ptr::null()]).collect::<Vec<*const libc::c_char>>();
    let (argv, env) = (pointers(argv), pointers(env));
    unsafe { libc::execve(program.as_ptr(), argv.as_ptr(), env.as_ptr()) };
    io::Error::last_os_error()
}

pub fn create_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir(path)
}

pub fn create_dir_all(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)
}

/// Creates or empties a file and writes `data` to it.
pub fn write_file(path: &Path, data: &[u8]) -> io::Result<()> {
    std::fs::write(path, data)
}

/// Creates a file, or empties the one there.
pub fn create_file(path: &Path) -> io::Result<()> {
    std::fs::File::create(path).map(|_| ())
}

pub fn symlink(target: &Path, path: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, path)
}
