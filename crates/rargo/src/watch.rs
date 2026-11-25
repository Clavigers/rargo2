use std::collections::{BTreeMap, BTreeSet};
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

struct Event {
    descriptor: i32,
    mask: u32,
}

enum WatchResult {
    Added(i32),
    TooManyDirectories,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Changes {
    pub dirty: BTreeMap<u64, BTreeSet<PathBuf>>,
    pub rescan: BTreeSet<u64>,
    pub rescan_all: bool,
}

impl Changes {
    fn is_empty(&self) -> bool {
        self.dirty.is_empty() && self.rescan.is_empty() && !self.rescan_all
    }
}

#[derive(Default)]
struct WatchState {
    owners: BTreeMap<i32, (u64, PathBuf)>,
}

impl WatchState {
    fn apply(&mut self, event: Event, changes: &mut Changes) {
        let Event { descriptor, mask } = event;
        if mask & libc::IN_Q_OVERFLOW != 0 {
            changes.rescan_all = true;
            return;
        }
        if let Some((owner, dir)) = self.owners.get(&descriptor) {
            if mask & libc::IN_MOVE_SELF != 0
                || (mask & libc::IN_ISDIR != 0 && mask & (libc::IN_MOVED_FROM | libc::IN_MOVED_TO) != 0)
            {
                changes.rescan.insert(*owner);
            } else {
                changes.dirty.entry(*owner).or_default().insert(dir.clone());
            }
        }
        if mask & libc::IN_IGNORED != 0 {
            self.owners.remove(&descriptor);
        }
    }

    fn register(&mut self, owner: u64, dir: PathBuf, result: WatchResult) -> Registration {
        match result {
            WatchResult::Added(descriptor) => {
                self.owners.insert(descriptor, (owner, dir));
                Registration::Watched(descriptor)
            }
            WatchResult::TooManyDirectories => Registration::Poll(dir),
        }
    }

    fn watches(&self, descriptor: i32, dir: &Path) -> bool {
        self.owners.get(&descriptor).is_some_and(|(_, watched)| watched == dir)
    }

    fn remove(&mut self, descriptor: i32, dir: &Path) -> bool {
        self.watches(descriptor, dir) && self.owners.remove(&descriptor).is_some()
    }

    fn forget(&mut self, owner: u64) -> Vec<i32> {
        let descriptors: Vec<_> = self.owners.iter().filter(|(_, (id, _))| *id == owner).map(|(&descriptor, _)| descriptor).collect();
        for descriptor in &descriptors {
            self.owners.remove(descriptor);
        }
        descriptors
    }

    #[cfg(test)]
    fn invariants(&self, changes: &Changes) -> bool {
        self.owners.iter().all(|(descriptor, (_, dir))| *descriptor >= 0 && !dir.as_os_str().is_empty())
            && changes.dirty.values().all(|paths| !paths.is_empty() && paths.iter().all(|path| !path.as_os_str().is_empty()))
    }
}

pub enum Registration {
    Watched(i32),
    Poll(PathBuf),
}

pub struct Watcher {
    fd: tokio::io::unix::AsyncFd<OwnedFd>,
    state: Mutex<WatchState>,
}

impl Watcher {
    pub fn new() -> std::io::Result<Watcher> {
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let fd = unsafe { tokio::io::unix::AsyncFd::register(fd) }.map_err(std::io::Error::from)?;
        Ok(Watcher { fd, state: Mutex::new(WatchState::default()) })
    }

    pub fn add(&self, dir: &Path, owner: u64) -> std::io::Result<Registration> {
        let c = CString::new(dir.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
        let mut state = self.state.lock().unwrap();
        let wd = unsafe { libc::inotify_add_watch(self.fd.get_ref().as_raw_fd(), c.as_ptr(), MASK) };
        let result = if wd >= 0 {
            WatchResult::Added(wd)
        } else {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ENOSPC) {
                return Err(error);
            }
            WatchResult::TooManyDirectories
        };
        Ok(state.register(owner, dir.to_path_buf(), result))
    }

    pub fn watches(&self, wd: i32, dir: &Path) -> bool {
        self.state.lock().unwrap().watches(wd, dir)
    }

    pub fn remove(&self, wd: i32, dir: &Path) {
        let mut state = self.state.lock().unwrap();
        if state.remove(wd, dir) {
            unsafe { libc::inotify_rm_watch(self.fd.get_ref().as_raw_fd(), wd) };
        }
    }

    pub fn limit() -> Option<u64> {
        ["/proc/sys/user/max_inotify_watches", "/proc/sys/fs/inotify/max_user_watches"]
            .iter()
            .find_map(|p| std::fs::read_to_string(p).ok()?.trim().parse().ok())
    }

    pub fn forget(&self, owner: u64) {
        let mut state = self.state.lock().unwrap();
        for wd in state.forget(owner) {
            unsafe { libc::inotify_rm_watch(self.fd.get_ref().as_raw_fd(), wd) };
        }
    }

    pub async fn next(&self) -> std::io::Result<Changes> {
        let mut buf = vec![0u8; 64 << 10];
        loop {
            let mut guard = self.fd.readable().await?;
            let mut state = self.state.lock().unwrap();
            let n = unsafe { libc::read(self.fd.get_ref().as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    guard.clear_ready();
                    continue;
                }
                return Err(e);
            }
            let mut out = Changes::default();
            let mut at = 0usize;
            let n = n as usize;
            const HEAD: usize = std::mem::size_of::<libc::inotify_event>();
            while at + HEAD <= n {
                let ev: libc::inotify_event = unsafe { std::ptr::read_unaligned(buf.as_ptr().add(at) as *const _) };
                at += HEAD + ev.len as usize;
                state.apply(Event { descriptor: ev.wd, mask: ev.mask }, &mut out);
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
    fn generated_changes_remain_visible_until_drained() {
        let masks = [libc::IN_CREATE, libc::IN_MODIFY, libc::IN_DELETE, libc::IN_CLOSE_WRITE, libc::IN_ATTRIB];
        for seed in 0..32 {
            let mut state = WatchState::default();
            let paths: Vec<_> = (0..8).map(|i| PathBuf::from(format!("worktree/dir{i}"))).collect();
            for (i, path) in paths.iter().enumerate() {
                state.register((i % 2) as u64, path.clone(), WatchResult::Added(i as i32));
                assert!(state.invariants(&Changes::default()));
            }
            let mut changes = Changes::default();
            let mut seen = BTreeMap::<_, BTreeSet<_>>::new();
            let mut random = seed;
            for i in 0..128 {
                random = (random * 109 + 89) % 4093;
                let descriptor = random % paths.len();
                state.apply(Event { descriptor: descriptor as i32, mask: masks[(random / 8) % masks.len()] }, &mut changes);
                seen.entry((descriptor % 2) as u64).or_default().insert(paths[descriptor].clone());
                if i % 13 == 0 {
                    state.apply(Event { descriptor: -1, mask: libc::IN_Q_OVERFLOW }, &mut changes);
                    assert!(changes.rescan_all);
                    assert!(state.invariants(&changes));
                }
                assert!(state.invariants(&changes));
                assert_eq!(changes.dirty, seen);
            }
        }
    }

    #[test]
    fn unregistering_a_directory_does_not_remove_another_worktree() {
        let mut state = WatchState::default();
        let mut changes = Changes::default();
        for (descriptor, owner, dir) in [(1, 10, "first"), (2, 10, "first/src"), (3, 20, "second")] {
            state.register(owner, dir.into(), WatchResult::Added(descriptor));
            assert!(state.invariants(&changes));
        }
        assert!(!state.remove(2, Path::new("different")));
        assert!(state.invariants(&changes));
        assert_eq!(state.forget(10), [1, 2]);
        assert!(state.invariants(&changes));
        for descriptor in [1, 2, 3] {
            state.apply(Event { descriptor, mask: libc::IN_CREATE }, &mut changes);
            assert!(state.invariants(&changes));
        }
        assert_eq!(changes.dirty, BTreeMap::from([(20, BTreeSet::from(["second".into()]))]));
    }

    #[test]
    fn renames_cover_both_directories_and_invalidate_moved_directory_watches() {
        let mut state = WatchState::default();
        let mut changes = Changes::default();
        state.register(10, "from".into(), WatchResult::Added(1));
        assert!(state.invariants(&changes));
        state.register(20, "to".into(), WatchResult::Added(2));
        assert!(state.invariants(&changes));
        for (descriptor, mask) in [(1, libc::IN_MOVED_FROM), (2, libc::IN_MOVED_TO)] {
            state.apply(Event { descriptor, mask }, &mut changes);
            assert!(state.invariants(&changes));
        }
        assert_eq!(changes.dirty, BTreeMap::from([(10, BTreeSet::from(["from".into()])), (20, BTreeSet::from(["to".into()]))]));
        assert!(changes.rescan.is_empty());

        for (descriptor, mask) in [(1, libc::IN_ISDIR | libc::IN_MOVED_FROM), (2, libc::IN_ISDIR | libc::IN_MOVED_TO),
            (1, libc::IN_MOVE_SELF), (1, libc::IN_IGNORED)] {
            state.apply(Event { descriptor, mask }, &mut changes);
            assert!(state.invariants(&changes));
        }
        assert_eq!(changes.rescan, BTreeSet::from([10, 20]));
        assert!(!state.owners.contains_key(&1));
        let decision = state.register(20, "to/new".into(), WatchResult::TooManyDirectories);
        assert!(state.invariants(&changes));
        assert!(matches!(decision, Registration::Poll(path) if path == Path::new("to/new")));
    }
}
