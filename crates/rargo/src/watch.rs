#[cfg(test)]
use rargo_machine::StateMachine;
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

}

#[cfg(test)]
impl StateMachine for WatchState {
    fn invariants(&self) {
        for (descriptor, (owner, dir)) in &self.owners {
            assert!(*descriptor >= 0, "watch descriptor {descriptor} is not negative");
            assert!(!dir.as_os_str().is_empty(), "watch descriptor {descriptor} watches a named directory");
            assert!(self.registered.contains(owner), "watch descriptor {descriptor} belongs to registered worktree {owner}");
        }
        for descriptor in &self.moved {
            assert!(self.owners.contains_key(descriptor), "moved descriptor {descriptor} is watched");
        }
    }
}

pub enum Registration {
    Watched(i32),
    Poll(PathBuf),
}

pub struct Watcher {
    fd: OwnedFd,
    state: Mutex<WatchState>,
}

impl Watcher {
    pub fn new() -> std::io::Result<Watcher> {
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(Watcher { fd, state: Mutex::new(WatchState::default()) })
    }

    pub fn add(&self, dir: &Path, owner: u64) -> std::io::Result<Registration> {
        let c = CString::new(dir.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
        let mut state = self.state.lock().unwrap();
        let wd = unsafe { libc::inotify_add_watch(self.fd.as_raw_fd(), c.as_ptr(), MASK) };
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
            unsafe { libc::inotify_rm_watch(self.fd.as_raw_fd(), wd) };
        }
    }

    pub fn limit() -> Option<u64> {
        ["/proc/sys/user/max_inotify_watches", "/proc/sys/fs/inotify/max_user_watches"]
            .iter()
            .find_map(|p| rargo_net::io::file::read_to_string(p).ok()?.trim().parse().ok())
    }

    pub fn forget(&self, owner: u64) {
        let mut state = self.state.lock().unwrap();
        for wd in state.forget(owner) {
            unsafe { libc::inotify_rm_watch(self.fd.as_raw_fd(), wd) };
        }
    }

    pub fn next(&self) -> std::io::Result<Changes> {
        loop {
            let buf = rargo_net::io::read_ready(self.fd.as_raw_fd(), 64 << 10)?;
            let out = self.parse(&buf);
            if !out.is_empty() {
                return Ok(out);
            }
        }
    }

    pub fn queued(&self) -> std::io::Result<Option<Changes>> {
        Ok(rargo_net::io::try_read(self.fd.as_raw_fd(), 64 << 10)?.map(|buf| self.parse(&buf)))
    }

    fn parse(&self, buf: &[u8]) -> Changes {
        let mut state = self.state.lock().unwrap();
        let mut out = Changes::default();
        let mut at = 0usize;
        const HEAD: usize = std::mem::size_of::<libc::inotify_event>();
        while at + HEAD <= buf.len() {
            let ev: libc::inotify_event = unsafe { std::ptr::read_unaligned(buf.as_ptr().add(at) as *const _) };
            at += HEAD + ev.len as usize;
            state.apply(Event { descriptor: ev.wd, mask: ev.mask }, &mut out);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use expect_test::expect;

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

    fn checked(state: &WatchState, changes: &Changes) {
        state.invariants();
        for (owner, paths) in &changes.dirty {
            assert!(state.registered.contains(owner), "dirty worktree {owner} is registered");
            assert!(!paths.is_empty(), "dirty worktree {owner} names a directory");
            assert!(paths.iter().all(|path| !path.as_os_str().is_empty()), "dirty worktree {owner} names directories by path");
        }
        for owner in &changes.rescan {
            assert!(state.registered.contains(owner), "rescanned worktree {owner} is registered");
        }
    }

    fn apply(state: &mut WatchState, changes: &mut Changes, descriptor: Option<i32>, mask: u32) {
        if let Some(descriptor) = descriptor {
            state.apply(Event { descriptor, mask }, changes);
            checked(state, changes);
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
                checked(&state, &changes);
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
                            checked(&state, &changes);
                            apply(&mut state, &mut changes, Some(wd), libc::IN_IGNORED);
                            assert!(!state.owners.contains_key(&wd), "seed {seed}: an ignored watch is gone");
                            let fresh = m.next_wd;
                            m.next_wd += 1;
                            state.register(s.owner, s.path.clone(), WatchResult::Added(fresh));
                            s.wd = Some(fresh);
                        }
                        s.moved = false;
                        checked(&state, &changes);
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
                        checked(&state, &changes);
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
                        checked(&state, &changes);
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
                checked(&state, &changes);
                m.check(&changes, seed, step);
            }
        }
    }

    enum Ev {
        Added(i32, u64, &'static str),
        TooMany(u64, &'static str),
        Remove(i32, &'static str),
        Forget(u64),
        Apply(i32, u32),
        /// The shell collects the changes gathered since the last collection.
        Collect,
    }

    fn mask_names(mask: u32) -> String {
        let names = [
            (libc::IN_CREATE, "CREATE"),
            (libc::IN_MODIFY, "MODIFY"),
            (libc::IN_MOVED_FROM, "MOVED_FROM"),
            (libc::IN_MOVED_TO, "MOVED_TO"),
            (libc::IN_MOVE_SELF, "MOVE_SELF"),
            (libc::IN_IGNORED, "IGNORED"),
            (libc::IN_ISDIR, "ISDIR"),
            (libc::IN_Q_OVERFLOW, "Q_OVERFLOW"),
        ];
        assert_eq!(names.iter().fold(mask, |m, (bit, _)| m & !bit), 0, "every bit of {mask:#x} is named");
        names.iter().filter(|(bit, _)| mask & bit != 0).map(|(_, name)| *name).collect::<Vec<_>>().join("|")
    }

    /// Steps a watch state through the events, checking it after each, and lists what each step decided.
    fn run(events: impl IntoIterator<Item = Ev>) -> String {
        let mut state = WatchState::default();
        let mut changes = Changes::default();
        let mut out = String::new();
        for ev in events {
            let line = match ev {
                Ev::Added(descriptor, owner, dir) => match state.register(owner, dir.into(), WatchResult::Added(descriptor)) {
                    Registration::Watched(d) => format!("register {owner} {dir} => watched {d}"),
                    Registration::Poll(path) => format!("register {owner} {dir} => poll {}", path.display()),
                },
                Ev::TooMany(owner, dir) => match state.register(owner, dir.into(), WatchResult::TooManyDirectories) {
                    Registration::Watched(d) => format!("register {owner} {dir} (too many) => watched {d}"),
                    Registration::Poll(path) => format!("register {owner} {dir} (too many) => poll {}", path.display()),
                },
                Ev::Remove(descriptor, dir) => format!("remove {descriptor} {dir} => {}", state.remove(descriptor, Path::new(dir))),
                Ev::Forget(owner) => format!("forget {owner} => {:?}", state.forget(owner)),
                Ev::Apply(descriptor, mask) => {
                    state.apply(Event { descriptor, mask }, &mut changes);
                    format!("event {descriptor} {}", mask_names(mask))
                }
                Ev::Collect => format!("collect => {:?}", std::mem::take(&mut changes)),
            };
            checked(&state, &changes);
            out += &line;
            out.push('\n');
        }
        out
    }

    #[test]
    fn unregistering_a_directory_does_not_remove_another_worktree() {
        let events = [
            Ev::Added(1, 10, "first"),
            Ev::Added(2, 10, "first/src"),
            Ev::Added(3, 20, "second"),
            Ev::Remove(2, "different"),
            Ev::Forget(10),
            Ev::Apply(1, libc::IN_CREATE),
            Ev::Apply(2, libc::IN_CREATE),
            Ev::Apply(3, libc::IN_CREATE),
            Ev::Collect,
            Ev::Remove(3, "second"),
            Ev::Apply(3, libc::IN_CREATE),
            Ev::Collect,
        ];
        expect![[r#""#]].assert_eq(&run(events));
    }

    #[test]
    fn renames_cover_both_directories_and_invalidate_moved_directory_watches() {
        let events = [
            Ev::Added(1, 10, "from"),
            Ev::Added(2, 20, "to"),
            Ev::Apply(1, libc::IN_MOVED_FROM),
            Ev::Apply(2, libc::IN_MOVED_TO),
            Ev::Collect,
            Ev::Added(3, 10, "from/sub"),
            Ev::Apply(3, libc::IN_MOVE_SELF),
            Ev::Collect,
            Ev::Apply(3, libc::IN_MODIFY),
            Ev::Collect,
            Ev::Added(3, 10, "to/sub"),
            Ev::Apply(3, libc::IN_MODIFY),
            Ev::Collect,
            Ev::Apply(1, libc::IN_ISDIR | libc::IN_MOVED_FROM),
            Ev::Apply(2, libc::IN_ISDIR | libc::IN_MOVED_TO),
            Ev::Apply(1, libc::IN_MOVE_SELF),
            Ev::Apply(1, libc::IN_IGNORED),
            Ev::Apply(1, libc::IN_MODIFY),
            Ev::Collect,
            Ev::TooMany(20, "to/new"),
            Ev::Apply(-1, libc::IN_Q_OVERFLOW),
            Ev::Collect,
        ];
        expect![[r#""#]].assert_eq(&run(events));
    }
}
