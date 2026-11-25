pub mod abi;

use abi::*;
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Errno(pub i32);

impl Errno {
    pub const ENOENT: Errno = Errno(libc::ENOENT);
    pub const ENOSYS: Errno = Errno(libc::ENOSYS);
    pub const EIO: Errno = Errno(libc::EIO);
    pub const EACCES: Errno = Errno(libc::EACCES);
    pub const EPERM: Errno = Errno(libc::EPERM);
    pub const EEXIST: Errno = Errno(libc::EEXIST);
    pub const ENOTDIR: Errno = Errno(libc::ENOTDIR);
    pub const EISDIR: Errno = Errno(libc::EISDIR);
    pub const EINVAL: Errno = Errno(libc::EINVAL);
    pub const ENOTEMPTY: Errno = Errno(libc::ENOTEMPTY);
    pub const ESTALE: Errno = Errno(libc::ESTALE);
    pub const EXDEV: Errno = Errno(libc::EXDEV);
    pub const EOPNOTSUPP: Errno = Errno(libc::EOPNOTSUPP);
    pub const EROFS: Errno = Errno(libc::EROFS);
    pub const EBADF: Errno = Errno(libc::EBADF);
    pub const ENOTTY: Errno = Errno(libc::ENOTTY);
    pub const ENAMETOOLONG: Errno = Errno(libc::ENAMETOOLONG);
    pub const EBUSY: Errno = Errno(libc::EBUSY);

    pub fn last() -> Errno {
        Errno(io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO))
    }
}

impl From<io::Error> for Errno {
    fn from(e: io::Error) -> Self {
        Errno(e.raw_os_error().unwrap_or(libc::EIO))
    }
}

pub type Res<T> = Result<T, Errno>;

#[derive(Clone, Copy, Debug)]
pub struct Ctx {
    pub uid: u32,
    pub gid: u32,
    pub pid: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct Entry {
    pub nodeid: u64,
    pub generation: u64,
    pub attr: fuse_attr,
    pub entry_ttl: Duration,
    pub attr_ttl: Duration,
}

impl Entry {
    pub fn negative(ttl: Duration) -> Entry {
        Entry {
            nodeid: 0,
            generation: 0,
            attr: fuse_attr::default(),
            entry_ttl: ttl,
            attr_ttl: Duration::ZERO,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Opened {
    pub fh: u64,
    pub flags: u32,
    pub backing_id: i32,
}

#[derive(Clone, Copy, Debug)]
pub enum TimeSet {
    Now,
    At(i64, u32),
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SetAttr {
    pub fh: Option<u64>,
    pub size: Option<u64>,
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub atime: Option<TimeSet>,
    pub mtime: Option<TimeSet>,
}

pub struct DirSink<'a> {
    buf: &'a mut Vec<u8>,
    cap: usize,
}

impl DirSink<'_> {
    pub fn push(&mut self, ino: u64, next_off: u64, dtype: u32, name: &[u8]) -> bool {
        let ent = std::mem::size_of::<fuse_dirent>() + name.len();
        let padded = (ent + 7) & !7;
        assert!(padded >= ent);
        assert!(!name.contains(&0), "dirent names carry no NUL");
        if self.buf.len() + padded > self.cap {
            return false;
        }
        let d = fuse_dirent { ino, off: next_off, namelen: name.len() as u32, typ: dtype };
        self.buf.extend_from_slice(as_bytes(&d));
        self.buf.extend_from_slice(name);
        self.buf.resize(self.buf.len() + (padded - ent), 0);
        assert!(self.buf.len() <= self.cap);
        assert_eq!(self.buf.len() % 8, 0, "dirents stay 8-byte aligned");
        true
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Caps {
    pub minor: u32,
    pub passthrough: bool,
    pub max_write: u32,
}

#[allow(unused_variables)]
pub trait Filesystem: Send + Sync + 'static {
    fn lookup(&self, ctx: &Ctx, parent: u64, name: &[u8]) -> Res<Entry>;
    fn forget(&self, nodeid: u64, nlookup: u64) {}
    fn getattr(&self, ctx: &Ctx, nodeid: u64, fh: Option<u64>) -> Res<(fuse_attr, Duration)>;
    fn setattr(&self, ctx: &Ctx, nodeid: u64, set: &SetAttr) -> Res<(fuse_attr, Duration)> {
        Err(Errno::EROFS)
    }
    fn readlink(&self, ctx: &Ctx, nodeid: u64) -> Res<Vec<u8>> {
        Err(Errno::EINVAL)
    }
    fn symlink(&self, ctx: &Ctx, parent: u64, name: &[u8], target: &[u8]) -> Res<Entry> {
        Err(Errno::EROFS)
    }
    fn mknod(&self, ctx: &Ctx, parent: u64, name: &[u8], mode: u32, rdev: u32) -> Res<Entry> {
        Err(Errno::EROFS)
    }
    fn mkdir(&self, ctx: &Ctx, parent: u64, name: &[u8], mode: u32) -> Res<Entry> {
        Err(Errno::EROFS)
    }
    fn unlink(&self, ctx: &Ctx, parent: u64, name: &[u8]) -> Res<()> {
        Err(Errno::EROFS)
    }
    fn rmdir(&self, ctx: &Ctx, parent: u64, name: &[u8]) -> Res<()> {
        Err(Errno::EROFS)
    }
    fn rename(
        &self,
        ctx: &Ctx,
        parent: u64,
        name: &[u8],
        newparent: u64,
        newname: &[u8],
        flags: u32,
    ) -> Res<()> {
        Err(Errno::EROFS)
    }
    fn link(&self, ctx: &Ctx, nodeid: u64, newparent: u64, newname: &[u8]) -> Res<Entry> {
        Err(Errno::EROFS)
    }
    fn open(&self, ctx: &Ctx, nodeid: u64, flags: u32) -> Res<Opened>;
    fn create(
        &self,
        ctx: &Ctx,
        parent: u64,
        name: &[u8],
        mode: u32,
        flags: u32,
    ) -> Res<(Entry, Opened)> {
        Err(Errno::EROFS)
    }
    fn read(
        &self,
        ctx: &Ctx,
        nodeid: u64,
        fh: u64,
        offset: u64,
        size: u32,
        out: &mut Vec<u8>,
    ) -> Res<()>;
    fn write(&self, ctx: &Ctx, nodeid: u64, fh: u64, offset: u64, data: &[u8]) -> Res<u32> {
        Err(Errno::EROFS)
    }
    fn release(&self, nodeid: u64, fh: u64, flags: u32) {}
    fn fsync(&self, ctx: &Ctx, nodeid: u64, fh: u64, datasync: bool) -> Res<()> {
        Ok(())
    }
    fn readdir(
        &self,
        ctx: &Ctx,
        nodeid: u64,
        fh: u64,
        offset: u64,
        sink: &mut DirSink<'_>,
    ) -> Res<()>;
    fn statfs(&self, ctx: &Ctx, nodeid: u64) -> Res<fuse_kstatfs> {
        Ok(fuse_kstatfs {
            blocks: 1 << 30,
            bfree: 1 << 29,
            bavail: 1 << 29,
            files: 1 << 30,
            ffree: 1 << 29,
            bsize: 4096,
            namelen: 255,
            frsize: 4096,
            ..Default::default()
        })
    }
    fn fallocate(
        &self,
        ctx: &Ctx,
        nodeid: u64,
        fh: u64,
        offset: u64,
        length: u64,
        mode: u32,
    ) -> Res<()> {
        Err(Errno::EOPNOTSUPP)
    }
    fn lseek(&self, ctx: &Ctx, nodeid: u64, fh: u64, offset: u64, whence: u32) -> Res<u64> {
        Err(Errno::ENOSYS)
    }
    fn destroy(&self) {}
}

fn as_bytes<T: Copy>(v: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v as *const T as *const u8, std::mem::size_of::<T>()) }
}

fn read_struct<T: Copy>(b: &[u8]) -> Option<T> {
    if b.len() < std::mem::size_of::<T>() {
        return None;
    }
    Some(unsafe { std::ptr::read_unaligned(b.as_ptr() as *const T) })
}



pub struct Session {
    dev: OwnedFd,
    mountpoint: PathBuf,
    caps: Caps,
    unmounted: AtomicBool,
    helper: bool,
    guard: std::sync::Mutex<Option<Guard>>,
    mount_id: Option<u64>,
    connected: AtomicBool,
}

struct Guard {
    pipe: OwnedFd,
    pid: libc::pid_t,
}

#[derive(Clone, Debug)]
pub struct MountOptions {
    pub fsname: String,
    pub allow_other: bool,
    pub default_permissions: bool,
    pub passthrough: bool,
    pub auto_unmount: bool,
}

impl Default for MountOptions {
    fn default() -> Self {
        MountOptions {
            fsname: "fusewire".into(),
            allow_other: true,
            default_permissions: true,
            passthrough: true,
            auto_unmount: false,
        }
    }
}

const MAX_WRITE: u32 = 1 << 20;
const BUF_SIZE: usize = MAX_WRITE as usize + 2 * 4096;
const _: () = assert!(MAX_WRITE % 4096 == 0);
const _: () = assert!(MAX_WRITE / 4096 <= u16::MAX as u32);
const _: () = assert!(BUF_SIZE % 8 == 0);
const _: () = assert!(BUF_SIZE >= FUSE_MIN_READ_BUFFER);
const _: () = assert!(
    BUF_SIZE >= MAX_WRITE as usize + std::mem::size_of::<fuse_in_header>() + std::mem::size_of::<fuse_write_in>()
);

impl Session {
    pub fn mount(mountpoint: &Path, opts: &MountOptions) -> io::Result<Session> {
        let dev = open_dev()?;
        let meta = std::fs::metadata(mountpoint)?;
        if !meta.is_dir() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "mountpoint is not a directory"));
        }
        let mut data = format!(
            "fd={},rootmode=40000,user_id={},group_id={}",
            dev.as_raw_fd(),
            unsafe { libc::geteuid() },
            unsafe { libc::getegid() }
        );
        if opts.allow_other {
            data.push_str(",allow_other");
        }
        if opts.default_permissions {
            data.push_str(",default_permissions");
        }
        let src = CString::new(opts.fsname.clone()).unwrap();
        let tgt = CString::new(mountpoint.as_os_str().as_encoded_bytes()).unwrap();
        let fstype = CString::new(format!("fuse.{}", opts.fsname)).unwrap();
        let cdata = CString::new(data).unwrap();
        let flags = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOATIME;
        let r = unsafe {
            libc::mount(
                src.as_ptr(),
                tgt.as_ptr(),
                fstype.as_ptr(),
                flags,
                cdata.as_ptr() as *const libc::c_void,
            )
        };
        let mut helper = false;
        let mut guard = None;
        let dev = if r == 0 {
            dev
        } else {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EPERM) {
                return Err(e);
            }
            helper = true;
            mount_with_helper(mountpoint, opts)
                .map_err(|h| io::Error::new(h.kind(), format!("mount: {e}; fusermount3: {h}")))?
        };
        let mount_id = mount_id(&tgt);
        if opts.auto_unmount {
            guard = spawn_guard(&tgt, mount_id, helper).ok();
        }
        let mut sess = Session {
            dev,
            mountpoint: mountpoint.to_path_buf(),
            caps: Caps::default(),
            unmounted: AtomicBool::new(false),
            helper,
            guard: std::sync::Mutex::new(guard),
            mount_id,
            connected: AtomicBool::new(true),
        };
        if let Err(e) = sess.handshake(opts) {
            sess.unmount();
            return Err(e);
        }
        assert_eq!(sess.caps.max_write, MAX_WRITE);
        assert!(sess.caps.minor >= 31);
        Ok(sess)
    }

    fn handshake(&mut self, opts: &MountOptions) -> io::Result<()> {
        let mut buf = vec![0u8; BUF_SIZE];
        let n = loop {
            let n = unsafe {
                libc::read(self.dev.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, buf.len())
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(e);
            }
            break n as usize;
        };
        let hdr: fuse_in_header =
            read_struct(&buf[..n]).ok_or_else(|| io::Error::other("short INIT"))?;
        if hdr.opcode != FUSE_INIT {
            return Err(io::Error::other("first FUSE request was not INIT"));
        }
        let init: fuse_init_in = read_struct(&buf[std::mem::size_of::<fuse_in_header>()..n])
            .ok_or_else(|| io::Error::other("short INIT body"))?;
        if init.major != FUSE_KERNEL_VERSION || init.minor < 31 {
            return Err(io::Error::other(format!(
                "unsupported FUSE protocol {}.{}",
                init.major, init.minor
            )));
        }
        let kflags = init.flags as u64 | ((init.flags2 as u64) << 32);
        let mut want = FUSE_ASYNC_READ
            | FUSE_BIG_WRITES
            | FUSE_ATOMIC_O_TRUNC
            | FUSE_PARALLEL_DIROPS
            | FUSE_MAX_PAGES
            | FUSE_CACHE_SYMLINKS
            | FUSE_NO_OPENDIR_SUPPORT
            | FUSE_ASYNC_DIO
            | FUSE_DIRECT_IO_ALLOW_MMAP
            | FUSE_INIT_EXT;
        let passthrough = opts.passthrough && kflags & FUSE_PASSTHROUGH != 0;
        if passthrough {
            want |= FUSE_PASSTHROUGH;
        } else {
            want |= FUSE_WRITEBACK_CACHE;
        }
        let flags = want & kflags;
        let out = fuse_init_out {
            major: FUSE_KERNEL_VERSION,
            minor: FUSE_KERNEL_MINOR_VERSION.min(init.minor),
            max_readahead: init.max_readahead,
            flags: flags as u32,
            max_background: 256,
            congestion_threshold: 192,
            max_write: MAX_WRITE,
            time_gran: 1,
            max_pages: (MAX_WRITE / 4096) as u16,
            map_alignment: 0,
            flags2: (flags >> 32) as u32,
            max_stack_depth: if passthrough { 2 } else { 0 },
            request_timeout: 0,
            unused: [0; 11],
        };
        assert!(out.minor >= 31);
        assert!(out.minor <= FUSE_KERNEL_MINOR_VERSION);
        assert!(flags & !kflags == 0, "only flags the kernel offers");
        assert!(!(flags & FUSE_PASSTHROUGH != 0 && flags & FUSE_WRITEBACK_CACHE != 0));
        reply(self.dev.as_raw_fd(), hdr.unique, 0, &[as_bytes(&out)])?;
        self.caps = Caps {
            minor: out.minor,
            passthrough: flags & FUSE_PASSTHROUGH != 0,
            max_write: MAX_WRITE,
        };
        Ok(())
    }

    pub fn caps(&self) -> Caps {
        self.caps
    }

    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    pub fn spawn_workers<F: Filesystem>(
        self: &Arc<Self>,
        fs: Arc<F>,
        threads: usize,
    ) -> io::Result<Vec<std::thread::JoinHandle<()>>> {
        let mut handles = Vec::new();
        for i in 0..threads.max(1) {
            let fd = self.clone_dev()?;
            let fs = fs.clone();
            let sess = self.clone();
            let h = std::thread::Builder::new()
                .name(format!("fuse-{i}"))
                .spawn(move || worker(sess, fd, fs))?;
            handles.push(h);
        }
        assert_eq!(handles.len(), threads.max(1));
        Ok(handles)
    }

    fn clone_dev(&self) -> io::Result<OwnedFd> {
        let fd = open_dev()?;
        let master: u32 = self.dev.as_raw_fd() as u32;
        let r = unsafe { libc::ioctl(fd.as_raw_fd(), FUSE_DEV_IOC_CLONE as libc::Ioctl, &master as *const u32) };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(fd)
    }

    pub fn backing_open(&self, fd: RawFd) -> io::Result<i32> {
        let map = fuse_backing_map { fd, flags: 0, padding: 0 };
        let r = unsafe {
            libc::ioctl(self.dev.as_raw_fd(), FUSE_DEV_IOC_BACKING_OPEN as libc::Ioctl, &map as *const _)
        };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(r)
    }

    pub fn backing_close(&self, id: i32) -> io::Result<()> {
        let id = id as u32;
        let r = unsafe {
            libc::ioctl(self.dev.as_raw_fd(), FUSE_DEV_IOC_BACKING_CLOSE as libc::Ioctl, &id as *const u32)
        };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn notify_inval_entry(&self, parent: u64, name: &[u8]) -> io::Result<()> {
        let out = fuse_notify_inval_entry_out { parent, namelen: name.len() as u32, flags: 0 };
        let r = reply(
            self.dev.as_raw_fd(),
            0,
            FUSE_NOTIFY_INVAL_ENTRY,
            &[as_bytes(&out), name, &[0u8]],
        );
        match r {
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => Ok(()),
            r => r,
        }
    }

    pub fn auto_unmount_active(&self) -> bool {
        self.guard.lock().unwrap().is_some()
    }

    pub fn shown(&self) -> bool {
        if self.unmounted.load(Ordering::SeqCst) || !self.connected.load(Ordering::SeqCst) {
            return false;
        }
        let Ok(tgt) = CString::new(self.mountpoint.as_os_str().as_encoded_bytes()) else { return false };
        match (self.mount_id, mount_id(&tgt)) {
            (Some(mine), Some(there)) => mine == there,
            (Some(_), None) => false,
            (None, _) => true,
        }
    }

    pub fn unmount(&self) {
        if self.unmounted.swap(true, Ordering::SeqCst) {
            return;
        }
        let tgt = CString::new(self.mountpoint.as_os_str().as_encoded_bytes()).ok();
        if let Some(Guard { pipe, pid }) = self.guard.lock().unwrap().take() {
            unsafe {
                libc::write(pipe.as_raw_fd(), [1u8].as_ptr() as *const libc::c_void, 1);
                drop(pipe);
                libc::waitpid(pid, std::ptr::null_mut(), 0);
            }
        }
        let ours = match (self.mount_id, tgt.as_deref().and_then(mount_id)) {
            (Some(mine), Some(there)) => mine == there,
            _ => true,
        };
        if !ours {
            return;
        }
        if self.helper {
            unmount_with_helper(&self.mountpoint);
        } else if let Some(tgt) = tgt {
            unsafe { libc::umount2(tgt.as_ptr(), libc::MNT_DETACH) };
        }
    }
}

fn mount_id(path: &std::ffi::CStr) -> Option<u64> {
    const AT_STATX_DONT_SYNC: libc::c_int = 0x4000;
    const STATX_MNT_ID: u32 = 0x1000;
    const STATX_MNT_ID_UNIQUE: u32 = 0x4000;
    let mut buf = [0u64; 32];
    let r = unsafe {
        libc::syscall(
            libc::SYS_statx,
            libc::AT_FDCWD,
            path.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW | AT_STATX_DONT_SYNC,
            STATX_MNT_ID | STATX_MNT_ID_UNIQUE,
            buf.as_mut_ptr(),
        )
    };
    let mask = (buf[0] & 0xffff_ffff) as u32;
    (r == 0 && mask & (STATX_MNT_ID | STATX_MNT_ID_UNIQUE) != 0).then_some(buf[18])
}

fn spawn_guard(tgt: &std::ffi::CStr, mount_id: Option<u64>, helper: bool) -> io::Result<Guard> {
    let name = c"unmount-guard";
    let mut title = b"unmount-guard: ".to_vec();
    title.extend_from_slice(tgt.to_bytes());
    let args: Option<(usize, usize)> = std::fs::read_to_string("/proc/self/stat").ok().and_then(|s| {
        let rest = &s[s.rfind(')')? + 1..];
        let mut f = rest.split_ascii_whitespace().skip(45);
        let (start, end) = (f.next()?.parse().ok()?, f.next()?.parse().ok()?);
        (start != 0 && end > start).then_some((start, end))
    });
    let argv_owned = [c"fusermount3", c"-u", c"-z", c"-q", c"--", tgt];
    let mut argv: Vec<*const libc::c_char> = argv_owned.iter().map(|a| a.as_ptr()).collect();
    argv.push(std::ptr::null());
    let mut p = [0 as libc::c_int; 2];
    if unsafe { libc::pipe2(p.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let (r, w) = (p[0], p[1]);
    let mut q = [0 as libc::c_int; 2];
    if unsafe { libc::pipe2(q.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        let e = io::Error::last_os_error();
        unsafe {
            libc::close(r);
            libc::close(w);
        }
        return Err(e);
    }
    let (ready_r, ready_w) = (q[0], q[1]);
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
        unsafe {
            libc::setsid();
            libc::prctl(libc::PR_SET_NAME, name.as_ptr());
            if let Some((start, end)) = args {
                let (area, len) = (start as *mut u8, end - start);
                std::ptr::write_bytes(area, 0, len);
                std::ptr::copy_nonoverlapping(title.as_ptr(), area, title.len().min(len - 1));
            }
            for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT, libc::SIGPIPE] {
                libc::signal(sig, libc::SIG_IGN);
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
            let still_ours = match (mount_id, self::mount_id(tgt)) {
                (Some(mine), Some(there)) => mine == there,
                _ => true,
            };
            if still_ours {
                if helper {
                    libc::execvp(argv[0], argv.as_ptr());
                    libc::_exit(1);
                }
                libc::umount2(tgt.as_ptr(), libc::MNT_DETACH);
            }
            libc::_exit(0);
        }
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

impl Drop for Session {
    fn drop(&mut self) {
        self.unmount();
    }
}

pub fn unmount_with_helper(mountpoint: &Path) {
    let _ = std::process::Command::new("fusermount3")
        .args(["-u", "-z", "-q", "--"])
        .arg(mountpoint)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

fn mount_with_helper(mountpoint: &Path, opts: &MountOptions) -> io::Result<OwnedFd> {
    let mut pair = [0 as libc::c_int; 2];
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, pair.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let (ours, theirs) = unsafe { (OwnedFd::from_raw_fd(pair[0]), OwnedFd::from_raw_fd(pair[1])) };
    unsafe { libc::fcntl(ours.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    let mut options = format!("rw,nosuid,nodev,noatime,fsname={0},subtype={0}", opts.fsname);
    if opts.default_permissions {
        options.push_str(",default_permissions");
    }
    if opts.allow_other {
        options.push_str(",allow_other");
    }
    let mut cmd = std::process::Command::new("fusermount3");
    cmd.arg("-o").arg(options).arg("--").arg(mountpoint).env("_FUSE_COMMFD", theirs.as_raw_fd().to_string());
    cmd.stdin(std::process::Stdio::null());
    let out = cmd.output()?;
    drop(theirs);
    if !out.status.success() {
        return Err(io::Error::other(String::from_utf8_lossy(&out.stderr).trim().to_string()));
    }
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec { iov_base: byte.as_mut_ptr() as *mut libc::c_void, iov_len: 1 };
    let mut control = [0u8; 64];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = control.len() as _;
    let n = unsafe { libc::recvmsg(ours.as_raw_fd(), &mut msg, 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let fd = std::ptr::read_unaligned(libc::CMSG_DATA(c) as *const libc::c_int);
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                return Ok(OwnedFd::from_raw_fd(fd));
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    Err(io::Error::other("fusermount3 did not hand over the connection"))
}

fn open_dev() -> io::Result<OwnedFd> {
    let fd = unsafe { libc::open(c"/dev/fuse".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn reply(fd: RawFd, unique: u64, error: i32, parts: &[&[u8]]) -> io::Result<()> {
    let len: usize = std::mem::size_of::<fuse_out_header>() + parts.iter().map(|p| p.len()).sum::<usize>();
    assert!(parts.len() < 5, "iov has room for the header and four parts");
    assert!(len <= u32::MAX as usize);
    if unique != 0 {
        assert!(error <= 0);
    } else {
        assert!(error > 0);
    }
    let hdr = fuse_out_header { len: len as u32, error, unique };
    let mut iov: [libc::iovec; 5] = [libc::iovec { iov_base: std::ptr::null_mut(), iov_len: 0 }; 5];
    iov[0] = libc::iovec {
        iov_base: &hdr as *const _ as *mut libc::c_void,
        iov_len: std::mem::size_of::<fuse_out_header>(),
    };
    let mut n = 1;
    for p in parts {
        if p.is_empty() {
            continue;
        }
        iov[n] = libc::iovec { iov_base: p.as_ptr() as *mut libc::c_void, iov_len: p.len() };
        n += 1;
    }
    assert_eq!(iov[..n].iter().map(|v| v.iov_len).sum::<usize>(), len);
    let r = unsafe { libc::writev(fd, iov.as_ptr(), n as i32) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn ttl_parts(d: Duration) -> (u64, u32) {
    (d.as_secs(), d.subsec_nanos())
}

fn entry_out(e: &Entry) -> fuse_entry_out {
    let (es, en) = ttl_parts(e.entry_ttl);
    let (as_, an) = ttl_parts(e.attr_ttl);
    fuse_entry_out {
        nodeid: e.nodeid,
        generation: e.generation,
        entry_valid: es,
        attr_valid: as_,
        entry_valid_nsec: en,
        attr_valid_nsec: an,
        attr: e.attr,
    }
}

fn attr_out(a: &fuse_attr, ttl: Duration) -> fuse_attr_out {
    let (s, n) = ttl_parts(ttl);
    fuse_attr_out { attr_valid: s, attr_valid_nsec: n, dummy: 0, attr: *a }
}

fn open_out(o: &Opened) -> fuse_open_out {
    if o.flags & FOPEN_PASSTHROUGH != 0 {
        assert!(o.backing_id > 0, "passthrough needs a backing id");
    }
    fuse_open_out { fh: o.fh, open_flags: o.flags, backing_id: o.backing_id }
}

fn worker<F: Filesystem>(sess: Arc<Session>, dev: OwnedFd, fs: Arc<F>) {
    let fd = dev.as_raw_fd();
    let mut storage = vec![0u64; BUF_SIZE / 8];
    let mut handler = RequestHandler::new(sess.caps.max_write);
    loop {
        let n = unsafe { libc::read(fd, storage.as_mut_ptr() as *mut libc::c_void, BUF_SIZE) };
        if n == 0 {
            sess.connected.store(false, Ordering::SeqCst);
            break;
        }
        if n < 0 {
            match io::Error::last_os_error().raw_os_error() {
                Some(libc::EINTR) | Some(libc::EAGAIN) | Some(libc::ENOENT) => continue,
                _ => {
                    sess.connected.store(false, Ordering::SeqCst);
                    break;
                }
            }
        }
        let n = n as usize;
        assert!(n <= BUF_SIZE);
        let buf = unsafe { std::slice::from_raw_parts(storage.as_ptr() as *const u8, n) };
        let result = handler.handle(&*fs, buf);
        if !result.bytes.is_empty() {
            unsafe { libc::write(fd, result.bytes.as_ptr().cast(), result.bytes.len()) };
        }
        if result.destroyed {
            sess.connected.store(false, Ordering::SeqCst);
            break;
        }
    }
}

pub struct RequestHandler {
    scratch: Vec<u8>,
    reply: Vec<u8>,
    max_write: u32,
}

pub struct Handled<'a> {
    pub bytes: &'a [u8],
    pub destroyed: bool,
}

impl RequestHandler {
    pub fn new(max_write: u32) -> Self {
        Self { scratch: Vec::new(), reply: Vec::new(), max_write }
    }

    pub fn handle<F: Filesystem>(&mut self, fs: &F, request: &[u8]) -> Handled<'_> {
        self.reply.clear();
        let Some(hdr) = read_struct::<fuse_in_header>(request) else {
            return Handled { bytes: &self.reply, destroyed: false };
        };
        if hdr.unique == 0 {
            return Handled { bytes: &self.reply, destroyed: false };
        }
        if hdr.len as usize != request.len() {
            if !matches!(hdr.opcode, FUSE_FORGET | FUSE_BATCH_FORGET | FUSE_INTERRUPT) {
                encode_reply(&mut self.reply, hdr.unique, -libc::EINVAL, &[]);
            }
            return Handled { bytes: &self.reply, destroyed: false };
        }
        let destroyed = hdr.opcode == FUSE_DESTROY;
        if destroyed {
            fs.destroy();
            encode_reply(&mut self.reply, hdr.unique, 0, &[]);
        } else {
            let body = &request[std::mem::size_of::<fuse_in_header>()..];
            dispatch(fs, &mut self.reply, &hdr, body, &mut self.scratch, self.max_write);
        }
        Handled { bytes: &self.reply, destroyed }
    }

    #[cfg(test)]
    fn invariants(&self) -> bool {
        self.reply.is_empty() || read_struct::<fuse_out_header>(&self.reply)
            .is_some_and(|header| header.len as usize == self.reply.len() && header.unique != 0 && header.error <= 0)
    }
}

fn encode_reply(out: &mut Vec<u8>, unique: u64, error: i32, parts: &[&[u8]]) {
    let len = std::mem::size_of::<fuse_out_header>() + parts.iter().map(|part| part.len()).sum::<usize>();
    out.extend(as_bytes(&fuse_out_header { len: len as u32, error, unique }));
    for part in parts {
        out.extend_from_slice(part);
    }
}

macro_rules! arg {
    ($ty:ty, $body:expr, $reply:expr, $hdr:expr) => {
        match read_struct::<$ty>($body) {
            Some(v) => v,
            None => {
                encode_reply($reply, $hdr.unique, -libc::EINVAL, &[]);
                return;
            }
        }
    };
}

macro_rules! names {
    ($body:expr, $reply:expr, $hdr:expr) => {
        match abi::name_arg($body) {
            Some(value) => value,
            None => {
                encode_reply($reply, $hdr.unique, -libc::EINVAL, &[]);
                return;
            }
        }
    };
}

fn send<T: Copy>(out: &mut Vec<u8>, unique: u64, result: Res<T>) {
    match result {
        Ok(v) => encode_reply(out, unique, 0, &[as_bytes(&v)]),
        Err(err) => encode_reply(out, unique, -err.0, &[]),
    }
}

fn send_empty(out: &mut Vec<u8>, unique: u64, result: Res<()>) {
    send(out, unique, result);
}

fn dispatch<F: Filesystem>(
    fs: &F,
    reply: &mut Vec<u8>,
    hdr: &fuse_in_header,
    body: &[u8],
    out: &mut Vec<u8>,
    max_write: u32,
) {
    let ctx = Ctx { uid: hdr.uid, gid: hdr.gid, pid: hdr.pid };
    let u = hdr.unique;
    let node = hdr.nodeid;
    match hdr.opcode {
        FUSE_LOOKUP => {
            let (name, _) = names!(body, reply, hdr);
            send(reply, u, fs.lookup(&ctx, node, name).map(|e| entry_out(&e)));
        }
        FUSE_FORGET => {
            if let Some(f) = read_struct::<fuse_forget_in>(body) {
                fs.forget(node, f.nlookup);
            }
        }
        FUSE_BATCH_FORGET => {
            if let Some(b) = read_struct::<fuse_batch_forget_in>(body) {
                let mut rest = &body[std::mem::size_of::<fuse_batch_forget_in>()..];
                for _ in 0..b.count {
                    let Some(one) = read_struct::<fuse_forget_one>(rest) else { break };
                    fs.forget(one.nodeid, one.nlookup);
                    rest = &rest[std::mem::size_of::<fuse_forget_one>()..];
                }
            }
        }
        FUSE_GETATTR => {
            let a = arg!(fuse_getattr_in, body, reply, hdr);
            let fh = (a.getattr_flags & FUSE_GETATTR_FH != 0).then_some(a.fh);
            send(reply, u, fs.getattr(&ctx, node, fh).map(|(a, t)| attr_out(&a, t)));
        }
        FUSE_SETATTR => {
            let a = arg!(fuse_setattr_in, body, reply, hdr);
            let v = a.valid;
            let time = |set: u32, now: u32, s: u64, ns: u32| {
                if v & now != 0 {
                    Some(TimeSet::Now)
                } else if v & set != 0 {
                    Some(TimeSet::At(s as i64, ns))
                } else {
                    None
                }
            };
            let set = SetAttr {
                fh: (v & FATTR_FH != 0).then_some(a.fh),
                size: (v & FATTR_SIZE != 0).then_some(a.size),
                mode: (v & FATTR_MODE != 0).then_some(a.mode),
                uid: (v & FATTR_UID != 0).then_some(a.uid),
                gid: (v & FATTR_GID != 0).then_some(a.gid),
                atime: time(FATTR_ATIME, FATTR_ATIME_NOW, a.atime, a.atimensec),
                mtime: time(FATTR_MTIME, FATTR_MTIME_NOW, a.mtime, a.mtimensec),
            };
            send(reply, u, fs.setattr(&ctx, node, &set).map(|(a, t)| attr_out(&a, t)));
        }
        FUSE_READLINK => {
            match fs.readlink(&ctx, node) {
                Ok(t) => encode_reply(reply, u, 0, &[&t]),
                Err(e) => encode_reply(reply, u, -e.0, &[]),
            };
        }
        FUSE_SYMLINK => {
            let (name, rest) = names!(body, reply, hdr);
            let Some((target, _)) = abi::nul_arg(rest) else {
                encode_reply(reply, u, -libc::EINVAL, &[]);
                return;
            };
            send(reply, u, fs.symlink(&ctx, node, name, target).map(|e| entry_out(&e)));
        }
        FUSE_MKNOD => {
            let a = arg!(fuse_mknod_in, body, reply, hdr);
            let (name, _) = names!(&body[std::mem::size_of::<fuse_mknod_in>()..], reply, hdr);
            let mode = a.mode & !a.umask;
            send(reply, u, fs.mknod(&ctx, node, name, mode, a.rdev).map(|e| entry_out(&e)));
        }
        FUSE_MKDIR => {
            let a = arg!(fuse_mkdir_in, body, reply, hdr);
            let (name, _) = names!(&body[std::mem::size_of::<fuse_mkdir_in>()..], reply, hdr);
            let mode = a.mode & !a.umask;
            send(reply, u, fs.mkdir(&ctx, node, name, mode).map(|e| entry_out(&e)));
        }
        FUSE_UNLINK => {
            let (name, _) = names!(body, reply, hdr);
            send_empty(reply, u, fs.unlink(&ctx, node, name));
        }
        FUSE_RMDIR => {
            let (name, _) = names!(body, reply, hdr);
            send_empty(reply, u, fs.rmdir(&ctx, node, name));
        }
        FUSE_RENAME => {
            let a = arg!(fuse_rename_in, body, reply, hdr);
            let (name, rest) = names!(&body[std::mem::size_of::<fuse_rename_in>()..], reply, hdr);
            let (newname, _) = names!(rest, reply, hdr);
            send_empty(reply, u, fs.rename(&ctx, node, name, a.newdir, newname, 0));
        }
        FUSE_RENAME2 => {
            let a = arg!(fuse_rename2_in, body, reply, hdr);
            let (name, rest) = names!(&body[std::mem::size_of::<fuse_rename2_in>()..], reply, hdr);
            let (newname, _) = names!(rest, reply, hdr);
            send_empty(reply, u, fs.rename(&ctx, node, name, a.newdir, newname, a.flags));
        }
        FUSE_LINK => {
            let a = arg!(fuse_link_in, body, reply, hdr);
            let (name, _) = names!(&body[std::mem::size_of::<fuse_link_in>()..], reply, hdr);
            send(reply, u, fs.link(&ctx, a.oldnodeid, node, name).map(|e| entry_out(&e)));
        }
        FUSE_OPEN => {
            let a = arg!(fuse_open_in, body, reply, hdr);
            send(reply, u, fs.open(&ctx, node, a.flags).map(|o| open_out(&o)));
        }
        FUSE_CREATE => {
            let a = arg!(fuse_create_in, body, reply, hdr);
            let (name, _) = names!(&body[std::mem::size_of::<fuse_create_in>()..], reply, hdr);
            let mode = a.mode & !a.umask;
            match fs.create(&ctx, node, name, mode, a.flags) {
                Ok((e, o)) => encode_reply(reply, u, 0, &[as_bytes(&entry_out(&e)), as_bytes(&open_out(&o))]),
                Err(e) => encode_reply(reply, u, -e.0, &[]),
            };
        }
        FUSE_READ => {
            let a = arg!(fuse_read_in, body, reply, hdr);
            out.clear();
            let size = a.size.min(max_write);
            match fs.read(&ctx, node, a.fh, a.offset, size, out) {
                Ok(()) => {
                    if out.len() > size as usize {
                        encode_reply(reply, u, -libc::EIO, &[]);
                    } else {
                        encode_reply(reply, u, 0, &[out]);
                    }
                }
                Err(e) => encode_reply(reply, u, -e.0, &[]),
            };
        }
        FUSE_WRITE => {
            let a = arg!(fuse_write_in, body, reply, hdr);
            let data = &body[std::mem::size_of::<fuse_write_in>()..];
            let Some(data) = data.get(..a.size as usize) else {
                encode_reply(reply, u, -libc::EINVAL, &[]);
                return;
            };
            send(
                reply,
                u,
                fs.write(&ctx, node, a.fh, a.offset, data)
                    .map(|n| fuse_write_out { size: n, padding: 0 }),
            );
        }
        FUSE_STATFS => {
            send(reply, u, fs.statfs(&ctx, node).map(|st| fuse_statfs_out { st }));
        }
        FUSE_RELEASE | FUSE_RELEASEDIR => {
            if hdr.opcode == FUSE_RELEASE {
                if let Some(a) = read_struct::<fuse_release_in>(body) {
                    fs.release(node, a.fh, a.flags);
                }
            }
            encode_reply(reply, u, 0, &[]);
        }
        FUSE_FSYNC => {
            let a = arg!(fuse_fsync_in, body, reply, hdr);
            send_empty(reply, u, fs.fsync(&ctx, node, a.fh, a.fsync_flags & 1 != 0));
        }
        FUSE_FSYNCDIR | FUSE_SYNCFS => {
            encode_reply(reply, u, 0, &[]);
        }
        FUSE_READDIR => {
            let a = arg!(fuse_read_in, body, reply, hdr);
            out.clear();
            let mut sink = DirSink { buf: out, cap: a.size as usize };
            let r = fs.readdir(&ctx, node, a.fh, a.offset, &mut sink);
            assert!(out.len() <= a.size as usize);
            match r {
                Ok(()) => encode_reply(reply, u, 0, &[out]),
                Err(e) => encode_reply(reply, u, -e.0, &[]),
            };
        }
        FUSE_FALLOCATE => {
            let a = arg!(fuse_fallocate_in, body, reply, hdr);
            send_empty(reply, u, fs.fallocate(&ctx, node, a.fh, a.offset, a.length, a.mode));
        }
        FUSE_LSEEK => {
            let a = arg!(fuse_lseek_in, body, reply, hdr);
            send(
                reply,
                u,
                fs.lseek(&ctx, node, a.fh, a.offset, a.whence).map(|offset| fuse_lseek_out { offset }),
            );
        }
        FUSE_INTERRUPT => {}
        FUSE_FLUSH | FUSE_OPENDIR | FUSE_ACCESS | FUSE_GETXATTR | FUSE_LISTXATTR
        | FUSE_SETXATTR | FUSE_REMOVEXATTR | FUSE_IOCTL | FUSE_POLL | FUSE_BMAP
        | FUSE_COPY_FILE_RANGE | FUSE_GETLK | FUSE_SETLK | FUSE_SETLKW | FUSE_STATX => {
            encode_reply(reply, u, -libc::ENOSYS, &[]);
        }
        FUSE_TMPFILE => {
            encode_reply(reply, u, -libc::EOPNOTSUPP, &[]);
        }
        _ => {
            encode_reply(reply, u, -libc::ENOSYS, &[]);
        }
    }
}

#[cfg(test)]
mod request_tests {
    use std::sync::atomic::AtomicU64;
    use super::*;

    #[derive(Default)]
    struct Memory {
        forgotten: AtomicU64,
        released: AtomicU64,
    }

    impl Filesystem for Memory {
        fn lookup(&self, _: &Ctx, parent: u64, name: &[u8]) -> Res<Entry> {
            if parent != 1 || name != b"file" {
                return Err(Errno(libc::ENOENT));
            }
            Ok(Entry { nodeid: 2, ..Entry::negative(Duration::ZERO) })
        }

        fn getattr(&self, _: &Ctx, node: u64, _: Option<u64>) -> Res<(fuse_attr, Duration)> {
            Ok((fuse_attr { ino: node, size: 4, ..fuse_attr::default() }, Duration::ZERO))
        }

        fn open(&self, _: &Ctx, _: u64, _: u32) -> Res<Opened> {
            Ok(Opened { fh: 17, flags: 0, backing_id: 0 })
        }

        fn read(&self, _: &Ctx, _: u64, _: u64, offset: u64, size: u32, out: &mut Vec<u8>) -> Res<()> {
            let start = offset.min(4) as usize;
            let end = (start + size as usize).min(4);
            out.extend_from_slice(&b"abcd"[start..end]);
            Ok(())
        }

        fn readdir(&self, _: &Ctx, _: u64, _: u64, _: u64, sink: &mut DirSink<'_>) -> Res<()> {
            sink.push(2, 1, libc::DT_REG as u32, b"file");
            Ok(())
        }

        fn release(&self, _: u64, fh: u64, _: u32) {
            self.released.store(fh, Ordering::Relaxed);
        }

        fn forget(&self, _: u64, count: u64) {
            self.forgotten.store(count, Ordering::Relaxed);
        }
    }

    fn request(opcode: u32, nodeid: u64, body: &[u8]) -> Vec<u8> {
        let header = fuse_in_header {
            len: (std::mem::size_of::<fuse_in_header>() + body.len()) as u32, opcode,
            unique: 51, nodeid, uid: 1000, gid: 1000, pid: 100, total_extlen: 0, padding: 0,
        };
        [as_bytes(&header), body].concat()
    }

    #[test]
    fn memory_filesystem_replies_to_kernel_request_shapes() {
        let fs = Memory::default();
        let mut handler = RequestHandler::new(1024);
        let mut run = |opcode, node, body: &[u8]| {
            let frame = request(opcode, node, body);
            let reply = handler.handle(&fs, &frame).bytes.to_vec();
            assert!(handler.invariants());
            reply
        };
        let payload = |reply: Vec<u8>| reply[std::mem::size_of::<fuse_out_header>()..].to_vec();
        let lookup = run(FUSE_LOOKUP, 1, b"file\0");
        assert_eq!(read_struct::<fuse_entry_out>(&payload(lookup)).unwrap().nodeid, 2);
        let getattr = fuse_getattr_in { getattr_flags: 0, dummy: 0, fh: 0 };
        assert_eq!(read_struct::<fuse_attr_out>(&payload(run(FUSE_GETATTR, 2, as_bytes(&getattr)))).unwrap().attr.size, 4);
        let open = fuse_open_in { flags: 0, open_flags: 0 };
        assert_eq!(read_struct::<fuse_open_out>(&payload(run(FUSE_OPEN, 2, as_bytes(&open)))).unwrap().fh, 17);
        let read = fuse_read_in { fh: 17, offset: 1, size: 2, read_flags: 0, lock_owner: 0, flags: 0, padding: 0 };
        assert_eq!(payload(run(FUSE_READ, 2, as_bytes(&read))), b"bc");
        let listing = fuse_read_in { offset: 0, size: 256, ..read };
        assert_eq!(read_struct::<fuse_dirent>(&payload(run(FUSE_READDIR, 1, as_bytes(&listing)))).unwrap().namelen, 4);
        let release = fuse_release_in { fh: 17, flags: 0, release_flags: 0, lock_owner: 0 };
        assert_eq!(read_struct::<fuse_out_header>(&run(FUSE_RELEASE, 2, as_bytes(&release))).unwrap().error, 0);
        assert_eq!(fs.released.load(Ordering::Relaxed), 17);
        assert!(run(FUSE_FORGET, 2, as_bytes(&fuse_forget_in { nlookup: 3 })).is_empty());
        assert_eq!(fs.forgotten.load(Ordering::Relaxed), 3);
        assert!(run(FUSE_INTERRUPT, 0, &1u64.to_ne_bytes()).is_empty());
    }

    #[test]
    fn incomplete_requests_are_rejected_without_losing_the_next_reply() {
        let fs = Memory::default();
        let mut handler = RequestHandler::new(1024);
        let lookup = request(FUSE_LOOKUP, 1, b"file\0");
        for len in 0..lookup.len() {
            handler.handle(&fs, &lookup[..len]);
            assert!(handler.invariants());
        }
        for body in [&[][..], &b"file"[..]] {
            let reply = handler.handle(&fs, &request(FUSE_LOOKUP, 1, body));
            assert_eq!(read_struct::<fuse_out_header>(reply.bytes).unwrap().error, -libc::EINVAL);
            assert!(handler.invariants());
        }
        assert_eq!(read_struct::<fuse_out_header>(handler.handle(&fs, &lookup).bytes).unwrap().error, 0);
        assert!(handler.invariants());
    }
}
