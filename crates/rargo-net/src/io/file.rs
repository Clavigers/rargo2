use std::io::{self, ErrorKind};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, SystemTime};

use super::fiber::*;
use super::fs::{close_fd, open_at, read_at, write_at};
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

fn path_op(op: Op) -> io::Result<()> {
    match op_event(op)? {
        Event::Done => Ok(()),
        _ => Err(io::Error::other("a path operation")),
    }
}

fn stat_with(path: &Path, flags: i32) -> io::Result<Stat> {
    match op_event(Op::Statx { dir: CWD, path: c_path(path)?, flags })? {
        Event::Stat { buf } => Ok(Stat::of_statx(&buf)),
        _ => Err(io::Error::other("stat")),
    }
}

pub fn metadata(path: impl AsRef<Path>) -> io::Result<Stat> {
    let path = path.as_ref();
    if !on_fiber() {
        return std::fs::metadata(path).map(|md| Stat::of(&md));
    }
    stat_with(path, 0)
}

pub fn symlink_metadata(path: impl AsRef<Path>) -> io::Result<Stat> {
    let path = path.as_ref();
    if !on_fiber() {
        return std::fs::symlink_metadata(path).map(|md| Stat::of(&md));
    }
    stat_with(path, libc::AT_SYMLINK_NOFOLLOW)
}

pub fn exists(path: impl AsRef<Path>) -> bool {
    metadata(path).is_ok()
}

pub fn remove_file(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    if !on_fiber() {
        return std::fs::remove_file(path);
    }
    path_op(Op::UnlinkAt { dir: CWD, path: c_path(path)?, flags: 0 })
}

pub fn remove_dir(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    if !on_fiber() {
        return std::fs::remove_dir(path);
    }
    path_op(Op::UnlinkAt { dir: CWD, path: c_path(path)?, flags: libc::AT_REMOVEDIR })
}

pub fn create_dir(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    if !on_fiber() {
        return std::fs::create_dir(path);
    }
    path_op(Op::MkdirAt { dir: CWD, path: c_path(path)?, mode: 0o777 })
}

pub fn create_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    if !on_fiber() {
        return std::fs::create_dir_all(path);
    }
    if path.as_os_str().is_empty() {
        return Ok(());
    }
    match create_dir(path) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(_) if metadata(path).is_ok_and(|st| st.is_dir()) => return Ok(()),
        Err(e) => return Err(e),
    }
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }
    match create_dir(path) {
        Ok(()) => Ok(()),
        Err(_) if metadata(path).is_ok_and(|st| st.is_dir()) => Ok(()),
        Err(e) => Err(e),
    }
}

pub fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    let (from, to) = (from.as_ref(), to.as_ref());
    if !on_fiber() {
        return std::fs::rename(from, to);
    }
    match op_event(Op::RenameAt { old_dir: CWD, old: c_path(from)?, new_dir: CWD, new: c_path(to)? })? {
        Event::Renamed => Ok(()),
        _ => Err(io::Error::other("rename")),
    }
}

pub fn hard_link(original: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
    let (original, link) = (original.as_ref(), link.as_ref());
    if !on_fiber() {
        return std::fs::hard_link(original, link);
    }
    path_op(Op::LinkAt { old_dir: CWD, old: c_path(original)?, new_dir: CWD, new: c_path(link)? })
}

pub fn symlink(target: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
    let (target, link) = (target.as_ref(), link.as_ref());
    if !on_fiber() {
        return std::os::unix::fs::symlink(target, link);
    }
    path_op(Op::SymlinkAt { target: c_path(target)?, new_dir: CWD, new: c_path(link)? })
}

fn with_fd<T>(fd: i32, body: impl FnOnce(i32) -> io::Result<T>) -> io::Result<T> {
    let out = body(fd);
    let closed = close_fd(fd);
    let out = out?;
    closed?;
    Ok(out)
}

pub fn read(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    let path = path.as_ref();
    if !on_fiber() {
        return std::fs::read(path);
    }
    let fd = open_at(CWD, path, libc::O_RDONLY | libc::O_CLOEXEC, 0)?;
    with_fd(fd, |fd| {
        let mut out = Vec::new();
        loop {
            let chunk = read_at(fd, out.len() as u64, CHUNK)?;
            if chunk.is_empty() {
                return Ok(out);
            }
            out.extend_from_slice(&chunk);
        }
    })
}

pub fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
    String::from_utf8(read(path)?).map_err(|_| io::Error::new(ErrorKind::InvalidData, "stream did not contain valid UTF-8"))
}

pub fn write(path: impl AsRef<Path>, bytes: impl AsRef<[u8]>) -> io::Result<()> {
    let (path, bytes) = (path.as_ref(), bytes.as_ref());
    if !on_fiber() {
        return std::fs::write(path, bytes);
    }
    let fd = open_at(CWD, path, libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_CLOEXEC, 0o666)?;
    with_fd(fd, |fd| {
        let mut off = 0;
        while off < bytes.len() {
            let end = (off + CHUNK).min(bytes.len());
            let n = write_at(fd, off as u64, &bytes[off..end])?;
            if n == 0 {
                return Err(io::Error::new(ErrorKind::WriteZero, "a write wrote nothing"));
            }
            off += n;
        }
        Ok(())
    })
}
