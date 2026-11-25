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
    moved: BTreeSet<i32>,
    #[cfg(test)]
    registered: BTreeSet<u64>,
}

impl WatchState {
    fn apply(&mut self, event: Event, changes: &mut Changes) {
        let Event { descriptor, mask } = event;
        if mask & libc::IN_Q_OVERFLOW != 0 {
            changes.rescan_all = true;
            return;
        }
        if let Some((owner, dir)) = self.owners.get(&descriptor) {
            if mask & libc::IN_MOVE_SELF != 0 {
                self.moved.insert(descriptor);
            }
            if self.moved.contains(&descriptor)
                || (mask & libc::IN_ISDIR != 0 && mask & (libc::IN_MOVED_FROM | libc::IN_MOVED_TO) != 0)
            {
                changes.rescan.insert(*owner);
            } else {
                changes.dirty.entry(*owner).or_default().insert(dir.clone());
            }
        }
        if mask & libc::IN_IGNORED != 0 {
            self.owners.remove(&descriptor);
            self.moved.remove(&descriptor);
        }
    }

    fn register(&mut self, owner: u64, dir: PathBuf, result: WatchResult) -> Registration {
        match result {
            WatchResult::Added(descriptor) => {
                #[cfg(test)]
                self.registered.insert(owner);
                self.moved.remove(&descriptor);
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
        if !self.watches(descriptor, dir) {
            return false;
        }
        self.moved.remove(&descriptor);
        self.owners.remove(&descriptor).is_some()
    }

    fn forget(&mut self, owner: u64) -> Vec<i32> {
        let descriptors: Vec<_> = self.owners.iter().filter(|(_, (id, _))| *id == owner).map(|(&descriptor, _)| descriptor).collect();
        for descriptor in &descriptors {
            self.owners.remove(descriptor);
            self.moved.remove(descriptor);
        }
        descriptors
    }

    #[cfg(test)]
    fn invariants(&self, changes: &Changes) -> bool {
        self.owners.iter().all(|(descriptor, (owner, dir))| {
            *descriptor >= 0 && !dir.as_os_str().is_empty() && self.registered.contains(owner)
        }) && self.moved.iter().all(|descriptor| self.owners.contains_key(descriptor))
            && changes.dirty.iter().all(|(owner, paths)| {
                self.registered.contains(owner) && !paths.is_empty() && paths.iter().all(|path| !path.as_os_str().is_empty())
            })
            && changes.rescan.iter().all(|owner| self.registered.contains(owner))
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

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % n as u64) as usize
        }
    }

    struct Slot {
        owner: u64,
        path: PathBuf,
        wd: Option<i32>,
        moved: bool,
        alive: bool,
    }

    struct Model {
        slots: Vec<Slot>,
        next_wd: i32,
        touched: BTreeSet<(u64, PathBuf)>,
        must_rescan: BTreeSet<u64>,
    }

    impl Model {
        fn touch(&mut self, slot: usize) -> Option<i32> {
            let s = &self.slots[slot];
            let wd = s.wd?;
            if s.moved {
                self.must_rescan.insert(s.owner);
            } else {
                self.touched.insert((s.owner, s.path.clone()));
            }
            Some(wd)
        }

        fn rescans(&mut self, slot: usize) -> Option<i32> {
            let s = &self.slots[slot];
            let wd = s.wd?;
            self.must_rescan.insert(s.owner);
            Some(wd)
        }

        fn check(&self, changes: &Changes, seed: u64, step: usize) {
            for (owner, path) in &self.touched {
                assert!(
                    changes.rescan_all
                        || changes.rescan.contains(owner)
                        || changes.dirty.get(owner).is_some_and(|paths| paths.contains(path)),
                    "seed {seed} step {step}: a change in {} of worktree {owner} was lost",
                    path.display()
                );
            }
            for owner in &self.must_rescan {
                assert!(
                    changes.rescan_all || changes.rescan.contains(owner),
                    "seed {seed} step {step}: worktree {owner} must be rescanned"
                );
            }
        }
    }

    fn apply(state: &mut WatchState, changes: &mut Changes, descriptor: Option<i32>, mask: u32) {
        if let Some(descriptor) = descriptor {
            state.apply(Event { descriptor, mask }, changes);
            assert!(state.invariants(changes));
        }
    }

    #[test]
    fn generated_streams_with_renames_across_directories_and_overflow_never_lose_a_change() {
        let file_ops = [libc::IN_CREATE, libc::IN_MODIFY, libc::IN_DELETE, libc::IN_CLOSE_WRITE, libc::IN_ATTRIB];
        for seed in 1..=300u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let mut state = WatchState::default();
            let mut changes = Changes::default();
            let mut m = Model { slots: Vec::new(), next_wd: 1, touched: BTreeSet::new(), must_rescan: BTreeSet::new() };
            for i in 0..10 {
                let owner = (i % 3 + 1) as u64;
                let path = PathBuf::from(format!("w{owner}/d{i}"));
                let wd = m.next_wd;
                m.next_wd += 1;
                assert!(matches!(state.register(owner, path.clone(), WatchResult::Added(wd)), Registration::Watched(w) if w == wd));
                assert!(state.invariants(&changes));
                m.slots.push(Slot { owner, path, wd: Some(wd), moved: false, alive: true });
            }
            for step in 0..300 {
                let n = m.slots.len();
                let (a, b, x) = (rng.below(n), rng.below(n), rng.below(n));
                match rng.below(12) {
                    0..=2 if m.slots[a].alive => {
                        let wd = m.touch(a);
                        apply(&mut state, &mut changes, wd, file_ops[rng.below(file_ops.len())]);
                    }
                    3 | 4 if m.slots[a].alive && m.slots[b].alive => {
                        let from = m.touch(a);
                        apply(&mut state, &mut changes, from, libc::IN_MOVED_FROM);
                        let to = m.touch(b);
                        apply(&mut state, &mut changes, to, libc::IN_MOVED_TO);
                    }
                    5 if x != a && x != b && m.slots[a].alive && m.slots[b].alive && m.slots[x].alive => {
                        let from = m.rescans(a);
                        apply(&mut state, &mut changes, from, libc::IN_ISDIR | libc::IN_MOVED_FROM);
                        let to = m.rescans(b);
                        apply(&mut state, &mut changes, to, libc::IN_ISDIR | libc::IN_MOVED_TO);
                        let me = m.rescans(x);
                        apply(&mut state, &mut changes, me, libc::IN_MOVE_SELF);
                        let s = &mut m.slots[x];
                        s.moved = s.wd.is_some();
                        s.path = PathBuf::from(format!("{}/moved{step}", s.path.display()));
                    }
                    6 if m.slots[x].moved => {
                        let s = &mut m.slots[x];
                        let wd = s.wd.expect("a moved slot is watched");
                        if rng.below(2) == 0 {
                            assert!(matches!(state.register(s.owner, s.path.clone(), WatchResult::Added(wd)), Registration::Watched(_)));
                        } else {
                            assert!(state.remove(wd, &state.owners[&wd].1.clone()));
                            assert!(state.invariants(&changes));
                            apply(&mut state, &mut changes, Some(wd), libc::IN_IGNORED);
                            assert!(!state.owners.contains_key(&wd), "seed {seed}: an ignored watch is gone");
                            let fresh = m.next_wd;
                            m.next_wd += 1;
                            state.register(s.owner, s.path.clone(), WatchResult::Added(fresh));
                            s.wd = Some(fresh);
                        }
                        s.moved = false;
                        assert!(state.invariants(&changes));
                    }
                    7 if x != a && m.slots[x].alive && m.slots[a].alive => {
                        let parent = m.touch(a);
                        apply(&mut state, &mut changes, parent, libc::IN_ISDIR | libc::IN_DELETE);
                        let me = m.slots[x].wd;
                        apply(&mut state, &mut changes, me, libc::IN_DELETE_SELF);
                        apply(&mut state, &mut changes, me, libc::IN_IGNORED);
                        if let Some(wd) = me {
                            assert!(!state.owners.contains_key(&wd), "seed {seed}: a deleted directory's watch is gone");
                            assert!(!state.remove(wd, &m.slots[x].path), "seed {seed}: a gone watch is not removed twice");
                        }
                        let s = &mut m.slots[x];
                        s.alive = false;
                        s.wd = None;
                        s.moved = false;
                    }
                    8 if !m.slots[x].alive && m.slots[a].alive && a != x => {
                        let parent = m.touch(a);
                        apply(&mut state, &mut changes, parent, libc::IN_ISDIR | libc::IN_CREATE);
                        let s = &mut m.slots[x];
                        s.alive = true;
                        if rng.below(4) == 0 {
                            let decision = state.register(s.owner, s.path.clone(), WatchResult::TooManyDirectories);
                            assert!(matches!(decision, Registration::Poll(ref p) if *p == s.path));
                        } else {
                            let wd = m.next_wd;
                            m.next_wd += 1;
                            state.register(s.owner, s.path.clone(), WatchResult::Added(wd));
                            s.wd = Some(wd);
                        }
                        assert!(state.invariants(&changes));
                    }
                    9 => {
                        apply(&mut state, &mut changes, Some(-1), libc::IN_Q_OVERFLOW);
                        assert!(changes.rescan_all, "seed {seed}: overflow forces a rescan (#10)");
                        apply(&mut state, &mut changes, Some(m.next_wd + 100), libc::IN_MODIFY);
                    }
                    10 => {
                        m.check(&changes, seed, step);
                        changes = Changes::default();
                        m.touched.clear();
                        m.must_rescan.clear();
                    }
                    11 if rng.below(8) == 0 => {
                        let owner = m.slots[x].owner;
                        let mut gone = state.forget(owner);
                        assert!(state.invariants(&changes));
                        let mut want: Vec<i32> = m.slots.iter().filter(|s| s.owner == owner).filter_map(|s| s.wd).collect();
                        gone.sort();
                        want.sort();
                        assert_eq!(gone, want, "seed {seed}: forgetting a worktree removes exactly its watches");
                        for s in m.slots.iter_mut().filter(|s| s.owner == owner) {
                            s.wd = None;
                            s.moved = false;
                        }
                    }
                    _ => {}
                }
                assert!(state.invariants(&changes));
                m.check(&changes, seed, step);
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

        state.register(10, "from/sub".into(), WatchResult::Added(3));
        state.apply(Event { descriptor: 3, mask: libc::IN_MOVE_SELF }, &mut changes);
        assert!(state.invariants(&changes));
        let mut after = Changes::default();
        state.apply(Event { descriptor: 3, mask: libc::IN_MODIFY }, &mut after);
        assert!(state.invariants(&after));
        assert_eq!(after.rescan, BTreeSet::from([10]), "a moved directory's old path is not trusted until it is watched again");
        state.register(10, "to/sub".into(), WatchResult::Added(3));
        let mut rewatched = Changes::default();
        state.apply(Event { descriptor: 3, mask: libc::IN_MODIFY }, &mut rewatched);
        assert_eq!(rewatched.dirty, BTreeMap::from([(10, BTreeSet::from(["to/sub".into()]))]));
        assert!(rewatched.rescan.is_empty());

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
