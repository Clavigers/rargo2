pub mod abi;

use rargo_machine::StateMachine;
use abi::*;
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
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
pub struct Ctx<'a> {
    pub uid: u32,
    pub gid: u32,
    pub pid: u32,
    pub effects: &'a Effects,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    Notify(Vec<u8>),
    Register { fd: RawFd, token: u64 },
    Unregister(i32),
}

#[derive(Debug, Default)]
pub struct Effects(std::cell::RefCell<Vec<Effect>>);

impl Effects {
    pub fn register(&self, fd: RawFd, token: u64) {
        self.0.borrow_mut().push(Effect::Register { fd, token });
    }

    pub fn unregister(&self, id: i32) {
        self.0.borrow_mut().push(Effect::Unregister(id));
    }

    pub fn inval_entry(&self, parent: u64, name: &[u8]) {
        self.0.borrow_mut().push(Effect::Notify(inval_entry_bytes(parent, name)));
    }

    fn take(&self) -> Vec<Effect> {
        std::mem::take(&mut *self.0.borrow_mut())
    }
}

fn inval_entry_bytes(parent: u64, name: &[u8]) -> Vec<u8> {
    let out = fuse_notify_inval_entry_out { parent, namelen: name.len() as u32, flags: 0 };
    let mut bytes = Vec::new();
    encode_reply(&mut bytes, 0, FUSE_NOTIFY_INVAL_ENTRY, &[as_bytes(&out), name, &[0]]);
    bytes
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
    fn registered(&self, ctx: &Ctx, token: u64, id: Res<i32>) -> Res<Opened> {
        Err(Errno::EIO)
    }
    fn release(&self, nodeid: u64, fh: u64, flags: u32) {}
    fn release_in(&self, ctx: &Ctx, nodeid: u64, fh: u64, flags: u32) {
        self.release(nodeid, fh, flags)
    }
    fn fsync(&self, ctx: &Ctx, nodeid: u64, fh: u64, data_sync: bool) -> Res<()> {
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

fn require_uring(offered: bool) -> io::Result<()> {
    if offered {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "FUSE-over-io_uring is unavailable. Use Linux 6.14 or newer with CONFIG_FUSE_IO_URING and set fuse.enable_uring=Y.",
    ))
}

const MAX_WRITE: u32 = 1 << 20;
pub const BUF_SIZE: usize = MAX_WRITE as usize + 2 * 4096;
const _: () = assert!(MAX_WRITE % 4096 == 0);
const _: () = assert!(MAX_WRITE / 4096 <= u16::MAX as u32);
const _: () = assert!(BUF_SIZE % 8 == 0);
const _: () = assert!(BUF_SIZE >= FUSE_MIN_READ_BUFFER);
const _: () = assert!(
    BUF_SIZE >= MAX_WRITE as usize + std::mem::size_of::<fuse_in_header>() + std::mem::size_of::<fuse_write_in>()
);

impl Session {
    pub fn mount(mountpoint: &Path, opts: &MountOptions) -> io::Result<Session> {
        Self::mount_with_transport(mountpoint, opts, false)
    }

    pub fn mount_uring(mountpoint: &Path, opts: &MountOptions) -> io::Result<Self> {
        Self::mount_with_transport(mountpoint, opts, true)
    }

    fn mount_with_transport(mountpoint: &Path, opts: &MountOptions, uring: bool) -> io::Result<Self> {
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
        if let Err(e) = sess.handshake(opts, uring) {
            sess.unmount();
            return Err(e);
        }
        assert_eq!(sess.caps.max_write, MAX_WRITE);
        assert!(sess.caps.minor >= 31);
        Ok(sess)
    }

    fn handshake(&mut self, opts: &MountOptions, uring: bool) -> io::Result<()> {
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
        let init: Option<fuse_init_in> = read_struct(&buf[std::mem::size_of::<fuse_in_header>()..n]);
        if uring {
            require_uring(init.as_ref().is_some_and(|init| {
                init.major == FUSE_KERNEL_VERSION
                    && init.flags & FUSE_INIT_EXT as u32 != 0
                    && (u64::from(init.flags2) << 32) & FUSE_OVER_IO_URING != 0
            }))?;
        }
        let init = init.ok_or_else(|| io::Error::other("short INIT body"))?;
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
        if uring {
            want |= FUSE_OVER_IO_URING;
        }
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

    pub fn device_fd(&self) -> RawFd {
        self.dev.as_raw_fd()
    }

    pub fn carry(&self, effect: &Effect) -> io::Result<i32> {
        let fd = self.dev.as_raw_fd();
        let status = match effect {
            Effect::Notify(bytes) => {
                let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
                return match written {
                    w if w >= 0 => Ok(0),
                    _ => match io::Error::last_os_error() {
                        e if e.raw_os_error() == Some(libc::ENOENT) => Ok(0),
                        e => Err(e),
                    },
                };
            }
            Effect::Register { fd: source, .. } => {
                let map = fuse_backing_map { fd: *source, flags: 0, padding: 0 };
                unsafe { libc::ioctl(fd, FUSE_DEV_IOC_BACKING_OPEN as libc::Ioctl, &map as *const _) }
            }
            Effect::Unregister(id) => {
                let id = *id as u32;
                unsafe { libc::ioctl(fd, FUSE_DEV_IOC_BACKING_CLOSE as libc::Ioctl, &id as *const _) }
            }
        };
        if status < 0 { Err(io::Error::last_os_error()) } else { Ok(status) }
    }

    pub fn backing_open(&self, fd: RawFd) -> io::Result<i32> {
        self.carry(&Effect::Register { fd, token: 0 })
    }

    pub fn backing_close(&self, id: i32) -> io::Result<()> {
        self.carry(&Effect::Unregister(id)).map(|_| ())
    }

    pub fn notify_inval_entry(&self, parent: u64, name: &[u8]) -> io::Result<()> {
        self.carry(&Effect::Notify(inval_entry_bytes(parent, name))).map(|_| ())
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

impl Session {
    pub fn serve<F, R, W>(&self, fs: &F, mut read: R, mut write: W)
    where
        F: Filesystem,
        R: FnMut(&mut [u8]) -> io::Result<usize>,
        W: FnMut(&[u8]) -> io::Result<()>,
    {
        let mut storage = vec![0u64; BUF_SIZE / 8];
        let mut handler = self.handler();
        loop {
            let buf = unsafe {
                std::slice::from_raw_parts_mut(storage.as_mut_ptr() as *mut u8, BUF_SIZE)
            };
            let n = match read(buf) {
                Ok(0) => {
                    self.ended();
                    break;
                }
                Ok(n) => n,
                Err(e) if matches!(e.raw_os_error(), Some(libc::EINTR | libc::EAGAIN | libc::ENOENT)) => continue,
                Err(_) => {
                    self.ended();
                    break;
                }
            };
            assert!(n <= BUF_SIZE);
            let request = unsafe { std::slice::from_raw_parts(storage.as_ptr() as *const u8, n) };
            if !self.respond(&mut handler, fs, request, &mut write) {
                return;
            }
        }
    }

    pub fn handler(&self) -> RequestHandler {
        RequestHandler::new(self.caps.max_write)
    }

    pub fn ended(&self) {
        self.connected.store(false, Ordering::SeqCst);
    }

    pub fn respond<F, W>(&self, handler: &mut RequestHandler, fs: &F, request: &[u8], mut write: W) -> bool
    where
        F: Filesystem,
        W: FnMut(&[u8]) -> io::Result<()>,
    {
        let mut handled = handler.handle(fs, request);
        loop {
            let mut registered = None;
            for effect in std::mem::take(&mut handled.effects) {
                let done = self.carry(&effect);
                if let Effect::Register { token, .. } = effect {
                    registered = Some((token, done.map_err(Errno::from)));
                }
            }
            if !handled.bytes.is_empty() && write(handled.bytes).is_err() {
                self.ended();
                return false;
            }
            if handled.destroyed {
                self.ended();
                return false;
            }
            let Some((token, id)) = registered else { return true };
            handled = handler.registered(fs, token, id);
        }
    }
}

pub struct RequestHandler {
    scratch: Vec<u8>,
    reply: Vec<u8>,
    max_write: u32,
    pending: Option<Awaiting>,
}

#[derive(Clone, Copy, Debug)]
struct Awaiting {
    unique: u64,
    token: u64,
    ctx: (u32, u32, u32),
    entry: Option<fuse_entry_out>,
}

#[derive(Debug)]
pub struct Handled<'a> {
    pub bytes: &'a [u8],
    pub effects: Vec<Effect>,
    pub destroyed: bool,
}

impl RequestHandler {
    pub fn new(max_write: u32) -> Self {
        Self { scratch: Vec::new(), reply: Vec::new(), max_write, pending: None }
    }

    pub fn handle<F: Filesystem>(&mut self, fs: &F, request: &[u8]) -> Handled<'_> {
        assert!(self.pending.is_none(), "a registration is carried out before the next request");
        self.reply.clear();
        let Some(hdr) = read_struct::<fuse_in_header>(request) else {
            return Handled { bytes: &self.reply, effects: Vec::new(), destroyed: false };
        };
        if hdr.unique == 0 {
            return Handled { bytes: &self.reply, effects: Vec::new(), destroyed: false };
        }
        if hdr.len as usize != request.len() {
            if !matches!(hdr.opcode, FUSE_FORGET | FUSE_BATCH_FORGET | FUSE_INTERRUPT) {
                encode_reply(&mut self.reply, hdr.unique, -libc::EINVAL, &[]);
            }
            return Handled { bytes: &self.reply, effects: Vec::new(), destroyed: false };
        }
        let effects = Effects::default();
        let ctx = Ctx { uid: hdr.uid, gid: hdr.gid, pid: hdr.pid, effects: &effects };
        let destroyed = hdr.opcode == FUSE_DESTROY;
        if destroyed {
            fs.destroy();
            encode_reply(&mut self.reply, hdr.unique, 0, &[]);
        } else {
            let body = &request[std::mem::size_of::<fuse_in_header>()..];
            let mut call = Call { reply: &mut self.reply, out: &mut self.scratch, max_write: self.max_write, pending: None };
            dispatch(fs, &ctx, &mut call, &hdr, body);
            self.pending = call.pending;
        }
        let mut effects = effects.take();
        let registrations = effects.iter().filter(|e| matches!(e, Effect::Register { .. })).count();
        match self.pending {
            Some(a) => {
                assert_eq!(registrations, 1, "an open awaits exactly one registration");
                assert!(effects.iter().any(|e| matches!(e, Effect::Register { token, .. } if *token == a.token)));
            }
            None => effects.retain(|e| !matches!(e, Effect::Register { .. })),
        }
        Handled { bytes: &self.reply, effects, destroyed }
    }

    pub fn registered<F: Filesystem>(&mut self, fs: &F, token: u64, id: Res<i32>) -> Handled<'_> {
        let a = self.pending.take().expect("a registration answers an open that awaits it");
        assert_eq!(a.token, token, "the registration is the one the open asked for");
        self.reply.clear();
        let effects = Effects::default();
        let ctx = Ctx { uid: a.ctx.0, gid: a.ctx.1, pid: a.ctx.2, effects: &effects };
        let opened = fs.registered(&ctx, token, id).and_then(|o| {
            if o.flags & FOPEN_PASSTHROUGH != 0 && o.backing_id <= 0 { Err(Errno::EIO) } else { Ok(o) }
        });
        match (opened, a.entry) {
            (Ok(o), Some(e)) => encode_reply(&mut self.reply, a.unique, 0, &[as_bytes(&e), as_bytes(&open_out(&o))]),
            (Ok(o), None) => encode_reply(&mut self.reply, a.unique, 0, &[as_bytes(&open_out(&o))]),
            (Err(e), _) => encode_reply(&mut self.reply, a.unique, -e.0, &[]),
        }
        let mut effects = effects.take();
        effects.retain(|e| !matches!(e, Effect::Register { .. }));
        Handled { bytes: &self.reply, effects, destroyed: false }
    }

    pub fn pending(&self) -> bool {
        self.pending.is_some()
    }
}

impl StateMachine for RequestHandler {
    fn invariants(&self) {
        if self.pending.is_some() {
            assert!(self.reply.is_empty(), "an open that awaits its registration has not been answered");
        }
        if !self.reply.is_empty() {
            let header = read_struct::<fuse_out_header>(&self.reply).expect("a reply has a header");
            assert_eq!(header.len as usize, self.reply.len(), "a reply's length is its header's");
            assert!(header.unique != 0 && header.error <= 0, "a reply answers a request");
        }
    }
}

struct Call<'a> {
    reply: &'a mut Vec<u8>,
    out: &'a mut Vec<u8>,
    max_write: u32,
    pending: Option<Awaiting>,
}

impl Call<'_> {
    fn opened(&mut self, ctx: &Ctx, hdr: &fuse_in_header, entry: Option<fuse_entry_out>, o: Opened) {
        let asked = ctx.effects.0.borrow().iter().find_map(|e| match e {
            Effect::Register { token, .. } => Some(*token),
            _ => None,
        });
        match asked {
            Some(token) => {
                self.pending =
                    Some(Awaiting { unique: hdr.unique, token, ctx: (ctx.uid, ctx.gid, ctx.pid), entry });
            }
            None => match entry {
                Some(e) => encode_reply(self.reply, hdr.unique, 0, &[as_bytes(&e), as_bytes(&open_out(&o))]),
                None => encode_reply(self.reply, hdr.unique, 0, &[as_bytes(&open_out(&o))]),
            },
        }
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

fn dispatch<F: Filesystem>(fs: &F, ctx: &Ctx, call: &mut Call, hdr: &fuse_in_header, body: &[u8]) {
    let ctx = *ctx;
    let max_write = call.max_write;
    let reply = &mut *call.reply;
    let out = &mut *call.out;
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
            match fs.open(&ctx, node, a.flags) {
                Ok(o) => call.opened(&ctx, hdr, None, o),
                Err(e) => encode_reply(reply, u, -e.0, &[]),
            }
        }
        FUSE_CREATE => {
            let a = arg!(fuse_create_in, body, reply, hdr);
            let (name, _) = names!(&body[std::mem::size_of::<fuse_create_in>()..], reply, hdr);
            let mode = a.mode & !a.umask;
            match fs.create(&ctx, node, name, mode, a.flags) {
                Ok((e, o)) => call.opened(&ctx, hdr, Some(entry_out(&e)), o),
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
                    fs.release_in(&ctx, node, a.fh, a.flags);
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
    use super::*;
    use expect_test::{Expect, expect};
    use std::fmt::Write as _;
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU64;

    const HEADER: usize = std::mem::size_of::<fuse_in_header>();
    const OUT: usize = std::mem::size_of::<fuse_out_header>();
    const BACKING_FD: RawFd = 99;

    const RECORDED: &[(&str, &str)] = &[
        ("INIT", "680000001a0000000200000000000000000000000000000000000000000000000000000000000000070000002d00000000000200fbffff73ff0500000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"),
        ("LOOKUP", "2d0000000100000004000000000000000100000000000000000000000000000028212d000000000066696c6500"),
        ("STATX", "400000003400000006000000000000000200000000000000000000000000000028212d00000000000000000000000000000000000000000000000000ff0f0000"),
        ("GETATTR", "380000000300000008000000000000000200000000000000000000000000000028212d000000000000000000000000000000000000000000"),
        ("GETATTR", "38000000030000000a00000000000000010000000000000000000000000000002a212d000000000000000000000000000000000000000000"),
        ("OPENDIR", "300000001b0000000c00000000000000010000000000000000000000000000002a212d00000000000088010000000000"),
        ("GETATTR", "38000000030000000e00000000000000010000000000000000000000000000002a212d000000000000000000000000000000000000000000"),
        ("READDIR", "500000001c0000001000000000000000010000000000000000000000000000002a212d000000000000000000000000000000000000000000008000000000000000000000000000000088010000000000"),
        ("READDIR", "500000001c0000001200000000000000010000000000000000000000000000002a212d000000000000000000000000000100000000000000008000000000000000000000000000000088010000000000"),
        ("LOOKUP", "2d000000010000001400000000000000010000000000000000000000000000002c212d000000000066696c6500"),
        ("OPEN", "300000000e0000001600000000000000020000000000000000000000000000002c212d00000000000080000000000000"),
        ("GETATTR", "38000000030000001800000000000000020000000000000000000000000000002c212d000000000000000000000000000000000000000000"),
        ("GETATTR", "38000000030000001a00000000000000020000000000000000000000000000002c212d000000000001000000000000001100000000000000"),
        ("READ", "500000000f0000001c00000000000000020000000000000000000000000000002c212d000000000011000000000000000000000000000000001000000000000000000000000000000080000000000000"),
        ("GETATTR", "38000000030000001e00000000000000020000000000000000000000000000002c212d000000000001000000000000001100000000000000"),
        ("FLUSH", "40000000190000002000000000000000020000000000000000000000000000002c212d000000000011000000000000000000000000000000775d2e0b408f2a87"),
        ("RELEASE", "40000000120000002200000000000000020000000000000000000000000000000000000000000000110000000000000000800000000000000000000000000000"),
        ("LOOKUP", "30000000010000002400000000000000010000000000000000000000000000002e212d00000000006d697373696e6700"),
        ("LOOKUP", "2d0000000100000026000000000000000100000000000000000000000000000031212d0000000000736c6f7700"),
        ("OPEN", "300000000e00000028000000000000000300000000000000000000000000000031212d000000000000c0000000000000"),
        ("GETATTR", "38000000030000002a000000000000000300000000000000000000000000000031212d000000000001000000000000001100000000000000"),
        ("READ", "500000000f0000002c000000000000000300000000000000000000000000000031212d00000000001100000000000000000000000000000000100000020000000b59fdcb37768c1200c0000000000000"),
        ("RELEASE", "40000000120000002e00000000000000030000000000000000000000000000000000000000000000110000000000000000c00000000000000000000000000000"),
        ("LOOKUP", "2d0000000100000030000000000000000100000000000000000000000000000045212d0000000000676f6e6500"),
        ("GETATTR", "380000000300000032000000000000000400000000000000000000000000000045212d000000000000000000000000000000000000000000"),
        ("LOOKUP", "2d0000000100000034000000000000000100000000000000000000000000000048212d00000000007761697400"),
        ("INTERRUPT", "300000002400000035000000000000000000000000000000000000000000000000000000000000003400000000000000"),
        ("LOOKUP", "2d000000010000003600000000000000010000000000000000000000000000004a212d0000000000666c697000"),
        ("GETATTR", "38000000030000003800000000000000050000000000000000000000000000004a212d000000000000000000000000000000000000000000"),
        ("LOOKUP", "2d000000010000003a00000000000000010000000000000000000000000000004c212d0000000000666c697000"),
        ("LOOKUP", "2d000000010000003c00000000000000010000000000000000000000000000004c212d0000000000666c697000"),
        ("BATCH_FORGET", "500000002a0000003e0000000000000000000000000000000000000000000000000000000000000002000000000000000600000000000000010000000000000005000000000000000100000000000000"),
        ("GETATTR", "38000000030000004000000000000000060000000000000000000000000000004c212d000000000000000000000000000000000000000000"),
        ("STATFS", "28000000110000004200000000000000010000000000000000000000000000004f212d0000000000"),
    ];

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }

        fn bytes(&mut self, n: usize) -> Vec<u8> {
            (0..n).map(|_| self.below(256) as u8).collect()
        }
    }

    fn attr(ino: u64) -> fuse_attr {
        let mode = if ino == 1 { libc::S_IFDIR | 0o755 } else { libc::S_IFREG | 0o644 };
        let nlink = if ino == 4 || ino == 5 { 0 } else { 1 };
        fuse_attr { ino, size: if ino == 1 { 0 } else { 4 }, mode, nlink, blksize: 4096, ..fuse_attr::default() }
    }

    #[derive(Default)]
    struct Memory {
        flips: AtomicU64,
        next_fh: AtomicU64,
        forgotten: Mutex<Vec<(u64, u64)>>,
        released: Mutex<Vec<u64>>,
        backing: Mutex<BTreeMap<u64, i32>>,
    }

    impl Memory {
        fn node_of(&self, parent: u64, name: &[u8]) -> Res<u64> {
            match (parent, name) {
                (1, b"file") => Ok(2),
                (1, b"slow") => Ok(3),
                (1, b"gone") => Ok(4),
                (1, b"flip") => Ok(5 + self.flips.fetch_add(1, Ordering::SeqCst).min(1)),
                (1, b"through") => Ok(7),
                (1, b"refused") => Ok(9),
                _ => Err(Errno::ENOENT),
            }
        }

        fn passthrough(&self, ctx: &Ctx) -> Opened {
            let fh = 70 + self.next_fh.fetch_add(1, Ordering::SeqCst);
            ctx.effects.register(BACKING_FD, fh);
            Opened { fh, flags: FOPEN_PASSTHROUGH, backing_id: 0 }
        }
    }

    fn entry(nodeid: u64) -> Entry {
        Entry { nodeid, generation: 0, attr: attr(nodeid), entry_ttl: Duration::ZERO, attr_ttl: Duration::ZERO }
    }

    impl Filesystem for Memory {
        fn lookup(&self, _: &Ctx, parent: u64, name: &[u8]) -> Res<Entry> {
            self.node_of(parent, name).map(entry)
        }

        fn forget(&self, nodeid: u64, nlookup: u64) {
            self.forgotten.lock().unwrap().push((nodeid, nlookup));
        }

        fn getattr(&self, _: &Ctx, node: u64, _: Option<u64>) -> Res<(fuse_attr, Duration)> {
            Ok((attr(node), Duration::ZERO))
        }

        fn unlink(&self, ctx: &Ctx, parent: u64, name: &[u8]) -> Res<()> {
            ctx.effects.inval_entry(parent, name);
            Ok(())
        }

        fn open(&self, ctx: &Ctx, node: u64, _: u32) -> Res<Opened> {
            match node {
                7 => Ok(self.passthrough(ctx)),
                9 => {
                    ctx.effects.register(BACKING_FD, 0);
                    Err(Errno::EACCES)
                }
                _ => Ok(Opened { fh: 17, flags: 0, backing_id: 0 }),
            }
        }

        fn create(&self, ctx: &Ctx, _: u64, _: &[u8], _: u32, _: u32) -> Res<(Entry, Opened)> {
            Ok((entry(8), self.passthrough(ctx)))
        }

        fn registered(&self, _: &Ctx, token: u64, id: Res<i32>) -> Res<Opened> {
            Ok(match id {
                Ok(id) => {
                    self.backing.lock().unwrap().insert(token, id);
                    Opened { fh: token, flags: FOPEN_PASSTHROUGH, backing_id: id }
                }
                Err(_) => Opened { fh: token, flags: FOPEN_KEEP_CACHE, backing_id: 0 },
            })
        }

        fn read(&self, _: &Ctx, _: u64, _: u64, offset: u64, size: u32, out: &mut Vec<u8>) -> Res<()> {
            let start = offset.min(4) as usize;
            out.extend_from_slice(&b"abcd"[start..(start + size as usize).min(4)]);
            Ok(())
        }

        fn readdir(&self, _: &Ctx, _: u64, _: u64, offset: u64, sink: &mut DirSink<'_>) -> Res<()> {
            if offset == 0 {
                sink.push(2, 1, libc::DT_REG as u32, b"file");
            }
            Ok(())
        }

        fn release_in(&self, ctx: &Ctx, _: u64, fh: u64, _: u32) {
            self.released.lock().unwrap().push(fh);
            if let Some(id) = self.backing.lock().unwrap().remove(&fh) {
                ctx.effects.unregister(id);
            }
        }
    }

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    fn frame(opcode: u32, unique: u64, nodeid: u64, body: &[u8]) -> Vec<u8> {
        let header = fuse_in_header {
            len: (HEADER + body.len()) as u32,
            opcode,
            unique,
            nodeid,
            uid: 1000,
            gid: 1000,
            pid: 100,
            total_extlen: 0,
            padding: 0,
        };
        [as_bytes(&header), body].concat()
    }

    fn answer(unique: u64, error: i32, parts: &[&[u8]]) -> Vec<u8> {
        let len = OUT + parts.iter().map(|p| p.len()).sum::<usize>();
        let mut v = as_bytes(&fuse_out_header { len: len as u32, error, unique }).to_vec();
        for p in parts {
            v.extend_from_slice(p);
        }
        v
    }

    fn entry_reply(unique: u64, nodeid: u64) -> Vec<u8> {
        let e = fuse_entry_out {
            nodeid,
            generation: 0,
            entry_valid: 0,
            attr_valid: 0,
            entry_valid_nsec: 0,
            attr_valid_nsec: 0,
            attr: attr(nodeid),
        };
        answer(unique, 0, &[as_bytes(&e)])
    }

    fn opened_reply(unique: u64, fh: u64, flags: u32, backing_id: i32) -> Vec<u8> {
        answer(unique, 0, &[as_bytes(&fuse_open_out { fh, open_flags: flags, backing_id })])
    }

    fn dirents(unique: u64, offset: u64, size: u32) -> Vec<u8> {
        if offset != 0 || size < 32 {
            return answer(unique, 0, &[]);
        }
        let d = fuse_dirent { ino: 2, off: 1, namelen: 4, typ: libc::DT_REG as u32 };
        answer(unique, 0, &[as_bytes(&d), b"file", &[0; 4]])
    }

    fn data(unique: u64, offset: u64, size: u32, max_write: u32) -> Vec<u8> {
        let start = offset.min(4) as usize;
        answer(unique, 0, &[&b"abcd"[start..(start + size.min(max_write) as usize).min(4)]])
    }

    struct Model {
        flips: u64,
        forgotten: Vec<(u64, u64)>,
        released: Vec<u64>,
    }

    impl Model {
        fn lookup(&mut self, unique: u64, parent: u64, name: &[u8]) -> Vec<u8> {
            let node = match (parent, name) {
                (1, b"file") => 2,
                (1, b"slow") => 3,
                (1, b"gone") => 4,
                (1, b"flip") => {
                    self.flips += 1;
                    if self.flips == 1 { 5 } else { 6 }
                }
                (1, b"through") => 7,
                (1, b"refused") => 9,
                _ => return answer(unique, -libc::ENOENT, &[]),
            };
            entry_reply(unique, node)
        }
    }

    fn checked<'a>(h: &'a mut RequestHandler, fs: &Memory, request: &[u8]) -> (Vec<u8>, Vec<Effect>) {
        let handled = h.handle(fs, request);
        let out = (handled.bytes.to_vec(), handled.effects);
        h.invariants();
        out
    }

    fn after_registration(h: &mut RequestHandler, fs: &Memory, token: u64, id: Res<i32>) -> (Vec<u8>, Vec<Effect>) {
        let handled = h.registered(fs, token, id);
        let out = (handled.bytes.to_vec(), handled.effects);
        h.invariants();
        assert!(!h.pending());
        out
    }

    fn record_frame(out: &mut String, bytes: &[u8]) {
        let hdr = read_struct::<fuse_out_header>(bytes).unwrap();
        assert_eq!(hdr.len as usize, bytes.len());
        write!(out, "unique={} code={} payload=", hdr.unique, hdr.error).unwrap();
        for byte in &bytes[OUT..] {
            write!(out, "{byte:02x}").unwrap();
        }
        out.push('\n');
    }

    fn record(out: &mut String, label: &str, result: Handled<'_>) {
        writeln!(out, "{label}").unwrap();
        if result.bytes.is_empty() {
            writeln!(out, "  no reply").unwrap();
        } else {
            write!(out, "  reply ").unwrap();
            record_frame(out, result.bytes);
        }
        for effect in result.effects {
            match effect {
                Effect::Notify(bytes) => {
                    write!(out, "  notify ").unwrap();
                    record_frame(out, &bytes);
                }
                effect => writeln!(out, "  {effect:?}").unwrap(),
            }
        }
        if result.destroyed {
            writeln!(out, "  destroyed").unwrap();
        }
    }

    enum Event {
        Request(Vec<u8>),
        Registered(u64, Res<i32>),
    }

    fn replay(
        handler: &mut RequestHandler,
        fs: &Memory,
        out: &mut String,
        events: impl IntoIterator<Item = (&'static str, Event)>,
    ) {
        for (label, event) in events {
            let result = match event {
                Event::Request(bytes) => handler.handle(fs, &bytes),
                Event::Registered(token, id) => handler.registered(fs, token, id),
            };
            record(out, label, result);
            handler.invariants();
            if handler.pending() {
                writeln!(out, "  awaiting registration").unwrap();
            }
        }
    }

    #[test]
    fn recorded_kernel_requests_get_the_expected_replies() {
        let fs = Memory::default();
        let mut h = RequestHandler::new(MAX_WRITE);
        let mut out = String::new();
        replay(&mut h, &fs, &mut out, RECORDED.iter().map(|&(name, hex)| (name, Event::Request(unhex(hex)))));
        writeln!(out, "forgotten {:?}", fs.forgotten.lock().unwrap()).unwrap();
        writeln!(out, "released {:?}", fs.released.lock().unwrap()).unwrap();
        expect![[r#"
            INIT
              reply unique=2 code=-38 payload=
            LOOKUP
              reply unique=4 code=0 payload=02000000000000000000000000000000000000000000000000000000000000000000000000000000020000000000000004000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a4810000010000000000000000000000000000000010000000000000
            STATX
              reply unique=6 code=-38 payload=
            GETATTR
              reply unique=8 code=0 payload=00000000000000000000000000000000020000000000000004000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a4810000010000000000000000000000000000000010000000000000
            GETATTR
              reply unique=10 code=0 payload=00000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000ed410000010000000000000000000000000000000010000000000000
            OPENDIR
              reply unique=12 code=-38 payload=
            GETATTR
              reply unique=14 code=0 payload=00000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000ed410000010000000000000000000000000000000010000000000000
            READDIR
              reply unique=16 code=0 payload=02000000000000000100000000000000040000000800000066696c6500000000
            READDIR
              reply unique=18 code=0 payload=
            LOOKUP
              reply unique=20 code=0 payload=02000000000000000000000000000000000000000000000000000000000000000000000000000000020000000000000004000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a4810000010000000000000000000000000000000010000000000000
            OPEN
              reply unique=22 code=0 payload=11000000000000000000000000000000
            GETATTR
              reply unique=24 code=0 payload=00000000000000000000000000000000020000000000000004000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a4810000010000000000000000000000000000000010000000000000
            GETATTR
              reply unique=26 code=0 payload=00000000000000000000000000000000020000000000000004000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a4810000010000000000000000000000000000000010000000000000
            READ
              reply unique=28 code=0 payload=61626364
            GETATTR
              reply unique=30 code=0 payload=00000000000000000000000000000000020000000000000004000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a4810000010000000000000000000000000000000010000000000000
            FLUSH
              reply unique=32 code=-38 payload=
            RELEASE
              reply unique=34 code=0 payload=
            LOOKUP
              reply unique=36 code=-2 payload=
            LOOKUP
              reply unique=38 code=0 payload=03000000000000000000000000000000000000000000000000000000000000000000000000000000030000000000000004000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a4810000010000000000000000000000000000000010000000000000
            OPEN
              reply unique=40 code=0 payload=11000000000000000000000000000000
            GETATTR
              reply unique=42 code=0 payload=00000000000000000000000000000000030000000000000004000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a4810000010000000000000000000000000000000010000000000000
            READ
              reply unique=44 code=0 payload=61626364
            RELEASE
              reply unique=46 code=0 payload=
            LOOKUP
              reply unique=48 code=0 payload=04000000000000000000000000000000000000000000000000000000000000000000000000000000040000000000000004000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a4810000000000000000000000000000000000000010000000000000
            GETATTR
              reply unique=50 code=0 payload=00000000000000000000000000000000040000000000000004000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a4810000000000000000000000000000000000000010000000000000
            LOOKUP
              reply unique=52 code=-2 payload=
            INTERRUPT
              no reply
            LOOKUP
              reply unique=54 code=0 payload=05000000000000000000000000000000000000000000000000000000000000000000000000000000050000000000000004000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a4810000000000000000000000000000000000000010000000000000
            GETATTR
              reply unique=56 code=0 payload=00000000000000000000000000000000050000000000000004000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a4810000000000000000000000000000000000000010000000000000
            LOOKUP
              reply unique=58 code=0 payload=06000000000000000000000000000000000000000000000000000000000000000000000000000000060000000000000004000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a4810000010000000000000000000000000000000010000000000000
            LOOKUP
              reply unique=60 code=0 payload=06000000000000000000000000000000000000000000000000000000000000000000000000000000060000000000000004000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a4810000010000000000000000000000000000000010000000000000
            BATCH_FORGET
              no reply
            GETATTR
              reply unique=64 code=0 payload=00000000000000000000000000000000060000000000000004000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a4810000010000000000000000000000000000000010000000000000
            STATFS
              reply unique=66 code=0 payload=0000004000000000000000200000000000000020000000000000004000000000000000200000000000100000ff0000000010000000000000000000000000000000000000000000000000000000000000
            forgotten [(6, 1), (5, 1)]
            released [17, 17]
        "#]].assert_eq(&out);
    }

    #[test]
    fn forgets_and_interrupts_get_no_reply_and_an_interrupted_request_still_answers() {
        use Event::{Registered, Request};

        let fs = Memory::default();
        let mut h = RequestHandler::new(MAX_WRITE);
        let mut waiting = RequestHandler::new(MAX_WRITE);
        let mut out = String::new();

        let mut batch = as_bytes(&fuse_batch_forget_in { count: 3, dummy: 0 }).to_vec();
        batch.extend_from_slice(as_bytes(&fuse_forget_one { nodeid: 6, nlookup: 1 }));
        batch.extend_from_slice(as_bytes(&fuse_forget_one { nodeid: 5, nlookup: 2 }));
        replay(&mut h, &fs, &mut out, [
            ("FORGET", Request(frame(FUSE_FORGET, 5, 2, as_bytes(&fuse_forget_in { nlookup: 3 })))),
            ("BATCH_FORGET with fewer entries than advertised", Request(frame(FUSE_BATCH_FORGET, 6, 0, &batch))),
        ]);
        writeln!(out, "forgotten {:?}", fs.forgotten.lock().unwrap()).unwrap();

        let open = fuse_open_in { flags: 0, open_flags: 0 };
        replay(&mut waiting, &fs, &mut out, [
            ("OPEN on another worker", Request(frame(FUSE_OPEN, 52, 7, as_bytes(&open)))),
        ]);
        replay(&mut h, &fs, &mut out, [
            ("INTERRUPT the in-flight OPEN", Request(frame(FUSE_INTERRUPT, 53, 0, &52u64.to_ne_bytes()))),
        ]);
        replay(&mut waiting, &fs, &mut out, [("OPEN registration completes", Registered(70, Ok(4)))]);
        replay(&mut h, &fs, &mut out, [
            ("INTERRUPT an unknown request", Request(frame(FUSE_INTERRUPT, 54, 0, &999u64.to_ne_bytes()))),
            ("LOOKUP after the interrupts", Request(frame(FUSE_LOOKUP, 55, 1, b"file\0"))),
        ]);
        expect![[r#"
            FORGET
              no reply
            BATCH_FORGET with fewer entries than advertised
              no reply
            forgotten [(2, 3), (6, 1), (5, 2)]
            OPEN on another worker
              no reply
              Register { fd: 99, token: 70 }
              awaiting registration
            INTERRUPT the in-flight OPEN
              no reply
            OPEN registration completes
              reply unique=52 code=0 payload=46000000000000008000000004000000
            INTERRUPT an unknown request
              no reply
            LOOKUP after the interrupts
              reply unique=55 code=0 payload=02000000000000000000000000000000000000000000000000000000000000000000000000000000020000000000000004000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a4810000010000000000000000000000000000000010000000000000
        "#]].assert_eq(&out);
    }

    #[test]
    fn registrations_and_notifications_are_effects_in_the_result() {
        use Event::{Registered, Request};

        let fs = Memory::default();
        let mut h = RequestHandler::new(MAX_WRITE);
        let mut out = String::new();
        let open = |unique, node| Request(frame(FUSE_OPEN, unique, node, as_bytes(&fuse_open_in { flags: 0, open_flags: 0 })));
        let release = fuse_release_in { fh: 70, flags: 0, release_flags: 0, lock_owner: 0 };
        let mut create = as_bytes(&fuse_create_in { flags: 0, mode: 0o644, umask: 0, open_flags: 0 }).to_vec();
        create.extend_from_slice(b"made\0");

        replay(&mut h, &fs, &mut out, [
            ("OPEN for passthrough", open(10, 7)),
            ("OPEN registration succeeds", Registered(70, Ok(5))),
            ("RELEASE the registered handle", Request(frame(FUSE_RELEASE, 11, 7, as_bytes(&release)))),
            ("OPEN for passthrough again", open(12, 7)),
            ("OPEN registration fails", Registered(71, Err(Errno::EPERM))),
            ("CREATE for passthrough", Request(frame(FUSE_CREATE, 13, 1, &create))),
            ("CREATE registration succeeds", Registered(72, Ok(6))),
            ("OPEN fails before registration", open(14, 9)),
            ("UNLINK invalidates the entry", Request(frame(FUSE_UNLINK, 15, 1, b"file\0"))),
            ("DESTROY", Request(frame(FUSE_DESTROY, 16, 0, &[]))),
        ]);
        expect![[r#"
            OPEN for passthrough
              no reply
              Register { fd: 99, token: 70 }
              awaiting registration
            OPEN registration succeeds
              reply unique=10 code=0 payload=46000000000000008000000005000000
            RELEASE the registered handle
              reply unique=11 code=0 payload=
              Unregister(5)
            OPEN for passthrough again
              no reply
              Register { fd: 99, token: 71 }
              awaiting registration
            OPEN registration fails
              reply unique=12 code=0 payload=47000000000000000200000000000000
            CREATE for passthrough
              no reply
              Register { fd: 99, token: 72 }
              awaiting registration
            CREATE registration succeeds
              reply unique=13 code=0 payload=08000000000000000000000000000000000000000000000000000000000000000000000000000000080000000000000004000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a481000001000000000000000000000000000000001000000000000048000000000000008000000006000000
            OPEN fails before registration
              reply unique=14 code=-13 payload=
            UNLINK invalidates the entry
              reply unique=15 code=0 payload=
              notify unique=0 code=3 payload=0100000000000000040000000000000066696c6500
            DESTROY
              reply unique=16 code=0 payload=
              destroyed
        "#]].assert_eq(&out);
    }

    fn check_uring_init(flags2: u32, expected: Expect) {
        let (device, kernel) = std::os::unix::net::UnixDatagram::pair().unwrap();
        kernel.set_nonblocking(true).unwrap();
        let mut session = Session {
            dev: device.into(),
            mountpoint: PathBuf::new(),
            caps: Caps::default(),
            unmounted: AtomicBool::new(true),
            helper: false,
            guard: Mutex::new(None),
            mount_id: None,
            connected: AtomicBool::new(false),
        };
        let init = fuse_init_in {
            major: FUSE_KERNEL_VERSION,
            minor: FUSE_KERNEL_MINOR_VERSION,
            flags: FUSE_INIT_EXT as u32,
            flags2,
            max_readahead: 0,
            unused: [0; 11],
        };
        kernel.send(&frame(FUSE_INIT, 1, 0, as_bytes(&init))).unwrap();
        let mut out = String::new();
        match session.handshake(&MountOptions::default(), true) {
            Ok(()) => {
                let mut reply = [0; 256];
                let len = kernel.recv(&mut reply).unwrap();
                record_frame(&mut out, &reply[..len]);
            }
            Err(error) => writeln!(out, "{:?}: {error}", error.kind()).unwrap(),
        }
        expected.assert_eq(&out);
    }

    #[test]
    fn uring_init_negotiates_the_offer_or_explains_how_to_enable_it() {
        check_uring_init((FUSE_OVER_IO_URING >> 32) as u32, expect![[r#"
            unique=1 code=0 payload=070000002d00000000000000000000400001c0000000100001000000000100000002000000000000000000000000000000000000000000000000000000000000
        "#]]);
        check_uring_init(0, expect![[r#"
            Unsupported: FUSE-over-io_uring is unavailable. Use Linux 6.14 or newer with CONFIG_FUSE_IO_URING and set fuse.enable_uring=Y.
        "#]]);
    }

    #[test]
    fn generated_requests_get_the_replies_of_a_model() {
        let names: [&[u8]; 6] = [b"file", b"slow", b"gone", b"missing", b"through", b"x"];
        for seed in 1..=300u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let fs = Memory::default();
            let max_write = [2u32, 1024, MAX_WRITE][rng.below(3) as usize];
            let mut h = RequestHandler::new(max_write);
            h.invariants();
            let mut model = Model { flips: 0, forgotten: Vec::new(), released: Vec::new() };
            let mut held: BTreeMap<u64, i32> = BTreeMap::new();
            let mut next_fh = 70u64;
            let mut next_id = 1i32;
            for step in 0..80u64 {
                let u = 1000 + step * 2;
                let node = 1 + rng.below(8);
                match rng.below(9) {
                    0 => {
                        let name = names[rng.below(names.len() as u64) as usize];
                        let (bytes, _) = checked(&mut h, &fs, &frame(FUSE_LOOKUP, u, 1, &[name, b"\0"].concat()));
                        assert_eq!(bytes, model.lookup(u, 1, name), "seed {seed} step {step}");
                    }
                    1 => {
                        let g = fuse_getattr_in { getattr_flags: 0, dummy: 0, fh: 0 };
                        let (bytes, _) = checked(&mut h, &fs, &frame(FUSE_GETATTR, u, node, as_bytes(&g)));
                        let a = fuse_attr_out { attr_valid: 0, attr_valid_nsec: 0, dummy: 0, attr: attr(node) };
                        assert_eq!(bytes, answer(u, 0, &[as_bytes(&a)]), "seed {seed} step {step}");
                    }
                    2 => {
                        let target = if rng.below(2) == 0 { 7 } else { 2 };
                        let o = fuse_open_in { flags: 0, open_flags: 0 };
                        let (bytes, effects) = checked(&mut h, &fs, &frame(FUSE_OPEN, u, target, as_bytes(&o)));
                        if target == 2 {
                            assert_eq!(bytes, opened_reply(u, 17, 0, 0), "seed {seed} step {step}");
                            continue;
                        }
                        let fh = next_fh;
                        next_fh += 1;
                        assert!(bytes.is_empty(), "seed {seed} step {step}: no reply before the registration");
                        assert_eq!(effects, [Effect::Register { fd: BACKING_FD, token: fh }]);
                        if rng.below(4) == 0 {
                            let (bytes, _) = after_registration(&mut h, &fs, fh, Err(Errno::EPERM));
                            assert_eq!(bytes, opened_reply(u, fh, FOPEN_KEEP_CACHE, 0), "seed {seed} step {step}");
                        } else {
                            let id = next_id;
                            next_id += 1;
                            held.insert(fh, id);
                            let (bytes, _) = after_registration(&mut h, &fs, fh, Ok(id));
                            assert_eq!(bytes, opened_reply(u, fh, FOPEN_PASSTHROUGH, id), "seed {seed} step {step}");
                        }
                    }
                    3 => {
                        let r = fuse_read_in {
                            fh: 17,
                            offset: rng.below(6),
                            size: rng.below(2 * max_write as u64) as u32,
                            read_flags: 0,
                            lock_owner: 0,
                            flags: 0,
                            padding: 0,
                        };
                        let (bytes, _) = checked(&mut h, &fs, &frame(FUSE_READ, u, 2, as_bytes(&r)));
                        assert_eq!(bytes, data(u, r.offset, r.size, max_write), "seed {seed} step {step}");
                    }
                    4 => {
                        let r = fuse_read_in {
                            fh: 0,
                            offset: rng.below(3),
                            size: rng.below(64) as u32,
                            read_flags: 0,
                            lock_owner: 0,
                            flags: 0,
                            padding: 0,
                        };
                        let (bytes, _) = checked(&mut h, &fs, &frame(FUSE_READDIR, u, 1, as_bytes(&r)));
                        assert_eq!(bytes, dirents(u, r.offset, r.size), "seed {seed} step {step}");
                    }
                    5 => {
                        let fh = match held.keys().nth(rng.below(held.len() as u64 + 1) as usize) {
                            Some(fh) => *fh,
                            None => 17,
                        };
                        let r = fuse_release_in { fh, flags: 0, release_flags: 0, lock_owner: 0 };
                        let (bytes, effects) = checked(&mut h, &fs, &frame(FUSE_RELEASE, u, 2, as_bytes(&r)));
                        assert_eq!(bytes, answer(u, 0, &[]), "seed {seed} step {step}");
                        model.released.push(fh);
                        let want: Vec<Effect> = held.remove(&fh).map(Effect::Unregister).into_iter().collect();
                        assert_eq!(effects, want, "seed {seed} step {step}: a passthrough file's release unregisters it");
                    }
                    6 => {
                        let n = rng.below(5);
                        let (bytes, _) = checked(&mut h, &fs, &frame(FUSE_FORGET, u, node, as_bytes(&fuse_forget_in { nlookup: n })));
                        assert!(bytes.is_empty(), "seed {seed} step {step}");
                        model.forgotten.push((node, n));
                    }
                    7 => {
                        let count = rng.below(4);
                        let mut b = as_bytes(&fuse_batch_forget_in { count: count as u32, dummy: 0 }).to_vec();
                        for _ in 0..count {
                            let one = fuse_forget_one { nodeid: 1 + rng.below(8), nlookup: rng.below(3) };
                            b.extend_from_slice(as_bytes(&one));
                            model.forgotten.push((one.nodeid, one.nlookup));
                        }
                        let (bytes, _) = checked(&mut h, &fs, &frame(FUSE_BATCH_FORGET, u, 0, &b));
                        assert!(bytes.is_empty(), "seed {seed} step {step}");
                    }
                    _ => {
                        let target = u - 2 * rng.below(4);
                        let (bytes, effects) = checked(&mut h, &fs, &frame(FUSE_INTERRUPT, u, 0, &target.to_ne_bytes()));
                        assert!(bytes.is_empty() && effects.is_empty(), "seed {seed} step {step}");
                    }
                }
                assert!(!h.pending(), "seed {seed} step {step}");
            }
            assert_eq!(*fs.forgotten.lock().unwrap(), model.forgotten, "seed {seed}");
            assert_eq!(*fs.released.lock().unwrap(), model.released, "seed {seed}");
            assert_eq!(*fs.backing.lock().unwrap(), held, "seed {seed}: what is registered is what was not released");
        }
    }

    #[test]
    fn malformed_requests_never_panic_and_are_refused() {
        let recorded: Vec<Vec<u8>> = RECORDED.iter().map(|(_, hex)| unhex(hex)).collect();
        for seed in 1..=300u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let fs = Memory::default();
            let mut h = RequestHandler::new([2u32, 1024, MAX_WRITE][rng.below(3) as usize]);
            h.invariants();
            for step in 0..100 {
                let opcode = match rng.below(10) {
                    0 => rng.below(u32::MAX as u64) as u32,
                    _ => rng.below(64) as u32,
                };
                let unique = if rng.below(10) == 0 { 0 } else { 1 + rng.below(1 << 20) };
                let mut request = match rng.below(4) {
                    0 => {
                        let r = &recorded[rng.below(recorded.len() as u64) as usize];
                        r[..rng.below(r.len() as u64 + 1) as usize].to_vec()
                    }
                    1 => {
                        let n = rng.below(96) as usize;
                        let mut f = frame(opcode, unique, rng.below(10), &rng.bytes(n));
                        let len = rng.below(f.len() as u64 + 64) as u32;
                        f[..4].copy_from_slice(&len.to_ne_bytes());
                        f
                    }
                    2 => {
                        let wanted = match opcode % 6 {
                            0 => as_bytes(&fuse_batch_forget_in { count: u32::MAX, dummy: 0 }).to_vec(),
                            1 => as_bytes(&fuse_write_in {
                                fh: 0,
                                offset: 0,
                                size: u32::MAX,
                                write_flags: 0,
                                lock_owner: 0,
                                flags: 0,
                                padding: 0,
                            })
                            .to_vec(),
                            2 => as_bytes(&fuse_rename_in { newdir: 1 }).to_vec(),
                            _ => {
                                let n = rng.below(40) as usize;
                                rng.bytes(n)
                            }
                        };
                        let op = [FUSE_BATCH_FORGET, FUSE_WRITE, FUSE_RENAME, FUSE_SETATTR, FUSE_RENAME2, FUSE_MKNOD]
                            [(opcode % 6) as usize];
                        frame(op, unique, rng.below(10), &wanted)
                    }
                    _ => {
                        let n = rng.below(96) as usize;
                        let node = rng.below(10);
                        frame(opcode, unique, node, &rng.bytes(n))
                    }
                };
                if rng.below(8) == 0 && request.len() > HEADER {
                    request.truncate(HEADER + rng.below((request.len() - HEADER) as u64) as usize);
                }
                let hdr = read_struct::<fuse_in_header>(&request);
                let (bytes, effects) = checked(&mut h, &fs, &request);
                if let Some(hdr) = hdr.filter(|_| !bytes.is_empty()) {
                    let out = read_struct::<fuse_out_header>(&bytes).unwrap();
                    assert_eq!(out.unique, hdr.unique, "seed {seed} step {step}: a reply answers its own request");
                    if hdr.len as usize != request.len() {
                        assert_eq!(out.error, -libc::EINVAL, "seed {seed} step {step}: a frame of the wrong length");
                    }
                }
                if hdr.is_none_or(|hdr| hdr.unique == 0) {
                    assert!(bytes.is_empty() && effects.is_empty(), "seed {seed} step {step}: nothing to answer");
                }
                if h.pending() {
                    let [Effect::Register { token, .. }] = effects.as_slice() else { panic!("seed {seed}: {effects:?}") };
                    after_registration(&mut h, &fs, *token, Ok(1 + step));
                }
            }
        }
    }
}
