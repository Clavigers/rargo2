//! Watching worktrees for changes (inotify).
//!
//! The daemon does not learn *what* changed from the watcher, only *that*
//! something did, and where: a worktree with no event since its last scan
//! is known to be as that scan saw it, so a command needs no scan at all,
//! and an event starts the scan that pushes the change to the server before
//! anyone asks for a build. Whenever the watcher cannot vouch for a
//! worktree (the kernel dropped events, a directory could not be watched)
//! the worktree is simply scanned again, as it was before there was a
//! watcher.

use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Mutex;

const MASK: u32 = libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_MODIFY
    | libc::IN_CLOSE_WRITE
    | libc::IN_MOVED_FROM
    | libc::IN_MOVED_TO
    | libc::IN_ATTRIB
    | libc::IN_DELETE_SELF
    | libc::IN_MOVE_SELF
    | libc::IN_ONLYDIR
    | libc::IN_DONT_FOLLOW;

/// What an event batch says.
pub enum Event {
    /// Something changed in a directory of worktree `0`.
    Changed(u64),
    /// Events were lost: nothing is known about any worktree.
    Overflow,
}

pub struct Watcher {
    fd: tokio::io::unix::AsyncFd<OwnedFd>,
    /// Watch descriptor to the worktree it belongs to.
    owners: Mutex<HashMap<i32, u64>>,
}

impl Watcher {
    pub fn new() -> std::io::Result<Watcher> {
        // SAFETY: plain syscall; the result is checked.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `fd` is a descriptor we own.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: the descriptor is owned by the `AsyncFd` from here on
        // and closed only when it is dropped.
        let fd = unsafe { tokio::io::unix::AsyncFd::register(fd) }.map_err(std::io::Error::from)?;
        Ok(Watcher { fd, owners: Mutex::new(HashMap::new()) })
    }

    /// Watches directory `dir` on behalf of worktree `owner`.
    pub fn add(&self, dir: &Path, owner: u64) -> std::io::Result<i32> {
        let c = CString::new(dir.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
        // SAFETY: valid descriptor and NUL-terminated path.
        let wd = unsafe { libc::inotify_add_watch(self.fd.get_ref().as_raw_fd(), c.as_ptr(), MASK) };
        if wd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        self.owners.lock().unwrap().insert(wd, owner);
        Ok(wd)
    }

    /// Stops watching everything of worktree `owner`.
    pub fn forget(&self, owner: u64) {
        let mut owners = self.owners.lock().unwrap();
        let wds: Vec<i32> = owners.iter().filter(|(_, o)| **o == owner).map(|(wd, _)| *wd).collect();
        for wd in wds {
            owners.remove(&wd);
            // SAFETY: valid descriptor; a stale watch descriptor is an error we ignore.
            unsafe { libc::inotify_rm_watch(self.fd.get_ref().as_raw_fd(), wd) };
        }
    }

    /// Waits for events and returns what they say, without duplicates.
    pub async fn next(&self) -> std::io::Result<Vec<Event>> {
        let mut buf = vec![0u8; 64 << 10];
        loop {
            let mut guard = self.fd.readable().await?;
            // SAFETY: reading into a buffer of the given length.
            let n = unsafe { libc::read(self.fd.get_ref().as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    guard.clear_ready();
                    continue;
                }
                return Err(e);
            }
            let mut out: Vec<Event> = Vec::new();
            let mut seen: Vec<u64> = Vec::new();
            let mut at = 0usize;
            let n = n as usize;
            const HEAD: usize = std::mem::size_of::<libc::inotify_event>();
            while at + HEAD <= n {
                // SAFETY: the kernel wrote a complete event header here.
                let ev: libc::inotify_event = unsafe { std::ptr::read_unaligned(buf.as_ptr().add(at) as *const _) };
                at += HEAD + ev.len as usize;
                if ev.mask & libc::IN_Q_OVERFLOW != 0 {
                    out.push(Event::Overflow);
                    continue;
                }
                let mut owners = self.owners.lock().unwrap();
                let owner = owners.get(&ev.wd).copied();
                if ev.mask & libc::IN_IGNORED != 0 {
                    // The directory is gone and its watch with it.
                    owners.remove(&ev.wd);
                }
                if let Some(o) = owner {
                    if !seen.contains(&o) {
                        seen.push(o);
                        out.push(Event::Changed(o));
                    }
                }
            }
            if !out.is_empty() {
                return Ok(out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_changes_per_worktree() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let a = tempfile::tempdir().unwrap();
            let b = tempfile::tempdir().unwrap();
            std::fs::create_dir(a.path().join("src")).unwrap();
            let w = Watcher::new().unwrap();
            w.add(a.path(), 1).unwrap();
            w.add(&a.path().join("src"), 1).unwrap();
            w.add(b.path(), 2).unwrap();
            std::fs::write(a.path().join("src/lib.rs"), "x").unwrap();
            let ev = tokio::time::timeout(std::time::Duration::from_secs(5), w.next()).await.unwrap().unwrap();
            assert!(matches!(ev[..], [Event::Changed(1)]));
            std::fs::write(b.path().join("new"), "y").unwrap();
            let ev = tokio::time::timeout(std::time::Duration::from_secs(5), w.next()).await.unwrap().unwrap();
            assert!(matches!(ev[..], [Event::Changed(2)]));
            // Nothing more about a worktree that is no longer watched.
            w.forget(2);
            std::fs::write(b.path().join("newer"), "z").unwrap();
            std::fs::remove_file(a.path().join("src/lib.rs")).unwrap();
            let ev = tokio::time::timeout(std::time::Duration::from_secs(5), w.next()).await.unwrap().unwrap();
            assert!(matches!(ev[..], [Event::Changed(1)]));
        });
    }
}
