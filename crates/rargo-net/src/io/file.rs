use std::io::{self, ErrorKind};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, SystemTime};

use super::fiber::*;
use super::fs::{close_fd, fsync_fd, open_at, read_at, write_at};
use super::ops::{c_path, Event, Op};

const CWD: i32 = libc::AT_FDCWD;
const CHUNK: usize = 256 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct Stat {
    mode: u32,
    size: u64,
    blocks: u64,
    nlink: u64,
    ino: u64,
    dev: u64,
    rdev: u64,
    uid: u32,
    gid: u32,
    atime: (i64, u32),
    mtime: (i64, u32),
    ctime: (i64, u32),
}

impl Stat {
    fn of_statx(st: &libc::statx) -> Self {
        Self {
            mode: u32::from(st.stx_mode),
            size: st.stx_size,
            blocks: st.stx_blocks,
            nlink: u64::from(st.stx_nlink),
            ino: st.stx_ino,
            dev: libc::makedev(st.stx_dev_major, st.stx_dev_minor),
            rdev: libc::makedev(st.stx_rdev_major, st.stx_rdev_minor),
            uid: st.stx_uid,
            gid: st.stx_gid,
            atime: (st.stx_atime.tv_sec, st.stx_atime.tv_nsec),
            mtime: (st.stx_mtime.tv_sec, st.stx_mtime.tv_nsec),
            ctime: (st.stx_ctime.tv_sec, st.stx_ctime.tv_nsec),
        }
    }

    fn of_mode(kind: u32) -> Self {
        Self { mode: kind, size: 0, blocks: 0, nlink: 1, ino: 0, dev: 0, rdev: 0, uid: 0, gid: 0, atime: (0, 0), mtime: (0, 0), ctime: (0, 0) }
    }

    pub fn of(md: &std::fs::Metadata) -> Self {
        Self {
            mode: md.mode(),
            size: md.size(),
            blocks: md.blocks(),
            nlink: md.nlink(),
            ino: md.ino(),
            dev: md.dev(),
            rdev: md.rdev(),
            uid: md.uid(),
            gid: md.gid(),
            atime: (md.atime(), md.atime_nsec() as u32),
            mtime: (md.mtime(), md.mtime_nsec() as u32),
            ctime: (md.ctime(), md.ctime_nsec() as u32),
        }
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn blocks(&self) -> u64 {
        self.blocks
    }

    pub fn rdev(&self) -> u64 {
        self.rdev
    }

    pub fn uid(&self) -> u32 {
        self.uid
    }

    pub fn gid(&self) -> u32 {
        self.gid
    }

    pub fn atime(&self) -> i64 {
        self.atime.0
    }

    pub fn atime_nsec(&self) -> i64 {
        i64::from(self.atime.1)
    }

    pub fn mtime(&self) -> i64 {
        self.mtime.0
    }

    pub fn mtime_nsec(&self) -> i64 {
        i64::from(self.mtime.1)
    }

    pub fn ctime(&self) -> i64 {
        self.ctime.0
    }

    pub fn ctime_nsec(&self) -> i64 {
        i64::from(self.ctime.1)
    }

    pub fn len(&self) -> u64 {
        self.size
    }

    pub fn mode(&self) -> u32 {
        self.mode
    }

    pub fn nlink(&self) -> u64 {
        self.nlink
    }

    pub fn ino(&self) -> u64 {
        self.ino
    }

    pub fn dev(&self) -> u64 {
        self.dev
    }

    pub fn file_type(&self) -> Stat {
        *self
    }

    pub fn is_dir(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFDIR
    }

    pub fn is_file(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFREG
    }

    pub fn is_symlink(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFLNK
    }

    pub fn is_executable(&self) -> bool {
        self.mode & 0o111 != 0
    }

    pub fn mtime_ns(&self) -> i64 {
        self.mtime.0 * 1_000_000_000 + i64::from(self.mtime.1)
    }

    pub fn modified(&self) -> io::Result<SystemTime> {
        let (secs, nanos) = self.mtime;
        let at = if secs >= 0 {
            SystemTime::UNIX_EPOCH.checked_add(Duration::new(secs as u64, nanos))
        } else {
            SystemTime::UNIX_EPOCH.checked_sub(Duration::new(secs.unsigned_abs(), 0)).and_then(|t| t.checked_add(Duration::new(0, nanos)))
        };
        at.ok_or_else(|| io::Error::other("a modification time out of range"))
    }
}

fn route<T: Send + 'static>(
    on_ring: impl FnOnce() -> io::Result<T> + Send + 'static,
    direct: impl FnOnce() -> io::Result<T>,
) -> io::Result<T> {
    if on_fiber() {
        on_ring()
    } else if let Some(owner) = super::task::home() {
        super::task::call_home(owner, on_ring)
    } else {
        direct()
    }
}

fn path_op(op: Op) -> io::Result<()> {
    match op_event(op)? {
        Event::Done => Ok(()),
        _ => Err(io::Error::other("a path operation")),
    }
}

fn stat_with(dir: i32, path: &Path, flags: i32) -> io::Result<Stat> {
    match op_event(Op::Statx { dir, path: c_path(path)?, flags })? {
        Event::Stat { buf } => Ok(Stat::of_statx(&buf)),
        _ => Err(io::Error::other("stat")),
    }
}

pub fn metadata(path: impl AsRef<Path>) -> io::Result<Stat> {
    let path = path.as_ref().to_path_buf();
    let at = path.clone();
    route(move || stat_with(CWD, &at, 0), || std::fs::metadata(&path).map(|md| Stat::of(&md)))
}

pub fn symlink_metadata(path: impl AsRef<Path>) -> io::Result<Stat> {
    let path = path.as_ref().to_path_buf();
    let at = path.clone();
    route(move || stat_with(CWD, &at, libc::AT_SYMLINK_NOFOLLOW), || std::fs::symlink_metadata(&path).map(|md| Stat::of(&md)))
}

pub fn exists(path: impl AsRef<Path>) -> bool {
    metadata(path).is_ok()
}

pub fn remove_file(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref().to_path_buf();
    let at = path.clone();
    route(move || path_op(Op::UnlinkAt { dir: CWD, path: c_path(&at)?, flags: 0 }), || std::fs::remove_file(&path))
}

pub fn remove_dir(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref().to_path_buf();
    let at = path.clone();
    route(move || path_op(Op::UnlinkAt { dir: CWD, path: c_path(&at)?, flags: libc::AT_REMOVEDIR }), || std::fs::remove_dir(&path))
}

pub fn create_dir(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref().to_path_buf();
    let at = path.clone();
    route(move || path_op(Op::MkdirAt { dir: CWD, path: c_path(&at)?, mode: 0o777 }), || std::fs::create_dir(&path))
}

fn ring_create_dir_all(path: &Path, mode: u32) -> io::Result<()> {
    if path.as_os_str().is_empty() {
        return Ok(());
    }
    let mkdir = |p: &Path| path_op(Op::MkdirAt { dir: CWD, path: c_path(p)?, mode });
    let is_dir = |p: &Path| stat_with(CWD, p, 0).is_ok_and(|st| st.is_dir());
    match mkdir(path) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(_) if is_dir(path) => return Ok(()),
        Err(e) => return Err(e),
    }
    if let Some(parent) = path.parent() {
        ring_create_dir_all(parent, mode)?;
    }
    match mkdir(path) {
        Ok(()) => Ok(()),
        Err(_) if is_dir(path) => Ok(()),
        Err(e) => Err(e),
    }
}

pub fn create_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    DirBuilder::new().recursive(true).create(path)
}

/// Makes a directory with chosen permission bits, and its missing parents when asked.
#[derive(Clone, Debug)]
pub struct DirBuilder {
    recursive: bool,
    mode: u32,
}

impl Default for DirBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl DirBuilder {
    pub fn new() -> Self {
        Self { recursive: false, mode: 0o777 }
    }

    pub fn recursive(&mut self, on: bool) -> &mut Self {
        self.recursive = on;
        self
    }

    pub fn mode(&mut self, mode: u32) -> &mut Self {
        self.mode = mode;
        self
    }

    pub fn create(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let (recursive, mode) = (self.recursive, self.mode);
        let path = path.as_ref().to_path_buf();
        let at = path.clone();
        route(
            move || if recursive { ring_create_dir_all(&at, mode) } else { path_op(Op::MkdirAt { dir: CWD, path: c_path(&at)?, mode }) },
            || std::os::unix::fs::DirBuilderExt::mode(std::fs::DirBuilder::new().recursive(recursive), mode).create(&path),
        )
    }
}

pub fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    let (from, to) = (from.as_ref().to_path_buf(), to.as_ref().to_path_buf());
    let (old, new) = (from.clone(), to.clone());
    route(
        move || match op_event(Op::RenameAt { old_dir: CWD, old: c_path(&old)?, new_dir: CWD, new: c_path(&new)? })? {
            Event::Renamed => Ok(()),
            _ => Err(io::Error::other("rename")),
        },
        || std::fs::rename(&from, &to),
    )
}

pub fn hard_link(original: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
    let (original, link) = (original.as_ref().to_path_buf(), link.as_ref().to_path_buf());
    let (old, new) = (original.clone(), link.clone());
    route(move || path_op(Op::LinkAt { old_dir: CWD, old: c_path(&old)?, new_dir: CWD, new: c_path(&new)? }), || std::fs::hard_link(&original, &link))
}

pub fn symlink(target: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
    let (target, link) = (target.as_ref().to_path_buf(), link.as_ref().to_path_buf());
    let (to, at) = (target.clone(), link.clone());
    route(move || path_op(Op::SymlinkAt { target: c_path(&to)?, new_dir: CWD, new: c_path(&at)? }), || std::os::unix::fs::symlink(&target, &link))
}

pub fn read(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    let mut f = File::open(path)?;
    let mut out = Vec::new();
    std::io::Read::read_to_end(&mut f, &mut out)?;
    Ok(out)
}

pub fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
    String::from_utf8(read(path)?).map_err(|_| io::Error::new(ErrorKind::InvalidData, "stream did not contain valid UTF-8"))
}

pub fn write(path: impl AsRef<Path>, bytes: impl AsRef<[u8]>) -> io::Result<()> {
    let f = File::create(path)?;
    f.write_all_at(bytes.as_ref(), 0)
}

fn offload<T: Send>(body: impl FnOnce() -> T + Send) -> T {
    if on_fiber() { super::blocking(body) } else { body() }
}

pub fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<u64> {
    let (from, to) = (from.as_ref(), to.as_ref());
    let src = File::open(from)?;
    let mode = src.metadata()?.mode() & 0o7777;
    let dst = OpenOptions::new().write(true).create(true).truncate(true).mode(mode).open(to)?;
    let copied = copy_fd(std::os::fd::AsRawFd::as_raw_fd(&src), std::os::fd::AsRawFd::as_raw_fd(&dst))?;
    dst.set_mode(mode)?;
    Ok(copied)
}

/// One name in a directory.
#[derive(Clone, Debug)]
pub struct DirEntry {
    path: std::path::PathBuf,
    kind: char,
}

impl DirEntry {
    pub fn path(&self) -> std::path::PathBuf {
        self.path.clone()
    }

    pub fn file_name(&self) -> std::ffi::OsString {
        self.path.file_name().unwrap_or_default().to_os_string()
    }

    /// What the name is, without following a link.
    pub fn file_type(&self) -> io::Result<Stat> {
        match self.kind {
            'd' => Ok(Stat::of_mode(libc::S_IFDIR)),
            'f' => Ok(Stat::of_mode(libc::S_IFREG)),
            'l' => Ok(Stat::of_mode(libc::S_IFLNK)),
            _ => symlink_metadata(&self.path),
        }
    }

    /// The entry's metadata, without following a link.
    pub fn metadata(&self) -> io::Result<Stat> {
        symlink_metadata(&self.path)
    }
}

pub fn read_dir(path: impl AsRef<Path>) -> io::Result<std::vec::IntoIter<io::Result<DirEntry>>> {
    let path = path.as_ref();
    if let Some(names) = super::sim::read_dir(path) {
        let entries: Vec<_> = names?.into_iter().map(|(name, kind)| Ok(DirEntry { path: path.join(name), kind })).collect();
        return Ok(entries.into_iter());
    }
    offload(|| {
        std::fs::read_dir(path).map(|d| {
            d.map(|e| {
                e.map(|e| {
                    let kind = match e.file_type() {
                        Ok(t) if t.is_dir() => 'd',
                        Ok(t) if t.is_file() => 'f',
                        Ok(t) if t.is_symlink() => 'l',
                        _ => '?',
                    };
                    DirEntry { path: e.path(), kind }
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
        })
    })
}

pub fn remove_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    if let Some(done) = super::sim::remove_dir_all(path) {
        return done;
    }
    offload(|| std::fs::remove_dir_all(path))
}

pub fn set_permissions(path: impl AsRef<Path>, mode: std::fs::Permissions) -> io::Result<()> {
    set_mode(path, std::os::unix::fs::PermissionsExt::mode(&mode))
}

/// Sets a path's permission bits.
pub fn set_mode(path: impl AsRef<Path>, mode: u32) -> io::Result<()> {
    let path = path.as_ref();
    if let Some(done) = super::sim::set_mode(path, mode) {
        return done;
    }
    offload(|| std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(mode)))
}

pub fn read_link(path: impl AsRef<Path>) -> io::Result<std::path::PathBuf> {
    let path = path.as_ref();
    if let Some(done) = super::sim::read_link(path) {
        return done;
    }
    offload(|| std::fs::read_link(path))
}

pub fn canonicalize(path: impl AsRef<Path>) -> io::Result<std::path::PathBuf> {
    let path = path.as_ref();
    if let Some(done) = super::sim::canonicalize(path) {
        return done;
    }
    offload(|| std::fs::canonicalize(path))
}

pub fn set_modified(path: impl AsRef<Path>, at: SystemTime) -> io::Result<()> {
    let path = path.as_ref();
    if let Some(done) = super::sim::set_modified(path, at) {
        return done;
    }
    offload(|| std::fs::File::options().append(true).open(path).and_then(|f| f.set_modified(at)))
}

pub trait PathRing {
    fn ring_exists(&self) -> bool;
    fn ring_is_file(&self) -> bool;
    fn ring_is_dir(&self) -> bool;
    fn ring_canonicalize(&self) -> io::Result<std::path::PathBuf>;
}

impl PathRing for Path {
    fn ring_exists(&self) -> bool {
        exists(self)
    }

    fn ring_is_file(&self) -> bool {
        metadata(self).is_ok_and(|m| m.is_file())
    }

    fn ring_is_dir(&self) -> bool {
        metadata(self).is_ok_and(|m| m.is_dir())
    }

    fn ring_canonicalize(&self) -> io::Result<std::path::PathBuf> {
        canonicalize(self)
    }
}

#[derive(Clone, Debug)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
    mode: u32,
    custom: i32,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenOptions {
    pub fn new() -> Self {
        Self { read: false, write: false, append: false, truncate: false, create: false, create_new: false, mode: 0o666, custom: 0 }
    }

    pub fn read(&mut self, on: bool) -> &mut Self {
        self.read = on;
        self
    }

    pub fn write(&mut self, on: bool) -> &mut Self {
        self.write = on;
        self
    }

    pub fn append(&mut self, on: bool) -> &mut Self {
        self.append = on;
        self
    }

    pub fn truncate(&mut self, on: bool) -> &mut Self {
        self.truncate = on;
        self
    }

    pub fn create(&mut self, on: bool) -> &mut Self {
        self.create = on;
        self
    }

    pub fn create_new(&mut self, on: bool) -> &mut Self {
        self.create_new = on;
        self
    }

    pub fn mode(&mut self, mode: u32) -> &mut Self {
        self.mode = mode;
        self
    }

    pub fn custom_flags(&mut self, flags: i32) -> &mut Self {
        self.custom = flags;
        self
    }

    fn flags(&self) -> i32 {
        let access = match (self.read, self.write || self.append) {
            (_, false) => libc::O_RDONLY,
            (false, true) => libc::O_WRONLY,
            (true, true) => libc::O_RDWR,
        };
        let create = if self.create_new {
            libc::O_CREAT | libc::O_EXCL
        } else if self.create {
            libc::O_CREAT
        } else {
            0
        };
        access | create | if self.truncate { libc::O_TRUNC } else { 0 } | if self.append { libc::O_APPEND } else { 0 } | libc::O_CLOEXEC | self.custom
    }

    pub fn open(&self, path: impl AsRef<Path>) -> io::Result<File> {
        let (flags, mode) = (self.flags(), self.mode);
        let path = path.as_ref().to_path_buf();
        let at = path.clone();
        let fd = route(
            move || open_at(CWD, &at, flags, mode),
            || {
                let c = c_path(&path)?;
                let fd = unsafe { libc::open(c.as_ptr(), flags, mode) };
                if fd < 0 { Err(io::Error::last_os_error()) } else { Ok(fd) }
            },
        )?;
        Ok(File { fd, pos: 0 })
    }
}

#[derive(Debug)]
pub struct File {
    fd: i32,
    pos: u64,
}

fn direct<T>(n: isize, ok: impl FnOnce(usize) -> T) -> io::Result<T> {
    if n < 0 { Err(io::Error::last_os_error()) } else { Ok(ok(n as usize)) }
}

impl File {
    pub fn open(path: impl AsRef<Path>) -> io::Result<File> {
        OpenOptions::new().read(true).open(path)
    }

    pub fn create(path: impl AsRef<Path>) -> io::Result<File> {
        OpenOptions::new().write(true).create(true).truncate(true).open(path)
    }

    pub fn read_at(&self, buf: &mut [u8], off: u64) -> io::Result<usize> {
        let (fd, len) = (self.fd, buf.len().min(CHUNK));
        let got = route(
            move || read_at(fd, off, len),
            || {
                let mut got = vec![0u8; len];
                direct(unsafe { libc::pread(fd, got.as_mut_ptr().cast(), len, off as libc::off_t) }, |n| {
                    got.truncate(n);
                    got
                })
            },
        )?;
        buf[..got.len()].copy_from_slice(&got);
        Ok(got.len())
    }

    pub fn read_exact_at(&self, mut buf: &mut [u8], mut off: u64) -> io::Result<()> {
        while !buf.is_empty() {
            match self.read_at(buf, off)? {
                0 => return Err(io::Error::new(ErrorKind::UnexpectedEof, "failed to fill whole buffer")),
                n => {
                    buf = &mut buf[n..];
                    off += n as u64;
                }
            }
        }
        Ok(())
    }

    pub fn write_at(&self, buf: &[u8], off: u64) -> io::Result<usize> {
        let fd = self.fd;
        let part = buf[..buf.len().min(CHUNK)].to_vec();
        route(
            move || write_at(fd, off, &part),
            || direct(unsafe { libc::pwrite(fd, buf.as_ptr().cast(), buf.len().min(CHUNK), off as libc::off_t) }, |n| n),
        )
    }

    pub fn write_all_at(&self, mut buf: &[u8], mut off: u64) -> io::Result<()> {
        while !buf.is_empty() {
            match self.write_at(buf, off)? {
                0 => return Err(io::Error::new(ErrorKind::WriteZero, "a write wrote nothing")),
                n => {
                    buf = &buf[n..];
                    off += n as u64;
                }
            }
        }
        Ok(())
    }

    pub fn metadata(&self) -> io::Result<Stat> {
        let fd = self.fd;
        route(
            move || stat_with(fd, Path::new(""), libc::AT_EMPTY_PATH),
            || {
                let f = std::mem::ManuallyDrop::new(unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fd) });
                f.metadata().map(|md| Stat::of(&md))
            },
        )
    }

    pub fn sync_all(&self) -> io::Result<()> {
        let fd = self.fd;
        route(move || fsync_fd(fd), || direct(unsafe { libc::fsync(fd) } as isize, |_| ()))
    }

    pub fn sync_data(&self) -> io::Result<()> {
        self.sync_all()
    }

    pub fn try_clone(&self) -> io::Result<File> {
        if let Some(fd) = super::sim::dup(self.fd) {
            return Ok(File { fd: fd?, pos: self.pos });
        }
        let fd = unsafe { libc::fcntl(self.fd, libc::F_DUPFD_CLOEXEC, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(File { fd, pos: self.pos })
    }

    pub fn set_len(&self, len: u64) -> io::Result<()> {
        let fd = self.fd;
        route(move || path_op(Op::Ftruncate { fd, len }), || direct(unsafe { libc::ftruncate(fd, len as libc::off_t) } as isize, |_| ()))
    }

    pub fn set_mode(&self, mode: u32) -> io::Result<()> {
        if let Some(done) = super::sim::set_fd_mode(self.fd, mode) {
            return done;
        }
        direct(unsafe { libc::fchmod(self.fd, mode) } as isize, |_| ())
    }

    /// Sets the access and modification times as `futimens` does, so a time may be `UTIME_NOW` or `UTIME_OMIT`. A simulated file keeps only its modification time.
    pub fn set_times(&self, atime: libc::timespec, mtime: libc::timespec) -> io::Result<()> {
        if super::sim::active() {
            let mtime_ns = match mtime.tv_nsec {
                libc::UTIME_OMIT => None,
                libc::UTIME_NOW => Some(super::sim::system_now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as i64)),
                nsec => Some(mtime.tv_sec * 1_000_000_000 + nsec),
            };
            if let Some(done) = super::sim::set_fd_modified(self.fd, mtime_ns) {
                return done;
            }
        }
        let times = [atime, mtime];
        direct(unsafe { libc::futimens(self.fd, times.as_ptr()) } as isize, |_| ())
    }

    /// Reserves or releases a range of the file as `fallocate` does.
    pub fn allocate(&self, mode: i32, offset: u64, len: u64) -> io::Result<()> {
        if let Some(done) = super::sim::allocate(self.fd, mode, offset, len) {
            return done;
        }
        direct(unsafe { libc::fallocate(self.fd, mode, offset as libc::off_t, len as libc::off_t) } as isize, |_| ())
    }
}

impl std::io::Read for File {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.read_at(buf, self.pos)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl std::io::Write for File {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.write_at(buf, self.pos)?;
        self.pos += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl std::io::Seek for File {
    fn seek(&mut self, to: std::io::SeekFrom) -> io::Result<u64> {
        let base = match to {
            std::io::SeekFrom::Start(n) => return Ok({ self.pos = n; n }),
            std::io::SeekFrom::Current(d) => self.pos as i64 + d,
            std::io::SeekFrom::End(d) => self.metadata()?.len() as i64 + d,
        };
        self.pos = u64::try_from(base).map_err(|_| io::Error::new(ErrorKind::InvalidInput, "a seek before the start"))?;
        Ok(self.pos)
    }
}

impl std::os::unix::fs::FileExt for File {
    fn read_at(&self, buf: &mut [u8], off: u64) -> io::Result<usize> {
        File::read_at(self, buf, off)
    }

    fn write_at(&self, buf: &[u8], off: u64) -> io::Result<usize> {
        File::write_at(self, buf, off)
    }
}

impl std::os::fd::AsRawFd for File {
    fn as_raw_fd(&self) -> i32 {
        self.fd
    }
}

impl std::os::fd::IntoRawFd for File {
    fn into_raw_fd(self) -> i32 {
        let fd = self.fd;
        std::mem::forget(self);
        fd
    }
}

impl Drop for File {
    fn drop(&mut self) {
        let fd = self.fd;
        let _ = route(move || close_fd(fd), || direct(unsafe { libc::close(fd) } as isize, |_| ()));
    }
}

pub fn copy_fd(src: i32, dst: i32) -> io::Result<u64> {
    let mut buf = vec![0u8; CHUNK];
    let mut off = 0u64;
    let (from, to) = (std::mem::ManuallyDrop::new(File { fd: src, pos: 0 }), std::mem::ManuallyDrop::new(File { fd: dst, pos: 0 }));
    loop {
        let n = from.read_at(&mut buf, off)?;
        if n == 0 {
            return Ok(off);
        }
        to.write_all_at(&buf[..n], off)?;
        off += n as u64;
    }
}

impl From<File> for std::os::fd::OwnedFd {
    fn from(file: File) -> Self {
        use std::os::fd::{FromRawFd, IntoRawFd};
        unsafe { std::os::fd::OwnedFd::from_raw_fd(file.into_raw_fd()) }
    }
}
