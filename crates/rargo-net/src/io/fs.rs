
use std::io::{self, ErrorKind};
use std::path::Path;


use super::fiber::*;
use super::ops::{c_path, Event, Op};


pub fn read_at(fd: i32, off: u64, len: usize) -> io::Result<Vec<u8>> {
    match op_event(Op::Read { fd, off, len })? {
        Event::Read { n, mut buf } => {
            buf.truncate(n);
            Ok(buf)
        }
        _ => Err(io::Error::other("read")),
    }
}

pub fn write_at(fd: i32, off: u64, buf: &[u8]) -> io::Result<usize> {
    match op_event(Op::Write { fd, off, buf: buf.to_vec() })? {
        Event::Write { n } => Ok(n),
        _ => Err(io::Error::other("write")),
    }
}

pub fn open_at(dir: i32, path: &Path, flags: i32, mode: u32) -> io::Result<i32> {
    let path = c_path(path)?;
    match op_event(Op::OpenAt { dir, path, flags, mode })? {
        Event::Open { fd } => Ok(fd),
        _ => Err(io::Error::other("open")),
    }
}

pub fn stat_at(dir: i32, path: &Path) -> io::Result<Box<libc::statx>> {
    let path = c_path(path)?;
    match op_event(Op::Statx { dir, path })? {
        Event::Stat { buf } => Ok(buf),
        _ => Err(io::Error::other("stat")),
    }
}

pub fn rename_at(old_dir: i32, old: &Path, new_dir: i32, new: &Path) -> io::Result<()> {
    let old = c_path(old)?;
    let new = c_path(new)?;
    match op_event(Op::RenameAt { old_dir, old, new_dir, new })? {
        Event::Renamed => Ok(()),
        _ => Err(io::Error::other("rename")),
    }
}

pub fn fsync_fd(fd: i32) -> io::Result<()> {
    match op_event(Op::Fsync { fd })? {
        Event::Synced => Ok(()),
        _ => Err(io::Error::other("fsync")),
    }
}

pub fn close_fd(fd: i32) -> io::Result<()> {
    match op_event(Op::Close { fd })? {
        Event::Closed => Ok(()),
        _ => Err(io::Error::other("close")),
    }
}

pub fn try_read(fd: i32, len: usize) -> io::Result<Option<Vec<u8>>> {
    let mut buf = vec![0u8; len];
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, len) };
        if n >= 0 {
            buf.truncate(n as usize);
            return Ok(Some(buf));
        }
        let err = io::Error::last_os_error();
        match err.kind() {
            ErrorKind::Interrupted => continue,
            ErrorKind::WouldBlock => return Ok(None),
            _ => return Err(err),
        }
    }
}

pub fn read_ready(fd: i32, len: usize) -> io::Result<Vec<u8>> {
    loop {
        if let Some(buf) = try_read(fd, len)? {
            return Ok(buf);
        }
        match op_event(Op::Readable { fd })? {
            Event::Readable => {}
            _ => return Err(io::Error::other("poll")),
        }
    }
}

