use fusewire::abi::*;
use fusewire::{Ctx, DirSink, Entry, Errno, Filesystem, Opened, Res, Session, SetAttr, TimeSet};
use crate::partial::Partial;
use rargo_proto::Digest;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

const TTL: Duration = Duration::from_secs(3600);
const ROOT: u64 = 1;
const OPS: [&str; 20] = ["lookup", "forget", "getattr", "setattr", "readlink", "symlink", "mknod", "mkdir", "unlink", "rmdir", "rename", "link", "open", "create", "read", "write", "release", "fsync", "readdir", "statfs"];

enum Patch {
    Add(Vec<u8>, u32),
    Del(Vec<u8>),
}

#[derive(Default)]
struct DirState {
    live: Option<BTreeMap<u64, (Vec<u8>, u32)>>,
    building: Option<Vec<Patch>>,
}

struct Timed<'a>(&'a OutFs, usize, std::time::Instant);

impl Drop for Timed<'_> {
    fn drop(&mut self) {
        self.0.op_count[self.1].fetch_add(1, Ordering::Relaxed);
        self.0.op_nanos[self.1].fetch_add(self.2.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }
}
const LISTING_KEPT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VFile {
    pub hash: Digest,
    pub size: u64,
    pub exec: bool,
    pub mtime_ns: i64,
}

struct Node {
    parent: u64,
    name: Vec<u8>,
    nlookup: u64,
    kids: u64,
    cached: u32,
    backing: Option<(i32, u32, bool)>,
}

struct Inodes {
    nodes: HashMap<u64, Node>,
    children: HashMap<(u64, Vec<u8>), u64>,
    next: u64,
}

impl Inodes {
    #[cfg(test)]
    fn invariants(&self) -> bool {
        self.nodes.contains_key(&ROOT) && self.children.iter().all(|((parent, name), id)| {
            self.nodes.get(id).is_some_and(|node| node.parent == *parent && node.name == *name)
        })
    }

    fn path(&self, mut id: u64) -> Res<Vec<u8>> {
        let mut parts: Vec<&[u8]> = Vec::new();
        while id != ROOT {
            let n = self.nodes.get(&id).ok_or(Errno(libc::ESTALE))?;
            assert!(!n.name.is_empty());
            assert_ne!(n.parent, id);
            parts.push(&n.name);
            id = n.parent;
        }
        parts.reverse();
        Ok(parts.join(&b'/'))
    }

    fn lookup(&mut self, parent: u64, name: &[u8]) -> u64 {
        assert!(!name.is_empty());
        if let Some(id) = self.children.get(&(parent, name.to_vec())) {
            let id = *id;
            assert!(self.nodes.contains_key(&id), "a name leads only to a known node");
            if let Some(n) = self.nodes.get_mut(&id) {
                assert_eq!(n.parent, parent);
                n.nlookup += 1;
            }
            return id;
        }
        let id = self.next;
        assert_ne!(id, ROOT);
        self.next += 1;
        assert!(self.nodes.insert(id, Node { parent, name: name.to_vec(), nlookup: 1, kids: 0, cached: 0, backing: None }).is_none());
        assert!(self.children.insert((parent, name.to_vec()), id).is_none());
        if let Some(p) = self.nodes.get_mut(&parent) {
            p.kids += 1;
        }
        id
    }

    fn forget(&mut self, nodeid: u64, nlookup: u64) -> Vec<u64> {
        let mut gone: Vec<u64> = Vec::new();
        let mut id = nodeid;
        let mut drop_by = nlookup;
        while id != ROOT {
            let Some(n) = self.nodes.get_mut(&id) else { break };
            n.nlookup = n.nlookup.saturating_sub(drop_by);
            if n.nlookup > 0 || n.kids > 0 {
                break;
            }
            let (parent, name) = (n.parent, n.name.clone());
            self.nodes.remove(&id);
            gone.push(id);
            if self.children.get(&(parent, name.clone())) == Some(&id) {
                self.children.remove(&(parent, name));
            }
            if let Some(p) = self.nodes.get_mut(&parent) {
                assert!(p.kids > 0, "a parent counts the node that leaves it");
                p.kids -= 1;
            }
            id = parent;
            drop_by = 0;
        }
        gone
    }

    fn changed(&mut self, rel: &[u8]) -> (u64, Vec<u8>) {
        let mut parent = ROOT;
        let parts: Vec<_> = rel.split(|&b| b == b'/').collect();
        for (i, &part) in parts.iter().enumerate() {
            if i + 1 == parts.len() {
                self.children.remove(&(parent, part.to_vec()));
                return (parent, part.to_vec());
            }
            match self.children.get(&(parent, part.to_vec())) {
                Some(id) => parent = *id,
                None => return (parent, part.to_vec()),
            }
        }
        unreachable!("a path has a component")
    }
}

type Table = BTreeMap<Vec<u8>, VFile>;

enum Edit {
    Put { path: Vec<u8>, file: VFile, replaced_real: bool },
    Remove(Vec<u8>),
    Shadow(Vec<u8>),
}

struct OutputTree {
    files: Table,
    generation: u64,
}

impl OutputTree {
    fn same(&self, rel: &[u8], file: &VFile) -> bool {
        self.files.get(rel).is_some_and(|v| v.hash == file.hash && v.exec == file.exec)
    }

    fn has_below(&self, rel: &[u8]) -> bool {
        let prefix = below(rel);
        self.files.range(prefix.clone()..).next().is_some_and(|(k, _)| k.starts_with(&prefix))
    }

    fn lookup<T>(&self, rel: &[u8], real: Option<T>) -> Option<Kind<T>> {
        if let Some(real) = real {
            return Some(Kind::Real(real));
        }
        if rel.is_empty() {
            return None;
        }
        if let Some(file) = self.files.get(rel) {
            return Some(Kind::Virtual(file.clone()));
        }
        self.has_below(rel).then_some(Kind::VDir)
    }

    fn apply(&mut self, generation: u64, edits: Vec<Edit>) -> Vec<Vec<u8>> {
        if generation <= self.generation {
            return Vec::new();
        }
        self.generation = generation;
        let mut changed = Vec::new();
        for edit in edits {
            let path = match edit {
                Edit::Put { path, file, replaced_real } => {
                    if !replaced_real && self.same(&path, &file) {
                        continue;
                    }
                    self.files.insert(path.clone(), file);
                    path
                }
                Edit::Remove(path) => {
                    let prefix = below(&path);
                    self.files.remove(&path);
                    let removed: Vec<_> = self.files.range(prefix.clone()..)
                        .take_while(|(key, _)| key.starts_with(&prefix)).map(|(key, _)| key.clone()).collect();
                    for key in removed {
                        self.files.remove(&key);
                    }
                    path
                }
                Edit::Shadow(path) => {
                    self.files.remove(&path);
                    path
                }
            };
            changed.push(path);
        }
        changed
    }

    #[cfg(test)]
    fn invariants(&self) -> bool {
        self.files.keys().all(|path| !path.is_empty() && !path.starts_with(b"/") && !path.ends_with(b"/"))
    }
}

pub struct Delivery<'a> {
    fs: &'a OutFs,
    _serial: MutexGuard<'a, ()>,
    generation: u64,
    edits: Vec<Edit>,
}

impl Delivery<'_> {
    fn remove_real(&self, rel: &[u8]) -> bool {
        let real = self.fs.real(rel);
        match std::fs::symlink_metadata(&real) {
            Ok(md) if md.is_dir() => {
                let _ = std::fs::remove_dir_all(&real);
            }
            Ok(_) => {
                let _ = std::fs::remove_file(&real);
            }
            Err(_) => return false,
        }
        true
    }

    pub fn put(&mut self, rel: &[u8], file: VFile) {
        self.fs.sweep_beside(rel);
        let replaced_real = self.remove_real(rel);
        self.edits.push(Edit::Put { path: rel.to_vec(), file, replaced_real });
    }

    pub fn remove(&mut self, rel: &[u8]) {
        self.remove_real(rel);
        self.edits.push(Edit::Remove(rel.to_vec()));
    }

    pub fn wrote(&mut self, rel: &[u8]) {
        self.edits.push(Edit::Shadow(rel.to_vec()));
    }
}

impl Drop for Delivery<'_> {
    fn drop(&mut self) {
        let invalidations = {
            let mut tree = self.fs.virt.write().unwrap();
            let changed = tree.apply(self.generation, std::mem::take(&mut self.edits));
            if !changed.is_empty() {
                self.fs.dirty.store(true, Ordering::SeqCst);
            }
            let mut inodes = self.fs.state.lock().unwrap();
            changed.iter().map(|path| inodes.changed(path)).collect::<Vec<_>>()
        };
        for (parent, name) in invalidations {
            self.fs.unlist(parent);
            self.fs.inval(parent, &name);
        }
    }
}

struct Handle {
    file: Content,
    node: u64,
    through: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Fetch {
    hash: Digest,
    size: u64,
    rel: Vec<u8>,
}

enum Content<F = File, R = Arc<Partial>> {
    Open(F),
    Unfetched(Fetch),
    Ranged(R),
}

enum ReadPlan<'a, F, R> {
    Whole(&'a F),
    Ranged(&'a R),
    Fetch(Fetch),
}

impl<F, R> Content<F, R> {
    fn read(&self) -> ReadPlan<'_, F, R> {
        match self {
            Self::Open(file) => ReadPlan::Whole(file),
            Self::Ranged(reader) => ReadPlan::Ranged(reader),
            Self::Unfetched(fetch) => ReadPlan::Fetch(fetch.clone()),
        }
    }

    fn fetched(&mut self, request: &Fetch, content: Self) {
        if matches!(self, Self::Unfetched(expected) if expected == request) {
            *self = content;
        }
    }
}

enum VirtualOpen {
    Empty,
    Read(VFile),
    Materialize { file: VFile, with_content: bool },
}

impl OutputTree {
    fn open(&self, rel: &[u8], writing: bool, truncate: bool) -> Res<VirtualOpen> {
        let file = self.files.get(rel).cloned().ok_or(Errno(libc::ENOENT))?;
        Ok(if writing {
            VirtualOpen::Materialize { file, with_content: !truncate }
        } else if file.size == 0 {
            VirtualOpen::Empty
        } else {
            VirtualOpen::Read(file)
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum How {
    Daemon,
    Real,
    Content,
}

pub enum Reader {
    Whole(File),
    Ranged(Arc<Partial>),
}

pub trait Source: Send + Sync {
    fn cached(&self, hash: &Digest, size: u64) -> Option<PathBuf>;
    fn whole(&self, hash: &Digest, size: u64) -> std::io::Result<PathBuf>;
    fn reader(&self, hash: &Digest, size: u64) -> std::io::Result<Reader>;
}

pub struct OutFs {
    under: PathBuf,
    _under_fd: File,
    state: Mutex<Inodes>,
    virt: std::sync::RwLock<OutputTree>,
    delivery: Mutex<()>,
    handles: Mutex<HashMap<u64, Handle>>,
    next_fh: AtomicU64,
    source: Box<dyn Source>,
    session: OnceLock<Arc<Session>>,
    table: PathBuf,
    dirty: AtomicBool,
    dirs: Mutex<HashMap<u64, DirState>>,
    listings: Mutex<HashMap<u64, (std::time::Instant, Arc<Vec<(u64, Vec<u8>, u32)>>)>>,
    swept: Mutex<std::collections::HashSet<Vec<u8>>>,
    uid: u32,
    gid: u32,
    op_count: [AtomicU64; OPS.len()],
    op_nanos: [AtomicU64; OPS.len()],
    pub opened_virtual: AtomicU64,
    pub made_real: AtomicU64,
}

fn errno(e: std::io::Error) -> Errno {
    Errno(e.raw_os_error().unwrap_or(libc::EIO))
}

fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    assert!(!name.is_empty());
    assert!(!name.contains(&b'/'), "a name is one component");
    if dir.is_empty() {
        return name.to_vec();
    }
    let mut p = dir.to_vec();
    p.push(b'/');
    p.extend_from_slice(name);
    p
}

fn below(prefix: &[u8]) -> Vec<u8> {
    let mut p = prefix.to_vec();
    p.push(b'/');
    p
}

fn name_cookie(name: &[u8]) -> u64 {
    let h = blake3::hash(name);
    let c = (u64::from_le_bytes(h.as_bytes()[..8].try_into().unwrap()) >> 1).max(1);
    assert!(c >= 1);
    assert!(c < 1 << 63);
    c
}

enum Kind<T = std::fs::Metadata> {
    Real(T),
    Virtual(VFile),
    VDir,
}

impl OutFs {
    pub fn new(dir: &Path, table: &Path, source: Box<dyn Source>) -> std::io::Result<Arc<OutFs>> {
        let fd = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_PATH).open(dir)?;
        let under = PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd()));
        let virt: Table = std::fs::read(table)
            .ok()
            .and_then(|raw| zstd::stream::decode_all(&raw[..]).ok())
            .and_then(|plain| bincode::deserialize(&plain).ok())
            .unwrap_or_default();
        let mut nodes = HashMap::new();
        nodes.insert(ROOT, Node { parent: 0, name: Vec::new(), nlookup: 1, kids: 0, cached: 0, backing: None });
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        Ok(Arc::new(OutFs {
            under,
            _under_fd: fd,
            state: Mutex::new(Inodes { nodes, children: HashMap::new(), next: 2 }),
            virt: std::sync::RwLock::new(OutputTree { files: virt, generation: 0 }),
            delivery: Mutex::new(()),
            handles: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
            source,
            session: OnceLock::new(),
            table: table.to_path_buf(),
            dirty: AtomicBool::new(false),
            dirs: Mutex::new(HashMap::new()),
            listings: Mutex::new(HashMap::new()),
            swept: Mutex::new(Default::default()),
            uid,
            gid,
            op_count: std::array::from_fn(|_| AtomicU64::new(0)),
            op_nanos: std::array::from_fn(|_| AtomicU64::new(0)),
            opened_virtual: AtomicU64::new(0),
            made_real: AtomicU64::new(0),
        }))
    }

    fn timed(&self, op: &str) -> Timed<'_> {
        Timed(self, OPS.iter().position(|o| *o == op).unwrap_or(0), std::time::Instant::now())
    }

    pub fn op_report(&self) -> String {
        let mut rows: Vec<(u64, u64, &str)> = OPS
            .iter()
            .enumerate()
            .map(|(i, name)| (self.op_nanos[i].load(Ordering::Relaxed), self.op_count[i].load(Ordering::Relaxed), *name))
            .filter(|r| r.1 > 0)
            .collect();
        rows.sort_by(|a, b| b.0.cmp(&a.0));
        rows.iter().map(|(ns, n, name)| format!("{name} {n} ({:.2} s)", *ns as f64 / 1e9)).collect::<Vec<_>>().join(", ")
    }

    pub fn attach(&self, session: Arc<Session>) {
        let _ = self.session.set(session);
    }

    fn give_back_if_empty(&self, parent: u64) {
        if parent != ROOT || !self.virt.read().unwrap().files.is_empty() {
            return;
        }
        if std::fs::read_dir(&self.under).map(|mut d| d.next().is_some()).unwrap_or(true) {
            return;
        }
        let _ = std::fs::remove_file(&self.table);
        let _ = std::fs::remove_file(self.table.with_file_name("delivered"));
        self.dirty.store(false, Ordering::SeqCst);
        if let Some(session) = self.session.get() {
            session.unmount();
        }
    }

    pub fn save(&self) {
        if !self.dirty.swap(false, Ordering::SeqCst) {
            return;
        }
        let plain = bincode::serialize(&self.virt.read().unwrap().files).unwrap_or_default();
        if let Ok(packed) = zstd::stream::encode_all(&plain[..], 1) {
            let _ = crate::write_atomic(&self.table, &packed);
        }
    }

    pub fn virtual_count(&self) -> usize {
        self.virt.read().unwrap().files.len()
    }

    fn real(&self, rel: &[u8]) -> PathBuf {
        if rel.is_empty() { self.under.join(".") } else { self.under.join(std::ffi::OsStr::from_bytes(rel)) }
    }

    fn kind(&self, rel: &[u8]) -> Option<Kind> {
        let real = std::fs::symlink_metadata(self.real(rel)).ok();
        self.virt.read().unwrap().lookup(rel, real)
    }

    fn path(&self, id: u64) -> Res<Vec<u8>> {
        self.state.lock().unwrap().path(id)
    }

    fn has_virtual_below(&self, rel: &[u8]) -> bool {
        self.virt.read().unwrap().has_below(rel)
    }

    fn attr(&self, id: u64, kind: &Kind) -> fuse_attr {
        match kind {
            Kind::Real(md) => fuse_attr {
                ino: id,
                size: md.size(),
                blocks: md.blocks(),
                atime: md.atime() as u64,
                mtime: md.mtime() as u64,
                ctime: md.ctime() as u64,
                atimensec: md.atime_nsec() as u32,
                mtimensec: md.mtime_nsec() as u32,
                ctimensec: md.ctime_nsec() as u32,
                mode: md.mode(),
                nlink: md.nlink() as u32,
                uid: md.uid(),
                gid: md.gid(),
                rdev: md.rdev() as u32,
                blksize: 4096,
                flags: 0,
            },
            Kind::Virtual(v) => {
                let (s, ns) = (v.mtime_ns.div_euclid(1_000_000_000) as u64, v.mtime_ns.rem_euclid(1_000_000_000) as u32);
                fuse_attr {
                    ino: id,
                    size: v.size,
                    blocks: v.size.div_ceil(512),
                    atime: s,
                    mtime: s,
                    ctime: s,
                    atimensec: ns,
                    mtimensec: ns,
                    ctimensec: ns,
                    mode: libc::S_IFREG | if v.exec { 0o755 } else { 0o644 },
                    nlink: 1,
                    uid: self.uid,
                    gid: self.gid,
                    rdev: 0,
                    blksize: 4096,
                    flags: 0,
                }
            }
            Kind::VDir => fuse_attr {
                ino: id,
                size: 4096,
                blocks: 8,
                mode: libc::S_IFDIR | 0o755,
                nlink: 2,
                uid: self.uid,
                gid: self.gid,
                blksize: 4096,
                ..fuse_attr::default()
            },
        }
    }

    fn detach(&self, parent: u64, name: &[u8]) {
        self.state.lock().unwrap().children.remove(&(parent, name.to_vec()));
    }

    fn entry(&self, parent: u64, name: &[u8], kind: &Kind) -> Entry {
        let id = self.state.lock().unwrap().lookup(parent, name);
        Entry { nodeid: id, generation: 0, attr: self.attr(id, kind), entry_ttl: TTL, attr_ttl: TTL }
    }

    fn make_parents(&self, rel: &[u8]) -> Res<()> {
        if let Some(i) = rel.iter().rposition(|&b| b == b'/') {
            std::fs::create_dir_all(self.real(&rel[..i])).map_err(errno)?;
        }
        Ok(())
    }

    fn make_real(&self, rel: &[u8], v: &VFile, with_content: bool) -> Res<()> {
        self.make_parents(rel)?;
        let dst = self.real(rel);
        let mode = if v.exec { 0o755 } else { 0o644 };
        if with_content && v.size > 0 {
            let src = self.source.whole(&v.hash, v.size).map_err(errno)?;
            let tmp = self.real(&join(b"", format!(".rargo-real-{}-{}", std::process::id(), self.next_fh.fetch_add(1, Ordering::Relaxed)).as_bytes()));
            std::fs::copy(&src, &tmp).map_err(errno)?;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)).map_err(errno)?;
            std::fs::rename(&tmp, &dst).map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                errno(e)
            })?;
        } else {
            std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(mode).open(&dst).map_err(errno)?;
        }
        self.virt.write().unwrap().files.remove(rel);
        self.dirty.store(true, Ordering::SeqCst);
        self.made_real.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn open_handle(&self, node: u64, file: Content, how: How) -> Res<Opened> {
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        let mut st = self.state.lock().unwrap();
        let mut backing_id = None;
        let content = how == How::Content;
        if let (Some(n), true, Some(s), Content::Open(file)) = (st.nodes.get_mut(&node), how != How::Daemon, self.session.get(), &file) {
            match &mut n.backing {
                Some((_, _, c)) if *c != content => return Err(Errno(libc::ETXTBSY)),
                Some((id, refs, _)) if n.cached == 0 => {
                    *refs += 1;
                    backing_id = Some(*id);
                }
                None if n.cached == 0 && s.caps().passthrough => {
                    if let Ok(id) = s.backing_open(file.as_raw_fd()) {
                        n.backing = Some((id, 1, content));
                        backing_id = Some(id);
                    }
                }
                _ => {}
            }
        }
        if backing_id.is_none() {
            if let Some(n) = st.nodes.get_mut(&node) {
                n.cached += 1;
            }
        }
        drop(st);
        self.handles.lock().unwrap().insert(fh, Handle { file, node, through: backing_id.is_some() });
        Ok(match backing_id {
            Some(id) => Opened { fh, flags: FOPEN_PASSTHROUGH, backing_id: id },
            None => Opened { fh, flags: FOPEN_KEEP_CACHE, backing_id: 0 },
        })
    }

    fn handle_file(&self, fh: u64) -> Res<Option<File>> {
        match &self.handles.lock().unwrap().get(&fh).ok_or(Errno(libc::EBADF))?.file {
            Content::Open(file) => Ok(Some(file.try_clone().map_err(errno)?)),
            Content::Unfetched(_) | Content::Ranged(_) => Ok(None),
        }
    }

    fn handle_reader(&self, fh: u64) -> Res<Reader> {
        let request = match self.handles.lock().unwrap().get(&fh).ok_or(Errno(libc::EBADF))?.file.read() {
            ReadPlan::Whole(file) => return Ok(Reader::Whole(file.try_clone().map_err(errno)?)),
            ReadPlan::Ranged(reader) => return Ok(Reader::Ranged(reader.clone())),
            ReadPlan::Fetch(request) => request,
        };
        let reader = self.source.reader(&request.hash, request.size).map_err(|e| {
            eprintln!("rargo daemon: target/{}: its content could not be fetched: {e}", String::from_utf8_lossy(&request.rel));
            errno(e)
        })?;
        if let Some(handle) = self.handles.lock().unwrap().get_mut(&fh) {
            let content = match &reader {
                Reader::Whole(file) => Content::Open(file.try_clone().map_err(errno)?),
                Reader::Ranged(reader) => Content::Ranged(reader.clone()),
            };
            handle.file.fetched(&request, content);
        }
        Ok(reader)
    }

    fn open_wide(path: &Path, acc: i32, extra: i32, create: Option<u32>) -> std::io::Result<File> {
        let mut wide = std::fs::OpenOptions::new();
        wide.read(true).write(true).custom_flags(extra);
        let mut asked = std::fs::OpenOptions::new();
        asked.read(acc != libc::O_WRONLY).write(acc != libc::O_RDONLY || create.is_some()).custom_flags(extra);
        if let Some(mode) = create {
            wide.create(true).mode(mode);
            asked.create(true).mode(mode);
        }
        match wide.open(path) {
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => asked.open(path),
            r => r,
        }
    }

    fn patch(&self, dir: u64, p: Patch) {
        let mut dirs = self.dirs.lock().unwrap();
        let Some(d) = dirs.get_mut(&dir) else { return };
        if let Some(log) = &mut d.building {
            log.push(p);
            return;
        }
        if let Some(live) = &mut d.live {
            match p {
                Patch::Add(name, t) => {
                    live.insert(name_cookie(&name), (name, t));
                }
                Patch::Del(name) => {
                    live.remove(&name_cookie(&name));
                }
            }
        }
    }

    fn unlist(&self, dir: u64) {
        self.dirs.lock().unwrap().remove(&dir);
        self.listings.lock().unwrap().remove(&dir);
    }

    fn listing(&self, rel: &[u8]) -> Vec<(u64, Vec<u8>, u32)> {
        let mut out: Vec<(u64, Vec<u8>, u32)> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(self.real(rel)) {
            for e in rd.flatten() {
                let t = match e.file_type() {
                    Ok(t) if t.is_dir() => libc::DT_DIR,
                    Ok(t) if t.is_symlink() => libc::DT_LNK,
                    _ => libc::DT_REG,
                };
                let name = e.file_name().as_bytes().to_vec();
                out.push((name_cookie(&name), name, t as u32));
            }
        }
        let real = out.len();
        {
            let tree = self.virt.read().unwrap();
            let virt = &tree.files;
            if !virt.is_empty() {
                let prefix = if rel.is_empty() { Vec::new() } else { below(rel) };
                let mut last: Option<&[u8]> = None;
                for (k, _) in virt.range(prefix.clone()..).take_while(|(k, _)| k.starts_with(&prefix)) {
                    let rest = &k[prefix.len()..];
                    let (name, t) = match rest.iter().position(|&b| b == b'/') {
                        Some(i) => (&rest[..i], libc::DT_DIR),
                        None => (rest, libc::DT_REG),
                    };
                    if last != Some(name) {
                        out.push((name_cookie(name), name.to_vec(), t as u32));
                        last = Some(name);
                    }
                }
            }
        }
        let delivered = out.split_off(real);
        out.sort();
        for d in delivered {
            if let Err(at) = out.binary_search_by(|x| (x.0, &x.1).cmp(&(d.0, &d.1))) {
                out.insert(at, d);
            }
        }
        out
    }

    fn touches_virtual(&self, rel: &[u8]) -> bool {
        let tree = self.virt.read().unwrap();
        tree.files.contains_key(rel) || tree.has_below(rel)
    }

    fn inval(&self, parent: u64, name: &[u8]) {
        if let Some(s) = self.session.get() {
            let _ = s.notify_inval_entry(parent, name);
        }
    }

    fn changed(&self, rel: &[u8]) {
        let (parent, name) = self.state.lock().unwrap().changed(rel);
        self.unlist(parent);
        self.inval(parent, &name);
    }

    pub fn begin_delivery(&self) -> Delivery<'_> {
        let serial = self.delivery.lock().unwrap();
        self.swept.lock().unwrap().clear();
        let generation = self.virt.read().unwrap().generation + 1;
        Delivery { fs: self, _serial: serial, generation, edits: Vec::new() }
    }

    fn sweep_beside(&self, rel: &[u8]) {
        let dir = match rel.iter().rposition(|&b| b == b'/') {
            Some(i) => rel[..i].to_vec(),
            None => Vec::new(),
        };
        if !self.swept.lock().unwrap().insert(dir.clone()) {
            return;
        }
        let Ok(rd) = std::fs::read_dir(self.real(&dir)) else { return };
        for e in rd.flatten() {
            let name = e.file_name();
            let Some(text) = name.to_str() else { continue };
            let Some(pid) = text.strip_prefix(crate::tmp::BESIDE).and_then(|r| r.split('-').next()).and_then(|p| p.parse::<i32>().ok()) else {
                continue;
            };
            let alive = unsafe { libc::kill(pid, 0) } == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
            if !alive && std::fs::remove_file(e.path()).is_ok() {
                self.changed(&join(&dir, name.as_bytes()));
            }
        }
    }

    pub fn real_for_write(&self, rel: &[u8]) -> std::io::Result<PathBuf> {
        if let Some(i) = rel.iter().rposition(|&b| b == b'/') {
            std::fs::create_dir_all(self.real(&rel[..i]))?;
        }
        Ok(self.real(rel))
    }

}

impl Filesystem for OutFs {
    fn lookup(&self, _ctx: &Ctx, parent: u64, name: &[u8]) -> Res<Entry> {
        let _t = self.timed("lookup");
        let rel = join(&self.path(parent)?, name);
        let tree = self.virt.read().unwrap();
        let real = std::fs::symlink_metadata(self.real(&rel)).ok();
        match tree.lookup(&rel, real) {
            Some(kind) => Ok(self.entry(parent, name, &kind)),
            None => Ok(Entry::negative(TTL)),
        }
    }

    fn forget(&self, nodeid: u64, nlookup: u64) {
        let _t = self.timed("forget");
        let gone = self.state.lock().unwrap().forget(nodeid, nlookup);
        if !gone.is_empty() {
            let mut dirs = self.dirs.lock().unwrap();
            for id in gone {
                dirs.remove(&id);
            }
        }
    }

    fn getattr(&self, _ctx: &Ctx, nodeid: u64, _fh: Option<u64>) -> Res<(fuse_attr, Duration)> {
        let _t = self.timed("getattr");
        let rel = self.path(nodeid)?;
        let kind = self.kind(&rel).ok_or(Errno(libc::ENOENT))?;
        Ok((self.attr(nodeid, &kind), TTL))
    }

    fn setattr(&self, ctx: &Ctx, nodeid: u64, set: &SetAttr) -> Res<(fuse_attr, Duration)> {
        let _t = self.timed("setattr");
        let rel = self.path(nodeid)?;
        let kind = self.kind(&rel).ok_or(Errno(libc::ENOENT))?;
        match kind {
            Kind::Virtual(mut v) if set.size.is_none() => {
                if let Some(mode) = set.mode {
                    v.exec = mode & 0o111 != 0;
                }
                match set.mtime {
                    Some(TimeSet::At(s, ns)) => v.mtime_ns = s * 1_000_000_000 + ns as i64,
                    Some(TimeSet::Now) => {
                        v.mtime_ns = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as i64).unwrap_or(0)
                    }
                    None => {}
                }
                self.virt.write().unwrap().files.insert(rel, v.clone());
                self.dirty.store(true, Ordering::SeqCst);
                return Ok((self.attr(nodeid, &Kind::Virtual(v)), TTL));
            }
            Kind::Virtual(v) => self.make_real(&rel, &v, set.size != Some(0))?,
            Kind::VDir => std::fs::create_dir_all(self.real(&rel)).map_err(errno)?,
            Kind::Real(_) => {}
        }
        let path = self.real(&rel);
        if let Some(size) = set.size {
            let f = std::fs::OpenOptions::new().write(true).open(&path).map_err(errno)?;
            f.set_len(size).map_err(errno)?;
        }
        if let Some(mode) = set.mode {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode & 0o7777)).map_err(errno)?;
        }
        if set.atime.is_some() || set.mtime.is_some() {
            let spec = |t: Option<TimeSet>| match t {
                None => libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_OMIT },
                Some(TimeSet::Now) => libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_NOW },
                Some(TimeSet::At(s, ns)) => libc::timespec { tv_sec: s, tv_nsec: ns as i64 },
            };
            let times = [spec(set.atime), spec(set.mtime)];
            let c = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| Errno(libc::EINVAL))?;
            if unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
                return Err(Errno::last());
            }
        }
        self.getattr(ctx, nodeid, None)
    }

    fn readlink(&self, _ctx: &Ctx, nodeid: u64) -> Res<Vec<u8>> {
        let _t = self.timed("readlink");
        let rel = self.path(nodeid)?;
        std::fs::read_link(self.real(&rel)).map(|t| t.as_os_str().as_bytes().to_vec()).map_err(errno)
    }

    fn symlink(&self, _ctx: &Ctx, parent: u64, name: &[u8], target: &[u8]) -> Res<Entry> {
        let _t = self.timed("symlink");
        let rel = join(&self.path(parent)?, name);
        if self.virt.read().unwrap().files.contains_key(&rel) {
            return Err(Errno(libc::EEXIST));
        }
        self.make_parents(&rel)?;
        std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(target), self.real(&rel)).map_err(errno)?;
        let kind = self.kind(&rel).ok_or(Errno(libc::EIO))?;
        self.patch(parent, Patch::Add(name.to_vec(), libc::DT_LNK as u32));
        Ok(self.entry(parent, name, &kind))
    }

    fn mknod(&self, _ctx: &Ctx, parent: u64, name: &[u8], mode: u32, _rdev: u32) -> Res<Entry> {
        let _t = self.timed("mknod");
        if mode & libc::S_IFMT != libc::S_IFREG {
            return Err(Errno(libc::EPERM));
        }
        let rel = join(&self.path(parent)?, name);
        if self.virt.read().unwrap().files.contains_key(&rel) {
            return Err(Errno(libc::EEXIST));
        }
        self.make_parents(&rel)?;
        std::fs::OpenOptions::new().write(true).create_new(true).mode(mode & 0o7777).open(self.real(&rel)).map_err(errno)?;
        let kind = self.kind(&rel).ok_or(Errno(libc::EIO))?;
        self.patch(parent, Patch::Add(name.to_vec(), libc::DT_REG as u32));
        Ok(self.entry(parent, name, &kind))
    }

    fn mkdir(&self, _ctx: &Ctx, parent: u64, name: &[u8], mode: u32) -> Res<Entry> {
        let _t = self.timed("mkdir");
        let rel = join(&self.path(parent)?, name);
        if self.touches_virtual(&rel) {
            return Err(Errno(libc::EEXIST));
        }
        let make = || std::fs::DirBuilder::new().mode(mode & 0o7777).create(self.real(&rel));
        match make() {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.make_parents(&rel)?;
                make().map_err(errno)?
            }
            r => r.map_err(errno)?,
        }
        let md = std::fs::symlink_metadata(self.real(&rel)).map_err(errno)?;
        self.patch(parent, Patch::Add(name.to_vec(), libc::DT_DIR as u32));
        Ok(self.entry(parent, name, &Kind::Real(md)))
    }

    fn unlink(&self, _ctx: &Ctx, parent: u64, name: &[u8]) -> Res<()> {
        let _t = self.timed("unlink");
        let rel = join(&self.path(parent)?, name);
        let real = std::fs::remove_file(self.real(&rel));
        let listed = self.virt.read().unwrap().files.contains_key(&rel);
        let virt = listed && self.virt.write().unwrap().files.remove(&rel).is_some();
        if virt {
            self.dirty.store(true, Ordering::SeqCst);
        }
        match real {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && virt => {}
            Err(e) => return Err(errno(e)),
        }
        self.patch(parent, Patch::Del(name.to_vec()));
        self.detach(parent, name);
        self.give_back_if_empty(parent);
        Ok(())
    }

    fn rmdir(&self, _ctx: &Ctx, parent: u64, name: &[u8]) -> Res<()> {
        let _t = self.timed("rmdir");
        let rel = join(&self.path(parent)?, name);
        if self.has_virtual_below(&rel) {
            return Err(Errno(libc::ENOTEMPTY));
        }
        match std::fs::remove_dir(self.real(&rel)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(errno(e)),
        }
        self.patch(parent, Patch::Del(name.to_vec()));
        self.detach(parent, name);
        self.give_back_if_empty(parent);
        Ok(())
    }

    fn rename(&self, _ctx: &Ctx, parent: u64, name: &[u8], newparent: u64, newname: &[u8], flags: u32) -> Res<()> {
        let _t = self.timed("rename");
        const NOREPLACE: u32 = 1;
        if flags & !NOREPLACE != 0 {
            return Err(Errno(libc::EINVAL));
        }
        let (from, to) = {
            let st = self.state.lock().unwrap();
            (join(&st.path(parent)?, name), join(&st.path(newparent)?, newname))
        };
        let plain = flags == 0 && !self.touches_virtual(&from) && !self.touches_virtual(&to);
        if plain {
            let go = || std::fs::rename(self.real(&from), self.real(&to));
            match go() {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && std::fs::symlink_metadata(self.real(&from)).is_ok() => {
                    self.make_parents(&to)?;
                    go().map_err(errno)?
                }
                r => r.map_err(errno)?,
            }
        }
        let src = if plain { Kind::VDir } else { self.kind(&from).ok_or(Errno(libc::ENOENT))? };
        let dst = if plain { None } else { self.kind(&to) };
        if flags & NOREPLACE != 0 && dst.is_some() {
            return Err(Errno(libc::EEXIST));
        }
        let (from_below, to_below) = (below(&from), below(&to));
        match &src {
            _ if plain => {}
            Kind::Real(md) => {
                if md.is_dir() && self.has_virtual_below(&to) {
                    return Err(Errno(libc::ENOTEMPTY));
                }
                self.make_parents(&to)?;
                std::fs::rename(self.real(&from), self.real(&to)).map_err(errno)?;
                if self.virt.read().unwrap().files.contains_key(&to) {
                    self.virt.write().unwrap().files.remove(&to);
                    self.dirty.store(true, Ordering::SeqCst);
                }
            }
            Kind::Virtual(v) => {
                match &dst {
                    Some(Kind::Real(md)) if md.is_dir() => return Err(Errno(libc::EISDIR)),
                    Some(Kind::VDir) => return Err(Errno(libc::EISDIR)),
                    Some(Kind::Real(_)) => std::fs::remove_file(self.real(&to)).map_err(errno)?,
                    _ => {}
                }
                let mut tree = self.virt.write().unwrap();
                let virt = &mut tree.files;
                virt.remove(&from);
                virt.insert(to.clone(), v.clone());
                self.dirty.store(true, Ordering::SeqCst);
            }
            Kind::VDir => match &dst {
                Some(Kind::Real(md)) if md.is_dir() => std::fs::remove_dir(self.real(&to)).map_err(errno)?,
                Some(Kind::Real(_)) | Some(Kind::Virtual(_)) => return Err(Errno(libc::ENOTDIR)),
                Some(Kind::VDir) => return Err(Errno(libc::ENOTEMPTY)),
                None => {}
            },
        }
        if self.has_virtual_below(&from) {
            let mut tree = self.virt.write().unwrap();
            let virt = &mut tree.files;
            let moved: Vec<Vec<u8>> = virt.range(from_below.clone()..).take_while(|(k, _)| k.starts_with(&from_below)).map(|(k, _)| k.clone()).collect();
            for k in moved {
                if let Some(v) = virt.remove(&k) {
                    let mut nk = to_below.clone();
                    nk.extend_from_slice(&k[from_below.len()..]);
                    virt.insert(nk, v);
                }
            }
            self.dirty.store(true, Ordering::SeqCst);
        }
        if plain {
            let t = match std::fs::symlink_metadata(self.real(&to)) {
                Ok(md) if md.is_dir() => libc::DT_DIR,
                Ok(md) if md.file_type().is_symlink() => libc::DT_LNK,
                _ => libc::DT_REG,
            };
            self.patch(parent, Patch::Del(name.to_vec()));
            self.patch(newparent, Patch::Add(newname.to_vec(), t as u32));
        } else {
            self.unlist(parent);
            self.unlist(newparent);
        }
        let mut st = self.state.lock().unwrap();
        st.children.remove(&(newparent, newname.to_vec()));
        if let Some(id) = st.children.remove(&(parent, name.to_vec())) {
            if let Some(n) = st.nodes.get_mut(&id) {
                n.parent = newparent;
                n.name = newname.to_vec();
            }
            st.children.insert((newparent, newname.to_vec()), id);
            if parent != newparent {
                if let Some(p) = st.nodes.get_mut(&parent) {
                    p.kids = p.kids.saturating_sub(1);
                }
                if let Some(p) = st.nodes.get_mut(&newparent) {
                    p.kids += 1;
                }
            }
        }
        Ok(())
    }

    fn link(&self, _ctx: &Ctx, nodeid: u64, newparent: u64, newname: &[u8]) -> Res<Entry> {
        let _t = self.timed("link");
        let (from, to) = {
            let st = self.state.lock().unwrap();
            (st.path(nodeid)?, join(&st.path(newparent)?, newname))
        };
        if !self.touches_virtual(&to) && !self.virt.read().unwrap().files.contains_key(&from) {
            let go = || std::fs::hard_link(self.real(&from), self.real(&to));
            match go() {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    self.make_parents(&to)?;
                    go().map_err(errno)?
                }
                r => r.map_err(errno)?,
            }
            let md = std::fs::symlink_metadata(self.real(&to)).map_err(errno)?;
            let t = if md.file_type().is_symlink() { libc::DT_LNK } else { libc::DT_REG };
            self.patch(newparent, Patch::Add(newname.to_vec(), t as u32));
            return Ok(self.entry(newparent, newname, &Kind::Real(md)));
        }
        if self.kind(&to).is_some() {
            return Err(Errno(libc::EEXIST));
        }
        match self.kind(&from).ok_or(Errno(libc::ENOENT))? {
            Kind::Real(_) => {
                self.make_parents(&to)?;
                std::fs::hard_link(self.real(&from), self.real(&to)).map_err(errno)?;
            }
            Kind::Virtual(v) => {
                self.virt.write().unwrap().files.insert(to.clone(), v);
                self.dirty.store(true, Ordering::SeqCst);
            }
            Kind::VDir => return Err(Errno(libc::EPERM)),
        }
        let kind = self.kind(&to).ok_or(Errno(libc::EIO))?;
        self.patch(newparent, Patch::Add(newname.to_vec(), libc::DT_REG as u32));
        Ok(self.entry(newparent, newname, &kind))
    }

    fn open(&self, _ctx: &Ctx, nodeid: u64, flags: u32) -> Res<Opened> {
        let _t = self.timed("open");
        let rel = self.path(nodeid)?;
        let acc = flags as i32 & libc::O_ACCMODE;
        let keep = flags as i32 & (libc::O_TRUNC | libc::O_APPEND | libc::O_NOFOLLOW);
        match Self::open_wide(&self.real(&rel), acc, keep, None) {
            Ok(file) => return self.open_handle(nodeid, Content::Open(file), How::Real),
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(errno(e)),
            Err(_) => {}
        }
        let truncate = keep & libc::O_TRUNC != 0;
        let plan = self.virt.read().unwrap().open(&rel, acc != libc::O_RDONLY || truncate, truncate)?;
        match plan {
            VirtualOpen::Empty => {
                self.opened_virtual.fetch_add(1, Ordering::Relaxed);
                self.open_handle(nodeid, Content::Open(File::open("/dev/null").map_err(errno)?), How::Daemon)
            }
            VirtualOpen::Read(file) => {
                self.opened_virtual.fetch_add(1, Ordering::Relaxed);
                if let Some(cached) = self.source.cached(&file.hash, file.size).and_then(|p| File::open(p).ok()) {
                    return self.open_handle(nodeid, Content::Open(cached), How::Content);
                }
                self.open_handle(nodeid, Content::Unfetched(Fetch { hash: file.hash, size: file.size, rel }), How::Daemon)
            }
            VirtualOpen::Materialize { file, with_content } => {
                if self.state.lock().unwrap().nodes.get(&nodeid).is_some_and(|n| n.backing.is_some()) {
                    return Err(Errno(libc::ETXTBSY));
                }
                self.make_real(&rel, &file, with_content)?;
                let file = Self::open_wide(&self.real(&rel), acc, keep, None).map_err(errno)?;
                self.open_handle(nodeid, Content::Open(file), How::Real)
            }
        }
    }

    fn create(&self, _ctx: &Ctx, parent: u64, name: &[u8], mode: u32, flags: u32) -> Res<(Entry, Opened)> {
        let _t = self.timed("create");
        let rel = join(&self.path(parent)?, name);
        let had_virtual = {
            let tree = self.virt.read().unwrap();
            let virt = &tree.files;
            !virt.is_empty() && virt.contains_key(&rel)
        };
        if had_virtual && flags as i32 & libc::O_EXCL != 0 {
            return Err(Errno(libc::EEXIST));
        }
        let acc = flags as i32 & libc::O_ACCMODE;
        let keep = flags as i32 & (libc::O_TRUNC | libc::O_APPEND | libc::O_EXCL | libc::O_NOFOLLOW);
        let open = || Self::open_wide(&self.real(&rel), acc, keep, Some(mode & 0o7777));
        let file = match open() {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.make_parents(&rel)?;
                open().map_err(errno)?
            }
            r => r.map_err(errno)?,
        };
        if had_virtual && self.virt.write().unwrap().files.remove(&rel).is_some() {
            self.dirty.store(true, Ordering::SeqCst);
            self.detach(parent, name);
        }
        let md = file.metadata().map_err(errno)?;
        self.patch(parent, Patch::Add(name.to_vec(), libc::DT_REG as u32));
        let entry = self.entry(parent, name, &Kind::Real(md));
        Ok((entry, self.open_handle(entry.nodeid, Content::Open(file), How::Real)?))
    }

    fn read(&self, _ctx: &Ctx, _nodeid: u64, fh: u64, offset: u64, size: u32, out: &mut Vec<u8>) -> Res<()> {
        let _t = self.timed("read");
        let file = match self.handle_reader(fh)? {
            Reader::Whole(file) => file,
            Reader::Ranged(p) => {
                *out = p.read_at(offset, size as usize).map_err(|e| {
                    eprintln!("rargo daemon: content {}: {e}", crate::partial::hex(p.hash()));
                    Errno(libc::EIO)
                })?;
                return Ok(());
            }
        };
        out.resize(size as usize, 0);
        let mut done = 0;
        while done < out.len() {
            match file.read_at(&mut out[done..], offset + done as u64) {
                Ok(0) => break,
                Ok(n) => done += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(errno(e)),
            }
        }
        out.truncate(done);
        Ok(())
    }

    fn write(&self, _ctx: &Ctx, _nodeid: u64, fh: u64, offset: u64, data: &[u8]) -> Res<u32> {
        let _t = self.timed("write");
        let file = self.handle_file(fh)?.ok_or(Errno(libc::EBADF))?;
        file.write_all_at(data, offset).map_err(errno)?;
        Ok(data.len() as u32)
    }

    fn release(&self, _nodeid: u64, fh: u64, _flags: u32) {
        let _t = self.timed("release");
        let Some(h) = self.handles.lock().unwrap().remove(&fh) else { return };
        let mut st = self.state.lock().unwrap();
        if let Some(n) = st.nodes.get_mut(&h.node) {
            if !h.through {
                n.cached = n.cached.saturating_sub(1);
            } else if let Some((id, refs, _)) = &mut n.backing {
                *refs -= 1;
                if *refs == 0 {
                    if let Some(s) = self.session.get() {
                        let _ = s.backing_close(*id);
                    }
                    n.backing = None;
                }
            }
        }
    }

    fn fsync(&self, _ctx: &Ctx, _nodeid: u64, fh: u64, datasync: bool) -> Res<()> {
        let _t = self.timed("fsync");
        let Some(file) = self.handle_file(fh)? else { return Ok(()) };
        if datasync { file.sync_data() } else { file.sync_all() }.map_err(errno)
    }

    fn readdir(&self, _ctx: &Ctx, nodeid: u64, _fh: u64, offset: u64, sink: &mut DirSink) -> Res<()> {
        let _t = self.timed("readdir");
        {
            let dirs = self.dirs.lock().unwrap();
            if let Some(live) = dirs.get(&nodeid).and_then(|d| d.live.as_ref()) {
                for (cookie, (name, t)) in live.range(offset + 1..) {
                    if !sink.push(u64::MAX - 1, *cookie, *t, name) {
                        break;
                    }
                }
                return Ok(());
            }
        }
        let cached = if offset == 0 { None } else { self.listings.lock().unwrap().get(&nodeid).filter(|(at, _)| at.elapsed() < LISTING_KEPT).map(|(_, l)| l.clone()) };
        let listing = match cached {
            Some(l) => l,
            None => {
                let mine = {
                    let mut dirs = self.dirs.lock().unwrap();
                    let d = dirs.entry(nodeid).or_default();
                    if d.building.is_none() {
                        d.building = Some(Vec::new());
                        true
                    } else {
                        false
                    }
                };
                let l = Arc::new(self.listing(&self.path(nodeid)?));
                if mine {
                    let mut dirs = self.dirs.lock().unwrap();
                    if let Some(d) = dirs.get_mut(&nodeid) {
                        let mut live: BTreeMap<u64, (Vec<u8>, u32)> = l.iter().map(|(c, n, t)| (*c, (n.clone(), *t))).collect();
                        for p in d.building.take().unwrap_or_default() {
                            match p {
                                Patch::Add(name, t) => {
                                    live.insert(name_cookie(&name), (name, t));
                                }
                                Patch::Del(name) => {
                                    live.remove(&name_cookie(&name));
                                }
                            }
                        }
                        d.live = Some(live);
                    }
                }
                let mut all = self.listings.lock().unwrap();
                if all.len() > 256 {
                    all.retain(|_, (at, _)| at.elapsed() < LISTING_KEPT);
                }
                all.insert(nodeid, (std::time::Instant::now(), l.clone()));
                l
            }
        };
        let from = listing.partition_point(|(cookie, _, _)| *cookie <= offset);
        for (cookie, name, t) in &listing[from..] {
            if !sink.push(u64::MAX - 1, *cookie, *t, name) {
                break;
            }
        }
        Ok(())
    }

    fn statfs(&self, _ctx: &Ctx, _nodeid: u64) -> Res<fuse_kstatfs> {
        let _t = self.timed("statfs");
        let c = std::ffi::CString::new(self.under.as_os_str().as_bytes()).map_err(|_| Errno(libc::EINVAL))?;
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
            return Err(Errno::last());
        }
        Ok(fuse_kstatfs {
            blocks: s.f_blocks as u64,
            bfree: s.f_bfree as u64,
            bavail: s.f_bavail as u64,
            files: s.f_files as u64,
            ffree: s.f_ffree as u64,
            bsize: s.f_bsize as u32,
            namelen: s.f_namemax as u32,
            frsize: s.f_frsize as u32,
            ..fuse_kstatfs::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_cookies_are_stable_and_ordered() {
        let names: Vec<Vec<u8>> = (0..5000).map(|i| format!("libcrate{i}-0123456789abcdef.rmeta").into_bytes()).collect();
        let mut cookies: Vec<u64> = names.iter().map(|n| name_cookie(n)).collect();
        assert!(cookies.iter().all(|c| *c >= 1 && *c < 1 << 63));
        assert_eq!(name_cookie(&names[7]), name_cookie(&names[7]));
        cookies.sort();
        cookies.dedup();
        assert_eq!(cookies.len(), names.len(), "no two of these names share a place");
    }

    #[test]
    fn generated_deliveries_and_late_reads_keep_the_right_content() {
        let mut tree = OutputTree { files: Table::new(), generation: 0 };
        for generation in 1..=64 {
            let path = b"debug/app".to_vec();
            let newer = vec![generation as u8; 30];
            let older = vec![generation as u8 - 1; 30];
            let file = |bytes: &[u8]| VFile { hash: *blake3::hash(bytes).as_bytes(), size: bytes.len() as u64, exec: true, mtime_ns: generation as i64 };
            let put = |file| Edit::Put { path: path.clone(), file, replaced_real: false };
            assert_eq!(tree.apply(generation, vec![put(file(&newer))]), [path.clone()]);
            assert!(tree.invariants());
            assert!(tree.apply(generation - 1, vec![put(file(&older))]).is_empty());
            assert!(tree.invariants());
            assert!(matches!(tree.lookup::<()>(&path, None), Some(Kind::Virtual(v)) if v.hash == file(&newer).hash));
            let VirtualOpen::Read(entry) = tree.open(&path, false, false).ok().unwrap() else { panic!("expected a read") };
            let request = Fetch { hash: entry.hash, size: entry.size, rel: path.clone() };
            let old_request = Fetch { hash: file(&older).hash, ..request.clone() };
            let mut handle = Content::<Vec<u8>, ()>::Unfetched(request.clone());
            // The shell proves fetched bytes against the request's hash before returning a reader.
            assert_ne!(*blake3::hash(&older).as_bytes(), request.hash);
            handle.fetched(&old_request, Content::Open(older));
            assert!(matches!(handle.read(), ReadPlan::Fetch(effect) if effect == request));
            assert_eq!(*blake3::hash(&newer).as_bytes(), request.hash);
            handle.fetched(&request, Content::Open(newer.clone()));
            assert!(matches!(handle.read(), ReadPlan::Whole(bytes) if bytes[3..9] == newer[3..9]));
            handle.fetched(&request, Content::Ranged(()));
            assert!(matches!(handle.read(), ReadPlan::Whole(_)));
        }
    }

    #[test]
    fn replacing_output_detaches_the_name_without_reusing_its_inode() {
        let mut nodes = HashMap::new();
        nodes.insert(ROOT, Node { parent: 0, name: Vec::new(), nlookup: 1, kids: 0, cached: 0, backing: None });
        let mut state = Inodes { nodes, children: HashMap::new(), next: 2 };
        let old = state.lookup(ROOT, b"app");
        assert!(state.invariants());
        assert_eq!(state.changed(b"app"), (ROOT, b"app".to_vec()));
        assert!(state.invariants());
        let new = state.lookup(ROOT, b"app");
        assert!(state.invariants());
        assert_ne!(old, new);
        assert_eq!(state.path(new).ok().unwrap(), b"app");
        assert_eq!(state.forget(old, 1), [old]);
        assert!(state.invariants());
        assert_eq!(state.lookup(ROOT, b"app"), new);
        assert!(state.invariants());
    }
}
