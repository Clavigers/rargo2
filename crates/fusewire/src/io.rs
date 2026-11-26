//! The system calls behind a FUSE session.
//!
//! [`Real`] answers each [`Kernel`] request with the one system call, or the
//! one child process, that carries it out, and reports what the kernel said.
//! What to mount, with which options, in what order, and what an answer means
//! is decided by the session, which hands everything over as plain data.

use crate::abi::{fuse_backing_map, FUSE_DEV_IOC_BACKING_CLOSE, FUSE_DEV_IOC_BACKING_OPEN};
use crate::{mount_id_in, unmount_action, GuardPlan, Helped, Kernel, Unmount};
use std::ffi::{CStr, OsString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::process::{Command, Stdio};

/// The kernel this process runs on.
#[derive(Clone, Copy, Debug, Default)]
pub struct Real;

/// A running unmount guard: the write end of the pipe it waits on, and its
/// process id.
pub struct Guard {
    pipe: OwnedFd,
    pid: libc::pid_t,
}

fn checked(status: libc::c_int) -> io::Result<i32> {
    if status < 0 { Err(io::Error::last_os_error()) } else { Ok(status) }
}

fn counted(n: isize) -> io::Result<usize> {
    if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
}

fn pipe() -> io::Result<(RawFd, RawFd)> {
    let mut p = [0 as libc::c_int; 2];
    checked(unsafe { libc::pipe2(p.as_mut_ptr(), libc::O_CLOEXEC) })?;
    Ok((p[0], p[1]))
}

fn statx_mount_id(path: &CStr, mask: u32) -> Option<(u32, u64)> {
    const AT_STATX_DONT_SYNC: libc::c_int = 0x4000;
    let mut buf = [0u64; 32];
    let r = unsafe {
        libc::syscall(
            libc::SYS_statx,
            libc::AT_FDCWD,
            path.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW | AT_STATX_DONT_SYNC,
            mask,
            buf.as_mut_ptr(),
        )
    };
    (r == 0).then_some(((buf[0] & 0xffff_ffff) as u32, buf[18]))
}

/// The body of the guard process. It waits on `r` for its parent to write a
/// byte or to die, and in the second case carries out the plan's unmount.
unsafe fn guard_child(plan: &GuardPlan, argv: &[*const libc::c_char], r: RawFd, ready_w: RawFd) -> ! {
    unsafe {
        libc::setsid();
        libc::prctl(libc::PR_SET_NAME, plan.name.as_ptr());
        if let Some((start, end)) = plan.title_area {
            let (area, len) = (start as *mut u8, end - start);
            std::ptr::write_bytes(area, 0, len);
            std::ptr::copy_nonoverlapping(plan.title.as_ptr(), area, plan.title.len().min(len - 1));
        }
        for sig in &plan.ignored_signals {
            libc::signal(*sig, libc::SIG_IGN);
        }
        libc::write(ready_w, [1u8].as_ptr() as *const libc::c_void, 1);
        if r > 0 {
            libc::syscall(libc::SYS_close_range, 0 as libc::c_uint, (r - 1) as libc::c_uint, 0 as libc::c_uint);
        }
        if libc::syscall(libc::SYS_close_range, (r + 1) as libc::c_uint, libc::c_uint::MAX, 0 as libc::c_uint) != 0 {
            for fd in (r + 1)..65536 {
                libc::close(fd);
            }
        }
        let mut byte = [0u8; 1];
        loop {
            let n = libc::read(r, byte.as_mut_ptr() as *mut libc::c_void, 1);
            if n > 0 {
                libc::_exit(0);
            }
            if n == 0 || *libc::__errno_location() != libc::EINTR {
                break;
            }
        }
        let there = mount_id_in(statx_mount_id(&plan.target, plan.statx_mask));
        match unmount_action(plan.mount_id, there, !argv.is_empty()) {
            Unmount::Leave => {}
            Unmount::Detach => {
                libc::umount2(plan.target.as_ptr(), libc::MNT_DETACH);
            }
            Unmount::Helper => {
                libc::execvp(argv[0], argv.as_ptr());
                libc::_exit(1);
            }
        }
        libc::_exit(0);
    }
}

impl Kernel for Real {
    type Device = OwnedFd;
    type Socket = OwnedFd;
    type Guard = Guard;

    fn open_device(&self) -> io::Result<OwnedFd> {
        let fd = checked(unsafe { libc::open(c"/dev/fuse".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) })?;
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn is_dir(&self, path: &Path) -> io::Result<bool> {
        Ok(std::fs::metadata(path)?.is_dir())
    }

    fn euid(&self) -> u32 {
        unsafe { libc::geteuid() }
    }

    fn egid(&self) -> u32 {
        unsafe { libc::getegid() }
    }

    fn mount(&self, source: &CStr, target: &CStr, fstype: &CStr, flags: libc::c_ulong, data: &CStr) -> io::Result<()> {
        checked(unsafe {
            libc::mount(source.as_ptr(), target.as_ptr(), fstype.as_ptr(), flags, data.as_ptr() as *const libc::c_void)
        })
        .map(|_| ())
    }

    fn run_with_socket(&self, program: &str, args: &[OsString], fd_env: &str) -> io::Result<Helped<OwnedFd>> {
        let mut pair = [0 as libc::c_int; 2];
        checked(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, pair.as_mut_ptr()) })?;
        let (ours, theirs) = unsafe { (OwnedFd::from_raw_fd(pair[0]), OwnedFd::from_raw_fd(pair[1])) };
        unsafe { libc::fcntl(ours.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
        let out = Command::new(program)
            .args(args)
            .env(fd_env, theirs.as_raw_fd().to_string())
            .stdin(Stdio::null())
            .output()?;
        drop(theirs);
        Ok(Helped { success: out.status.success(), stderr: out.stderr, socket: ours })
    }

    fn receive_device(&self, socket: OwnedFd) -> io::Result<Option<OwnedFd>> {
        let mut byte = [0u8; 1];
        let mut iov = libc::iovec { iov_base: byte.as_mut_ptr() as *mut libc::c_void, iov_len: 1 };
        let mut control = [0u8; 64];
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = control.len() as _;
        counted(unsafe { libc::recvmsg(socket.as_raw_fd(), &mut msg, 0) })?;
        unsafe {
            let mut c = libc::CMSG_FIRSTHDR(&msg);
            while !c.is_null() {
                if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                    let fd = std::ptr::read_unaligned(libc::CMSG_DATA(c) as *const libc::c_int);
                    libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                    return Ok(Some(OwnedFd::from_raw_fd(fd)));
                }
                c = libc::CMSG_NXTHDR(&msg, c);
            }
        }
        Ok(None)
    }

    fn run_quiet(&self, program: &str, args: &[OsString]) {
        let _ = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    fn statx_mount_id(&self, path: &CStr, mask: u32) -> Option<(u32, u64)> {
        statx_mount_id(path, mask)
    }

    fn read_to_string(&self, path: &str) -> io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn spawn_guard(&self, plan: &GuardPlan) -> io::Result<Guard> {
        let mut argv: Vec<*const libc::c_char> = Vec::new();
        if let Some(helper) = &plan.helper {
            argv.extend(helper.iter().map(|a| a.as_ptr()));
            argv.push(std::ptr::null());
        }
        let (r, w) = pipe()?;
        let (ready_r, ready_w) = match pipe() {
            Ok(ready) => ready,
            Err(e) => {
                unsafe {
                    libc::close(r);
                    libc::close(w);
                }
                return Err(e);
            }
        };
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            let e = io::Error::last_os_error();
            unsafe {
                libc::close(r);
                libc::close(w);
                libc::close(ready_r);
                libc::close(ready_w);
            }
            return Err(e);
        }
        if pid == 0 {
            unsafe { guard_child(plan, &argv, r, ready_w) }
        }
        unsafe {
            libc::close(r);
            libc::close(ready_w);
            let mut byte = [0u8; 1];
            while libc::read(ready_r, byte.as_mut_ptr() as *mut libc::c_void, 1) < 0
                && io::Error::last_os_error().raw_os_error() == Some(libc::EINTR)
            {}
            libc::close(ready_r);
            Ok(Guard { pipe: OwnedFd::from_raw_fd(w), pid })
        }
    }

    fn stand_down(&self, guard: Guard) {
        let Guard { pipe, pid } = guard;
        unsafe {
            libc::write(pipe.as_raw_fd(), [1u8].as_ptr() as *const libc::c_void, 1);
            drop(pipe);
            libc::waitpid(pid, std::ptr::null_mut(), 0);
        }
    }

    fn detach(&self, target: &CStr) {
        unsafe { libc::umount2(target.as_ptr(), libc::MNT_DETACH) };
    }

    fn read(&self, device: &OwnedFd, buf: &mut [u8]) -> io::Result<usize> {
        counted(unsafe { libc::read(device.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, buf.len()) })
    }

    fn write(&self, device: &OwnedFd, bytes: &[u8]) -> io::Result<usize> {
        counted(unsafe { libc::write(device.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) })
    }

    fn backing_open(&self, device: &OwnedFd, fd: RawFd) -> io::Result<i32> {
        let map = fuse_backing_map { fd, flags: 0, padding: 0 };
        checked(unsafe { libc::ioctl(device.as_raw_fd(), FUSE_DEV_IOC_BACKING_OPEN as libc::Ioctl, &map as *const _) })
    }

    fn backing_close(&self, device: &OwnedFd, id: u32) -> io::Result<i32> {
        checked(unsafe { libc::ioctl(device.as_raw_fd(), FUSE_DEV_IOC_BACKING_CLOSE as libc::Ioctl, &id as *const _) })
    }
}
