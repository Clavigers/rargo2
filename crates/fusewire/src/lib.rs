//! A small FUSE server library that talks to `/dev/fuse` directly.
//!
//! Why not libfuse: rargo's sandbox filesystem lives or dies by per-request
//! latency, so this crate gives the filesystem full control over what the
//! kernel is allowed to cache, runs one worker thread per cloned device fd,
//! and exposes passthrough backing files (Linux 6.9+).

pub mod abi;

use abi::*;
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// A raw errno value returned to the kernel.
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

/// Who is asking.
#[derive(Clone, Copy, Debug)]
pub struct Ctx {
    pub uid: u32,
    pub gid: u32,
    pub pid: u32,
}

/// Reply to lookup-like operations.
#[derive(Clone, Copy, Debug)]
pub struct Entry {
    /// 0 means "does not exist" (a cacheable negative entry).
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

/// Reply to open/create.
#[derive(Clone, Copy, Debug, Default)]
pub struct Opened {
    pub fh: u64,
    /// `FOPEN_*` bits. Set `FOPEN_PASSTHROUGH` together with `backing_id`.
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

/// Collects directory entries for a READDIR reply.
pub struct DirSink<'a> {
    buf: &'a mut Vec<u8>,
    cap: usize,
}

impl DirSink<'_> {
    /// Adds an entry. `next_off` is the offset the kernel passes back to
    /// continue after this entry. Returns false when the buffer is full (the
    /// entry was not added).
    pub fn push(&mut self, ino: u64, next_off: u64, dtype: u32, name: &[u8]) -> bool {
        let ent = std::mem::size_of::<fuse_dirent>() + name.len();
        let padded = (ent + 7) & !7;
        if self.buf.len() + padded > self.cap {
            return false;
        }
        let d = fuse_dirent { ino, off: next_off, namelen: name.len() as u32, typ: dtype };
        self.buf.extend_from_slice(as_bytes(&d));
        self.buf.extend_from_slice(name);
        self.buf.resize(self.buf.len() + (padded - ent), 0);
        true
    }
}

/// What was negotiated with the kernel.
#[derive(Clone, Copy, Debug, Default)]
pub struct Caps {
    pub minor: u32,
    pub passthrough: bool,
    pub max_write: u32,
}

/// The operations a filesystem implements. Every method may be called from
/// any worker thread concurrently.
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
    /// Userspace read path. Append up to `size` bytes to `out`.
    fn read(
        &self,
        ctx: &Ctx,
        nodeid: u64,
        fh: u64,
        offset: u64,
        size: u32,
        out: &mut Vec<u8>,
    ) -> Res<()>;
    /// Userspace write path.
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
    // SAFETY: T is a plain repr(C) ABI struct without padding-sensitive
    // invariants; reading its bytes is fine.
    unsafe { std::slice::from_raw_parts(v as *const T as *const u8, std::mem::size_of::<T>()) }
}

fn read_struct<T: Copy>(b: &[u8]) -> Option<T> {
    if b.len() < std::mem::size_of::<T>() {
        return None;
    }
    // SAFETY: length checked; T is a plain ABI struct valid for any bit pattern.
    Some(unsafe { std::ptr::read_unaligned(b.as_ptr() as *const T) })
}

fn cstr_arg(b: &[u8]) -> (&[u8], &[u8]) {
    match b.iter().position(|&c| c == 0) {
        Some(i) => (&b[..i], &b[i + 1..]),
        None => (b, &[]),
    }
}

/// A mounted FUSE filesystem.
pub struct Session {
    dev: OwnedFd,
    mountpoint: PathBuf,
    caps: Caps,
    unmounted: AtomicBool,
    /// Mounted by `fusermount3`, and to be unmounted by it.
    helper: bool,
}

/// Mount options.
#[derive(Clone, Debug)]
pub struct MountOptions {
    pub fsname: String,
    pub allow_other: bool,
    pub default_permissions: bool,
    /// Ask for passthrough if the kernel has it.
    pub passthrough: bool,
}

impl Default for MountOptions {
    fn default() -> Self {
        MountOptions {
            fsname: "fusewire".into(),
            allow_other: true,
            default_permissions: true,
            passthrough: true,
        }
    }
}

const MAX_WRITE: u32 = 1 << 20;
const BUF_SIZE: usize = MAX_WRITE as usize + 2 * 4096;

impl Session {
    /// Mounts at `mountpoint` and completes the INIT handshake.
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
        // SAFETY: all pointers are valid NUL-terminated strings.
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
        let dev = if r == 0 {
            dev
        } else {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EPERM) {
                return Err(e);
            }
            // Not allowed to mount: the set-uid helper every FUSE
            // installation has does it for its user.
            helper = true;
            mount_with_helper(mountpoint, opts)
                .map_err(|h| io::Error::new(h.kind(), format!("mount: {e}; fusermount3: {h}")))?
        };
        let mut sess = Session {
            dev,
            mountpoint: mountpoint.to_path_buf(),
            caps: Caps::default(),
            unmounted: AtomicBool::new(false),
            helper,
        };
        if let Err(e) = sess.handshake(opts) {
            sess.unmount();
            return Err(e);
        }
        Ok(sess)
    }

    fn handshake(&mut self, opts: &MountOptions) -> io::Result<()> {
        let mut buf = vec![0u8; BUF_SIZE];
        let n = loop {
            // SAFETY: buf is a valid writable buffer of the given length.
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
            // Mutually exclusive with passthrough in the kernel; without
            // passthrough it batches small userspace writes.
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

    /// Starts `threads` worker threads serving `fs`. Returns their handles;
    /// they exit when the filesystem is unmounted.
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
        Ok(handles)
    }

    fn clone_dev(&self) -> io::Result<OwnedFd> {
        let fd = open_dev()?;
        let master: u32 = self.dev.as_raw_fd() as u32;
        // SAFETY: valid fd and pointer to a u32.
        let r = unsafe { libc::ioctl(fd.as_raw_fd(), FUSE_DEV_IOC_CLONE as libc::Ioctl, &master as *const u32) };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(fd)
    }

    /// Registers `fd` as a passthrough backing file. Returns the backing id to
    /// put in an open reply. The fd may be closed afterwards.
    pub fn backing_open(&self, fd: RawFd) -> io::Result<i32> {
        let map = fuse_backing_map { fd, flags: 0, padding: 0 };
        // SAFETY: valid fd and pointer to the ioctl argument struct.
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
        // SAFETY: valid fd and pointer to a u32.
        let r = unsafe {
            libc::ioctl(self.dev.as_raw_fd(), FUSE_DEV_IOC_BACKING_CLOSE as libc::Ioctl, &id as *const u32)
        };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Tells the kernel to drop the dentry `name` under `parent` (and with it
    /// the subtree's cached state once unused).
    pub fn notify_inval_entry(&self, parent: u64, name: &[u8]) -> io::Result<()> {
        let out = fuse_notify_inval_entry_out { parent, namelen: name.len() as u32, flags: 0 };
        let r = reply(
            self.dev.as_raw_fd(),
            0,
            FUSE_NOTIFY_INVAL_ENTRY,
            &[as_bytes(&out), name, &[0u8]],
        );
        match r {
            // Not cached is not an error.
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => Ok(()),
            r => r,
        }
    }

    /// Lazily unmounts. Worker threads exit once the kernel reports ENODEV.
    pub fn unmount(&self) {
        if self.unmounted.swap(true, Ordering::SeqCst) {
            return;
        }
        if self.helper {
            unmount_with_helper(&self.mountpoint);
        } else if let Ok(tgt) = CString::new(self.mountpoint.as_os_str().as_encoded_bytes()) {
            // SAFETY: valid C string.
            unsafe { libc::umount2(tgt.as_ptr(), libc::MNT_DETACH) };
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.unmount();
    }
}

/// Unmounts (lazily) through `fusermount3`, for mounts a user without the
/// right to unmount made through it.
pub fn unmount_with_helper(mountpoint: &Path) {
    let _ = std::process::Command::new("fusermount3")
        .args(["-u", "-z", "-q", "--"])
        .arg(mountpoint)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Has `fusermount3` mount at `mountpoint` and hand over the descriptor of
/// the connection (it sends it over the socket named in `_FUSE_COMMFD`).
fn mount_with_helper(mountpoint: &Path, opts: &MountOptions) -> io::Result<OwnedFd> {
    let mut pair = [0 as libc::c_int; 2];
    // SAFETY: a valid two-element array for the descriptors.
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, pair.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both were just returned by socketpair.
    let (ours, theirs) = unsafe { (OwnedFd::from_raw_fd(pair[0]), OwnedFd::from_raw_fd(pair[1])) };
    // SAFETY: plain fcntl on our own descriptor.
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
    // One message with one descriptor in its control data.
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec { iov_base: byte.as_mut_ptr() as *mut libc::c_void, iov_len: 1 };
    let mut control = [0u8; 64];
    // SAFETY: zeroed msghdr filled with valid pointers below.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = control.len() as _;
    // SAFETY: valid socket and message header.
    let n = unsafe { libc::recvmsg(ours.as_raw_fd(), &mut msg, 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the kernel filled the control buffer; the macros walk it.
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
    // SAFETY: valid C string literal.
    let fd = unsafe { libc::open(c"/dev/fuse".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd was just returned by open.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn reply(fd: RawFd, unique: u64, error: i32, parts: &[&[u8]]) -> io::Result<()> {
    let len: usize = std::mem::size_of::<fuse_out_header>() + parts.iter().map(|p| p.len()).sum::<usize>();
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
    // SAFETY: iov entries point to live buffers for the duration of the call.
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
    fuse_open_out { fh: o.fh, open_flags: o.flags, backing_id: o.backing_id }
}

fn worker<F: Filesystem>(sess: Arc<Session>, dev: OwnedFd, fs: Arc<F>) {
    let fd = dev.as_raw_fd();
    // u64 storage keeps the buffer 8-byte aligned.
    let mut storage = vec![0u64; BUF_SIZE / 8];
    let mut out: Vec<u8> = Vec::with_capacity(128 * 1024);
    loop {
        // SAFETY: storage is a valid writable buffer of BUF_SIZE bytes.
        let n = unsafe { libc::read(fd, storage.as_mut_ptr() as *mut libc::c_void, BUF_SIZE) };
        if n < 0 {
            match io::Error::last_os_error().raw_os_error() {
                Some(libc::EINTR) | Some(libc::EAGAIN) | Some(libc::ENOENT) => continue,
                // Unmounted or connection aborted.
                _ => break,
            }
        }
        let n = n as usize;
        // SAFETY: the first n bytes were initialised by the kernel.
        let buf = unsafe { std::slice::from_raw_parts(storage.as_ptr() as *const u8, n) };
        let Some(hdr) = read_struct::<fuse_in_header>(buf) else { continue };
        let body = &buf[std::mem::size_of::<fuse_in_header>()..];
        if hdr.opcode == FUSE_DESTROY {
            fs.destroy();
            let _ = reply(fd, hdr.unique, 0, &[]);
            break;
        }
        dispatch(&*fs, fd, &hdr, body, &mut out, sess.caps.max_write);
    }
}

macro_rules! arg {
    ($ty:ty, $body:expr, $fd:expr, $hdr:expr) => {
        match read_struct::<$ty>($body) {
            Some(v) => v,
            None => {
                let _ = reply($fd, $hdr.unique, -libc::EINVAL, &[]);
                return;
            }
        }
    };
}

fn send<T: Copy>(fd: RawFd, unique: u64, r: Res<T>) {
    let _ = match r {
        Ok(v) => reply(fd, unique, 0, &[as_bytes(&v)]),
        Err(e) => reply(fd, unique, -e.0, &[]),
    };
}

fn send_empty(fd: RawFd, unique: u64, r: Res<()>) {
    let _ = match r {
        Ok(()) => reply(fd, unique, 0, &[]),
        Err(e) => reply(fd, unique, -e.0, &[]),
    };
}

fn dispatch<F: Filesystem>(
    fs: &F,
    fd: RawFd,
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
            let (name, _) = cstr_arg(body);
            send(fd, u, fs.lookup(&ctx, node, name).map(|e| entry_out(&e)));
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
            let a = arg!(fuse_getattr_in, body, fd, hdr);
            let fh = (a.getattr_flags & FUSE_GETATTR_FH != 0).then_some(a.fh);
            send(fd, u, fs.getattr(&ctx, node, fh).map(|(a, t)| attr_out(&a, t)));
        }
        FUSE_SETATTR => {
            let a = arg!(fuse_setattr_in, body, fd, hdr);
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
            send(fd, u, fs.setattr(&ctx, node, &set).map(|(a, t)| attr_out(&a, t)));
        }
        FUSE_READLINK => {
            let _ = match fs.readlink(&ctx, node) {
                Ok(t) => reply(fd, u, 0, &[&t]),
                Err(e) => reply(fd, u, -e.0, &[]),
            };
        }
        FUSE_SYMLINK => {
            let (name, rest) = cstr_arg(body);
            let (target, _) = cstr_arg(rest);
            send(fd, u, fs.symlink(&ctx, node, name, target).map(|e| entry_out(&e)));
        }
        FUSE_MKNOD => {
            let a = arg!(fuse_mknod_in, body, fd, hdr);
            let (name, _) = cstr_arg(&body[std::mem::size_of::<fuse_mknod_in>()..]);
            let mode = a.mode & !a.umask;
            send(fd, u, fs.mknod(&ctx, node, name, mode, a.rdev).map(|e| entry_out(&e)));
        }
        FUSE_MKDIR => {
            let a = arg!(fuse_mkdir_in, body, fd, hdr);
            let (name, _) = cstr_arg(&body[std::mem::size_of::<fuse_mkdir_in>()..]);
            let mode = a.mode & !a.umask;
            send(fd, u, fs.mkdir(&ctx, node, name, mode).map(|e| entry_out(&e)));
        }
        FUSE_UNLINK => {
            let (name, _) = cstr_arg(body);
            send_empty(fd, u, fs.unlink(&ctx, node, name));
        }
        FUSE_RMDIR => {
            let (name, _) = cstr_arg(body);
            send_empty(fd, u, fs.rmdir(&ctx, node, name));
        }
        FUSE_RENAME => {
            let a = arg!(fuse_rename_in, body, fd, hdr);
            let (name, rest) = cstr_arg(&body[std::mem::size_of::<fuse_rename_in>()..]);
            let (newname, _) = cstr_arg(rest);
            send_empty(fd, u, fs.rename(&ctx, node, name, a.newdir, newname, 0));
        }
        FUSE_RENAME2 => {
            let a = arg!(fuse_rename2_in, body, fd, hdr);
            let (name, rest) = cstr_arg(&body[std::mem::size_of::<fuse_rename2_in>()..]);
            let (newname, _) = cstr_arg(rest);
            send_empty(fd, u, fs.rename(&ctx, node, name, a.newdir, newname, a.flags));
        }
        FUSE_LINK => {
            let a = arg!(fuse_link_in, body, fd, hdr);
            let (name, _) = cstr_arg(&body[std::mem::size_of::<fuse_link_in>()..]);
            send(fd, u, fs.link(&ctx, a.oldnodeid, node, name).map(|e| entry_out(&e)));
        }
        FUSE_OPEN => {
            let a = arg!(fuse_open_in, body, fd, hdr);
            send(fd, u, fs.open(&ctx, node, a.flags).map(|o| open_out(&o)));
        }
        FUSE_CREATE => {
            let a = arg!(fuse_create_in, body, fd, hdr);
            let (name, _) = cstr_arg(&body[std::mem::size_of::<fuse_create_in>()..]);
            let mode = a.mode & !a.umask;
            let _ = match fs.create(&ctx, node, name, mode, a.flags) {
                Ok((e, o)) => reply(fd, u, 0, &[as_bytes(&entry_out(&e)), as_bytes(&open_out(&o))]),
                Err(e) => reply(fd, u, -e.0, &[]),
            };
        }
        FUSE_READ => {
            let a = arg!(fuse_read_in, body, fd, hdr);
            out.clear();
            let size = a.size.min(max_write);
            let _ = match fs.read(&ctx, node, a.fh, a.offset, size, out) {
                Ok(()) => reply(fd, u, 0, &[out]),
                Err(e) => reply(fd, u, -e.0, &[]),
            };
        }
        FUSE_WRITE => {
            let a = arg!(fuse_write_in, body, fd, hdr);
            let data = &body[std::mem::size_of::<fuse_write_in>()..];
            let data = &data[..(a.size as usize).min(data.len())];
            send(
                fd,
                u,
                fs.write(&ctx, node, a.fh, a.offset, data)
                    .map(|n| fuse_write_out { size: n, padding: 0 }),
            );
        }
        FUSE_STATFS => {
            send(fd, u, fs.statfs(&ctx, node).map(|st| fuse_statfs_out { st }));
        }
        FUSE_RELEASE | FUSE_RELEASEDIR => {
            if hdr.opcode == FUSE_RELEASE {
                if let Some(a) = read_struct::<fuse_release_in>(body) {
                    fs.release(node, a.fh, a.flags);
                }
            }
            let _ = reply(fd, u, 0, &[]);
        }
        FUSE_FSYNC => {
            let a = arg!(fuse_fsync_in, body, fd, hdr);
            send_empty(fd, u, fs.fsync(&ctx, node, a.fh, a.fsync_flags & 1 != 0));
        }
        FUSE_FSYNCDIR | FUSE_SYNCFS => {
            let _ = reply(fd, u, 0, &[]);
        }
        FUSE_READDIR => {
            let a = arg!(fuse_read_in, body, fd, hdr);
            out.clear();
            let mut sink = DirSink { buf: out, cap: a.size as usize };
            let r = fs.readdir(&ctx, node, a.fh, a.offset, &mut sink);
            let _ = match r {
                Ok(()) => reply(fd, u, 0, &[out]),
                Err(e) => reply(fd, u, -e.0, &[]),
            };
        }
        FUSE_FALLOCATE => {
            let a = arg!(fuse_fallocate_in, body, fd, hdr);
            send_empty(fd, u, fs.fallocate(&ctx, node, a.fh, a.offset, a.length, a.mode));
        }
        FUSE_LSEEK => {
            let a = arg!(fuse_lseek_in, body, fd, hdr);
            send(
                fd,
                u,
                fs.lseek(&ctx, node, a.fh, a.offset, a.whence).map(|offset| fuse_lseek_out { offset }),
            );
        }
        FUSE_INTERRUPT => {}
        // Replying ENOSYS makes the kernel stop sending these for the life of
        // the mount: no flush on close, no xattr probes on exec and write, no
        // opendir/releasedir pairs, no access() calls (default_permissions
        // already checks modes).
        FUSE_FLUSH | FUSE_OPENDIR | FUSE_ACCESS | FUSE_GETXATTR | FUSE_LISTXATTR
        | FUSE_SETXATTR | FUSE_REMOVEXATTR | FUSE_IOCTL | FUSE_POLL | FUSE_BMAP
        | FUSE_COPY_FILE_RANGE | FUSE_GETLK | FUSE_SETLK | FUSE_SETLKW | FUSE_STATX => {
            let _ = reply(fd, u, -libc::ENOSYS, &[]);
        }
        FUSE_TMPFILE => {
            let _ = reply(fd, u, -libc::EOPNOTSUPP, &[]);
        }
        _ => {
            let _ = reply(fd, u, -libc::ENOSYS, &[]);
        }
    }
}
