#[cfg(test)]
use rargo_machine::StateMachine;
use crate::view::*;
use dashmap::DashMap;
use fusewire::abi::*;
use fusewire::{Ctx, DirSink, Entry, Errno, Filesystem, Opened, Res, Session, SetAttr, TimeSet};
use parking_lot::Mutex;
use std::collections::HashMap;
use crate::io::file::File;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

pub const LOWER_MTIME: u64 = 946_684_800;

const TTL: Duration = Duration::from_secs(365 * 86400);
const LOCAL_BITS: u32 = 24;
const LOCAL_MASK: u64 = (1 << LOCAL_BITS) - 1;
const _: () = assert!(LOCAL_BITS < 32, "a local id fits in u32");
const _: () = assert!(ROOT_LOCAL as u64 <= LOCAL_MASK);
const ROOT_LOCAL: u32 = 1;
const BACKING_CACHE_MAX: usize = 200_000;
const RETIRE_GRACE: Duration = Duration::from_secs(60);

fn nodeid(seq: u64, local: u32) -> u64 {
    assert!(local as u64 <= LOCAL_MASK);
    assert!(seq < 1 << (64 - LOCAL_BITS), "action sequence overflows a node id");
    let id = (seq << LOCAL_BITS) | local as u64;
    assert_eq!(id >> LOCAL_BITS, seq);
    assert_ne!(id, FUSE_ROOT_ID, "an action node is never the mount root");
    id
}

enum Slot {
    Lower(u32),
    Upper(u32, bool),
    Negative,
    Whiteout,
}

struct Dir {
    parent: u32,
    lower: Option<VPath>,
    traced: bool,
    entries: HashMap<Box<[u8]>, Slot>,
    dirty: bool,
    owner: Owner,
    lower_listed: bool,
    listing: Option<Arc<Vec<(Box<[u8]>, DType)>>>,
    mtime: (i64, u32),
}

enum Src {
    Lower(FileInfo),
    Upper { path: PathBuf, file: Arc<File>, backing: Option<i32> },
}

struct FileN {
    parent: u32,
    src: Src,
    mode: u32,
    opens: u32,
    links: u32,
    lower_pt_opens: u32,
    read_src: Option<FileSrc>,
    file: Option<Arc<File>>,
    obs: Option<usize>,
}

enum Kind {
    Dir(Box<Dir>),
    File(Box<FileN>),
    Symlink { target: Vec<u8> },
    Mount,
    Special { mode: u32, rdev: u32 },
}

#[cfg(test)]
fn path_ok(path: &[u8]) -> bool {
    if path == b"/" {
        return true;
    }
    path.starts_with(b"/")
        && !path.ends_with(b"/")
        && path[1..].split(|&c| c == b'/').all(|c| !c.is_empty() && c != b"." && c != b"..")
}

struct FileRec {
    size: u64,
    exec: bool,
    sem: Option<crate::hash::Hash>,
    read: bool,
    hashed: Option<crate::hash::Hash>,
    failed: bool,
}

enum Rec {
    Settled(Obs),
    File(FileRec),
}

struct ObsCore {
    size_blind: bool,
    items: Vec<(VPath, Rec)>,
}

impl ObsCore {
    fn new(size_blind: bool) -> ObsCore {
        ObsCore { size_blind, items: Vec::new() }
    }

    fn observe_lookup(&mut self, path: VPath, traced: bool, looked: &Looked) -> Option<usize> {
        if !traced {
            return None;
        }
        let rec = match looked {
            Looked::Absent => Rec::Settled(Obs::Absent),
            Looked::Other => Rec::Settled(Obs::Other),
            Looked::Dir(_) => Rec::Settled(Obs::Dir),
            Looked::Symlink(target) => Rec::Settled(Obs::Symlink(target.clone())),
            Looked::File(info) => Rec::File(FileRec {
                size: info.size,
                exec: info.exec(),
                sem: info.sem,
                read: false,
                hashed: None,
                failed: false,
            }),
        };
        let file = matches!(rec, Rec::File(_));
        self.items.push((path, rec));
        file.then_some(self.items.len() - 1)
    }

    fn observe_list(&mut self, path: VPath, list: &Listing) {
        self.items.push((path, Rec::Settled(Obs::Listing(listing_hash(list)))));
    }

    fn mark_read(&mut self, index: usize) {
        if let Some((_, Rec::File(file))) = self.items.get_mut(index) {
            file.read = true;
        }
    }

    fn pending_hashes(&self) -> Vec<usize> {
        self.items
            .iter()
            .enumerate()
            .filter_map(|(i, (_, rec))| match rec {
                Rec::File(file) if file.read && file.sem.is_none() && file.hashed.is_none() && !file.failed => Some(i),
                _ => None,
            })
            .collect()
    }

    fn supply_hash(&mut self, index: usize, hash: crate::hash::Hash) {
        let Some((_, Rec::File(file))) = self.items.get_mut(index) else {
            panic!("a hash answers a file");
        };
        assert!(file.read && file.sem.is_none() && file.hashed.is_none() && !file.failed, "a hash answers one unread content");
        file.hashed = Some(hash);
    }

    fn fail_hash(&mut self, index: usize) {
        let Some((_, Rec::File(file))) = self.items.get_mut(index) else {
            panic!("a hash answers a file");
        };
        assert!(file.read && file.sem.is_none() && file.hashed.is_none() && !file.failed, "a failed hash answers one unread content");
        file.failed = true;
    }

    #[cfg(test)]
    fn path_of(&self, index: usize) -> &[u8] {
        &self.items[index].0
    }

    #[cfg(test)]
    fn file_is(&self, index: usize, was_read: bool) -> bool {
        match self.items.get(index) {
            Some((_, Rec::File(file))) => !was_read || file.read,
            _ => false,
        }
    }

    fn outcome(&self) -> (Vec<(VPath, Obs)>, bool) {
        assert!(self.pending_hashes().is_empty(), "file hashes are supplied before the trace is read");
        let mut tainted = false;
        let mut out = Vec::with_capacity(self.items.len());
        for (path, rec) in &self.items {
            let obs = match rec {
                Rec::Settled(obs) => obs.clone(),
                Rec::File(file) => {
                    if file.read {
                        if let Some(hash) = file.sem.or(file.hashed) {
                            Obs::File { hash, exec: file.exec }
                        } else {
                            tainted = true;
                            Obs::File { hash: crate::hash::Hash::ZERO, exec: file.exec }
                        }
                    } else if self.size_blind {
                        Obs::IsFile { exec: file.exec }
                    } else {
                        Obs::Meta { size: file.size, exec: file.exec }
                    }
                }
            };
            out.push((path.clone(), obs));
        }
        (out, tainted)
    }
}

#[cfg(test)]
impl StateMachine for ObsCore {
    fn invariants(&self) {
        for (path, rec) in &self.items {
            let at = String::from_utf8_lossy(path);
            assert!(path_ok(path), "{at:?} is a well-formed path");
            if let Rec::File(file) = rec {
                if file.hashed.is_some() {
                    assert!(!file.failed, "{at}: a hashed file has not failed");
                    assert!(file.sem.is_none(), "{at}: a hashed file holds no hashing slot");
                    assert!(file.read, "{at}: a hashed file was read");
                } else if file.failed {
                    assert!(file.sem.is_none(), "{at}: a failed file holds no hashing slot");
                    assert!(file.read, "{at}: a failed file was read");
                }
            }
        }
    }
}

struct State {
    nodes: Vec<Kind>,
    trace: ObsCore,
    next_upper: u64,
    backing_ids: Vec<i32>,
    finished: bool,
}

pub struct ActionFs {
    pub seq: u64,
    view: Arc<dyn View>,
    uid: u32,
    gid: u32,
    scratch: PathBuf,
    st: Mutex<State>,
}

fn initial_nodes(view: &dyn View, mountpoints: &[Vec<u8>]) -> Vec<Kind> {
    let mut nodes = Vec::with_capacity(256);
    nodes.push(Kind::Mount);
    let root_path: VPath = Arc::from(&b"/"[..]);
    let mut root = Dir {
        parent: 0,
        traced: view.traced(&root_path),
        lower: Some(root_path),
        entries: HashMap::new(),
        dirty: false,
        owner: Owner { uid: 0, gid: 0, mode: 0o755 },
        lower_listed: false,
        listing: None,
        mtime: (LOWER_MTIME as i64, 0),
    };
    nodes.push(Kind::Mount);
    for m in mountpoints {
        let id = nodes.len() as u32;
        nodes.push(Kind::Mount);
        assert!(!m.is_empty());
        assert!(!m.contains(&b'/'), "a mountpoint is one name in the action root");
        assert!(root.entries.insert(m.clone().into_boxed_slice(), Slot::Upper(id, true)).is_none(), "duplicate mountpoint");
    }
    nodes[ROOT_LOCAL as usize] = Kind::Dir(Box::new(root));
    nodes
}

impl ActionFs {
    fn assemble(seq: u64, view: Arc<dyn View>, uid: u32, gid: u32, scratch: PathBuf, mountpoints: &[Vec<u8>], size_blind: bool) -> ActionFs {
        let nodes = initial_nodes(&*view, mountpoints);
        ActionFs {
            seq,
            view,
            uid,
            gid,
            scratch,
            st: Mutex::new(State {
                nodes,
                trace: ObsCore::new(size_blind),
                next_upper: 0,
                backing_ids: Vec::new(),
                finished: false,
            }),
        }
    }

    #[cfg(test)]
    fn in_memory(view: Arc<dyn View>, size_blind: bool) -> ActionFs {
        Self::assemble(1, view, 0, 0, PathBuf::new(), &[], size_blind)
    }
}

pub struct ActionFsConfig {
    pub view: Arc<dyn View>,
    pub uid: u32,
    pub gid: u32,
    pub scratch: PathBuf,
    pub mountpoints: Vec<Vec<u8>>,
    pub size_blind: bool,
}

#[derive(Debug)]
pub enum UpperKind {
    Dir,
    File { path: PathBuf, exec: bool },
    LowerFile { info: FileInfo },
    Symlink(Vec<u8>),
    Whiteout,
}

#[derive(Debug)]
pub struct UpperEnt {
    pub path: Vec<u8>,
    pub kind: UpperKind,
}

pub struct Finished {
    pub obs: Vec<(VPath, Obs)>,
    pub upper: Vec<UpperEnt>,
    pub tainted: bool,
}

pub struct SandboxFs {
    actions: DashMap<u64, Arc<ActionFs>>,
    next_seq: AtomicU64,
    content: Arc<Content>,
    session: OnceLock<Arc<Session>>,
    backing: DashMap<BackingKey, (i32, AtomicU64)>,
    retired: Mutex<Vec<(std::time::Instant, i32)>>,
    tick: AtomicU64,
    stats: FsStats,
}

#[derive(Default)]
pub struct FsStats {
    pub lookups: AtomicU64,
    pub opens: AtomicU64,
    pub reads: AtomicU64,
    pub writes: AtomicU64,
}

impl SandboxFs {
    pub fn new(content: Arc<Content>) -> SandboxFs {
        SandboxFs {
            actions: DashMap::new(),
            next_seq: AtomicU64::new(1),
            content,
            session: OnceLock::new(),
            backing: DashMap::new(),
            retired: Mutex::new(Vec::new()),
            tick: AtomicU64::new(0),
            stats: FsStats::default(),
        }
    }

    pub fn attach(&self, session: Arc<Session>) {
        let _ = self.session.set(session);
    }
}

impl SandboxFs {

    pub fn stats(&self) -> &FsStats {
        &self.stats
    }

    fn passthrough(&self) -> bool {
        self.session.get().map(|s| s.caps().passthrough).unwrap_or(false)
    }

    pub fn dir_name(seq: u64) -> String {
        format!("a{seq}")
    }

    pub fn create_action(&self, cfg: ActionFsConfig) -> Arc<ActionFs> {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let a = Arc::new(ActionFs::assemble(seq, cfg.view, cfg.uid, cfg.gid, cfg.scratch, &cfg.mountpoints, cfg.size_blind));
        assert!(self.actions.insert(seq, a.clone()).is_none(), "action sequence numbers are unique");
        a
    }

    pub fn finish_action(&self, a: &Arc<ActionFs>) -> Finished {
        self.actions.remove(&a.seq);
        if let Some(s) = self.session.get() {
            let _ = s.notify_inval_entry(FUSE_ROOT_ID, Self::dir_name(a.seq).as_bytes());
        }
        let mut st = a.st.lock();
        assert!(!st.finished, "action finished twice");
        st.finished = true;
        if let Some(s) = self.session.get() {
            for id in st.backing_ids.drain(..) {
                let _ = s.backing_close(id);
            }
        }
        self.reap_retired();
        let mut upper = Vec::new();
        walk_upper(&st, &*a.view, ROOT_LOCAL, b"/", false, &mut upper);

        let jobs = {
            let mut jobs = Vec::new();
            for i in st.trace.pending_hashes() {
                match file_hash_src(&st, i) {
                    Some(src) => jobs.push((i, src)),
                    None => st.trace.fail_hash(i),
                }
            }
            jobs
        };
        drop(st);

        let hashed: Vec<(usize, std::io::Result<crate::hash::Hash>)> =
            crate::io::thread::par_map(jobs, |(i, src)| (i, self.content.hash(&src)));
        let mut st = a.st.lock();
        for (i, h) in hashed {
            match h {
                Ok(hash) => st.trace.supply_hash(i, hash),
                Err(_) => st.trace.fail_hash(i),
            }
        }
        let (obs, tainted) = st.trace.outcome();
        Finished { obs: compact_obs(obs), upper, tainted }
    }

    fn backing_id(&self, key: BackingKey, file: &File) -> Res<i32> {
        let tick = self.tick.fetch_add(1, Ordering::Relaxed);
        let id = match self.backing.entry(key) {
            dashmap::mapref::entry::Entry::Occupied(e) => {
                e.get().1.store(tick, Ordering::Relaxed);
                return Ok(e.get().0);
            }
            dashmap::mapref::entry::Entry::Vacant(v) => {
                let sess = self.session.get().ok_or(Errno::EIO)?;
                let id = sess.backing_open(file.as_raw_fd())?;
                v.insert((id, AtomicU64::new(tick)));
                id
            }
        };
        if self.backing.len() > BACKING_CACHE_MAX {
            self.trim_backing();
        }
        self.reap_retired();
        Ok(id)
    }

    fn retire(&self, id: i32) {
        self.retired.lock().push((crate::io::now(), id));
    }

    fn reap_retired(&self) {
        let Some(sess) = self.session.get() else { return };
        let mut r = self.retired.lock();
        if r.is_empty() {
            return;
        }
        let now = crate::io::now();
        r.retain(|(t, id)| {
            if now.duration_since(*t) > RETIRE_GRACE {
                let _ = sess.backing_close(*id);
                false
            } else {
                true
            }
        });
    }

    fn trim_backing(&self) {
        let mut all: Vec<(u64, BackingKey)> =
            self.backing.iter().map(|e| (e.value().1.load(Ordering::Relaxed), *e.key())).collect();
        all.sort_unstable_by_key(|e| e.0);
        for (_, k) in all.into_iter().take(BACKING_CACHE_MAX / 4) {
            if let Some((_, (id, _))) = self.backing.remove(&k) {
                self.retire(id);
            }
        }
    }

    pub fn forget_backing(&self, h: &crate::hash::Hash) {
        if let Some((_, (id, _))) = self.backing.remove(&BackingKey::Blob(*h)) {
            self.retire(id);
        }
    }

    fn action(&self, node: u64) -> Res<(Arc<ActionFs>, u32)> {
        let seq = node >> LOCAL_BITS;
        let local = (node & LOCAL_MASK) as u32;
        let a = self.actions.get(&seq).ok_or(Errno::ESTALE)?.clone();
        Ok((a, local))
    }
}

fn compact_obs(obs: Vec<(VPath, Obs)>) -> Vec<(VPath, Obs)> {
    let mut implied: std::collections::HashSet<&[u8]> = Default::default();
    for (p, o) in &obs {
        match o {
            Obs::Absent => {}
            Obs::Listing(_) => {
                implied.insert(&p[..]);
            }
            _ => {
                if let Some((parent, _)) = vsplit(p) {
                    implied.insert(parent);
                }
            }
        }
    }
    let keep: Vec<bool> =
        obs.iter().map(|(p, o)| !(matches!(o, Obs::Dir) && implied.contains(&p[..]))).collect();
    drop(implied);
    let mut out: Vec<(VPath, Obs)> =
        obs.into_iter().zip(keep).filter(|(_, k)| *k).map(|(e, _)| e).collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
    out
}

fn expand_lower(view: &dyn View, lower: &[u8], out_path: &[u8], out: &mut Vec<UpperEnt>) {
    let Ok(list) = view.list(lower) else { return };
    let mut list = list;
    list.sort();
    for (name, _) in list {
        emit_lower_entry(view, lower, out_path, &name, out);
    }
}

fn emit_lower_entry(view: &dyn View, lower: &[u8], out_path: &[u8], name: &[u8], out: &mut Vec<UpperEnt>) {
    let path = vjoin(out_path, name);
    match view.lookup(lower, name) {
        Looked::File(info) => out.push(UpperEnt { path, kind: UpperKind::LowerFile { info } }),
        Looked::Symlink(t) => out.push(UpperEnt { path, kind: UpperKind::Symlink(t) }),
        Looked::Dir(_) => {
            out.push(UpperEnt { path: path.clone(), kind: UpperKind::Dir });
            expand_lower(view, &vjoin(lower, name), &path, out);
        }
        Looked::Absent | Looked::Other => {}
    }
}

fn walk_upper(st: &State, view: &dyn View, dir: u32, dir_path: &[u8], full: bool, out: &mut Vec<UpperEnt>) {
    let Kind::Dir(d) = &st.nodes[dir as usize] else { return };
    if full {
        if let Some(lower) = &d.lower {
            if let Ok(mut list) = view.list(lower) {
                list.sort();
                for (name, _) in list {
                    if !d.entries.contains_key(&name[..]) {
                        emit_lower_entry(view, lower, dir_path, &name, out);
                    }
                }
            }
        }
    }
    let mut names: Vec<&Box<[u8]>> = d.entries.keys().collect();
    names.sort();
    for name in names {
        let path = vjoin(dir_path, name);
        match &d.entries[name] {
            Slot::Negative => {}
            Slot::Whiteout => out.push(UpperEnt { path, kind: UpperKind::Whiteout }),
            Slot::Lower(n) => match &st.nodes[*n as usize] {
                Kind::Dir(cd) => {
                    if full {
                        out.push(UpperEnt { path: path.clone(), kind: UpperKind::Dir });
                        walk_upper(st, view, *n, &path, true, out);
                    } else if cd.dirty {
                        walk_upper(st, view, *n, &path, false, out);
                    }
                }
                Kind::File(f) => match &f.src {
                    Src::Upper { path: p, .. } => out.push(UpperEnt {
                        path,
                        kind: UpperKind::File { path: p.clone(), exec: f.mode & 0o111 != 0 },
                    }),
                    Src::Lower(info) => {
                        if full {
                            out.push(UpperEnt { path, kind: UpperKind::LowerFile { info: info.clone() } });
                        }
                    }
                },
                Kind::Symlink { target, .. } => {
                    if full {
                        out.push(UpperEnt { path, kind: UpperKind::Symlink(target.clone()) });
                    }
                }
                Kind::Mount | Kind::Special { .. } => {}
            },
            Slot::Upper(n, _) => match &st.nodes[*n as usize] {
                Kind::Mount | Kind::Special { .. } => {}
                Kind::Dir(cd) => {
                    out.push(UpperEnt { path: path.clone(), kind: UpperKind::Dir });
                    walk_upper(st, view, *n, &path, cd.lower.is_some(), out);
                }
                Kind::File(f) => match &f.src {
                    Src::Upper { path: p, .. } => out.push(UpperEnt {
                        path,
                        kind: UpperKind::File { path: p.clone(), exec: f.mode & 0o111 != 0 },
                    }),
                    Src::Lower(info) => {
                        out.push(UpperEnt { path, kind: UpperKind::LowerFile { info: info.clone() } })
                    }
                },
                Kind::Symlink { target, .. } => {
                    out.push(UpperEnt { path, kind: UpperKind::Symlink(target.clone()) })
                }
            },
        }
    }
}

fn now() -> (i64, u32) {
    let d = crate::io::system_now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    (d.as_secs() as i64, d.subsec_nanos())
}

impl ActionFs {
    fn own(&self, o: &Owner) -> (u32, u32) {
        (
            if o.uid == OWNER_SELF { self.uid } else { o.uid },
            if o.gid == OWNER_SELF { self.gid } else { o.gid },
        )
    }

    fn attr(&self, st: &State, local: u32) -> fuse_attr {
        let mut a = fuse_attr {
            ino: nodeid(self.seq, local),
            blksize: 4096,
            nlink: 1,
            atime: LOWER_MTIME,
            mtime: LOWER_MTIME,
            ctime: LOWER_MTIME,
            uid: self.uid,
            gid: self.gid,
            ..Default::default()
        };
        match &st.nodes[local as usize] {
            Kind::Dir(d) => {
                let (uid, gid) = self.own(&d.owner);
                a.mode = libc::S_IFDIR | d.owner.mode;
                a.nlink = 2;
                a.uid = uid;
                a.gid = gid;
                a.mtime = d.mtime.0 as u64;
                a.mtimensec = d.mtime.1;
                a.ctime = a.mtime;
                a.ctimensec = a.mtimensec;
            }
            Kind::Mount => {
                a.mode = libc::S_IFDIR | 0o755;
                a.nlink = 2;
                a.uid = 0;
                a.gid = 0;
            }
            Kind::Symlink { target, .. } => {
                a.mode = libc::S_IFLNK | 0o777;
                a.size = target.len() as u64;
            }
            Kind::Special { mode, rdev } => {
                a.mode = *mode;
                a.rdev = *rdev;
            }
            Kind::File(f) => {
                a.nlink = f.links.max(1);
                match &f.src {
                    Src::Lower(info) => {
                        let (uid, gid) = self.own(&info.owner);
                        a.uid = uid;
                        a.gid = gid;
                        a.mode = libc::S_IFREG | f.mode;
                        a.size = info.size;
                    }
                    Src::Upper { file, .. } => {
                        a.mode = libc::S_IFREG | f.mode;
                        if let Ok(m) = file.metadata() {
                            a.size = m.len();
                            a.mtime = m.mtime() as u64;
                            a.mtimensec = m.mtime_nsec() as u32;
                            a.atime = m.atime() as u64;
                            a.atimensec = m.atime_nsec() as u32;
                            a.ctime = m.ctime() as u64;
                            a.ctimensec = m.ctime_nsec() as u32;
                        }
                    }
                }
                a.blocks = a.size.div_ceil(512);
            }
        }
        a
    }

    fn entry(&self, st: &State, local: u32) -> Entry {
        Entry {
            nodeid: nodeid(self.seq, local),
            generation: 0,
            attr: self.attr(st, local),
            entry_ttl: TTL,
            attr_ttl: TTL,
        }
    }

    fn push(&self, st: &mut State, k: Kind) -> Res<u32> {
        let id = st.nodes.len();
        if id as u64 >= LOCAL_MASK {
            return Err(Errno(libc::ENOSPC));
        }
        st.nodes.push(k);
        assert_eq!(st.nodes.len(), id + 1);
        assert!(id as u32 > ROOT_LOCAL, "the root is created with the action");
        Ok(id as u32)
    }

    fn resolve(&self, st: &mut State, parent: u32, name: &[u8], trace: bool) -> Res<Option<u32>> {
        let (lower, traced) = match &st.nodes[parent as usize] {
            Kind::Dir(d) => {
                if let Some(slot) = d.entries.get(name) {
                    let found = match slot {
                        Slot::Lower(n) | Slot::Upper(n, _) => Some(*n),
                        Slot::Negative | Slot::Whiteout => None,
                    };
                    return Ok(found);
                }
                match &d.lower {
                    Some(l) => (l.clone(), d.traced && trace),
                    None => {
                        return Ok(None);
                    }
                }
            }
            Kind::Mount => {
                return Ok(None);
            }
            _ => return Err(Errno::ENOTDIR),
        };
        let looked = self.view.lookup(&lower, name);
        let path: VPath = vjoin(&lower, name).into();
        let child_traced = matches!(looked, Looked::Dir(_)) && self.view.traced(&path);
        let found = self.apply_lookup(st, parent, name, path, traced, child_traced, looked)?;
        Ok(found)
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_lookup(&self, st: &mut State, parent: u32, name: &[u8], path: VPath, traced: bool, child_traced: bool, looked: Looked) -> Res<Option<u32>> {
        let slot = match &looked {
            Looked::Absent | Looked::Other => Slot::Negative,
            Looked::Dir(owner) => {
                let n = self.push(
                    st,
                    Kind::Dir(Box::new(Dir {
                        parent,
                        lower: Some(path.clone()),
                        traced: child_traced,
                        entries: HashMap::new(),
                        dirty: false,
                        owner: *owner,
                        lower_listed: false,
                        listing: None,
                        mtime: (LOWER_MTIME as i64, 0),
                    })),
                )?;
                Slot::Lower(n)
            }
            Looked::Symlink(target) => {
                let n = self.push(st, Kind::Symlink { target: target.clone() })?;
                Slot::Lower(n)
            }
            Looked::File(info) => {
                let mode = info.owner.mode;
                let n = self.push(
                    st,
                    Kind::File(Box::new(FileN {
                        parent,
                        src: Src::Lower(info.clone()),
                        mode,
                        opens: 0,
                        links: 1,
                        lower_pt_opens: 0,
                        read_src: None,
                        file: None,
                        obs: None,
                    })),
                )?;
                Slot::Lower(n)
            }
        };
        let recorded = st.trace.observe_lookup(path, traced, &looked);
        if let Some(index) = recorded {
            if let Slot::Lower(n) = &slot {
                if let Kind::File(file) = &mut st.nodes[*n as usize] {
                    file.obs = Some(index);
                }
            }
        }
        let found = match &slot {
            Slot::Lower(n) => Some(*n),
            _ => None,
        };
        let Kind::Dir(d) = &mut st.nodes[parent as usize] else { unreachable!("checked to be a directory above") };
        assert!(d.entries.insert(name.into(), slot).is_none(), "a name is resolved from the view once");
        Ok(found)
    }

    fn dir_mut<'a>(&self, st: &'a mut State, local: u32) -> Res<&'a mut Dir> {
        match st.nodes.get_mut(local as usize) {
            Some(Kind::Dir(d)) => Ok(d),
            Some(Kind::Mount) => Err(Errno::EPERM),
            Some(_) => Err(Errno::ENOTDIR),
            None => Err(Errno::ESTALE),
        }
    }

    fn mark_dirty(&self, st: &mut State, mut dir: u32) {
        let t = now();
        let mut first = true;
        while dir != 0 {
            let Some(Kind::Dir(d)) = st.nodes.get_mut(dir as usize) else { break };
            if first {
                d.listing = None;
                d.mtime = t;
                first = false;
            }
            if d.dirty {
                break;
            }
            d.dirty = true;
            dir = d.parent;
        }
    }

    fn new_upper_file(&self, st: &mut State) -> Res<(PathBuf, Arc<File>)> {
        assert!(!st.finished, "upper file created after the action finished");
        let n = st.next_upper;
        st.next_upper += 1;
        let path = self.scratch.join(n.to_string());
        let file = crate::io::file::OpenOptions::new().read(true).write(true).create_new(true).mode(0o644).open(&path)?;
        Ok((path, Arc::new(file)))
    }

    fn covers_lower(&self, st: &State, parent: u32, name: &[u8]) -> bool {
        match &st.nodes[parent as usize] {
            Kind::Dir(d) => match d.entries.get(name) {
                Some(Slot::Lower(_)) | Some(Slot::Whiteout) | Some(Slot::Upper(_, true)) => true,
                _ => false,
            },
            _ => false,
        }
    }

    fn merged_listing(&self, st: &mut State, local: u32) -> Res<Arc<Vec<(Box<[u8]>, DType)>>> {
        let (lower, need_obs) = {
            let d = self.dir_mut(st, local)?;
            if let Some(l) = &d.listing {
                return Ok(l.clone());
            }
            (d.lower.clone(), d.traced && !d.lower_listed)
        };
        let names = if let Some(lower) = &lower {
            let list = self.view.list(lower).map_err(Errno::from)?;
            if need_obs {
                st.trace.observe_list(lower.clone(), &list);
            }
            list.into_iter().map(|(name, dtype)| (name.into_boxed_slice(), dtype)).collect()
        } else {
            Vec::new()
        };
        let upper: Vec<(Box<[u8]>, Option<DType>)> = {
            let Kind::Dir(d) = &st.nodes[local as usize] else { return Err(Errno::ENOTDIR) };
            d.entries
                .iter()
                .filter_map(|(name, slot)| match slot {
                    Slot::Whiteout => Some((name.clone(), None)),
                    Slot::Upper(n, _) => Some((
                        name.clone(),
                        Some(match &st.nodes[*n as usize] {
                            Kind::Dir(_) | Kind::Mount => DType::Dir,
                            Kind::File(_) => DType::File,
                            Kind::Symlink { .. } => DType::Symlink,
                            Kind::Special { .. } => DType::Other,
                        }),
                    )),
                    _ => None,
                })
                .collect()
        };
        let v = Arc::new(crate::view::merge_listings(names, upper));
        let d = self.dir_mut(st, local)?;
        d.lower_listed = true;
        d.listing = Some(v.clone());
        Ok(v)
    }

    fn is_empty_dir(&self, st: &mut State, local: u32) -> Res<bool> {
        Ok(self.merged_listing(st, local)?.is_empty())
    }

    pub fn mkdir_p(&self, path: &[u8]) -> Res<()> {
        let mut st = self.st.lock();
        let mut cur = ROOT_LOCAL;
        for comp in path.split(|&c| c == b'/').filter(|c| !c.is_empty()) {
            cur = match self.resolve(&mut st, cur, comp, false)? {
                Some(n) => match &st.nodes[n as usize] {
                    Kind::Dir(_) => n,
                    Kind::Mount => return Ok(()),
                    _ => return Err(Errno::ENOTDIR),
                },
                None => self.do_mkdir(&mut st, cur, comp, 0o755)?,
            };
        }
        Ok(())
    }

    fn do_mkdir(&self, st: &mut State, parent: u32, name: &[u8], mode: u32) -> Res<u32> {
        let covers = self.covers_lower(st, parent, name);
        let n = self.push(
            st,
            Kind::Dir(Box::new(Dir {
                parent,
                lower: None,
                traced: false,
                entries: HashMap::new(),
                dirty: true,
                owner: Owner::own(mode & 0o7777),
                lower_listed: true,
                listing: None,
                mtime: now(),
            })),
        )?;
        self.dir_mut(st, parent)?.entries.insert(name.into(), Slot::Upper(n, covers));
        self.mark_dirty(st, parent);
        Ok(n)
    }

    pub fn upper_file_at(&self, path: &[u8]) -> Option<(PathBuf, bool)> {
        let st = self.st.lock();
        let mut cur = ROOT_LOCAL;
        let mut comps = path.split(|&c| c == b'/').filter(|c| !c.is_empty()).peekable();
        while let Some(comp) = comps.next() {
            let Kind::Dir(d) = &st.nodes[cur as usize] else { return None };
            let n = match d.entries.get(comp)? {
                Slot::Lower(n) | Slot::Upper(n, _) => *n,
                _ => return None,
            };
            if comps.peek().is_none() {
                if let Kind::File(f) = &st.nodes[n as usize] {
                    if let Src::Upper { path, .. } = &f.src {
                        return Some((path.clone(), f.mode & 0o111 != 0));
                    }
                }
                return None;
            }
            cur = n;
        }
        None
    }

    fn copy_up(&self, fs: &SandboxFs, st: &mut State, n: u32, truncate: bool) -> Res<()> {
        let (info, parent) = match &st.nodes[n as usize] {
            Kind::File(f) => match &f.src {
                Src::Lower(info) => (info.clone(), f.parent),
                Src::Upper { .. } => return Ok(()),
            },
            _ => return Err(Errno::EINVAL),
        };
        let (path, file) = self.new_upper_file(st)?;
        let mut read_src = None;
        if !truncate {
            let (src, _, actual) = fs.content.open(&info.src)?;
            crate::store::clone_or_copy(&src, &file)?;
            read_src = Some(actual);
        }
        {
            let Kind::File(f) = &mut st.nodes[n as usize] else { unreachable!("checked to be a file above") };
            assert!(matches!(f.src, Src::Lower(_)));
            f.src = Src::Upper { path, file, backing: None };
            f.file = None;
        }
        if let Some(src) = read_src {
            accept_read(st, n, src);
        } else {
            mark_file_read(st, n);
        }
        self.mark_dirty(st, parent);
        Ok(())
    }

    fn conceal(&self, st: &mut State, parent: u32, name: &[u8]) -> Res<()> {
        let Some(n) = self.resolve(st, parent, name, true)? else { return Err(Errno::ENOENT) };
        if matches!(st.nodes[n as usize], Kind::Dir(_) | Kind::Mount) {
            return Err(Errno::EISDIR);
        }
        let covers = self.covers_lower(st, parent, name);
        {
            let d = self.dir_mut(st, parent)?;
            if covers {
                d.entries.insert(name.into(), Slot::Whiteout);
            } else {
                d.entries.remove(name);
            }
        }
        if let Kind::File(f) = &mut st.nodes[n as usize] {
            assert!(f.links > 0, "unlinking a file with no links");
            f.links = f.links.saturating_sub(1);
            if f.links == 0 && f.opens == 0 {
                if let Src::Upper { path, .. } = &f.src {
                    let _ = crate::io::file::remove_file(path);
                }
            }
        }
        self.mark_dirty(st, parent);
        Ok(())
    }

    fn open_upper(&self, fs: &SandboxFs, st: &mut State, n: u32) -> Res<Opened> {
        let Kind::File(f) = &mut st.nodes[n as usize] else { return Err(Errno::EINVAL) };
        let lower_pt = f.lower_pt_opens > 0;
        let Src::Upper { file, backing, .. } = &mut f.src else { return Err(Errno::EINVAL) };
        f.opens += 1;
        if fs.passthrough() && !lower_pt {
            let id = match backing {
                Some(id) => *id,
                None => {
                    let sess = fs.session.get().ok_or(Errno::EIO)?;
                    let id = sess.backing_open(file.as_raw_fd())?;
                    *backing = Some(id);
                    st.backing_ids.push(id);
                    id
                }
            };
            return Ok(Opened { fh: 0, flags: FOPEN_PASSTHROUGH, backing_id: id });
        }
        Ok(Opened { fh: 0, flags: if lower_pt { FOPEN_DIRECT_IO } else { 0 }, backing_id: 0 })
    }
}

fn file_hash_src(st: &State, index: usize) -> Option<FileSrc> {
    for node in &st.nodes {
        if let Kind::File(file) = node {
            if file.obs == Some(index) {
                return file.read_src.clone();
            }
        }
    }
    None
}

fn accept_read(st: &mut State, node: u32, actual: FileSrc) {
    if let Kind::File(file) = &mut st.nodes[node as usize] {
        if file.read_src.is_none() {
            file.read_src = Some(actual);
        }
    }
    mark_file_read(st, node);
}

fn note_read(st: &mut State, node: u32) {
    if st.nodes.get(node as usize).is_some() {
        mark_file_read(st, node);
    }
}

fn mark_file_read(st: &mut State, node: u32) {
    let Kind::File(file) = &st.nodes[node as usize] else { return };
    if file.read_src.is_some() {
        if let Some(index) = file.obs {
            st.trace.mark_read(index);
        }
    }
}

impl State {
    fn read_source(&self, node: u32) -> Res<&Arc<File>> {
        match self.nodes.get(node as usize) {
            Some(Kind::File(file)) => match &file.src {
                Src::Upper { file, .. } => Ok(file),
                Src::Lower(_) => file.file.as_ref().ok_or(Errno::EBADF),
            },
            _ => Err(Errno::EBADF),
        }
    }
}

#[cfg(test)]
impl StateMachine for State {
    fn invariants(&self) {
        self.trace.invariants();
        assert!(!self.nodes.is_empty(), "the root node is there");
        let n = self.nodes.len();
        for (i, node) in self.nodes.iter().enumerate() {
            match node {
                Kind::Dir(dir) => {
                    assert!((dir.parent as usize) < n, "directory {i}'s parent {} is a node", dir.parent);
                    for slot in dir.entries.values() {
                        let id = match slot {
                            Slot::Lower(n) | Slot::Upper(n, _) => *n as usize,
                            Slot::Negative | Slot::Whiteout => continue,
                        };
                        assert!(id != 0 && id < n, "directory {i} names node {id}, which is not a child node");
                    }
                }
                Kind::File(file) => {
                    assert!((file.parent as usize) < n, "file {i}'s parent {} is a node", file.parent);
                    if let Some(index) = file.obs {
                        assert!(self.trace.file_is(index, file.read_src.is_some()), "file {i} is traced as read exactly when it was read");
                    }
                }
                _ => {}
            }
        }
    }
}

impl Filesystem for SandboxFs {
    fn lookup(&self, _ctx: &Ctx, parent: u64, name: &[u8]) -> Res<Entry> {
        self.stats.lookups.fetch_add(1, Ordering::Relaxed);
        if parent == FUSE_ROOT_ID {
            let seq = std::str::from_utf8(name)
                .ok()
                .and_then(|s| s.strip_prefix('a'))
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or(Errno::ENOENT)?;
            let a = self.actions.get(&seq).ok_or(Errno::ENOENT)?.clone();
            let st = a.st.lock();
            return Ok(a.entry(&st, ROOT_LOCAL));
        }
        let (a, local) = self.action(parent)?;
        let mut st = a.st.lock();
        match a.resolve(&mut st, local, name, true)? {
            Some(n) => Ok(a.entry(&st, n)),
            None => Ok(Entry::negative(TTL)),
        }
    }

    fn getattr(&self, _ctx: &Ctx, node: u64, _fh: Option<u64>) -> Res<(fuse_attr, Duration)> {
        if node == FUSE_ROOT_ID {
            let a = fuse_attr {
                ino: FUSE_ROOT_ID,
                mode: libc::S_IFDIR | 0o755,
                nlink: 2,
                blksize: 4096,
                mtime: LOWER_MTIME,
                ..Default::default()
            };
            return Ok((a, TTL));
        }
        let (a, local) = self.action(node)?;
        let st = a.st.lock();
        if local as usize >= st.nodes.len() {
            return Err(Errno::ESTALE);
        }
        Ok((a.attr(&st, local), TTL))
    }

    fn setattr(&self, _ctx: &Ctx, node: u64, set: &SetAttr) -> Res<(fuse_attr, Duration)> {
        let (a, local) = self.action(node)?;
        let mut st = a.st.lock();
        if local as usize >= st.nodes.len() {
            return Err(Errno::ESTALE);
        }
        if let Some(size) = set.size {
            a.copy_up(self, &mut st, local, size == 0)?;
            if let Kind::File(f) = &st.nodes[local as usize] {
                if let Src::Upper { file, .. } = &f.src {
                    file.set_len(size)?;
                }
            }
        }
        if let Some(mode) = set.mode {
            match &mut st.nodes[local as usize] {
                Kind::File(f) => {
                    f.mode = mode & 0o7777;
                    let parent = f.parent;
                    if matches!(f.src, Src::Lower(_)) {
                        a.copy_up(self, &mut st, local, false)?;
                    }
                    a.mark_dirty(&mut st, parent);
                }
                Kind::Dir(d) => d.owner.mode = mode & 0o7777,
                _ => {}
            }
        }
        if set.atime.is_some() || set.mtime.is_some() {
            let conv = |t: Option<TimeSet>| match t {
                None => libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_OMIT },
                Some(TimeSet::Now) => libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_NOW },
                Some(TimeSet::At(s, ns)) => libc::timespec { tv_sec: s, tv_nsec: ns as i64 },
            };
            match &mut st.nodes[local as usize] {
                Kind::File(f) => {
                    if let Src::Upper { file, .. } = &f.src {
                        let _ = file.set_times(conv(set.atime), conv(set.mtime));
                    }
                }
                Kind::Dir(d) => {
                    if let Some(TimeSet::At(s, ns)) = set.mtime {
                        d.mtime = (s, ns);
                    } else if let Some(TimeSet::Now) = set.mtime {
                        d.mtime = now();
                    }
                }
                _ => {}
            }
        }
        Ok((a.attr(&st, local), TTL))
    }

    fn readlink(&self, _ctx: &Ctx, node: u64) -> Res<Vec<u8>> {
        let (a, local) = self.action(node)?;
        let st = a.st.lock();
        match st.nodes.get(local as usize) {
            Some(Kind::Symlink { target, .. }) => Ok(target.clone()),
            _ => Err(Errno::EINVAL),
        }
    }

    fn symlink(&self, _ctx: &Ctx, parent: u64, name: &[u8], target: &[u8]) -> Res<Entry> {
        let (a, p) = self.action(parent)?;
        let mut st = a.st.lock();
        if a.resolve(&mut st, p, name, true)?.is_some() {
            return Err(Errno::EEXIST);
        }
        let covers = a.covers_lower(&st, p, name);
        let n = a.push(&mut st, Kind::Symlink { target: target.to_vec() })?;
        a.dir_mut(&mut st, p)?.entries.insert(name.into(), Slot::Upper(n, covers));
        a.mark_dirty(&mut st, p);
        Ok(a.entry(&st, n))
    }

    fn mknod(&self, _ctx: &Ctx, parent: u64, name: &[u8], mode: u32, rdev: u32) -> Res<Entry> {
        let (a, p) = self.action(parent)?;
        let mut st = a.st.lock();
        if a.resolve(&mut st, p, name, true)?.is_some() {
            return Err(Errno::EEXIST);
        }
        let covers = a.covers_lower(&st, p, name);
        let n = match mode & libc::S_IFMT {
            libc::S_IFREG | 0 => {
                let (path, file) = a.new_upper_file(&mut st)?;
                a.push(
                    &mut st,
                    Kind::File(Box::new(FileN {
                        parent: p,
                        src: Src::Upper { path, file, backing: None },
                        mode: mode & 0o7777,
                        opens: 0,
                        links: 1,
                        lower_pt_opens: 0,
                        read_src: None,
                        file: None,
                        obs: None,
                    })),
                )?
            }
            libc::S_IFIFO | libc::S_IFSOCK => a.push(&mut st, Kind::Special { mode, rdev })?,
            _ => return Err(Errno::EPERM),
        };
        a.dir_mut(&mut st, p)?.entries.insert(name.into(), Slot::Upper(n, covers));
        a.mark_dirty(&mut st, p);
        Ok(a.entry(&st, n))
    }

    fn mkdir(&self, _ctx: &Ctx, parent: u64, name: &[u8], mode: u32) -> Res<Entry> {
        let (a, p) = self.action(parent)?;
        let mut st = a.st.lock();
        if a.resolve(&mut st, p, name, true)?.is_some() {
            return Err(Errno::EEXIST);
        }
        let n = a.do_mkdir(&mut st, p, name, mode)?;
        Ok(a.entry(&st, n))
    }

    fn unlink(&self, _ctx: &Ctx, parent: u64, name: &[u8]) -> Res<()> {
        let (a, p) = self.action(parent)?;
        let mut st = a.st.lock();
        a.conceal(&mut st, p, name)
    }

    fn rmdir(&self, _ctx: &Ctx, parent: u64, name: &[u8]) -> Res<()> {
        let (a, p) = self.action(parent)?;
        let mut st = a.st.lock();
        let Some(n) = a.resolve(&mut st, p, name, true)? else { return Err(Errno::ENOENT) };
        match &st.nodes[n as usize] {
            Kind::Dir(_) => {}
            Kind::Mount => return Err(Errno::EBUSY),
            _ => return Err(Errno::ENOTDIR),
        }
        if !a.is_empty_dir(&mut st, n)? {
            return Err(Errno::ENOTEMPTY);
        }
        let covers = a.covers_lower(&st, p, name);
        let d = a.dir_mut(&mut st, p)?;
        if covers {
            d.entries.insert(name.into(), Slot::Whiteout);
        } else {
            d.entries.remove(name);
        }
        a.mark_dirty(&mut st, p);
        Ok(())
    }

    fn rename(&self, _ctx: &Ctx, parent: u64, name: &[u8], newparent: u64, newname: &[u8], flags: u32) -> Res<()> {
        const RENAME_NOREPLACE: u32 = 1;
        if flags & !RENAME_NOREPLACE != 0 {
            return Err(Errno::EINVAL);
        }
        let (a, p) = self.action(parent)?;
        let (a2, np) = self.action(newparent)?;
        if a.seq != a2.seq {
            return Err(Errno::EXDEV);
        }
        let mut st = a.st.lock();
        let Some(n) = a.resolve(&mut st, p, name, true)? else { return Err(Errno::ENOENT) };
        if matches!(st.nodes[n as usize], Kind::Mount) {
            return Err(Errno::EBUSY);
        }
        let src_is_dir = matches!(st.nodes[n as usize], Kind::Dir(_));
        if let Some(t) = a.resolve(&mut st, np, newname, true)? {
            if flags & RENAME_NOREPLACE != 0 {
                return Err(Errno::EEXIST);
            }
            match &st.nodes[t as usize] {
                Kind::Mount => return Err(Errno::EBUSY),
                Kind::Dir(_) => {
                    if !src_is_dir {
                        return Err(Errno::EISDIR);
                    }
                    if !a.is_empty_dir(&mut st, t)? {
                        return Err(Errno::ENOTEMPTY);
                    }
                }
                _ => {
                    if src_is_dir {
                        return Err(Errno::ENOTDIR);
                    }
                }
            }
            if let Kind::File(f) = &mut st.nodes[t as usize] {
                f.links = f.links.saturating_sub(1);
            }
        }
        let src_covers = a.covers_lower(&st, p, name);
        let dst_covers = a.covers_lower(&st, np, newname);
        {
            let d = a.dir_mut(&mut st, p)?;
            if src_covers {
                d.entries.insert(name.into(), Slot::Whiteout);
            } else {
                d.entries.remove(name);
            }
        }
        a.dir_mut(&mut st, np)?.entries.insert(newname.into(), Slot::Upper(n, dst_covers));
        match &mut st.nodes[n as usize] {
            Kind::Dir(d) => {
                d.parent = np;
                d.dirty = true;
            }
            Kind::File(f) => f.parent = np,
            _ => {}
        }
        a.mark_dirty(&mut st, p);
        a.mark_dirty(&mut st, np);
        Ok(())
    }

    fn link(&self, _ctx: &Ctx, node: u64, newparent: u64, newname: &[u8]) -> Res<Entry> {
        let (a, n) = self.action(node)?;
        let (a2, np) = self.action(newparent)?;
        if a.seq != a2.seq {
            return Err(Errno::EXDEV);
        }
        let mut st = a.st.lock();
        if a.resolve(&mut st, np, newname, true)?.is_some() {
            return Err(Errno::EEXIST);
        }
        match st.nodes.get_mut(n as usize) {
            Some(Kind::File(f)) => f.links += 1,
            Some(Kind::Symlink { .. }) | Some(Kind::Special { .. }) => {}
            _ => return Err(Errno::EPERM),
        }
        let covers = a.covers_lower(&st, np, newname);
        a.dir_mut(&mut st, np)?.entries.insert(newname.into(), Slot::Upper(n, covers));
        a.mark_dirty(&mut st, np);
        Ok(a.entry(&st, n))
    }

    fn open(&self, _ctx: &Ctx, node: u64, flags: u32) -> Res<Opened> {
        self.stats.opens.fetch_add(1, Ordering::Relaxed);
        let (a, n) = self.action(node)?;
        let mut st = a.st.lock();
        let acc = flags as i32 & libc::O_ACCMODE;
        let wants_write = acc != libc::O_RDONLY;
        let trunc = flags as i32 & libc::O_TRUNC != 0;
        let is_lower = match st.nodes.get(n as usize) {
            Some(Kind::File(f)) => matches!(f.src, Src::Lower(_)),
            Some(Kind::Dir(_)) | Some(Kind::Mount) => return Err(Errno::EISDIR),
            _ => return Err(Errno::EINVAL),
        };
        if is_lower && (wants_write || trunc) {
            a.copy_up(self, &mut st, n, trunc)?;
        } else if is_lower {
            enum Next {
                Cached(i32),
                Opened { file: crate::io::file::File, key: BackingKey },
            }
            let (next, actual) = {
                let Kind::File(f) = &mut st.nodes[n as usize] else { unreachable!() };
                let Src::Lower(info) = &mut f.src else { unreachable!() };
                if self.passthrough() {
                    let key = match &info.src {
                        FileSrc::Blob(h) => Some(BackingKey::Blob(*h)),
                        FileSrc::Host { sig, .. } => Some(BackingKey::Host(*sig)),
                        FileSrc::Lazy(_) => self.content.known_hash(&info.src).map(BackingKey::Blob),
                    };
                    if let Some(e) = key.and_then(|k| self.backing.get(&k)) {
                        e.1.store(self.tick.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
                        let src = info.src.clone();
                        f.opens += 1;
                        f.lower_pt_opens += 1;
                        (Next::Cached(e.0), src)
                    } else {
                        let (file, key, actual) = self.content.open(&info.src)?;
                        let recorded = actual.clone();
                        info.src = actual;
                        f.opens += 1;
                        (Next::Opened { file, key }, recorded)
                    }
                } else {
                    let (file, key, actual) = self.content.open(&info.src)?;
                    let recorded = actual.clone();
                    info.src = actual;
                    f.opens += 1;
                    (Next::Opened { file, key }, recorded)
                }
            };
            accept_read(&mut st, n, actual);
            match next {
                Next::Cached(id) => return Ok(Opened { fh: 1, flags: FOPEN_PASSTHROUGH, backing_id: id }),
                Next::Opened { file, key } => {
                    if self.passthrough() {
                        let id = self.backing_id(key, &file)?;
                        let Kind::File(f) = &mut st.nodes[n as usize] else { unreachable!() };
                        f.lower_pt_opens += 1;
                        return Ok(Opened { fh: 1, flags: FOPEN_PASSTHROUGH, backing_id: id });
                    }
                    let Kind::File(f) = &mut st.nodes[n as usize] else { unreachable!() };
                    if f.file.is_none() {
                        f.file = Some(Arc::new(file));
                    }
                    return Ok(Opened { fh: 0, flags: FOPEN_KEEP_CACHE, backing_id: 0 });
                }
            }
        }
        if trunc {
            if let Kind::File(f) = &st.nodes[n as usize] {
                if let Src::Upper { file, .. } = &f.src {
                    file.set_len(0)?;
                }
            }
        }
        a.open_upper(self, &mut st, n)
    }

    fn create(&self, _ctx: &Ctx, parent: u64, name: &[u8], mode: u32, _flags: u32) -> Res<(Entry, Opened)> {
        let (a, p) = self.action(parent)?;
        let mut st = a.st.lock();
        if a.resolve(&mut st, p, name, true)?.is_some() {
            return Err(Errno::EEXIST);
        }
        a.dir_mut(&mut st, p)?;
        let covers = a.covers_lower(&st, p, name);
        let (path, file) = a.new_upper_file(&mut st)?;
        let n = a.push(
            &mut st,
            Kind::File(Box::new(FileN {
                parent: p,
                src: Src::Upper { path, file, backing: None },
                mode: mode & 0o7777,
                opens: 0,
                links: 1,
                lower_pt_opens: 0,
                read_src: None,
                file: None,
                obs: None,
            })),
        )?;
        a.dir_mut(&mut st, p)?.entries.insert(name.into(), Slot::Upper(n, covers));
        a.mark_dirty(&mut st, p);
        let opened = a.open_upper(self, &mut st, n)?;
        Ok((a.entry(&st, n), opened))
    }

    fn read(&self, _ctx: &Ctx, node: u64, _fh: u64, offset: u64, size: u32, out: &mut Vec<u8>) -> Res<()> {
        self.stats.reads.fetch_add(1, Ordering::Relaxed);
        let (a, n) = self.action(node)?;
        let file = a.st.lock().read_source(n)?.clone();
        out.resize(size as usize, 0);
        let mut done = 0;
        while done < out.len() {
            let r = file.read_at(&mut out[done..], offset + done as u64)?;
            if r == 0 {
                break;
            }
            done += r;
        }
        out.truncate(done);
        {
            let mut st = a.st.lock();
            note_read(&mut st, n);
        }
        Ok(())
    }

    fn write(&self, _ctx: &Ctx, node: u64, _fh: u64, offset: u64, data: &[u8]) -> Res<u32> {
        self.stats.writes.fetch_add(1, Ordering::Relaxed);
        let (a, n) = self.action(node)?;
        let file = {
            let st = a.st.lock();
            match st.nodes.get(n as usize) {
                Some(Kind::File(f)) => match &f.src {
                    Src::Upper { file, .. } => file.clone(),
                    Src::Lower(_) => return Err(Errno::EBADF),
                },
                _ => return Err(Errno::EBADF),
            }
        };
        file.write_all_at(data, offset)?;
        Ok(data.len() as u32)
    }

    fn release(&self, node: u64, fh: u64, _flags: u32) {
        let Ok((a, n)) = self.action(node) else { return };
        let mut st = a.st.lock();
        if let Some(Kind::File(f)) = st.nodes.get_mut(n as usize) {
            assert!(f.opens > 0, "release without an open");
            assert!(fh != 1 || f.lower_pt_opens > 0, "release of a passthrough open never counted");
            f.opens = f.opens.saturating_sub(1);
            if fh == 1 {
                f.lower_pt_opens = f.lower_pt_opens.saturating_sub(1);
            }
        }
    }

    fn readdir(&self, _ctx: &Ctx, node: u64, _fh: u64, offset: u64, sink: &mut DirSink<'_>) -> Res<()> {
        if node == FUSE_ROOT_ID {
            return Ok(());
        }
        let (a, n) = self.action(node)?;
        let mut st = a.st.lock();
        if matches!(st.nodes.get(n as usize), Some(Kind::Mount)) {
            return Ok(());
        }
        let list = a.merged_listing(&mut st, n)?;
        for (i, (name, t)) in list.iter().enumerate().skip(offset as usize) {
            let ino = crate::hash::Hash::of(name).0[..8].try_into().map(u64::from_le_bytes).unwrap_or(2) | 1;
            if !sink.push(ino, i as u64 + 1, t.as_dirent(), name) {
                break;
            }
        }
        Ok(())
    }

    fn fallocate(&self, _ctx: &Ctx, node: u64, _fh: u64, offset: u64, length: u64, mode: u32) -> Res<()> {
        let (a, n) = self.action(node)?;
        let st = a.st.lock();
        if let Some(Kind::File(f)) = st.nodes.get(n as usize) {
            if let Src::Upper { file, .. } = &f.src {
                return file.allocate(mode as i32, offset, length).map_err(Errno::from);
            }
        }
        Err(Errno::EBADF)
    }
}

#[cfg(test)]
mod model_tests {
    use crate::test_support::Transcript;
    use expect_test::expect;
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    enum Access {
        Lookup { dir: Vec<u8>, name: Vec<u8> },
        ReadDir(Vec<u8>),
        Open { path: Vec<u8> },
        Read { path: Vec<u8> },
        Unlink { dir: Vec<u8>, name: Vec<u8> },
        Mkdir { dir: Vec<u8>, name: Vec<u8> },
    }

    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
            self.0
        }
    }

    struct Built {
        view: MemView,
        untraced: Vec<Vec<u8>>,
        blind: bool,
        accesses: Vec<Access>,
        dir: Vec<u8>,
        plain: Vec<u8>,
        semantic: Vec<u8>,
        idle: Vec<u8>,
        idle_sem: Vec<u8>,
        link: Vec<u8>,
        other: Vec<u8>,
        hidden: Vec<u8>,
        hidden_dir: Vec<u8>,
        cover_name: Vec<u8>,
        made_name: Vec<u8>,
        sem: crate::hash::Hash,
    }

    struct Partial {
        inner: MemView,
        untraced: Vec<Vec<u8>>,
    }

    impl View for Partial {
        fn lookup(&self, dir: &[u8], name: &[u8]) -> Looked {
            self.inner.lookup(dir, name)
        }
        fn list(&self, dir: &[u8]) -> std::io::Result<Listing> {
            self.inner.list(dir)
        }
        fn traced(&self, dir: &[u8]) -> bool {
            !self.untraced.iter().any(|path| path.as_slice() == dir)
        }
        fn read_bytes(&self, path: &[u8]) -> Option<&[u8]> {
            self.inner.read_bytes(path)
        }
    }

    fn fresh_name(rng: &mut Lcg, view: &MemView, parent: &[u8]) -> Vec<u8> {
        let letters = b"abcdefghij";
        for _ in 0..24 {
            let letter = letters[(rng.next() as usize) % letters.len()];
            let mark = (rng.next() % 5) as u8;
            let name = if mark == 0 { vec![letter] } else { vec![letter, b'0' + mark] };
            if matches!(view.lookup(parent, &name), Looked::Absent) {
                return name;
            }
        }
        format!("n{}", rng.next() % 1_000_000).into_bytes()
    }

    fn generated(seed: u64) -> Built {
        let mut rng = Lcg(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1));
        let mut view = MemView::new();
        let mut dirs = vec![b"/".to_vec()];
        let mut made = Vec::new();
        let mut files = Vec::new();
        let extra = 4 + (rng.next() % 7) as usize;
        for i in 0..extra {
            let parent = dirs[(rng.next() as usize) % dirs.len()].clone();
            let depth = parent.iter().filter(|c| **c == b'/').count();
            let name = fresh_name(&mut rng, &view, &parent);
            let path = vjoin(&parent, &name);
            let roll = rng.next() % 8;
            if depth < 4 && roll < 3 {
                view.dir(&path);
                dirs.push(path.clone());
            } else if roll < 6 {
                let body = format!("c{seed}-{i}-{}", rng.next() % 10_000).into_bytes();
                let exec = rng.next() % 2 == 0;
                let sem = (rng.next() % 4 == 0).then(|| crate::hash::Hash::of(format!("sem-{seed}-{i}").as_bytes()));
                view.file(&path, &body, exec, sem);
                files.push(path.clone());
            } else if roll == 6 {
                view.symlink(&path, format!("t{}", rng.next() % 50).as_bytes());
            } else {
                view.other(&path);
            }
            made.push(path);
        }
        if dirs.len() == 1 {
            let path = vjoin(b"/", &fresh_name(&mut rng, &view, b"/"));
            view.dir(&path);
            dirs.push(path);
        }
        let dir = dirs[1 + (rng.next() as usize) % (dirs.len() - 1)].clone();
        let place = |rng: &mut Lcg, view: &mut MemView, parent: &[u8]| {
            let name = fresh_name(rng, view, parent);
            vjoin(parent, &name)
        };
        let child = place(&mut rng, &mut view, &dir);
        view.file(&child, format!("child-{seed}").as_bytes(), rng.next() % 2 == 0, None);
        let plain = place(&mut rng, &mut view, &dir);
        view.file(&plain, format!("plain-{seed}").as_bytes(), false, None);
        let sem = crate::hash::Hash::of(format!("anchor-sem-{seed}").as_bytes());
        let semantic = place(&mut rng, &mut view, &dir);
        view.file(&semantic, format!("semantic-{seed}").as_bytes(), true, Some(sem));
        let idle = place(&mut rng, &mut view, &dir);
        view.file(&idle, format!("idle-{seed}-{}", rng.next() % 100).as_bytes(), rng.next() % 2 == 0, None);
        let idle_sem_hash = crate::hash::Hash::of(format!("idle-sem-{seed}").as_bytes());
        let idle_sem = place(&mut rng, &mut view, &dir);
        view.file(&idle_sem, b"idle-sem-bytes", false, Some(idle_sem_hash));
        let link = place(&mut rng, &mut view, &dir);
        view.symlink(&link, format!("away-{seed}").as_bytes());
        let other = place(&mut rng, &mut view, &dir);
        view.other(&other);
        let hidden_dir = place(&mut rng, &mut view, b"/");
        view.dir(&hidden_dir);
        let hidden = place(&mut rng, &mut view, &hidden_dir);
        view.file(&hidden, b"hidden-bytes", false, None);
        let cover_name = fresh_name(&mut rng, &view, &dir);
        view.file(&vjoin(&dir, &cover_name), b"cover-bytes", false, None);
        let made_name = fresh_name(&mut rng, &view, &dir);

        let mut accesses = Vec::new();
        for path in &made {
            if rng.next() % 4 == 0 {
                continue;
            }
            let (parent, name) = vsplit(path).unwrap();
            accesses.push(Access::Lookup { dir: parent.to_vec(), name: name.to_vec() });
        }
        for _ in 0..(2 + rng.next() % 3) {
            let parent = dirs[(rng.next() as usize) % dirs.len()].clone();
            accesses.push(Access::Lookup {
                dir: parent,
                name: format!("miss{}", rng.next() % 1_000_000).into_bytes(),
                });
        }
        for directory in &dirs {
            if rng.next() % 2 == 0 {
                accesses.push(Access::ReadDir(directory.clone()));
            }
        }
        for path in &files {
            match rng.next() % 3 {
                0 => accesses.push(Access::Open { path: path.clone() }),
                1 => accesses.push(Access::Read { path: path.clone() }),
                _ => {}
            }
        }
        for i in (1..accesses.len()).rev() {
            let j = (rng.next() as usize) % (i + 1);
            accesses.swap(i, j);
        }
        let lookup = |path: &[u8]| {
            let (parent, name) = vsplit(path).unwrap();
            Access::Lookup { dir: parent.to_vec(), name: name.to_vec() }
        };
        accesses.push(lookup(&dir));
        accesses.push(lookup(&child));
        accesses.push(lookup(&plain));
        accesses.push(lookup(&semantic));
        accesses.push(lookup(&idle));
        accesses.push(lookup(&idle_sem));
        accesses.push(lookup(&link));
        accesses.push(lookup(&other));
        accesses.push(Access::Lookup { dir: dir.clone(), name: format!("gone-{seed}").into_bytes() });
        accesses.push(lookup(&hidden_dir));
        accesses.push(lookup(&hidden));
        accesses.push(Access::ReadDir(hidden_dir.clone()));
        accesses.push(Access::Open { path: hidden.clone() });
        accesses.push(Access::Read { path: hidden.clone() });
        accesses.push(Access::Open { path: plain.clone() });
        accesses.push(Access::Read { path: plain.clone() });
        accesses.push(Access::Open { path: semantic.clone() });
        accesses.push(Access::Read { path: idle_sem.clone() });
        accesses.push(lookup(&vjoin(&dir, &cover_name)));
        accesses.push(Access::Unlink { dir: dir.clone(), name: cover_name.clone() });
        accesses.push(Access::Mkdir { dir: dir.clone(), name: made_name.clone() });
        accesses.push(Access::ReadDir(dir.clone()));
        accesses.push(Access::ReadDir(b"/".to_vec()));
        Built {
            view,
            untraced: vec![hidden_dir.clone()],
            blind: seed % 2 == 0,
            accesses,
            dir,
            plain,
            semantic,
            idle,
            idle_sem,
            link,
            other,
            hidden,
            hidden_dir,
            cover_name,
            made_name,
            sem,
        }
    }

    enum Expect {
        Settled(Obs),
        File { size: u64, exec: bool, sem: Option<crate::hash::Hash>, read: bool },
    }

    struct Seen {
        traced: BTreeMap<Vec<u8>, bool>,
        resolved: BTreeSet<(Vec<u8>, Vec<u8>)>,
        listed: BTreeSet<Vec<u8>>,
        opened: BTreeSet<Vec<u8>>,
        overlay: BTreeMap<Vec<u8>, Vec<(Vec<u8>, Option<DType>)>>,
    }

    fn note_lookup(view: &dyn View, seen: &mut Seen, items: &mut Vec<(Vec<u8>, Expect)>, dir: &[u8], name: &[u8]) {
        if !seen.resolved.insert((dir.to_vec(), name.to_vec())) {
            return;
        }
        let parent_traced = seen.traced[dir];
        let looked = view.lookup(dir, name);
        let path = vjoin(dir, name);
        if parent_traced {
            let item = match &looked {
                Looked::Absent => Expect::Settled(Obs::Absent),
                Looked::Dir(_) => Expect::Settled(Obs::Dir),
                Looked::Other => Expect::Settled(Obs::Other),
                Looked::Symlink(target) => Expect::Settled(Obs::Symlink(target.clone())),
                Looked::File(info) => Expect::File {
                    size: info.size,
                    exec: info.exec(),
                    sem: info.sem,
                    read: false,
                },
            };
            items.push((path.clone(), item));
        }
        if matches!(looked, Looked::Dir(_)) {
            let child_traced = view.traced(&path);
            seen.traced.insert(path, child_traced);
        }
    }

    fn ensure(view: &dyn View, seen: &mut Seen, items: &mut Vec<(Vec<u8>, Expect)>, path: &[u8]) -> bool {
        if path == b"/" || seen.traced.contains_key(path) {
            return true;
        }
        let Some((parent, name)) = vsplit(path) else { return false };
        if !ensure(view, seen, items, parent) {
            return false;
        }
        note_lookup(view, seen, items, parent, name);
        seen.traced.contains_key(path)
    }

    fn mark_opened(items: &mut [(Vec<u8>, Expect)], path: &[u8]) {
        for (got, item) in items {
            if got == path {
                if let Expect::File { read, .. } = item {
                    *read = true;
                }
            }
        }
    }

    fn independent_merge(base: Listing, overlay: &[(Vec<u8>, Option<DType>)]) -> Vec<(Vec<u8>, DType)> {
        let mut names: BTreeMap<Vec<u8>, DType> = base.into_iter().collect();
        for (name, kind) in overlay {
            match kind {
                Some(kind) => {
                    names.insert(name.clone(), *kind);
                }
                None => {
                    names.remove(name);
                }
            }
        }
        names.into_iter().collect()
    }

    struct Oracle {
        obs: Vec<(VPath, Obs)>,
        listings: Vec<Vec<(Vec<u8>, DType)>>,
    }

    fn oracle(view: &dyn View, blind: bool, accesses: &[Access]) -> Oracle {
        let mut seen = Seen {
            traced: BTreeMap::new(),
            resolved: BTreeSet::new(),
            listed: BTreeSet::new(),
            opened: BTreeSet::new(),
            overlay: BTreeMap::new(),
        };
        seen.traced.insert(b"/".to_vec(), view.traced(b"/"));
        let mut items = Vec::new();
        let mut listings = Vec::new();
        for access in accesses {
            match access {
                Access::Lookup { dir, name } => {
                    assert!(ensure(view, &mut seen, &mut items, dir), "lookup parent {}", show(dir));
                    note_lookup(view, &mut seen, &mut items, dir, name);
                }
                Access::ReadDir(dir) => {
                    assert!(ensure(view, &mut seen, &mut items, dir), "readdir {}", show(dir));
                    let first = seen.listed.insert(dir.clone());
                    if first && seen.traced[dir.as_slice()] {
                        let list = view.list(dir).expect("a directory lists");
                        items.push((dir.clone(), Expect::Settled(Obs::Listing(listing_hash(&list)))));
                    }
                    let list = view.list(dir).expect("a directory lists");
                    let overlay = seen.overlay.get(dir).map(Vec::as_slice).unwrap_or(&[]);
                    listings.push(independent_merge(list, overlay));
                }
                Access::Open { path } => {
                    let (parent, name) = vsplit(path).unwrap();
                    assert!(ensure(view, &mut seen, &mut items, parent));
                    note_lookup(view, &mut seen, &mut items, parent, name);
                    seen.opened.insert(path.clone());
                    mark_opened(&mut items, path);
                }
                Access::Read { path } => {
                    let (parent, name) = vsplit(path).unwrap();
                    assert!(ensure(view, &mut seen, &mut items, parent));
                    note_lookup(view, &mut seen, &mut items, parent, name);
                    if seen.opened.contains(path) {
                        mark_opened(&mut items, path);
                    }
                }
                Access::Unlink { dir, name } => {
                    assert!(ensure(view, &mut seen, &mut items, dir));
                    note_lookup(view, &mut seen, &mut items, dir, name);
                    seen.overlay.entry(dir.clone()).or_default().push((name.clone(), None));
                }
                Access::Mkdir { dir, name } => {
                    assert!(ensure(view, &mut seen, &mut items, dir));
                    note_lookup(view, &mut seen, &mut items, dir, name);
                    seen.overlay.entry(dir.clone()).or_default().push((name.clone(), Some(DType::Dir)));
                }
            }
        }
        let obs = items
            .into_iter()
            .map(|(path, item)| {
                let obs = match item {
                    Expect::Settled(obs) => obs,
                    Expect::File { size, exec, sem, read } => {
                        if read {
                            if let Some(hash) = sem {
                                Obs::File { hash, exec }
                            } else {
                                let bytes = view.read_bytes(&path).expect("a read file has bytes");
                                Obs::File { hash: crate::hash::Hash::of(bytes), exec }
                            }
                        } else if blind {
                            Obs::IsFile { exec }
                        } else {
                            Obs::Meta { size, exec }
                        }
                    }
                };
                (path.into(), obs)
            })
            .collect();
        Oracle { obs, listings }
    }

    fn resolve_file(fs: &ActionFs, st: &mut State, ids: &mut BTreeMap<Vec<u8>, u32>, path: &[u8]) -> u32 {
        let (parent, name) = vsplit(path).expect("a path has a parent");
        let parent_id = ensure_dir(fs, st, ids, parent).unwrap_or_else(|| panic!("parent {}", show(parent)));
        let id = fs.resolve(st, parent_id, name, true).expect("resolve").unwrap_or_else(|| panic!("present {}", show(path)));
        st.invariants();
        if matches!(st.nodes[id as usize], Kind::Dir(_)) {
            ids.insert(path.to_vec(), id);
        }
        id
    }

    fn ensure_dir(fs: &ActionFs, st: &mut State, ids: &mut BTreeMap<Vec<u8>, u32>, path: &[u8]) -> Option<u32> {
        if path == b"/" {
            return Some(ROOT_LOCAL);
        }
        if let Some(&id) = ids.get(path) {
            return Some(id);
        }
        let id = resolve_file(fs, st, ids, path);
        match &st.nodes[id as usize] {
            Kind::Dir(_) => Some(id),
            _ => None,
        }
    }

    fn drive(view: Arc<dyn View>, blind: bool, accesses: &[Access]) -> (Vec<(VPath, Obs)>, bool, Vec<Vec<(Vec<u8>, DType)>>) {
        let fs = ActionFs::in_memory(view.clone(), blind);
        let mut st = fs.st.lock();
        st.invariants();
        let mut ids = BTreeMap::new();
        let mut listings = Vec::new();
        for access in accesses {
            match access {
                Access::Lookup { dir, name } => {
                    let parent = ensure_dir(&fs, &mut st, &mut ids, dir).unwrap_or_else(|| panic!("lookup parent {}", show(dir)));
                    let id = fs.resolve(&mut st, parent, name, true).expect("resolve");
                    st.invariants();
                    if let Some(id) = id {
                        if matches!(st.nodes[id as usize], Kind::Dir(_)) {
                            ids.insert(vjoin(dir, name), id);
                        }
                    }
                }
                Access::ReadDir(dir) => {
                    let id = ensure_dir(&fs, &mut st, &mut ids, dir).unwrap_or_else(|| panic!("readdir {}", show(dir)));
                    let list = fs.merged_listing(&mut st, id).expect("list");
                    st.invariants();
                    listings.push(list.iter().map(|(name, kind)| (name.to_vec(), *kind)).collect());
                }
                Access::Open { path } => {
                    let id = resolve_file(&fs, &mut st, &mut ids, path);
                    let (parent, name) = vsplit(path).unwrap();
                    let Looked::File(info) = view.lookup(parent, name) else { panic!("open expects a file {}", show(path)) };
                    accept_read(&mut st, id, info.src);
                    st.invariants();
                }
                Access::Read { path } => {
                    let id = resolve_file(&fs, &mut st, &mut ids, path);
                    assert!(st.read_source(id).is_err(), "an in-memory file has no content handle");
                    note_read(&mut st, id);
                    st.invariants();
                }
                Access::Unlink { dir, name } => {
                    let parent = ensure_dir(&fs, &mut st, &mut ids, dir).unwrap_or_else(|| panic!("unlink parent {}", show(dir)));
                    fs.conceal(&mut st, parent, name).expect("unlink");
                    st.invariants();
                }
                Access::Mkdir { dir, name } => {
                    let parent = ensure_dir(&fs, &mut st, &mut ids, dir).unwrap_or_else(|| panic!("mkdir parent {}", show(dir)));
                    assert!(fs.resolve(&mut st, parent, name, true).expect("resolve").is_none(), "mkdir names a new entry");
                    st.invariants();
                    fs.do_mkdir(&mut st, parent, name, 0o755).expect("mkdir");
                    st.invariants();
                }
            }
        }
        while let Some(&index) = st.trace.pending_hashes().first() {
            let path = st.trace.path_of(index).to_vec();
            let bytes = view.read_bytes(&path).expect("a read file has bytes");
            st.trace.supply_hash(index, crate::hash::Hash::of(bytes));
            st.invariants();
        }
        let (obs, tainted) = st.trace.outcome();
        st.invariants();
        (obs, tainted, listings)
    }

    fn holds(view: &dyn View, items: &[(VPath, Obs)]) -> bool {
        let hash_of = |src: &FileSrc| match src {
            FileSrc::Blob(hash) => Some(*hash),
            FileSrc::Host { .. } | FileSrc::Lazy(_) => None,
        };
        items.iter().all(|(path, obs)| obs_holds(view, &hash_of, path, obs))
    }

    fn last<'a>(obs: &'a [(VPath, Obs)], path: &[u8]) -> &'a Obs {
        obs.iter().rev().find(|(got, _)| got.as_ref() == path).map(|(_, obs)| obs).expect("the path was observed")
    }

    #[test]
    fn recorded_observations_match_the_view_and_survive_pruning() {
        let mut saw_meta = false;
        let mut saw_blind = false;
        for seed in 0..24 {
            let built = generated(seed);
            if built.blind {
                saw_blind = true;
            } else {
                saw_meta = true;
            }
            let partial: Arc<dyn View> = Arc::new(Partial { inner: built.view.clone(), untraced: built.untraced.clone() });
            let expected = oracle(partial.as_ref(), built.blind, &built.accesses);
            let (obs, tainted, listings) = drive(partial, built.blind, &built.accesses);
            assert!(!tainted, "seed {seed}");
            assert_eq!(obs, expected.obs, "seed {seed}");
            assert_eq!(listings, expected.listings, "seed {seed}");
            assert!(obs.iter().all(|(path, _)| path_ok(path)));
            let plain_hash = crate::hash::Hash::of(built.view.read_bytes(&built.plain).unwrap());
            match last(&obs, &built.plain) {
                Obs::File { hash, exec: false } => assert_eq!(*hash, plain_hash),
                other => panic!("a read file records its content hash: {other:?}"),
            }
            match last(&obs, &built.semantic) {
                Obs::File { hash, exec: true } => assert_eq!(*hash, built.sem),
                other => panic!("a read semantic file records that id: {other:?}"),
            }
            match last(&obs, &built.idle) {
                Obs::Meta { .. } if !built.blind => {}
                Obs::IsFile { .. } if built.blind => {}
                other => panic!("an unread file stays metadata: {other:?}"),
            }
            match last(&obs, &built.idle_sem) {
                Obs::Meta { .. } if !built.blind => {}
                Obs::IsFile { .. } if built.blind => {}
                other => panic!("a read without an open does not record the content: {other:?}"),
            }
            assert!(matches!(last(&obs, &built.link), Obs::Symlink(_)));
            assert!(matches!(last(&obs, &built.other), Obs::Other));
            assert!(obs.iter().any(|(_, obs)| matches!(obs, Obs::Absent)));
            assert!(obs.iter().any(|(_, obs)| matches!(obs, Obs::Listing(_))));
            assert!(obs.iter().any(|(_, obs)| matches!(obs, Obs::Dir)));
            assert!(obs.iter().all(|(path, _)| path.as_ref() != built.hidden.as_slice()));
            assert!(obs.iter().any(|(path, obs)| path.as_ref() == built.hidden_dir.as_slice() && matches!(obs, Obs::Dir)));
            assert!(obs.iter().all(|(path, obs)| !(path.as_ref() == built.hidden_dir.as_slice() && matches!(obs, Obs::Listing(_)))));

            let readdirs: Vec<&Vec<u8>> = built
                .accesses
                .iter()
                .filter_map(|access| match access {
                    Access::ReadDir(dir) => Some(dir),
                    _ => None,
                })
                .collect();
            let final_dir = listings
                .iter()
                .zip(readdirs)
                .filter(|(_, dir)| dir.as_slice() == built.dir.as_slice())
                .map(|(list, _)| list)
                .last()
                .expect("the anchor directory is listed");
            assert!(final_dir.iter().all(|(name, _)| name != &built.cover_name));
            assert!(final_dir.iter().any(|(name, kind)| name == &built.made_name && *kind == DType::Dir));
            let lower = built.view.list(&built.dir).unwrap();
            match obs.iter().rev().find(|(path, obs)| path.as_ref() == built.dir.as_slice() && matches!(obs, Obs::Listing(_))) {
                Some((_, Obs::Listing(hash))) => assert_eq!(*hash, listing_hash(&lower)),
                _ => panic!("a traced directory records the lower listing"),
            }
            let merged: Listing = final_dir.iter().map(|(name, kind)| (name.clone(), *kind)).collect();
            assert_ne!(listing_hash(&merged), listing_hash(&lower));

            assert!(holds(&built.view, &obs));
            let compact = compact_obs(obs.clone());
            assert_eq!(holds(&built.view, &obs), holds(&built.view, &compact));
            assert!(obs.iter().any(|(path, obs)| path.as_ref() == built.dir.as_slice() && matches!(obs, Obs::Dir)));
            assert!(compact.iter().all(|(path, obs)| !(path.as_ref() == built.dir.as_slice() && matches!(obs, Obs::Dir))));

            let mut gone = built.view.clone();
            gone.remove_tree(&built.dir);
            assert!(!holds(&gone, &obs));
            assert_eq!(holds(&gone, &obs), holds(&gone, &compact));

            let mut replaced = built.view.clone();
            replaced.remove_tree(&built.dir);
            replaced.file(&built.dir, b"was-a-directory", false, None);
            assert!(!holds(&replaced, &obs));
            assert_eq!(holds(&replaced, &obs), holds(&replaced, &compact_obs(obs.clone())));

            let mut listed = built.view.clone();
            listed.file(&vjoin(&built.dir, b"zz"), b"added", false, None);
            assert!(!holds(&listed, &obs));
            assert_eq!(holds(&listed, &obs), holds(&listed, &compact));

            let mut hashed = built.view.clone();
            hashed.rewrite(&built.plain, b"rewritten-bytes-not-the-original");
            assert!(!holds(&hashed, &obs));
            assert_eq!(holds(&hashed, &obs), holds(&hashed, &compact));

            let mut poisoned = obs.clone();
            poisoned.push((built.plain.as_slice().into(), Obs::Symlink(b"nope".to_vec())));
            assert!(!holds(&built.view, &poisoned));
            assert_eq!(holds(&built.view, &poisoned), holds(&built.view, &compact_obs(poisoned)));
        }
        assert!(saw_meta && saw_blind);
    }

    #[test]
    fn a_failed_content_hash_taints_the_trace() {
        let mut mem = MemView::new();
        mem.file(b"/f", b"abc", false, None);
        let view: Arc<dyn View> = Arc::new(mem);
        let fs = ActionFs::in_memory(view.clone(), false);
        let mut st = fs.st.lock();
        st.invariants();
        let id = fs.resolve(&mut st, ROOT_LOCAL, b"f", true).expect("resolve").expect("file");
        st.invariants();
        let Looked::File(info) = view.lookup(b"/", b"f") else { panic!("file") };
        accept_read(&mut st, id, info.src);
        st.invariants();
        assert!(st.read_source(id).is_err());
        note_read(&mut st, id);
        st.invariants();
        let index = st.trace.pending_hashes()[0];
        st.trace.fail_hash(index);
        st.invariants();
        let (obs, tainted) = st.trace.outcome();
        let mut outcomes = Transcript::default();
        outcomes.record("tainted", tainted);
        for (path, observation) in obs {
            outcomes.record(&String::from_utf8_lossy(path.as_ref()), observation);
        }
        expect![[r#"
            tainted: true
            /f: File { hash: 0000000000000000, exec: false }
        "#]].assert_eq(&outcomes.0);
    }
}
