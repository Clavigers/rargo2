pub mod abi;
pub mod io;

use rargo_machine::StateMachine;
use abi::*;
use std::ffi::{CStr, CString, OsString};
use std::os::fd::{AsRawFd, RawFd};
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
        Errno(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO))
    }
}

impl From<std::io::Error> for Errno {
    fn from(e: std::io::Error) -> Self {
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
pub trait Filesystem {
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

/// What a session asks of the kernel it is mounted on. Each method is one
/// request that is carried out as told; the session decides what to ask and
/// what an answer means.
pub trait Kernel {
    /// The open connection to the FUSE device.
    type Device: AsRawFd;
    /// The socket a mount helper hands the connection over on.
    type Socket;
    /// A running unmount guard.
    type Guard;

    fn open_device(&self) -> std::io::Result<Self::Device>;
    fn is_dir(&self, path: &Path) -> std::io::Result<bool>;
    fn euid(&self) -> u32;
    fn egid(&self) -> u32;
    fn mount(&self, source: &CStr, target: &CStr, fstype: &CStr, flags: libc::c_ulong, data: &CStr) -> std::io::Result<()>;
    /// Runs `program` to completion with one end of a socket pair, whose
    /// descriptor number is in the variable `fd_env`, and keeps the other end.
    fn run_with_socket(&self, program: &str, args: &[OsString], fd_env: &str) -> std::io::Result<Helped<Self::Socket>>;
    /// The device a helper passed over `socket`, if it passed one.
    fn receive_device(&self, socket: Self::Socket) -> std::io::Result<Option<Self::Device>>;
    /// Runs `program` to completion with no input or output.
    fn run_quiet(&self, program: &str, args: &[OsString]);
    /// `statx` of `path` asking for `mask`: the mask the kernel answered and
    /// the mount id field.
    fn statx_mount_id(&self, path: &CStr, mask: u32) -> Option<(u32, u64)>;
    fn read_to_string(&self, path: &str) -> std::io::Result<String>;
    /// Starts a process that outlives this one and carries out `plan` when
    /// this one dies without standing it down.
    fn spawn_guard(&self, plan: &GuardPlan) -> std::io::Result<Self::Guard>;
    /// Tells a guard to exit without unmounting, and waits for it.
    fn stand_down(&self, guard: Self::Guard);
    /// Lazily unmounts `target`.
    fn detach(&self, target: &CStr);
    fn read(&self, device: &Self::Device, buf: &mut [u8]) -> std::io::Result<usize>;
    fn write(&self, device: &Self::Device, bytes: &[u8]) -> std::io::Result<usize>;
    fn backing_open(&self, device: &Self::Device, fd: RawFd) -> std::io::Result<i32>;
    fn backing_close(&self, device: &Self::Device, id: u32) -> std::io::Result<i32>;
}

/// How a mount helper's run ended.
pub struct Helped<S> {
    pub success: bool,
    pub stderr: Vec<u8>,
    pub socket: S,
}

/// Everything an unmount guard needs to know before it is started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuardPlan {
    /// The guard's process name.
    pub name: CString,
    /// What the guard's command line reads as.
    pub title: Vec<u8>,
    /// Where this process's command line lives in memory, start and end.
    pub title_area: Option<(usize, usize)>,
    pub ignored_signals: Vec<i32>,
    /// The mountpoint.
    pub target: CString,
    /// The mount the guard is for, when the kernel names mounts.
    pub mount_id: Option<u64>,
    /// The `statx` mask that asks for a mount id.
    pub statx_mask: u32,
    /// The command that unmounts in place of a direct unmount, when the mount
    /// was made by a helper.
    pub helper: Option<Vec<CString>>,
}

/// What to do about a mountpoint when a session's mount should go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unmount {
    /// Something else is mounted there now.
    Leave,
    Detach,
    Helper,
}

/// Decides how to unmount, from the id of the session's mount, the id of what
/// is mounted there now, and whether a helper made the mount.
pub fn unmount_action(mine: Option<u64>, there: Option<u64>, helper: bool) -> Unmount {
    match (mine, there) {
        (Some(mine), Some(there)) if mine != there => Unmount::Leave,
        _ if helper => Unmount::Helper,
        _ => Unmount::Detach,
    }
}

const STATX_MNT_ID: u32 = 0x1000;
const STATX_MNT_ID_UNIQUE: u32 = 0x4000;
const MNT_ID_MASK: u32 = STATX_MNT_ID | STATX_MNT_ID_UNIQUE;

/// The mount id in a `statx` answer, if the kernel filled one in.
pub fn mount_id_in(answer: Option<(u32, u64)>) -> Option<u64> {
    answer.and_then(|(mask, id)| (mask & MNT_ID_MASK != 0).then_some(id))
}

fn mount_id<K: Kernel>(kernel: &K, path: &CStr) -> Option<u64> {
    mount_id_in(kernel.statx_mount_id(path, MNT_ID_MASK))
}

const HELPER: &str = "fusermount3";
const HELPER_FD_ENV: &str = "_FUSE_COMMFD";
const MOUNT_FLAGS: libc::c_ulong = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOATIME;
const GUARD_IGNORES: [i32; 5] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT, libc::SIGPIPE];

fn mount_data(fd: RawFd, uid: u32, gid: u32, opts: &MountOptions) -> String {
    let mut data = format!("fd={fd},rootmode=40000,user_id={uid},group_id={gid}");
    if opts.allow_other {
        data.push_str(",allow_other");
    }
    if opts.default_permissions {
        data.push_str(",default_permissions");
    }
    data
}

fn helper_mount_args(mountpoint: &Path, opts: &MountOptions) -> Vec<OsString> {
    let mut options = format!("rw,nosuid,nodev,noatime,fsname={0},subtype={0}", opts.fsname);
    if opts.default_permissions {
        options.push_str(",default_permissions");
    }
    if opts.allow_other {
        options.push_str(",allow_other");
    }
    vec!["-o".into(), options.into(), "--".into(), mountpoint.into()]
}

const HELPER_UNMOUNT_FLAGS: [&str; 4] = ["-u", "-z", "-q", "--"];

fn helper_unmount_args(mountpoint: &Path) -> Vec<OsString> {
    HELPER_UNMOUNT_FLAGS.iter().map(OsString::from).chain([mountpoint.into()]).collect()
}

/// Where the process's command line lives, from its `/proc/self/stat`.
fn arg_area(stat: &str) -> Option<(usize, usize)> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut f = rest.split_ascii_whitespace().skip(45);
    let (start, end) = (f.next()?.parse().ok()?, f.next()?.parse().ok()?);
    (start != 0 && end > start).then_some((start, end))
}

fn guard_plan(tgt: &CStr, stat: Option<&str>, mount_id: Option<u64>, helper: bool) -> GuardPlan {
    let mut title = b"unmount-guard: ".to_vec();
    title.extend_from_slice(tgt.to_bytes());
    let command = || {
        let words = [HELPER].into_iter().chain(HELPER_UNMOUNT_FLAGS).map(|word| CString::new(word).unwrap());
        words.chain([tgt.to_owned()]).collect()
    };
    GuardPlan {
        name: c"unmount-guard".to_owned(),
        title,
        title_area: stat.and_then(arg_area),
        ignored_signals: GUARD_IGNORES.to_vec(),
        target: tgt.to_owned(),
        mount_id,
        statx_mask: MNT_ID_MASK,
        helper: helper.then(command),
    }
}

fn mount_with_helper<K: Kernel>(kernel: &K, mountpoint: &Path, opts: &MountOptions) -> std::io::Result<K::Device> {
    let out = kernel.run_with_socket(HELPER, &helper_mount_args(mountpoint, opts), HELPER_FD_ENV)?;
    if !out.success {
        return Err(std::io::Error::other(String::from_utf8_lossy(&out.stderr).trim().to_string()));
    }
    kernel
        .receive_device(out.socket)?
        .ok_or_else(|| std::io::Error::other("fusermount3 did not hand over the connection"))
}

pub struct Session<K: Kernel = io::Real> {
    kernel: K,
    dev: K::Device,
    mountpoint: PathBuf,
    caps: Caps,
    unmounted: AtomicBool,
    helper: bool,
    guard: std::sync::Mutex<Option<K::Guard>>,
    mount_id: Option<u64>,
    connected: AtomicBool,
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

fn require_uring(offered: bool) -> std::io::Result<()> {
    if offered {
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "FUSE-over-io_uring is unavailable. Use Linux 6.14 or newer with CONFIG_FUSE_IO_URING and set fuse.enable_uring=Y.",
    ))
}

#[cfg(test)]
#[test]
fn missing_uring_offer_explains_how_to_enable_it() {
    assert!(require_uring(true).is_ok());
    let err = require_uring(false).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
    assert!(err.to_string().contains("fuse.enable_uring=Y"));
}

const MAX_WRITE: u32 = 1 << 20;
pub const BUF_SIZE: usize = MAX_WRITE as usize + 2 * 4096;
const _: () = assert!(MAX_WRITE.is_multiple_of(4096));
const _: () = assert!(MAX_WRITE / 4096 <= u16::MAX as u32);
const _: () = assert!(BUF_SIZE.is_multiple_of(8));
const _: () = assert!(BUF_SIZE >= FUSE_MIN_READ_BUFFER);
const _: () = assert!(
    BUF_SIZE >= MAX_WRITE as usize + std::mem::size_of::<fuse_in_header>() + std::mem::size_of::<fuse_write_in>()
);

/// The reply to the kernel's first request, which must be INIT, and the
/// capabilities that reply settles on.
fn init_reply(request: &[u8], want_passthrough: bool, uring: bool) -> std::io::Result<(Vec<u8>, Caps)> {
    let hdr: fuse_in_header = read_struct(request).ok_or_else(|| std::io::Error::other("short INIT"))?;
    if hdr.opcode != FUSE_INIT {
        return Err(std::io::Error::other("first FUSE request was not INIT"));
    }
    let init: Option<fuse_init_in> = read_struct(&request[std::mem::size_of::<fuse_in_header>()..]);
    if uring {
        require_uring(init.as_ref().is_some_and(|init| {
            init.major == FUSE_KERNEL_VERSION
                && init.flags & FUSE_INIT_EXT as u32 != 0
                && (u64::from(init.flags2) << 32) & FUSE_OVER_IO_URING != 0
        }))?;
    }
    let init = init.ok_or_else(|| std::io::Error::other("short INIT body"))?;
    if init.major != FUSE_KERNEL_VERSION || init.minor < 31 {
        return Err(std::io::Error::other(format!(
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
    let passthrough = want_passthrough && kflags & FUSE_PASSTHROUGH != 0;
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
    assert!(hdr.unique != 0, "INIT is a request that takes a reply");
    let mut bytes = Vec::new();
    encode_reply(&mut bytes, hdr.unique, 0, &[as_bytes(&out)]);
    let caps = Caps { minor: out.minor, passthrough: flags & FUSE_PASSTHROUGH != 0, max_write: MAX_WRITE };
    Ok((bytes, caps))
}

impl Session<io::Real> {
    pub fn mount(mountpoint: &Path, opts: &MountOptions) -> std::io::Result<Session> {
        Self::mount_on(io::Real, mountpoint, opts, false)
    }

    pub fn mount_uring(mountpoint: &Path, opts: &MountOptions) -> std::io::Result<Self> {
        Self::mount_on(io::Real, mountpoint, opts, true)
    }
}

impl<K: Kernel> Session<K> {
    /// Mounts on `kernel`, over io_uring when `uring` is set.
    pub fn mount_on(kernel: K, mountpoint: &Path, opts: &MountOptions, uring: bool) -> std::io::Result<Self> {
        let dev = kernel.open_device()?;
        if !kernel.is_dir(mountpoint)? {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "mountpoint is not a directory"));
        }
        let data = mount_data(dev.as_raw_fd(), kernel.euid(), kernel.egid(), opts);
        let src = CString::new(opts.fsname.clone()).unwrap();
        let tgt = CString::new(mountpoint.as_os_str().as_encoded_bytes()).unwrap();
        let fstype = CString::new(format!("fuse.{}", opts.fsname)).unwrap();
        let cdata = CString::new(data).unwrap();
        let mut helper = false;
        let dev = match kernel.mount(&src, &tgt, &fstype, MOUNT_FLAGS, &cdata) {
            Ok(()) => dev,
            Err(e) if e.raw_os_error() != Some(libc::EPERM) => return Err(e),
            Err(e) => {
                helper = true;
                mount_with_helper(&kernel, mountpoint, opts)
                    .map_err(|h| std::io::Error::new(h.kind(), format!("mount: {e}; fusermount3: {h}")))?
            }
        };
        let mount_id = mount_id(&kernel, &tgt);
        let mut guard = None;
        if opts.auto_unmount {
            let stat = kernel.read_to_string("/proc/self/stat").ok();
            guard = kernel.spawn_guard(&guard_plan(&tgt, stat.as_deref(), mount_id, helper)).ok();
        }
        let mut sess = Session {
            kernel,
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

    fn handshake(&mut self, opts: &MountOptions, uring: bool) -> std::io::Result<()> {
        let mut buf = vec![0u8; BUF_SIZE];
        let n = loop {
            match self.kernel.read(&self.dev, &mut buf) {
                Ok(n) => break n,
                Err(e) if e.raw_os_error() == Some(libc::EINTR) => continue,
                Err(e) => return Err(e),
            }
        };
        let (bytes, caps) = init_reply(&buf[..n], opts.passthrough, uring)?;
        self.kernel.write(&self.dev, &bytes)?;
        self.caps = caps;
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

    pub fn carry(&self, effect: &Effect) -> std::io::Result<i32> {
        match effect {
            Effect::Notify(bytes) => match self.kernel.write(&self.dev, bytes) {
                Ok(_) => Ok(0),
                Err(e) if e.raw_os_error() == Some(libc::ENOENT) => Ok(0),
                Err(e) => Err(e),
            },
            Effect::Register { fd: source, .. } => self.kernel.backing_open(&self.dev, *source),
            Effect::Unregister(id) => self.kernel.backing_close(&self.dev, *id as u32),
        }
    }

    pub fn backing_open(&self, fd: RawFd) -> std::io::Result<i32> {
        self.carry(&Effect::Register { fd, token: 0 })
    }

    pub fn backing_close(&self, id: i32) -> std::io::Result<()> {
        self.carry(&Effect::Unregister(id)).map(|_| ())
    }

    pub fn notify_inval_entry(&self, parent: u64, name: &[u8]) -> std::io::Result<()> {
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
        match (self.mount_id, mount_id(&self.kernel, &tgt)) {
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
        if let Some(guard) = self.guard.lock().unwrap().take() {
            self.kernel.stand_down(guard);
        }
        let there = tgt.as_deref().and_then(|tgt| mount_id(&self.kernel, tgt));
        match (unmount_action(self.mount_id, there, self.helper), tgt) {
            (Unmount::Helper, _) => self.kernel.run_quiet(HELPER, &helper_unmount_args(&self.mountpoint)),
            (Unmount::Detach, Some(tgt)) => self.kernel.detach(&tgt),
            (Unmount::Detach, None) | (Unmount::Leave, _) => {}
        }
    }
}

impl<K: Kernel> Drop for Session<K> {
    fn drop(&mut self) {
        self.unmount();
    }
}

pub fn unmount_with_helper(mountpoint: &Path) {
    io::Real.run_quiet(HELPER, &helper_unmount_args(mountpoint));
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

impl<K: Kernel> Session<K> {
    pub fn serve<F, R, W>(&self, fs: &F, mut read: R, mut write: W)
    where
        F: Filesystem,
        R: FnMut(&mut [u8]) -> std::io::Result<usize>,
        W: FnMut(&[u8]) -> std::io::Result<()>,
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
        W: FnMut(&[u8]) -> std::io::Result<()>,
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
            if hdr.opcode == FUSE_RELEASE
                && let Some(a) = read_struct::<fuse_release_in>(body)
            {
                fs.release_in(&ctx, node, a.fh, a.flags);
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
mod session_tests;

#[cfg(test)]
mod request_tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::cell::{Cell, RefCell};

    const HEADER: usize = std::mem::size_of::<fuse_in_header>();
    const OUT: usize = std::mem::size_of::<fuse_out_header>();
    const BACKING_FD: RawFd = 99;

    pub(crate) const RECORDED: &[(&str, &str)] = &[
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
        flips: Cell<u64>,
        next_fh: Cell<u64>,
        forgotten: RefCell<Vec<(u64, u64)>>,
        released: RefCell<Vec<u64>>,
        backing: RefCell<BTreeMap<u64, i32>>,
    }

    impl Memory {
        fn node_of(&self, parent: u64, name: &[u8]) -> Res<u64> {
            match (parent, name) {
                (1, b"file") => Ok(2),
                (1, b"slow") => Ok(3),
                (1, b"gone") => Ok(4),
                (1, b"flip") => Ok(5 + self.flips.replace(self.flips.get() + 1).min(1)),
                (1, b"through") => Ok(7),
                (1, b"refused") => Ok(9),
                _ => Err(Errno::ENOENT),
            }
        }

        fn passthrough(&self, ctx: &Ctx) -> Opened {
            let fh = 70 + self.next_fh.replace(self.next_fh.get() + 1);
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
            self.forgotten.borrow_mut().push((nodeid, nlookup));
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
                    self.backing.borrow_mut().insert(token, id);
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
            self.released.borrow_mut().push(fh);
            if let Some(id) = self.backing.borrow_mut().remove(&fh) {
                ctx.effects.unregister(id);
            }
        }
    }

    pub(crate) fn unhex(s: &str) -> Vec<u8> {
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

    fn checked(h: &mut RequestHandler, fs: &Memory, request: &[u8]) -> (Vec<u8>, Vec<Effect>) {
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

    #[test]
    fn recorded_kernel_requests_get_the_expected_replies() {
        let fs = Memory::default();
        let mut h = RequestHandler::new(MAX_WRITE);
        h.invariants();
        let mut model = Model { flips: 0, forgotten: Vec::new(), released: Vec::new() };
        let mut seen = std::collections::BTreeSet::new();
        for (name, hex) in RECORDED {
            let request = unhex(hex);
            let hdr = read_struct::<fuse_in_header>(&request).unwrap();
            let body = &request[HEADER..];
            let (u, node) = (hdr.unique, hdr.nodeid);
            let (bytes, effects) = checked(&mut h, &fs, &request);
            assert!(effects.is_empty(), "{name}: the recorded requests ask for no device effects");
            let want = match *name {
                "LOOKUP" => model.lookup(u, node, abi::name_arg(body).unwrap().0),
                "GETATTR" => {
                    let a = fuse_attr_out { attr_valid: 0, attr_valid_nsec: 0, dummy: 0, attr: attr(node) };
                    answer(u, 0, &[as_bytes(&a)])
                }
                "OPEN" => opened_reply(u, 17, 0, 0),
                "READ" => {
                    let r = read_struct::<fuse_read_in>(body).unwrap();
                    data(u, r.offset, r.size, MAX_WRITE)
                }
                "READDIR" => {
                    let r = read_struct::<fuse_read_in>(body).unwrap();
                    dirents(u, r.offset, r.size)
                }
                "RELEASE" => {
                    model.released.push(read_struct::<fuse_release_in>(body).unwrap().fh);
                    answer(u, 0, &[])
                }
                "BATCH_FORGET" => {
                    model.forgotten.extend([(6, 1), (5, 1)]);
                    Vec::new()
                }
                "INTERRUPT" => Vec::new(),
                "STATFS" => {
                    let ctx = Ctx { uid: 0, gid: 0, pid: 0, effects: &Effects::default() };
                    answer(u, 0, &[as_bytes(&fuse_statfs_out { st: fs.statfs(&ctx, 1).unwrap() })])
                }
                "INIT" | "STATX" | "OPENDIR" | "FLUSH" => answer(u, -libc::ENOSYS, &[]),
                other => panic!("no expectation for the recorded {other}"),
            };
            assert_eq!(bytes, want, "{name} (unique {u})");
            seen.insert(*name);
        }
        for listed in ["LOOKUP", "GETATTR", "OPEN", "READ", "READDIR", "RELEASE", "BATCH_FORGET", "INTERRUPT"] {
            assert!(seen.contains(listed), "the recording has a {listed}");
        }
        assert_eq!(*fs.forgotten.borrow(), model.forgotten);
        assert_eq!(*fs.released.borrow(), model.released);
    }

    #[test]
    fn forgets_and_interrupts_get_no_reply_and_an_interrupted_request_still_answers() {
        let fs = Memory::default();
        let mut h = RequestHandler::new(MAX_WRITE);
        h.invariants();
        let (bytes, _) = checked(&mut h, &fs, &frame(FUSE_FORGET, 5, 2, as_bytes(&fuse_forget_in { nlookup: 3 })));
        assert!(bytes.is_empty());
        let mut batch = as_bytes(&fuse_batch_forget_in { count: 3, dummy: 0 }).to_vec();
        batch.extend_from_slice(as_bytes(&fuse_forget_one { nodeid: 6, nlookup: 1 }));
        batch.extend_from_slice(as_bytes(&fuse_forget_one { nodeid: 5, nlookup: 2 }));
        let (bytes, _) = checked(&mut h, &fs, &frame(FUSE_BATCH_FORGET, 6, 0, &batch));
        assert!(bytes.is_empty(), "a batch that claims more entries than it carries forgets those it carries");
        assert_eq!(*fs.forgotten.borrow(), [(2, 3), (6, 1), (5, 2)]);

        let mut waiting = RequestHandler::new(MAX_WRITE);
        let open = frame(FUSE_OPEN, 52, 7, as_bytes(&fuse_open_in { flags: 0, open_flags: 0 }));
        let (bytes, effects) = checked(&mut waiting, &fs, &open);
        assert!(bytes.is_empty() && waiting.pending(), "the open waits for its registration");
        assert_eq!(effects, [Effect::Register { fd: BACKING_FD, token: 70 }]);
        let (bytes, effects) = checked(&mut h, &fs, &frame(FUSE_INTERRUPT, 53, 0, &52u64.to_ne_bytes()));
        assert!(bytes.is_empty() && effects.is_empty(), "an interrupt of a request in flight on another worker");
        let (bytes, _) = after_registration(&mut waiting, &fs, 70, Ok(4));
        assert_eq!(bytes, opened_reply(52, 70, FOPEN_PASSTHROUGH, 4), "the interrupted request is still answered");
        let (bytes, _) = checked(&mut h, &fs, &frame(FUSE_INTERRUPT, 54, 0, &999u64.to_ne_bytes()));
        assert!(bytes.is_empty(), "an interrupt of an unknown request");
        let (bytes, _) = checked(&mut h, &fs, &frame(FUSE_LOOKUP, 55, 1, b"file\0"));
        assert_eq!(bytes, entry_reply(55, 2), "and the next request is answered");
    }

    #[test]
    fn registrations_and_notifications_are_effects_in_the_result() {
        let fs = Memory::default();
        let mut h = RequestHandler::new(MAX_WRITE);
        h.invariants();
        let open = |u| frame(FUSE_OPEN, u, 7, as_bytes(&fuse_open_in { flags: 0, open_flags: 0 }));
        let (bytes, effects) = checked(&mut h, &fs, &open(10));
        assert!(bytes.is_empty() && h.pending());
        assert_eq!(effects, [Effect::Register { fd: BACKING_FD, token: 70 }]);
        let (bytes, effects) = after_registration(&mut h, &fs, 70, Ok(5));
        assert_eq!((bytes, effects), (opened_reply(10, 70, FOPEN_PASSTHROUGH, 5), Vec::new()));
        let release = fuse_release_in { fh: 70, flags: 0, release_flags: 0, lock_owner: 0 };
        let (bytes, effects) = checked(&mut h, &fs, &frame(FUSE_RELEASE, 11, 7, as_bytes(&release)));
        assert_eq!((bytes, effects), (answer(11, 0, &[]), vec![Effect::Unregister(5)]));

        let (_, effects) = checked(&mut h, &fs, &open(12));
        assert_eq!(effects, [Effect::Register { fd: BACKING_FD, token: 71 }]);
        let (bytes, _) = after_registration(&mut h, &fs, 71, Err(Errno::EPERM));
        assert_eq!(bytes, opened_reply(12, 71, FOPEN_KEEP_CACHE, 0), "a refused registration falls back to the cache");

        let mut create = as_bytes(&fuse_create_in { flags: 0, mode: 0o644, umask: 0, open_flags: 0 }).to_vec();
        create.extend_from_slice(b"made\0");
        let (bytes, effects) = checked(&mut h, &fs, &frame(FUSE_CREATE, 13, 1, &create));
        assert!(bytes.is_empty() && effects == [Effect::Register { fd: BACKING_FD, token: 72 }]);
        let (bytes, _) = after_registration(&mut h, &fs, 72, Ok(6));
        let e = &entry_reply(13, 8)[OUT..];
        let o = fuse_open_out { fh: 72, open_flags: FOPEN_PASSTHROUGH, backing_id: 6 };
        assert_eq!(bytes, answer(13, 0, &[e, as_bytes(&o)]), "a create answers with its entry and the open");

        let (bytes, effects) = checked(&mut h, &fs, &frame(FUSE_OPEN, 14, 9, as_bytes(&fuse_open_in { flags: 0, open_flags: 0 })));
        assert_eq!(bytes, answer(14, -libc::EACCES, &[]));
        assert!(effects.is_empty() && !h.pending(), "a failed open's registration is dropped");

        let (bytes, effects) = checked(&mut h, &fs, &frame(FUSE_UNLINK, 15, 1, b"file\0"));
        assert_eq!(bytes, answer(15, 0, &[]));
        let [Effect::Notify(note)] = effects.as_slice() else { panic!("one notification: {effects:?}") };
        let header = read_struct::<fuse_out_header>(note).unwrap();
        assert_eq!((header.len as usize, header.unique, header.error), (note.len(), 0, FUSE_NOTIFY_INVAL_ENTRY));
        let inval = read_struct::<fuse_notify_inval_entry_out>(&note[OUT..]).unwrap();
        assert_eq!((inval.parent, inval.namelen), (1, 4));
        assert!(note.ends_with(b"file\0"));
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
            assert_eq!(*fs.forgotten.borrow(), model.forgotten, "seed {seed}");
            assert_eq!(*fs.released.borrow(), model.released, "seed {seed}");
            assert_eq!(*fs.backing.borrow(), held, "seed {seed}: what is registered is what was not released");
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
