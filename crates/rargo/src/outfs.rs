#[cfg(test)]
use rargo_machine::StateMachine;
use fusewire::abi::*;
use fusewire::{Ctx, DirSink, Entry, Errno, Filesystem, Opened, Res, Session, SetAttr, TimeSet};
pub use rargo::out_tree::VFile;
use rargo::out_tree::{below, Edit, OutputTree, Table};
use rargo_proto::Digest;
use crate::partial::Partial;
use std::cell::{Cell, OnceCell, RefCell, RefMut};
use std::collections::{BTreeMap, BTreeSet};
use rargo_net::io::file::File;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
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
        bump(&self.0.op_count[self.1], 1);
        bump(&self.0.op_nanos[self.1], rargo_net::io::now().saturating_duration_since(self.2).as_nanos() as u64);
    }
}
const LISTING_KEPT: Duration = Duration::from_secs(30);

fn bump(counter: &Cell<u64>, by: u64) {
    counter.set(counter.get() + by);
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
    nodes: BTreeMap<u64, Node>,
    children: BTreeMap<(u64, Vec<u8>), u64>,
    next: u64,
}

impl Inodes {
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

#[cfg(test)]
impl StateMachine for Inodes {
    fn invariants(&self) {
        let mut kids: BTreeMap<u64, u64> = BTreeMap::new();
        for (id, node) in &self.nodes {
            if *id != ROOT {
                *kids.entry(node.parent).or_default() += 1;
            }
        }
        assert!(self.nodes.contains_key(&ROOT), "the root inode is there");
        for ((parent, name), id) in &self.children {
            assert!(
                self.nodes.get(id).is_some_and(|node| node.parent == *parent && node.name == *name),
                "child {:?} of inode {parent} is inode {id}, which has that parent and name",
                String::from_utf8_lossy(name)
            );
        }
        for (id, node) in &self.nodes {
            assert!(*id < self.next, "inode {id} was handed out, below {}", self.next);
            assert_eq!(node.kids, kids.get(id).copied().unwrap_or(0), "inode {id} counts its children");
            if *id != ROOT {
                assert!(self.nodes.contains_key(&node.parent), "inode {id}'s parent {} is there", node.parent);
                assert!(!node.name.is_empty(), "inode {id} has a name");
            }
        }
    }
}

/// One delivery's changes to an output directory. It works on the directory under the mount with plain values, so a pool thread can make it; the filesystem takes its edits on the loop when it is done.
pub struct Delivery {
    under: PathBuf,
    generation: u64,
    edits: Vec<Edit>,
    swept: BTreeSet<Vec<u8>>,
    stale: Vec<Vec<u8>>,
    /// Numbers for the temporary files this delivery writes.
    pub names: crate::tmp::Names,
}

fn real_under(under: &Path, rel: &[u8]) -> PathBuf {
    if rel.is_empty() { under.join(".") } else { under.join(std::ffi::OsStr::from_bytes(rel)) }
}

impl Delivery {
    fn real(&self, rel: &[u8]) -> PathBuf {
        real_under(&self.under, rel)
    }

    fn remove_real(&self, rel: &[u8]) -> bool {
        let real = self.real(rel);
        match rargo_net::io::file::symlink_metadata(&real) {
            Ok(md) if md.is_dir() => {
                let _ = rargo_net::io::file::remove_dir_all(&real);
            }
            Ok(_) => {
                let _ = rargo_net::io::file::remove_file(&real);
            }
            Err(_) => return false,
        }
        true
    }

    /// Removes what dead processes left beside `rel`, once for each directory.
    fn sweep_beside(&mut self, rel: &[u8]) {
        let dir = match rel.iter().rposition(|&b| b == b'/') {
            Some(i) => rel[..i].to_vec(),
            None => Vec::new(),
        };
        if !self.swept.insert(dir.clone()) {
            return;
        }
        let Ok(rd) = rargo_net::io::file::read_dir(self.real(&dir)) else { return };
        for e in rd.flatten() {
            let name = e.file_name();
            let Some(text) = name.to_str() else { continue };
            let Some(pid) = text.strip_prefix(crate::tmp::BESIDE).and_then(|r| r.split('-').next()).and_then(|p| p.parse::<i32>().ok()) else {
                continue;
            };
            let alive = crate::kernel::kill(pid, 0).map_or_else(|e| e.raw_os_error() == Some(libc::EPERM), |()| true);
            if !alive && rargo_net::io::file::remove_file(e.path()).is_ok() {
                self.stale.push(join(&dir, name.as_bytes()));
            }
        }
    }

    pub fn put(&mut self, rel: &[u8], file: VFile) {
        self.sweep_beside(rel);
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

    pub fn real_for_write(&self, rel: &[u8]) -> std::io::Result<PathBuf> {
        if let Some(i) = rel.iter().rposition(|&b| b == b'/') {
            rargo_net::io::file::create_dir_all(self.real(&rel[..i]))?;
        }
        Ok(self.real(rel))
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

enum Content<F = Rc<File>, R = Rc<Partial>> {
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

    fn fetched(&mut self, request: &Fetch, proven: &Digest, content: Self) -> Result<(), Mismatch> {
        if *proven != request.hash {
            return Err(Mismatch);
        }
        if matches!(self, Self::Unfetched(expected) if expected == request) {
            *self = content;
        }
        Ok(())
    }
}

#[cfg(test)]
impl<F, R> StateMachine for Content<F, R> {
    fn invariants(&self) {
        if let Self::Unfetched(fetch) = self {
            assert!(!fetch.rel.is_empty(), "an unfetched file has a path");
            assert!(fetch.size > 0, "an unfetched file has bytes to fetch");
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Mismatch;

enum VirtualOpen {
    Empty,
    Read(VFile),
    Materialize { file: VFile, with_content: bool },
}

fn open_virtual(tree: &OutputTree, rel: &[u8], writing: bool, truncate: bool) -> Res<VirtualOpen> {
    let file = tree.files.get(rel).cloned().ok_or(Errno(libc::ENOENT))?;
    Ok(if writing {
        VirtualOpen::Materialize { file, with_content: !truncate }
    } else if file.size == 0 {
        VirtualOpen::Empty
    } else {
        VirtualOpen::Read(file)
    })
}


#[derive(Clone, Copy, PartialEq, Eq)]
enum How {
    Daemon,
    Real,
    Content,
}

pub enum Reader {
    Whole(Rc<File>),
    Ranged(Rc<Partial>),
}

pub trait Source {
    fn cached(&self, hash: &Digest, size: u64) -> Option<PathBuf>;
    fn whole(&self, hash: &Digest, size: u64) -> std::io::Result<PathBuf>;
    fn reader(&self, hash: &Digest, size: u64) -> std::io::Result<(Reader, Digest)>;
}

pub struct OutFs {
    under: PathBuf,
    _under_fd: File,
    state: RefCell<Inodes>,
    virt: RefCell<OutputTree>,
    virt_edits: Cell<u64>,
    delivery: rargo_net::io::Mutex<()>,
    handles: RefCell<BTreeMap<u64, Handle>>,
    registering: RefCell<BTreeMap<u64, bool>>,
    next_fh: Cell<u64>,
    source: Box<dyn Source>,
    session: OnceCell<Arc<Session>>,
    table: PathBuf,
    dirty: Cell<bool>,
    dirs: RefCell<BTreeMap<u64, DirState>>,
    listings: RefCell<BTreeMap<u64, (std::time::Instant, Rc<Vec<(u64, Vec<u8>, u32)>>)>>,
    uid: u32,
    gid: u32,
    op_count: [Cell<u64>; OPS.len()],
    op_nanos: [Cell<u64>; OPS.len()],
    opened_virtual: Cell<u64>,
    made_real: Cell<u64>,
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


fn name_cookie(name: &[u8]) -> u64 {
    let h = blake3::hash(name);
    let c = (u64::from_le_bytes(h.as_bytes()[..8].try_into().unwrap()) >> 1).max(1);
    assert!(c >= 1);
    assert!(c < 1 << 63);
    c
}

fn lookup<T>(tree: &OutputTree, rel: &[u8], real: Option<T>) -> Option<Kind<T>> {
    if let Some(real) = real {
        return Some(Kind::Real(real));
    }
    if rel.is_empty() {
        return None;
    }
    if let Some(file) = tree.files.get(rel) {
        return Some(Kind::Virtual(file.clone()));
    }
    tree.has_below(rel).then_some(Kind::VDir)
}


enum Kind<T = rargo_net::io::file::Stat> {
    Real(T),
    Virtual(VFile),
    VDir,
}

impl OutFs {
    pub fn new(dir: &Path, table: &Path, source: Box<dyn Source>) -> std::io::Result<Rc<OutFs>> {
        let fd = rargo_net::io::file::OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_PATH).open(dir)?;
        let under = PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd()));
        let virt: Table = rargo_net::io::file::read(table)
            .ok()
            .and_then(|raw| zstd::stream::decode_all(&raw[..]).ok())
            .and_then(|plain| bincode::deserialize(&plain).ok())
            .unwrap_or_default();
        let mut nodes = BTreeMap::new();
        nodes.insert(ROOT, Node { parent: 0, name: Vec::new(), nlookup: 1, kids: 0, cached: 0, backing: None });
        let (uid, gid) = (rargo_net::io::process::euid(), rargo_net::io::process::egid());
        Ok(Rc::new(OutFs {
            under,
            _under_fd: fd,
            state: RefCell::new(Inodes { nodes, children: BTreeMap::new(), next: 2 }),
            virt: RefCell::new(OutputTree::new(virt)),
            virt_edits: Cell::new(0),
            delivery: rargo_net::io::Mutex::new(()),
            handles: RefCell::new(BTreeMap::new()),
            registering: RefCell::new(BTreeMap::new()),
            next_fh: Cell::new(1),
            source,
            session: OnceCell::new(),
            table: table.to_path_buf(),
            dirty: Cell::new(false),
            dirs: RefCell::new(BTreeMap::new()),
            listings: RefCell::new(BTreeMap::new()),
            uid,
            gid,
            op_count: std::array::from_fn(|_| Cell::new(0)),
            op_nanos: std::array::from_fn(|_| Cell::new(0)),
            opened_virtual: Cell::new(0),
            made_real: Cell::new(0),
        }))
    }

    /// The tree of delivered files, to change. Every change is counted, so a handler that looked at the real directory while the tree changed can tell.
    fn tree_mut(&self) -> RefMut<'_, OutputTree> {
        self.virt_edits.set(self.virt_edits.get() + 1);
        self.virt.borrow_mut()
    }

    fn fresh_fh(&self) -> u64 {
        let fh = self.next_fh.get();
        self.next_fh.set(fh + 1);
        fh
    }

    fn timed(&self, op: &str) -> Timed<'_> {
        Timed(self, OPS.iter().position(|o| *o == op).unwrap_or(0), rargo_net::io::now())
    }

    pub fn op_report(&self) -> String {
        let mut rows: Vec<(u64, u64, &str)> = OPS
            .iter()
            .enumerate()
            .map(|(i, name)| (self.op_nanos[i].get(), self.op_count[i].get(), *name))
            .filter(|r| r.1 > 0)
            .collect();
        rows.sort_by(|a, b| b.0.cmp(&a.0));
        rows.iter().map(|(ns, n, name)| format!("{name} {n} ({:.2} s)", *ns as f64 / 1e9)).collect::<Vec<_>>().join(", ")
    }

    pub fn attach(&self, session: Arc<Session>) {
        let _ = self.session.set(session);
    }

    fn give_back_if_empty(&self, parent: u64) {
        if parent != ROOT || !self.virt.borrow().files.is_empty() {
            return;
        }
        if rargo_net::io::file::read_dir(&self.under).map(|mut d| d.next().is_some()).unwrap_or(true) {
            return;
        }
        let _ = rargo_net::io::file::remove_file(&self.table);
        let _ = rargo_net::io::file::remove_file(self.table.with_file_name("delivered"));
        self.dirty.set(false);
        if let Some(session) = self.session.get() {
            rargo_net::io::blocking(|| session.unmount());
        }
    }

    /// Writes the table of delivered files if it changed, through a temporary that `names` numbers.
    pub fn save(&self, mut names: crate::tmp::Names) {
        if !self.dirty.replace(false) {
            return;
        }
        let plain = bincode::serialize(&self.virt.borrow().files).unwrap_or_default();
        let table = self.table.clone();
        rargo_net::io::cpu(move || {
            if let Ok(packed) = zstd::stream::encode_all(&plain[..], 1) {
                let _ = crate::write_atomic(&table, &packed, &mut names);
            }
        });
    }

    /// How many times a file that exists only as its hash was opened.
    pub fn opened_virtual(&self) -> u64 {
        self.opened_virtual.get()
    }

    /// How many files that existed only as their hash were written out as real files.
    pub fn made_real(&self) -> u64 {
        self.made_real.get()
    }

    pub fn virtual_count(&self) -> usize {
        self.virt.borrow().files.len()
    }

    fn real(&self, rel: &[u8]) -> PathBuf {
        real_under(&self.under, rel)
    }

    fn kind(&self, rel: &[u8]) -> Option<Kind> {
        let real = rargo_net::io::file::symlink_metadata(self.real(rel)).ok();
        lookup(&self.virt.borrow(), rel, real)
    }

    fn path(&self, id: u64) -> Res<Vec<u8>> {
        self.state.borrow_mut().path(id)
    }

    fn has_virtual_below(&self, rel: &[u8]) -> bool {
        self.virt.borrow().has_below(rel)
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
        self.state.borrow_mut().children.remove(&(parent, name.to_vec()));
    }

    fn entry(&self, parent: u64, name: &[u8], kind: &Kind) -> Entry {
        let id = self.state.borrow_mut().lookup(parent, name);
        Entry { nodeid: id, generation: 0, attr: self.attr(id, kind), entry_ttl: TTL, attr_ttl: TTL }
    }

    fn make_parents(&self, rel: &[u8]) -> Res<()> {
        if let Some(i) = rel.iter().rposition(|&b| b == b'/') {
            rargo_net::io::file::create_dir_all(self.real(&rel[..i])).map_err(errno)?;
        }
        Ok(())
    }

    fn make_real(&self, rel: &[u8], v: &VFile, with_content: bool) -> Res<()> {
        self.make_parents(rel)?;
        let dst = self.real(rel);
        let mode = if v.exec { 0o755 } else { 0o644 };
        if with_content && v.size > 0 {
            let src = self.source.whole(&v.hash, v.size).map_err(errno)?;
            let tmp = self.real(&join(b"", format!(".rargo-real-{}-{}", rargo_net::io::process::id(), self.fresh_fh()).as_bytes()));
            rargo_net::io::file::copy(&src, &tmp).map_err(errno)?;
            rargo_net::io::file::set_mode(&tmp, mode).map_err(errno)?;
            rargo_net::io::file::rename(&tmp, &dst).map_err(|e| {
                let _ = rargo_net::io::file::remove_file(&tmp);
                errno(e)
            })?;
        } else {
            rargo_net::io::file::OpenOptions::new().write(true).create(true).truncate(true).mode(mode).open(&dst).map_err(errno)?;
        }
        self.tree_mut().files.remove(rel);
        self.dirty.set(true);
        bump(&self.made_real, 1);
        Ok(())
    }

    fn open_handle(&self, ctx: &Ctx, node: u64, file: Content, how: How) -> Res<Opened> {
        let fh = self.fresh_fh();
        let mut st = self.state.borrow_mut();
        let mut backing_id = None;
        let mut register = None;
        let content = how == How::Content;
        if let (Some(n), true, Some(s), Content::Open(file)) = (st.nodes.get_mut(&node), how != How::Daemon, self.session.get(), &file) {
            match &mut n.backing {
                Some((_, _, c)) if *c != content => return Err(Errno(libc::ETXTBSY)),
                Some((id, refs, _)) if n.cached == 0 => {
                    *refs += 1;
                    backing_id = Some(*id);
                }
                None if n.cached == 0 && s.caps().passthrough => register = Some(file.as_raw_fd()),
                _ => {}
            }
        }
        if let Some(fd) = register {
            drop(st);
            self.registering.borrow_mut().insert(fh, content);
            self.handles.borrow_mut().insert(fh, Handle { file, node, through: false });
            ctx.effects.register(fd, fh);
            return Ok(Opened { fh, flags: FOPEN_PASSTHROUGH, backing_id: 0 });
        }
        if backing_id.is_none() {
            if let Some(n) = st.nodes.get_mut(&node) {
                n.cached += 1;
            }
        }
        drop(st);
        self.handles.borrow_mut().insert(fh, Handle { file, node, through: backing_id.is_some() });
        Ok(match backing_id {
            Some(id) => Opened { fh, flags: FOPEN_PASSTHROUGH, backing_id: id },
            None => Opened { fh, flags: FOPEN_KEEP_CACHE, backing_id: 0 },
        })
    }

    fn handle_file(&self, fh: u64) -> Res<Option<Rc<File>>> {
        match &self.handles.borrow().get(&fh).ok_or(Errno(libc::EBADF))?.file {
            Content::Open(file) => Ok(Some(file.clone())),
            Content::Unfetched(_) | Content::Ranged(_) => Ok(None),
        }
    }

    fn handle_reader(&self, fh: u64) -> Res<Reader> {
        let request = {
            let handles = self.handles.borrow();
            match handles.get(&fh).ok_or(Errno(libc::EBADF))?.file.read() {
                ReadPlan::Whole(file) => return Ok(Reader::Whole(file.clone())),
                ReadPlan::Ranged(reader) => return Ok(Reader::Ranged(reader.clone())),
                ReadPlan::Fetch(request) => request,
            }
        };
        let (reader, proven) = self.source.reader(&request.hash, request.size).map_err(|e| {
            eprintln!("rargo daemon: target/{}: its content could not be fetched: {e}", String::from_utf8_lossy(&request.rel));
            errno(e)
        })?;
        let content = match &reader {
            Reader::Whole(file) => Content::Open(file.clone()),
            Reader::Ranged(reader) => Content::Ranged(reader.clone()),
        };
        let fetched = {
            let mut handles = self.handles.borrow_mut();
            let handle = handles.get_mut(&fh).ok_or(Errno(libc::EBADF))?;
            handle.file.fetched(&request, &proven, content)
        };
        if fetched.is_err() {
            eprintln!(
                "rargo daemon: target/{}: what was fetched is not the content asked for",
                String::from_utf8_lossy(&request.rel)
            );
            return Err(Errno(libc::EIO));
        }
        Ok(reader)
    }

    fn open_wide(path: &Path, acc: i32, extra: i32, create: Option<u32>) -> std::io::Result<File> {
        let mut wide = rargo_net::io::file::OpenOptions::new();
        wide.read(true).write(true).custom_flags(extra);
        let mut asked = rargo_net::io::file::OpenOptions::new();
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
        let mut dirs = self.dirs.borrow_mut();
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
        self.dirs.borrow_mut().remove(&dir);
        self.listings.borrow_mut().remove(&dir);
    }

    fn listing(&self, rel: &[u8]) -> Vec<(u64, Vec<u8>, u32)> {
        let mut out: Vec<(u64, Vec<u8>, u32)> = Vec::new();
        if let Ok(rd) = rargo_net::io::file::read_dir(self.real(rel)) {
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
            let tree = self.virt.borrow();
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
        let tree = self.virt.borrow();
        tree.files.contains_key(rel) || tree.has_below(rel)
    }

    /// Runs `work` on a pool thread with a delivery to fill, one delivery at a time, and then takes what it changed.
    pub fn deliver<T: Send>(&self, names: crate::tmp::Names, work: impl FnOnce(&mut Delivery) -> T + Send) -> T {
        let _turn = self.delivery.lock();
        let generation = self.virt.borrow().generation + 1;
        let mut delivery = Delivery { under: self.under.clone(), generation, edits: Vec::new(), swept: BTreeSet::new(), stale: Vec::new(), names };
        let done = rargo_net::io::cpu(|| work(&mut delivery));
        self.delivered(delivery);
        done
    }

    fn delivered(&self, delivery: Delivery) {
        let Delivery { generation, edits, stale, .. } = delivery;
        let invalidations: Vec<(u64, Vec<u8>)> = {
            let mut tree = self.tree_mut();
            let changed = tree.apply(generation, edits);
            if !changed.is_empty() {
                self.dirty.set(true);
            }
            let mut inodes = self.state.borrow_mut();
            stale.iter().chain(&changed).map(|path| inodes.changed(path)).collect()
        };
        for (parent, _) in &invalidations {
            self.unlist(*parent);
        }
        if let Some(session) = self.session.get()
            && !invalidations.is_empty()
        {
            rargo_net::io::blocking(|| {
                for (parent, name) in &invalidations {
                    let _ = session.notify_inval_entry(*parent, name);
                }
            });
        }
    }
}

impl Filesystem for OutFs {
    fn lookup(&self, _ctx: &Ctx, parent: u64, name: &[u8]) -> Res<Entry> {
        let _t = self.timed("lookup");
        let rel = join(&self.path(parent)?, name);
        loop {
            let edits = self.virt_edits.get();
            let real = rargo_net::io::file::symlink_metadata(self.real(&rel)).ok();
            if self.virt_edits.get() != edits {
                continue;
            }
            return match lookup(&self.virt.borrow(), &rel, real) {
                Some(kind) => Ok(self.entry(parent, name, &kind)),
                None => Ok(Entry::negative(TTL)),
            };
        }
    }

    fn forget(&self, nodeid: u64, nlookup: u64) {
        let _t = self.timed("forget");
        let gone = self.state.borrow_mut().forget(nodeid, nlookup);
        if !gone.is_empty() {
            let mut dirs = self.dirs.borrow_mut();
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
                        v.mtime_ns = rargo_net::io::system_now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as i64).unwrap_or(0)
                    }
                    None => {}
                }
                self.tree_mut().files.insert(rel, v.clone());
                self.dirty.set(true);
                return Ok((self.attr(nodeid, &Kind::Virtual(v)), TTL));
            }
            Kind::Virtual(v) => self.make_real(&rel, &v, set.size != Some(0))?,
            Kind::VDir => rargo_net::io::file::create_dir_all(self.real(&rel)).map_err(errno)?,
            Kind::Real(_) => {}
        }
        let path = self.real(&rel);
        if let Some(size) = set.size {
            let f = rargo_net::io::file::OpenOptions::new().write(true).open(&path).map_err(errno)?;
            f.set_len(size).map_err(errno)?;
        }
        if let Some(mode) = set.mode {
            rargo_net::io::file::set_mode(&path, mode & 0o7777).map_err(errno)?;
        }
        if set.atime.is_some() || set.mtime.is_some() {
            let spec = |t: Option<TimeSet>| match t {
                None => libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_OMIT },
                Some(TimeSet::Now) => libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_NOW },
                Some(TimeSet::At(s, ns)) => libc::timespec { tv_sec: s, tv_nsec: ns as i64 },
            };
            let times = [spec(set.atime), spec(set.mtime)];
            let c = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| Errno(libc::EINVAL))?;
            rargo_net::io::blocking(|| crate::kernel::set_times(&c, &times, libc::AT_SYMLINK_NOFOLLOW)).map_err(errno)?;
        }
        self.getattr(ctx, nodeid, None)
    }

    fn readlink(&self, _ctx: &Ctx, nodeid: u64) -> Res<Vec<u8>> {
        let _t = self.timed("readlink");
        let rel = self.path(nodeid)?;
        rargo_net::io::file::read_link(self.real(&rel)).map(|t| t.as_os_str().as_bytes().to_vec()).map_err(errno)
    }

    fn symlink(&self, _ctx: &Ctx, parent: u64, name: &[u8], target: &[u8]) -> Res<Entry> {
        let _t = self.timed("symlink");
        let rel = join(&self.path(parent)?, name);
        if self.virt.borrow().files.contains_key(&rel) {
            return Err(Errno(libc::EEXIST));
        }
        self.make_parents(&rel)?;
        rargo_net::io::file::symlink(std::ffi::OsStr::from_bytes(target), self.real(&rel)).map_err(errno)?;
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
        if self.virt.borrow().files.contains_key(&rel) {
            return Err(Errno(libc::EEXIST));
        }
        self.make_parents(&rel)?;
        rargo_net::io::file::OpenOptions::new().write(true).create_new(true).mode(mode & 0o7777).open(self.real(&rel)).map_err(errno)?;
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
        let make = || rargo_net::io::file::DirBuilder::new().mode(mode & 0o7777).create(self.real(&rel));
        match make() {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.make_parents(&rel)?;
                make().map_err(errno)?
            }
            r => r.map_err(errno)?,
        }
        let md = rargo_net::io::file::symlink_metadata(self.real(&rel)).map_err(errno)?;
        self.patch(parent, Patch::Add(name.to_vec(), libc::DT_DIR as u32));
        Ok(self.entry(parent, name, &Kind::Real(md)))
    }

    fn unlink(&self, _ctx: &Ctx, parent: u64, name: &[u8]) -> Res<()> {
        let _t = self.timed("unlink");
        let rel = join(&self.path(parent)?, name);
        let real = rargo_net::io::file::remove_file(self.real(&rel));
        let listed = self.virt.borrow().files.contains_key(&rel);
        let virt = listed && self.tree_mut().files.remove(&rel).is_some();
        if virt {
            self.dirty.set(true);
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
        match rargo_net::io::file::remove_dir(self.real(&rel)) {
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
            let st = self.state.borrow_mut();
            (join(&st.path(parent)?, name), join(&st.path(newparent)?, newname))
        };
        let plain = flags == 0 && !self.touches_virtual(&from) && !self.touches_virtual(&to);
        if plain {
            let go = || rargo_net::io::file::rename(self.real(&from), self.real(&to));
            match go() {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && rargo_net::io::file::symlink_metadata(self.real(&from)).is_ok() => {
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
                rargo_net::io::file::rename(self.real(&from), self.real(&to)).map_err(errno)?;
                if self.virt.borrow().files.contains_key(&to) {
                    self.tree_mut().files.remove(&to);
                    self.dirty.set(true);
                }
            }
            Kind::Virtual(v) => {
                match &dst {
                    Some(Kind::Real(md)) if md.is_dir() => return Err(Errno(libc::EISDIR)),
                    Some(Kind::VDir) => return Err(Errno(libc::EISDIR)),
                    Some(Kind::Real(_)) => rargo_net::io::file::remove_file(self.real(&to)).map_err(errno)?,
                    _ => {}
                }
                let mut tree = self.tree_mut();
                let virt = &mut tree.files;
                virt.remove(&from);
                virt.insert(to.clone(), v.clone());
                self.dirty.set(true);
            }
            Kind::VDir => match &dst {
                Some(Kind::Real(md)) if md.is_dir() => rargo_net::io::file::remove_dir(self.real(&to)).map_err(errno)?,
                Some(Kind::Real(_)) | Some(Kind::Virtual(_)) => return Err(Errno(libc::ENOTDIR)),
                Some(Kind::VDir) => return Err(Errno(libc::ENOTEMPTY)),
                None => {}
            },
        }
        if self.has_virtual_below(&from) {
            let mut tree = self.tree_mut();
            let virt = &mut tree.files;
            let moved: Vec<Vec<u8>> = virt.range(from_below.clone()..).take_while(|(k, _)| k.starts_with(&from_below)).map(|(k, _)| k.clone()).collect();
            for k in moved {
                if let Some(v) = virt.remove(&k) {
                    let mut nk = to_below.clone();
                    nk.extend_from_slice(&k[from_below.len()..]);
                    virt.insert(nk, v);
                }
            }
            self.dirty.set(true);
        }
        if plain {
            let t = match rargo_net::io::file::symlink_metadata(self.real(&to)) {
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
        let mut st = self.state.borrow_mut();
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
            let st = self.state.borrow_mut();
            (st.path(nodeid)?, join(&st.path(newparent)?, newname))
        };
        if !self.touches_virtual(&to) && !self.virt.borrow().files.contains_key(&from) {
            let go = || rargo_net::io::file::hard_link(self.real(&from), self.real(&to));
            match go() {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    self.make_parents(&to)?;
                    go().map_err(errno)?
                }
                r => r.map_err(errno)?,
            }
            let md = rargo_net::io::file::symlink_metadata(self.real(&to)).map_err(errno)?;
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
                rargo_net::io::file::hard_link(self.real(&from), self.real(&to)).map_err(errno)?;
            }
            Kind::Virtual(v) => {
                self.tree_mut().files.insert(to.clone(), v);
                self.dirty.set(true);
            }
            Kind::VDir => return Err(Errno(libc::EPERM)),
        }
        let kind = self.kind(&to).ok_or(Errno(libc::EIO))?;
        self.patch(newparent, Patch::Add(newname.to_vec(), libc::DT_REG as u32));
        Ok(self.entry(newparent, newname, &kind))
    }

    fn open(&self, ctx: &Ctx, nodeid: u64, flags: u32) -> Res<Opened> {
        let _t = self.timed("open");
        let rel = self.path(nodeid)?;
        let acc = flags as i32 & libc::O_ACCMODE;
        let keep = flags as i32 & (libc::O_TRUNC | libc::O_APPEND | libc::O_NOFOLLOW);
        match Self::open_wide(&self.real(&rel), acc, keep, None) {
            Ok(file) => return self.open_handle(ctx, nodeid, Content::Open(Rc::new(file)), How::Real),
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(errno(e)),
            Err(_) => {}
        }
        let truncate = keep & libc::O_TRUNC != 0;
        let plan = open_virtual(&self.virt.borrow(), &rel, acc != libc::O_RDONLY || truncate, truncate)?;
        match plan {
            VirtualOpen::Empty => {
                bump(&self.opened_virtual, 1);
                self.open_handle(ctx, nodeid, Content::Open(Rc::new(File::open("/dev/null").map_err(errno)?)), How::Daemon)
            }
            VirtualOpen::Read(file) => {
                bump(&self.opened_virtual, 1);
                if let Some(cached) = self.source.cached(&file.hash, file.size).and_then(|p| File::open(p).ok()) {
                    return self.open_handle(ctx, nodeid, Content::Open(Rc::new(cached)), How::Content);
                }
                self.open_handle(ctx, nodeid, Content::Unfetched(Fetch { hash: file.hash, size: file.size, rel }), How::Daemon)
            }
            VirtualOpen::Materialize { file, with_content } => {
                if self.state.borrow_mut().nodes.get(&nodeid).is_some_and(|n| n.backing.is_some()) {
                    return Err(Errno(libc::ETXTBSY));
                }
                self.make_real(&rel, &file, with_content)?;
                let file = Self::open_wide(&self.real(&rel), acc, keep, None).map_err(errno)?;
                self.open_handle(ctx, nodeid, Content::Open(Rc::new(file)), How::Real)
            }
        }
    }

    fn create(&self, ctx: &Ctx, parent: u64, name: &[u8], mode: u32, flags: u32) -> Res<(Entry, Opened)> {
        let _t = self.timed("create");
        let rel = join(&self.path(parent)?, name);
        let had_virtual = {
            let tree = self.virt.borrow();
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
        if had_virtual && self.tree_mut().files.remove(&rel).is_some() {
            self.dirty.set(true);
            self.detach(parent, name);
        }
        let md = file.metadata().map_err(errno)?;
        self.patch(parent, Patch::Add(name.to_vec(), libc::DT_REG as u32));
        let entry = self.entry(parent, name, &Kind::Real(md));
        Ok((entry, self.open_handle(ctx, entry.nodeid, Content::Open(Rc::new(file)), How::Real)?))
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

    fn registered(&self, ctx: &Ctx, fh: u64, id: Res<i32>) -> Res<Opened> {
        let content = self.registering.borrow_mut().remove(&fh).ok_or(Errno(libc::EIO))?;
        let node = self.handles.borrow_mut().get(&fh).map(|h| h.node).ok_or(Errno(libc::EIO))?;
        let mut st = self.state.borrow_mut();
        let through = match (st.nodes.get_mut(&node), id) {
            (Some(n), Ok(id)) => match &mut n.backing {
                None if n.cached == 0 => {
                    n.backing = Some((id, 1, content));
                    Some(id)
                }
                Some((held, refs, c)) if *c == content && n.cached == 0 => {
                    *refs += 1;
                    ctx.effects.unregister(id);
                    Some(*held)
                }
                Some((_, _, c)) if *c != content => {
                    ctx.effects.unregister(id);
                    drop(st);
                    let closed = self.handles.borrow_mut().remove(&fh);
                    drop(closed);
                    return Err(Errno(libc::ETXTBSY));
                }
                _ => {
                    ctx.effects.unregister(id);
                    None
                }
            },
            (None, Ok(id)) => {
                ctx.effects.unregister(id);
                None
            }
            (_, Err(_)) => None,
        };
        if through.is_none()
            && let Some(n) = st.nodes.get_mut(&node)
        {
            n.cached += 1;
        }
        drop(st);
        if let Some(h) = self.handles.borrow_mut().get_mut(&fh) {
            h.through = through.is_some();
        }
        Ok(match through {
            Some(id) => Opened { fh, flags: FOPEN_PASSTHROUGH, backing_id: id },
            None => Opened { fh, flags: FOPEN_KEEP_CACHE, backing_id: 0 },
        })
    }

    fn release_in(&self, ctx: &Ctx, _nodeid: u64, fh: u64, _flags: u32) {
        let _t = self.timed("release");
        let closed = self.handles.borrow_mut().remove(&fh);
        let Some(h) = closed else { return };
        let mut st = self.state.borrow_mut();
        if let Some(n) = st.nodes.get_mut(&h.node) {
            if !h.through {
                n.cached = n.cached.saturating_sub(1);
            } else if let Some((id, refs, _)) = &mut n.backing {
                *refs -= 1;
                if *refs == 0 {
                    ctx.effects.unregister(*id);
                    n.backing = None;
                }
            }
        }
    }

    fn fsync(&self, _ctx: &Ctx, _nodeid: u64, fh: u64, data_sync: bool) -> Res<()> {
        let _t = self.timed("fsync");
        let Some(file) = self.handle_file(fh)? else { return Ok(()) };
        if data_sync { file.sync_data() } else { file.sync_all() }.map_err(errno)
    }

    fn readdir(&self, _ctx: &Ctx, nodeid: u64, _fh: u64, offset: u64, sink: &mut DirSink) -> Res<()> {
        let _t = self.timed("readdir");
        {
            let dirs = self.dirs.borrow_mut();
            if let Some(live) = dirs.get(&nodeid).and_then(|d| d.live.as_ref()) {
                for (cookie, (name, t)) in live.range(offset + 1..) {
                    if !sink.push(u64::MAX - 1, *cookie, *t, name) {
                        break;
                    }
                }
                return Ok(());
            }
        }
        let cached = if offset == 0 { None } else { self.listings.borrow_mut().get(&nodeid).filter(|(at, _)| rargo_net::io::now().saturating_duration_since(*at) < LISTING_KEPT).map(|(_, l)| l.clone()) };
        let listing = match cached {
            Some(l) => l,
            None => {
                let mine = {
                    let mut dirs = self.dirs.borrow_mut();
                    let d = dirs.entry(nodeid).or_default();
                    if d.building.is_none() {
                        d.building = Some(Vec::new());
                        true
                    } else {
                        false
                    }
                };
                let l = Rc::new(self.listing(&self.path(nodeid)?));
                if mine {
                    let mut dirs = self.dirs.borrow_mut();
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
                let mut all = self.listings.borrow_mut();
                if all.len() > 256 {
                    all.retain(|_, (at, _)| rargo_net::io::now().saturating_duration_since(*at) < LISTING_KEPT);
                }
                all.insert(nodeid, (rargo_net::io::now(), l.clone()));
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
        let s = rargo_net::io::blocking(|| crate::kernel::statvfs(&c)).map_err(errno)?;
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
    use expect_test::expect;

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

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % n as u64) as usize
        }
    }

    type Planned = Vec<(u8, Vec<u8>, Option<VFile>)>;
    type Opened = (Digest, Content<Vec<u8>, Vec<u8>>);

    const PATHS: [&str; 8] = ["a", "a/b", "a/c", "a/b/x", "d/e/f", "d/e/g", "d", "h"];

    fn bytes_of(path: &str, generation: u64, n: usize) -> Vec<u8> {
        format!("{path}@{generation};").repeat(n + 1).into_bytes()
    }

    fn model_apply(model: &mut Table, edits: &Planned) {
        for (kind, path, file) in edits {
            let under = |key: &Vec<u8>| key.len() > path.len() && key.starts_with(path) && key[path.len()] == b'/';
            match kind {
                0 => {
                    model.retain(|key, _| !under(key) && !(path.len() > key.len() && path.starts_with(key) && path[key.len()] == b'/'));
                    model.insert(path.clone(), file.clone().unwrap());
                }
                1 => model.retain(|key, _| key != path && !under(key)),
                _ => {
                    model.remove(path);
                }
            }
        }
    }

    fn walk(inodes: &mut Inodes, path: &[u8]) -> Vec<u64> {
        let mut parent = ROOT;
        let mut ids = Vec::new();
        for part in path.split(|&b| b == b'/') {
            parent = inodes.lookup(parent, part);
            ids.push(parent);
        }
        ids
    }

    mod generated_deliveries_reads_and_concurrent_fetches_return_only_proven_bytes_of_the_newest_tree {
        use super::generated_deliveries;

        rargo_machine::test_batches! {
            generated_deliveries;
            seeds_1_to_30: 1..=30,
            seeds_31_to_60: 31..=60,
            seeds_61_to_90: 61..=90,
            seeds_91_to_120: 91..=120,
            seeds_121_to_150: 121..=150,
            seeds_151_to_180: 151..=180,
            seeds_181_to_210: 181..=210,
            seeds_211_to_240: 211..=240,
            seeds_241_to_270: 241..=270,
            seeds_271_to_300: 271..=300,
        }
    }

    fn generated_deliveries(seeds: std::ops::RangeInclusive<u64>) {
        let (mut reads, mut refused, mut stale) = (0, 0, 0);
        for seed in seeds {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let mut tree = OutputTree { files: Table::new(), generation: 0 };
            let mut model = Table::new();
            let mut nodes = BTreeMap::new();
            nodes.insert(ROOT, Node { parent: 0, name: Vec::new(), nlookup: 1, kids: 0, cached: 0, backing: None });
            let mut inodes = Inodes { nodes, children: BTreeMap::new(), next: 2 };
            let mut store: BTreeMap<Digest, Vec<u8>> = BTreeMap::new();
            let mut next_generation = 0u64;
            let mut applied = 0u64;
            let mut deliveries: Vec<(u64, Planned)> = Vec::new();
            let mut handles: Vec<Opened> = Vec::new();
            let mut fetches: Vec<(usize, Fetch, Vec<u8>, bool)> = Vec::new();
            let mut seen: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
            for step in 0..400 {
                match rng.below(10) {
                    0 | 1 if deliveries.len() < 3 => {
                        next_generation += 1;
                        let edits = (0..1 + rng.below(4))
                            .map(|_| {
                                let path = PATHS[rng.below(PATHS.len())];
                                let kind = [0u8, 0, 0, 1, 2][rng.below(5)];
                                let file = (kind == 0).then(|| {
                                    let bytes = bytes_of(path, next_generation, rng.below(4));
                                    let hash = *blake3::hash(&bytes).as_bytes();
                                    let file = VFile { hash, size: bytes.len() as u64, exec: rng.below(2) == 0, mtime_ns: next_generation as i64 };
                                    store.insert(hash, bytes);
                                    file
                                });
                                (kind, path.as_bytes().to_vec(), file)
                            })
                            .collect();
                        deliveries.push((next_generation, edits));
                    }
                    2 | 3 if !deliveries.is_empty() => {
                        let (generation, edits) = deliveries.remove(rng.below(deliveries.len()));
                        let as_edits = edits
                            .iter()
                            .map(|(kind, path, file)| match kind {
                                0 => Edit::Put { path: path.clone(), file: file.clone().unwrap(), replaced_real: rng.below(4) == 0 },
                                1 => Edit::Remove(path.clone()),
                                _ => Edit::Shadow(path.clone()),
                            })
                            .collect();
                        let changed = tree.apply(generation, as_edits);
                        rargo_machine::check(&tree, format_args!("seed {seed} step {step}"));
                        if generation > applied {
                            applied = generation;
                            model_apply(&mut model, &edits);
                        } else {
                            stale += 1;
                            assert!(changed.is_empty(), "seed {seed} step {step}: a delivery older than the tree changes nothing");
                        }
                        assert_eq!(tree.generation, applied, "seed {seed} step {step}");
                        for path in &changed {
                            inodes.changed(path);
                            rargo_machine::check(&inodes, format_args!("seed {seed} step {step}"));
                            if let Some(before) = seen.remove(path) {
                                let now = *walk(&mut inodes, path).last().unwrap();
                                inodes.invariants();
                                assert_ne!(now, before, "seed {seed} step {step}: a changed name gets a new inode");
                                inodes.forget(now, 1);
                                inodes.invariants();
                            }
                        }
                    }
                    4 => {
                        let path = PATHS[rng.below(PATHS.len())].as_bytes();
                        let ids = walk(&mut inodes, path);
                        rargo_machine::check(&inodes, format_args!("seed {seed} step {step}"));
                        assert_eq!(inodes.path(*ids.last().unwrap()).ok().unwrap(), path);
                        seen.insert(path.to_vec(), *ids.last().unwrap());
                        if rng.below(2) == 0 {
                            for id in ids.iter().rev() {
                                inodes.forget(*id, 1);
                                rargo_machine::check(&inodes, format_args!("seed {seed} step {step}"));
                            }
                            seen.remove(path);
                        }
                    }
                    5 => {
                        let path = PATHS[rng.below(PATHS.len())].as_bytes();
                        if let Ok(VirtualOpen::Read(file)) = open_virtual(&tree, path, false, false) {
                            assert_eq!(model.get(path).map(|f| f.hash), Some(file.hash), "seed {seed} step {step}: an open sees the newest tree");
                            let content = Content::Unfetched(Fetch { hash: file.hash, size: file.size, rel: path.to_vec() });
                            content.invariants();
                            handles.push((file.hash, content));
                        }
                    }
                    6 if !handles.is_empty() => {
                        let h = rng.below(handles.len());
                        if let ReadPlan::Fetch(request) = handles[h].1.read() {
                            let served = match rng.below(4) {
                                0 => store.values().nth(rng.below(store.len())).unwrap().clone(),
                                1 => {
                                    let mut bytes = store[&request.hash].clone();
                                    bytes[0] ^= 1;
                                    bytes
                                }
                                _ => store[&request.hash].clone(),
                            };
                            fetches.push((h, request, served, rng.below(2) == 0));
                        }
                    }
                    7 | 8 if !fetches.is_empty() => {
                        let (h, request, served, ranged) = fetches.remove(rng.below(fetches.len()));
                        let proven = *blake3::hash(&served).as_bytes();
                        let content = if ranged { Content::Ranged(served) } else { Content::Open(served) };
                        let result = handles[h].1.fetched(&request, &proven, content);
                        rargo_machine::check(&handles[h].1, format_args!("seed {seed} step {step}"));
                        assert_eq!(result.is_err(), proven != request.hash, "seed {seed} step {step}: a completion is refused exactly when its bytes are not the content asked for");
                        refused += usize::from(result.is_err());
                    }
                    _ if !handles.is_empty() => {
                        let h = rng.below(handles.len());
                        let (opened, content) = &handles[h];
                        let bytes = match content.read() {
                            ReadPlan::Whole(bytes) | ReadPlan::Ranged(bytes) => bytes,
                            ReadPlan::Fetch(request) => {
                                assert_eq!(request.hash, *opened, "seed {seed} step {step}: a handle fetches what it opened");
                                continue;
                            }
                        };
                        assert_eq!(blake3::hash(bytes).as_bytes(), opened, "seed {seed} step {step}: a read returns only bytes of the content hash");
                        reads += 1;
                    }
                    _ => {}
                }
                rargo_machine::check(&tree, format_args!("seed {seed} step {step}"));
                rargo_machine::check(&inodes, format_args!("seed {seed} step {step}"));
                assert_eq!(tree.files.keys().collect::<Vec<_>>(), model.keys().collect::<Vec<_>>(), "seed {seed} step {step}: the tree is the newest delivery's");
                for path in PATHS {
                    let path = path.as_bytes();
                    let want = model.get(path).map(|file| file.hash);
                    let got = match lookup::<()>(&tree, path, None) {
                        Some(Kind::Virtual(file)) => Some(file.hash),
                        Some(Kind::VDir) => {
                            let prefix = below(path);
                            assert!(model.keys().any(|key| key.starts_with(&prefix)), "seed {seed} step {step}: a directory has something below it");
                            None
                        }
                        _ => None,
                    };
                    assert_eq!(got, want, "seed {seed} step {step}: {}", String::from_utf8_lossy(path));
                }
            }
        }
        assert!(reads > 100 && refused > 10 && stale > 10, "the runs read ({reads}), refuse bad completions ({refused}) and race deliveries ({stale})");
    }

    enum Ev {
        Lookup(u64, &'static str),
        Changed(&'static str),
        Path(u64),
        Forget(u64, u64),
    }

    /// Steps an inode table holding only the root through the events, checking it after each, and lists what
    /// each step gave back.
    fn run(events: impl IntoIterator<Item = Ev>) -> String {
        let mut nodes = BTreeMap::new();
        nodes.insert(ROOT, Node { parent: 0, name: Vec::new(), nlookup: 1, kids: 0, cached: 0, backing: None });
        let mut state = Inodes { nodes, children: BTreeMap::new(), next: 2 };
        state.invariants();
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        let mut out = String::new();
        for ev in events {
            let line = match ev {
                Ev::Lookup(parent, name) => format!("lookup {parent} {name} => {}", state.lookup(parent, name.as_bytes())),
                Ev::Changed(path) => {
                    let (parent, name) = state.changed(path.as_bytes());
                    format!("changed {path} => detached {} from {parent}", text(&name))
                }
                Ev::Path(node) => format!("path {node} => {:?}", state.path(node).map(|p| text(&p)).map_err(|e| e.0)),
                Ev::Forget(node, n) => format!("forget {node} {n} => {:?}", state.forget(node, n)),
            };
            state.invariants();
            out += &line;
            out.push('\n');
        }
        out
    }

    #[test]
    fn replacing_output_detaches_the_name_without_reusing_its_inode() {
        let events = [
            Ev::Lookup(ROOT, "app"),
            Ev::Changed("app"),
            Ev::Lookup(ROOT, "app"),
            Ev::Path(3),
            Ev::Path(2),
            Ev::Forget(2, 1),
            Ev::Lookup(ROOT, "app"),
            Ev::Forget(3, 2),
            Ev::Lookup(ROOT, "app"),
        ];
        expect![[r#"
            lookup 1 app => 2
            changed app => detached app from 1
            lookup 1 app => 3
            path 3 => Ok("app")
            path 2 => Ok("app")
            forget 2 1 => [2]
            lookup 1 app => 3
            forget 3 2 => [3]
            lookup 1 app => 4
        "#]].assert_eq(&run(events));
    }
}
