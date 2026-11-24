//! Watching worktrees for changes (inotify).
//!
//! The daemon does not learn *what* changed from the watcher, only *that*
//! something did, and in which directory: a directory with a watch and no
//! event since the last scan is as that scan saw it, so the next scan
//! looks only at the directories with events and at those without a watch
//! (see `scan::rescan`), and an event starts the scan that pushes the
//! change to the server before anyone asks for a build. When the kernel
//! dropped events, or a watched directory moved (its watches now stand for
//! other paths), the whole worktree is scanned again.

use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
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
#[derive(Debug, PartialEq)]
pub enum Event {
    /// Something changed in directory `1` of worktree `0`.
    Changed(u64, PathBuf),
    /// A directory of worktree `0` moved: the paths of its watches are no
    /// longer known.
    Moved(u64),
    /// Events were lost: nothing is known about any worktree.
    Overflow,
}

pub struct Watcher {
    fd: tokio::io::unix::AsyncFd<OwnedFd>,
    /// Watch descriptor to the worktree it belongs to, and its directory.
    owners: Mutex<HashMap<i32, (u64, PathBuf)>>,
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

    /// Watches directory `dir` on behalf of worktree `owner`. A directory
    /// watched already (under this path or another) keeps its descriptor.
    /// `ENOSPC` is the per-user limit on watches.
    pub fn add(&self, dir: &Path, owner: u64) -> std::io::Result<i32> {
        let c = CString::new(dir.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
        // SAFETY: valid descriptor and NUL-terminated path.
        let wd = unsafe { libc::inotify_add_watch(self.fd.get_ref().as_raw_fd(), c.as_ptr(), MASK) };
        if wd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        self.owners.lock().unwrap().insert(wd, (owner, dir.to_path_buf()));
        Ok(wd)
    }

    /// Whether `wd` still watches `dir` (the kernel drops the watch of a
    /// directory that is removed).
    pub fn watches(&self, wd: i32, dir: &Path) -> bool {
        self.owners.lock().unwrap().get(&wd).is_some_and(|(_, d)| d == dir)
    }

    /// Stops watching with `wd`, unless it now watches another directory.
    pub fn remove(&self, wd: i32, dir: &Path) {
        let mut owners = self.owners.lock().unwrap();
        if owners.get(&wd).is_some_and(|(_, d)| d == dir) {
            owners.remove(&wd);
            // SAFETY: valid descriptor; a stale watch descriptor is an error we ignore.
            unsafe { libc::inotify_rm_watch(self.fd.get_ref().as_raw_fd(), wd) };
        }
    }

    /// The kernel's limit on watches per user, for the message that it is
    /// reached: the one of this user namespace (`fs.inotify.max_user_watches`
    /// is the first namespace's).
    pub fn limit() -> Option<u64> {
        ["/proc/sys/user/max_inotify_watches", "/proc/sys/fs/inotify/max_user_watches"]
            .iter()
            .find_map(|p| std::fs::read_to_string(p).ok()?.trim().parse().ok())
    }

    /// Stops watching everything of worktree `owner`.
    pub fn forget(&self, owner: u64) {
        let mut owners = self.owners.lock().unwrap();
        let wds: Vec<i32> = owners.iter().filter(|(_, (o, _))| *o == owner).map(|(wd, _)| *wd).collect();
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
                let owner = owners.get(&ev.wd).cloned();
                if ev.mask & libc::IN_IGNORED != 0 {
                    // The directory is gone and its watch with it.
                    owners.remove(&ev.wd);
                }
                if let Some((o, dir)) = owner {
                    let moved = ev.mask & libc::IN_MOVE_SELF != 0
                        || (ev.mask & libc::IN_ISDIR != 0 && ev.mask & (libc::IN_MOVED_FROM | libc::IN_MOVED_TO) != 0);
                    let e = if moved { Event::Moved(o) } else { Event::Changed(o, dir) };
                    if !out.contains(&e) {
                        out.push(e);
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

    async fn next(w: &Watcher) -> Vec<Event> {
        tokio::time::timeout(std::time::Duration::from_secs(5), w.next()).await.unwrap().unwrap()
    }

    #[test]
    fn reports_changes_per_worktree_and_directory() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let a = tempfile::tempdir().unwrap();
            let b = tempfile::tempdir().unwrap();
            let src = a.path().join("src");
            std::fs::create_dir(&src).unwrap();
            let w = Watcher::new().unwrap();
            w.add(a.path(), 1).unwrap();
            let wd = w.add(&src, 1).unwrap();
            w.add(b.path(), 2).unwrap();
            std::fs::write(src.join("lib.rs"), "x").unwrap();
            assert_eq!(next(&w).await, [Event::Changed(1, src.clone())]);
            std::fs::write(b.path().join("new"), "y").unwrap();
            assert_eq!(next(&w).await, [Event::Changed(2, b.path().to_path_buf())]);
            // Nothing more about a worktree that is no longer watched.
            w.forget(2);
            std::fs::write(b.path().join("newer"), "z").unwrap();
            std::fs::write(a.path().join("top"), "x").unwrap();
            assert_eq!(next(&w).await, [Event::Changed(1, a.path().to_path_buf())]);
            // A directory removed loses its watch.
            assert!(w.watches(wd, &src));
            std::fs::remove_dir_all(&src).unwrap();
            let mut seen = Vec::new();
            while !seen.contains(&Event::Changed(1, a.path().to_path_buf())) || w.watches(wd, &src) {
                seen.extend(next(&w).await);
            }
            assert!(seen.contains(&Event::Changed(1, src.clone())), "{seen:?}");
            // A directory moved: its watches stand for other paths now.
            std::fs::create_dir(&src).unwrap();
            let _ = next(&w).await;
            w.add(&src, 1).unwrap();
            std::fs::rename(&src, a.path().join("lib")).unwrap();
            let mut seen = Vec::new();
            while !seen.contains(&Event::Moved(1)) {
                seen.extend(next(&w).await);
            }
        });
    }
}
