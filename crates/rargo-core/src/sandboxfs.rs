//! The sandbox filesystem: one FUSE mount, one top-level directory per action.
//!
//! Each action directory shows an immutable [`View`] with a private writable
//! upper layer on top. Every lookup, readlink, readdir and open that reaches
//! the view is recorded; together they are the action's trace. Whatever ends
//! up in the upper layer is the action's output.

use crate::view::*;
use dashmap::DashMap;
use fusewire::abi::*;
use fusewire::{Ctx, DirSink, Entry, Errno, Filesystem, Opened, Res, Session, SetAttr, TimeSet};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Timestamp shown for everything below the upper layer: 2000-01-01T00:00:00Z.
pub const LOWER_MTIME: u64 = 946_684_800;

const TTL: Duration = Duration::from_secs(365 * 86400);
const LOCAL_BITS: u32 = 24;
const LOCAL_MASK: u64 = (1 << LOCAL_BITS) - 1;
const _: () = assert!(LOCAL_BITS < 32, "a local id fits in u32");
const _: () = assert!(ROOT_LOCAL as u64 <= LOCAL_MASK);
const ROOT_LOCAL: u32 = 1;
const BACKING_CACHE_MAX: usize = 200_000;
/// How long a backing id stays registered after it was last handed out.
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
    /// Entry that exists in the view.
    Lower(u32),
    /// Entry created, moved or linked here by the action. The flag says
    /// whether the view also has an entry of this name underneath.
    Upper(u32, bool),
    Negative,
    Whiteout,
}

struct Dir {
    parent: u32,
    /// The view directory this node shows, if any.
    lower: Option<VPath>,
    /// Whether lookups in this directory are recorded.
    traced: bool,
    entries: HashMap<Box<[u8]>, Slot>,
    /// Something at or below this directory was modified.
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
    /// Opened through passthrough against the lower backing file.
    lower_pt_opens: u32,
    /// Source as it was when the content was first read (for the trace).
    read_src: Option<FileSrc>,
    /// Userspace I/O handle for lower files when passthrough is unavailable.
    file: Option<Arc<File>>,
    /// Size and executable bit of the view file this node started as. Kept
    /// after a copy-up: it is what the action observed.
    lower_meta: Option<(u64, bool)>,
    /// The view's own notion of this file's identity, if it has one.
    lower_sem: Option<crate::hash::Hash>,
}

enum Kind {
    Dir(Box<Dir>),
    File(Box<FileN>),
    Symlink { target: Vec<u8> },
    /// A directory that exists only to be mounted over.
    Mount,
    /// Fifo or socket created by the action. Never an output.
    Special { mode: u32, rdev: u32 },
}

enum RawObs {
    Absent,
    Other,
    Dir,
    Symlink(Vec<u8>),
    File(u32),
    Listing(crate::hash::Hash),
}

struct State {
    nodes: Vec<Kind>,
    obs: Vec<(VPath, RawObs)>,
    next_upper: u64,
    backing_ids: Vec<i32>,
    finished: bool,
}

/// Per-action filesystem state.
pub struct ActionFs {
    pub seq: u64,
    view: Arc<dyn View>,
    uid: u32,
    gid: u32,
    scratch: PathBuf,
    size_blind: bool,
    st: Mutex<State>,
}

pub struct ActionFsConfig {
    pub view: Arc<dyn View>,
    pub uid: u32,
    pub gid: u32,
    /// Directory on the store filesystem for this action's upper files.
    pub scratch: PathBuf,
    /// Top-level names that are bare mountpoints (`proc`, `dev`, ...).
    pub mountpoints: Vec<Vec<u8>>,
    /// Record files that were only looked up as "exists" rather than with
    /// their size. Only sound for actions known not to act on sizes.
    pub size_blind: bool,
}

/// One entry of an action's upper layer.
#[derive(Debug)]
pub enum UpperKind {
    Dir,
    /// A file written by the action; `path` is its scratch file.
    File { path: PathBuf, exec: bool },
    /// A view file that the action moved or linked to a new name.
    LowerFile { info: FileInfo },
    Symlink(Vec<u8>),
    /// The action deleted something the view has.
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
    /// A host file changed while the action was reading it; the trace must
    /// not be stored.
    pub tainted: bool,
}

pub struct SandboxFs {
    actions: DashMap<u64, Arc<ActionFs>>,
    next_seq: AtomicU64,
    content: Arc<Content>,
    session: OnceLock<Arc<Session>>,
    backing: DashMap<BackingKey, (i32, AtomicU64)>,
    /// Backing ids no longer in the map, waiting to be closed.
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

    /// Must be called once after mounting, before any action runs.
    pub fn attach(&self, session: Arc<Session>) {
        let _ = self.session.set(session);
    }

    pub fn stats(&self) -> &FsStats {
        &self.stats
    }

    fn passthrough(&self) -> bool {
        self.session.get().map(|s| s.caps().passthrough).unwrap_or(false)
    }

    /// Name of an action's directory under the mountpoint.
    pub fn dir_name(seq: u64) -> String {
        format!("a{seq}")
    }

    pub fn create_action(&self, cfg: ActionFsConfig) -> Arc<ActionFs> {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let mut nodes = Vec::with_capacity(256);
        nodes.push(Kind::Mount); // local id 0 is never used
        let root_path: VPath = Arc::from(&b"/"[..]);
        let mut root = Dir {
            parent: 0,
            traced: cfg.view.traced(&root_path),
            lower: Some(root_path),
            entries: HashMap::new(),
            dirty: false,
            owner: Owner { uid: 0, gid: 0, mode: 0o755 },
            lower_listed: false,
            listing: None,
            mtime: (LOWER_MTIME as i64, 0),
        };
        let mut mounts = Vec::new();
        for m in &cfg.mountpoints {
            mounts.push(m.clone());
        }
        nodes.push(Kind::Mount); // placeholder for root, replaced below
        for m in mounts {
            let id = nodes.len() as u32;
            nodes.push(Kind::Mount);
            assert!(!m.is_empty());
            assert!(!m.contains(&b'/'), "a mountpoint is one name in the action root");
            assert!(root.entries.insert(m.into_boxed_slice(), Slot::Upper(id, true)).is_none(), "duplicate mountpoint");
        }
        nodes[ROOT_LOCAL as usize] = Kind::Dir(Box::new(root));
        let a = Arc::new(ActionFs {
            seq,
            view: cfg.view,
            uid: cfg.uid,
            gid: cfg.gid,
            scratch: cfg.scratch,
            size_blind: cfg.size_blind,
            st: Mutex::new(State {
                nodes,
                obs: Vec::new(),
                next_upper: 0,
                backing_ids: Vec::new(),
                finished: false,
            }),
        });
        assert!(self.actions.insert(seq, a.clone()).is_none(), "action sequence numbers are unique");
        a
    }

    /// Ends an action: no further operations are served for it. Returns what
    /// it observed and what it wrote. Call only after all its processes are
    /// gone.
    pub fn finish_action(&self, a: &Arc<ActionFs>) -> Finished {
        self.actions.remove(&a.seq);
        if let Some(s) = self.session.get() {
            let _ = s.notify_inval_entry(FUSE_ROOT_ID, Self::dir_name(a.seq).as_bytes());
        }
        let mut st = a.st.lock();
        assert!(!st.finished, "action finished twice");
        st.finished = true;
        // Upper files were only ever opened by this action's processes, all
        // of which are gone, so no reply can still refer to these ids. They
        // are closed right away: the kernel's reference keeps the scratch
        // file open for writing, and an output must not stay "text file
        // busy" after its action has finished.
        if let Some(s) = self.session.get() {
            for id in st.backing_ids.drain(..) {
                let _ = s.backing_close(id);
            }
        }
        self.reap_retired();
        let mut upper = Vec::new();
        walk_upper(&st, &*a.view, ROOT_LOCAL, b"/", false, &mut upper);

        // Resolve raw observations. Hashing happens outside the lock.
        let raw = std::mem::take(&mut st.obs);
        let mut out: Vec<(VPath, Obs)> = Vec::with_capacity(raw.len());
        let mut to_hash: Vec<(usize, FileSrc, bool)> = Vec::new();
        for (path, r) in raw {
            let obs = match r {
                RawObs::Absent => Obs::Absent,
                RawObs::Other => Obs::Other,
                RawObs::Dir => Obs::Dir,
                RawObs::Symlink(t) => Obs::Symlink(t),
                RawObs::Listing(h) => Obs::Listing(h),
                RawObs::File(n) => {
                    let Kind::File(f) = &st.nodes[n as usize] else { continue };
                    // What was observed is the view file the node started
                    // as, even if the action later replaced its content.
                    let Some((size, exec)) = f.lower_meta else { continue };
                    match &f.read_src {
                        Some(_) if f.lower_sem.is_some() => Obs::File { hash: f.lower_sem.unwrap(), exec },
                        Some(src) => {
                            to_hash.push((out.len(), src.clone(), exec));
                            Obs::File { hash: crate::hash::Hash::ZERO, exec }
                        }
                        None if a.size_blind => Obs::IsFile { exec },
                        None => Obs::Meta { size, exec },
                    }
                }
            };
            out.push((path, obs));
        }
        drop(st);

        let mut tainted = false;
        use rayon::prelude::*;
        let hashed: Vec<(usize, std::io::Result<crate::hash::Hash>, bool)> = to_hash
            .into_par_iter()
            .map(|(i, src, exec)| (i, self.content.hash(&src), exec))
            .collect();
        for (i, h, exec) in hashed {
            match h {
                Ok(hash) => {
                    assert!(matches!(out[i].1, Obs::File { hash: crate::hash::Hash::ZERO, .. }));
                    out[i].1 = Obs::File { hash, exec }
                }
                Err(_) => tainted = true,
            }
        }
        Finished { obs: compact_obs(out), upper, tainted }
    }

    /// The passthrough backing id for an immutable backing file, registering
    /// it with the kernel on first use.
    ///
    /// Ids are small integers the kernel reuses as soon as they are closed,
    /// and the kernel resolves the id in an open reply only after our write
    /// of that reply has returned. An id must therefore never be closed
    /// while a reply carrying it may still be in flight: registration
    /// happens exactly once per key (under the map's entry lock), and ids
    /// taken out of the map are closed only after a grace period.
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
        self.retired.lock().push((std::time::Instant::now(), id));
    }

    /// Closes retired ids whose grace period is over.
    fn reap_retired(&self) {
        let Some(sess) = self.session.get() else { return };
        let mut r = self.retired.lock();
        if r.is_empty() {
            return;
        }
        let now = std::time::Instant::now();
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

    /// Drops the cached backing file of a blob (call before evicting it, so
    /// the kernel does not keep the unlinked file alive).
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

/// Drops directory observations implied by a deeper positive observation.
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

/// Collects the upper layer below `dir`. With `full`, the directory was moved
/// or linked to a new place and its view content has to be emitted too.
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
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
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

    /// Resolves `name` in directory `parent`, consulting the view (and
    /// recording the observation when `trace`) the first time.
    fn resolve(&self, st: &mut State, parent: u32, name: &[u8], trace: bool) -> Res<Option<u32>> {
        let (lower, traced) = match &st.nodes[parent as usize] {
            Kind::Dir(d) => {
                if let Some(slot) = d.entries.get(name) {
                    return Ok(match slot {
                        Slot::Lower(n) | Slot::Upper(n, _) => Some(*n),
                        Slot::Negative | Slot::Whiteout => None,
                    });
                }
                match &d.lower {
                    Some(l) => (l.clone(), d.traced && trace),
                    None => return Ok(None),
                }
            }
            Kind::Mount => return Ok(None),
            _ => return Err(Errno::ENOTDIR),
        };
        let looked = self.view.lookup(&lower, name);
        let path: VPath = vjoin(&lower, name).into();
        let (slot, raw) = match looked {
            Looked::Absent => (Slot::Negative, RawObs::Absent),
            Looked::Other => (Slot::Negative, RawObs::Other),
            Looked::Dir(owner) => {
                let child_traced = self.view.traced(&path);
                let n = self.push(
                    st,
                    Kind::Dir(Box::new(Dir {
                        parent,
                        lower: Some(path.clone()),
                        traced: child_traced,
                        entries: HashMap::new(),
                        dirty: false,
                        owner,
                        lower_listed: false,
                        listing: None,
                        mtime: (LOWER_MTIME as i64, 0),
                    })),
                )?;
                (Slot::Lower(n), RawObs::Dir)
            }
            Looked::Symlink(t) => {
                let n = self.push(st, Kind::Symlink { target: t.clone() })?;
                (Slot::Lower(n), RawObs::Symlink(t))
            }
            Looked::File(info) => {
                let mode = info.owner.mode;
                let lower_meta = Some((info.size, info.exec()));
                let lower_sem = info.sem;
                let n = self.push(
                    st,
                    Kind::File(Box::new(FileN {
                        parent,
                        src: Src::Lower(info),
                        mode,
                        opens: 0,
                        links: 1,
                        lower_pt_opens: 0,
                        read_src: None,
                        file: None,
                        lower_meta,
                        lower_sem,
                    })),
                )?;
                (Slot::Lower(n), RawObs::File(n))
            }
        };
        if traced {
            st.obs.push((path, raw));
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
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o644)
            .custom_flags(libc::O_CLOEXEC)
            .open(&path)?;
        Ok((path, Arc::new(file)))
    }

    /// Whether the view has an entry under the slot that `name` currently
    /// maps to (so that removing the upper entry must leave a whiteout).
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
        let mut names: HashMap<Box<[u8]>, DType> = HashMap::new();
        if let Some(lower) = &lower {
            let list = self.view.list(lower).map_err(Errno::from)?;
            if need_obs {
                st.obs.push((lower.clone(), RawObs::Listing(listing_hash(&list))));
            }
            for (n, t) in list {
                names.insert(n.into_boxed_slice(), t);
            }
        }
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
        for (name, t) in upper {
            match t {
                None => {
                    names.remove(&name);
                }
                Some(t) => {
                    names.insert(name, t);
                }
            }
        }
        let mut v: Vec<(Box<[u8]>, DType)> = names.into_iter().collect();
        v.sort();
        assert!(v.windows(2).all(|w| w[0].0 < w[1].0), "listing names are unique");
        let v = Arc::new(v);
        let d = self.dir_mut(st, local)?;
        d.lower_listed = true;
        d.listing = Some(v.clone());
        Ok(v)
    }

    fn is_empty_dir(&self, st: &mut State, local: u32) -> Res<bool> {
        Ok(self.merged_listing(st, local)?.is_empty())
    }

    /// Creates the directory `path` and any missing parents in the upper
    /// layer without recording observations. Used by the executor for
    /// directories cargo would have created before running the action.
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

    /// Scratch path of the upper file at sandbox path `path`, if the action
    /// has written one there.
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
        let Kind::File(f) = &mut st.nodes[n as usize] else { unreachable!("checked to be a file above") };
        {
            assert!(matches!(f.src, Src::Lower(_)));
            if f.read_src.is_none() {
                f.read_src = read_src;
            }
            f.src = Src::Upper { path, file, backing: None };
            f.file = None;
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
        // Either no passthrough, or the inode is still open against its old
        // lower backing file; the kernel does not allow two backing files on
        // one inode, so this handle does direct I/O through us.
        Ok(Opened { fh: 0, flags: if lower_pt { FOPEN_DIRECT_IO } else { 0 }, backing_id: 0 })
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
                    // A mode change on a view file is an output (exec bit).
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
                        let ts = [conv(set.atime), conv(set.mtime)];
                        // SAFETY: valid fd and a two-element timespec array.
                        unsafe { libc::futimens(file.as_raw_fd(), ts.as_ptr()) };
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
                        lower_meta: None,
                        lower_sem: None,
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
        let Some(n) = a.resolve(&mut st, p, name, true)? else { return Err(Errno::ENOENT) };
        if matches!(st.nodes[n as usize], Kind::Dir(_) | Kind::Mount) {
            return Err(Errno::EISDIR);
        }
        let covers = a.covers_lower(&st, p, name);
        let d = a.dir_mut(&mut st, p)?;
        if covers {
            d.entries.insert(name.into(), Slot::Whiteout);
        } else {
            d.entries.remove(name);
        }
        if let Kind::File(f) = &mut st.nodes[n as usize] {
            assert!(f.links > 0, "unlinking a file with no links");
            f.links = f.links.saturating_sub(1);
            if f.links == 0 && f.opens == 0 {
                if let Src::Upper { path, .. } = &f.src {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
        a.mark_dirty(&mut st, p);
        Ok(())
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
            let Kind::File(f) = &mut st.nodes[n as usize] else { unreachable!() };
            let Src::Lower(info) = &mut f.src else { unreachable!() };
            // Fast path: the backing file is already registered with the
            // kernel under the identity the lookup saw. No syscalls at all.
            if self.passthrough() {
                let key = match &info.src {
                    FileSrc::Blob(h) => Some(BackingKey::Blob(*h)),
                    FileSrc::Host { sig, .. } => Some(BackingKey::Host(*sig)),
                    FileSrc::Lazy(_) => {
                        self.content.known_hash(&info.src).map(BackingKey::Blob)
                    }
                };
                if let Some(e) = key.and_then(|k| self.backing.get(&k)) {
                    e.1.store(self.tick.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
                    if f.read_src.is_none() {
                        f.read_src = Some(info.src.clone());
                    }
                    f.opens += 1;
                    f.lower_pt_opens += 1;
                    return Ok(Opened { fh: 1, flags: FOPEN_PASSTHROUGH, backing_id: e.0 });
                }
            }
            let (file, key, actual) = self.content.open(&info.src)?;
            if f.read_src.is_none() {
                f.read_src = Some(actual.clone());
            }
            info.src = actual;
            f.opens += 1;
            if self.passthrough() {
                let id = self.backing_id(key, &file)?;
                f.lower_pt_opens += 1;
                return Ok(Opened { fh: 1, flags: FOPEN_PASSTHROUGH, backing_id: id });
            }
            if f.file.is_none() {
                f.file = Some(Arc::new(file));
            }
            return Ok(Opened { fh: 0, flags: FOPEN_KEEP_CACHE, backing_id: 0 });
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
                lower_meta: None,
                lower_sem: None,
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
        let file = {
            let st = a.st.lock();
            match st.nodes.get(n as usize) {
                Some(Kind::File(f)) => match &f.src {
                    Src::Upper { file, .. } => file.clone(),
                    Src::Lower(_) => f.file.clone().ok_or(Errno::EBADF)?,
                },
                _ => return Err(Errno::EBADF),
            }
        };
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
            // Inode numbers in readdir are advisory; they only must be nonzero.
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
                // SAFETY: valid fd.
                let r = unsafe {
                    libc::fallocate(file.as_raw_fd(), mode as i32, offset as i64, length as i64)
                };
                return if r == 0 { Ok(()) } else { Err(Errno::last()) };
            }
        }
        Err(Errno::EBADF)
    }
}
