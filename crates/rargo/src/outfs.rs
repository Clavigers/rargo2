//! The output directory as a virtual file system.
//!
//! The daemon mounts this over a worktree's `target/`. What the server
//! built appears in it as soon as the server says so, as names with a size
//! and a content hash; the content is fetched into the client's cache when
//! something opens the file, and never copied into the worktree. Everything
//! else (whatever stock cargo or anything else writes there) goes to the
//! real directory underneath the mount, which this file system shows
//! through. Unmounted, `target/` is that real directory: an ordinary cargo
//! output directory that lacks only what was never opened for writing.
//!
//! A name is real if the directory underneath has it, else virtual if the
//! server delivered it, else absent. Writing to a virtual file first makes
//! it real (its content is fetched and copied). Removing or renaming a
//! virtual file changes the table of delivered names; nothing is hidden
//! behind whiteouts.

use fusewire::abi::*;
use fusewire::{Ctx, DirSink, Entry, Errno, Filesystem, Opened, Res, Session, SetAttr, TimeSet};
use rargo_proto::Digest;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

const TTL: Duration = Duration::from_secs(3600);
const ROOT: u64 = 1;

/// A delivered file that has not been made real.
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
    /// Handles open on it, by how they do their I/O: the kernel does not
    /// let one file be open both ways.
    cached: u32,
    /// The kernel's handle on the real file behind this node while it is
    /// open for passthrough, and how many opens share it: the kernel
    /// refuses a second, different one.
    backing: Option<(i32, u32)>,
}

struct State {
    nodes: HashMap<u64, Node>,
    children: HashMap<(u64, Vec<u8>), u64>,
    next: u64,
    /// Delivered files by path relative to the mount.
    virt: BTreeMap<Vec<u8>, VFile>,
}

struct Handle {
    file: File,
    node: u64,
    through: bool,
}

/// Makes sure a content is in the client's cache and says where.
pub type Fetcher = Box<dyn Fn(&Digest, u64) -> std::io::Result<PathBuf> + Send + Sync>;

pub struct OutFs {
    /// The real directory under the mount, reached through a descriptor
    /// opened before mounting.
    under: PathBuf,
    _under_fd: File,
    state: Mutex<State>,
    handles: Mutex<HashMap<u64, Handle>>,
    next_fh: AtomicU64,
    fetch: Fetcher,
    session: OnceLock<Arc<Session>>,
    table: PathBuf,
    dirty: AtomicBool,
    /// Directories already cleared of dead clients' temporaries.
    swept: Mutex<std::collections::HashSet<Vec<u8>>>,
    uid: u32,
    gid: u32,
    pub opened_virtual: AtomicU64,
    pub made_real: AtomicU64,
}

fn errno(e: std::io::Error) -> Errno {
    Errno(e.raw_os_error().unwrap_or(libc::EIO))
}

fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
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

/// A name's place in its directory's listing: 63 bits of a hash of it,
/// never zero.
fn name_cookie(name: &[u8]) -> u64 {
    let h = blake3::hash(name);
    (u64::from_le_bytes(h.as_bytes()[..8].try_into().unwrap()) >> 1).max(1)
}

enum Kind {
    Real(std::fs::Metadata),
    Virtual(VFile),
    /// A directory that exists only because delivered files are below it.
    VDir,
}

impl OutFs {
    /// `dir` is the directory to mount over; `table` is where the list of
    /// delivered names is kept between runs of the daemon.
    pub fn new(dir: &Path, table: &Path, fetch: Fetcher) -> std::io::Result<Arc<OutFs>> {
        let fd = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_PATH).open(dir)?;
        let under = PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd()));
        let virt: BTreeMap<Vec<u8>, VFile> = std::fs::read(table)
            .ok()
            .and_then(|raw| zstd::stream::decode_all(&raw[..]).ok())
            .and_then(|plain| bincode::deserialize(&plain).ok())
            .unwrap_or_default();
        let mut nodes = HashMap::new();
        nodes.insert(ROOT, Node { parent: 0, name: Vec::new(), nlookup: 1, kids: 0, cached: 0, backing: None });
        // SAFETY: no arguments, cannot fail.
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        Ok(Arc::new(OutFs {
            under,
            _under_fd: fd,
            state: Mutex::new(State { nodes, children: HashMap::new(), next: 2, virt }),
            handles: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
            fetch,
            session: OnceLock::new(),
            table: table.to_path_buf(),
            dirty: AtomicBool::new(false),
            swept: Mutex::new(Default::default()),
            uid,
            gid,
            opened_virtual: AtomicU64::new(0),
            made_real: AtomicU64::new(0),
        }))
    }

    pub fn attach(&self, session: Arc<Session>) {
        let _ = self.session.set(session);
    }

    /// Writes the table of delivered names if it changed.
    pub fn save(&self) {
        if !self.dirty.swap(false, Ordering::SeqCst) {
            return;
        }
        let plain = bincode::serialize(&self.state.lock().unwrap().virt).unwrap_or_default();
        if let Ok(packed) = zstd::stream::encode_all(&plain[..], 1) {
            let _ = crate::write_atomic(&self.table, &packed);
        }
    }

    pub fn virtual_count(&self) -> usize {
        self.state.lock().unwrap().virt.len()
    }

    fn real(&self, rel: &[u8]) -> PathBuf {
        // (`under` is a link in /proc; "." makes it the directory.)
        if rel.is_empty() { self.under.join(".") } else { self.under.join(std::ffi::OsStr::from_bytes(rel)) }
    }

    fn path_of(st: &State, mut id: u64) -> Res<Vec<u8>> {
        let mut parts: Vec<&[u8]> = Vec::new();
        while id != ROOT {
            let n = st.nodes.get(&id).ok_or(Errno(libc::ESTALE))?;
            parts.push(&n.name);
            id = n.parent;
        }
        parts.reverse();
        Ok(parts.join(&b'/'))
    }

    fn kind(&self, st: &State, rel: &[u8]) -> Option<Kind> {
        if let Ok(md) = std::fs::symlink_metadata(self.real(rel)) {
            return Some(Kind::Real(md));
        }
        if rel.is_empty() {
            return None;
        }
        if let Some(v) = st.virt.get(rel) {
            return Some(Kind::Virtual(v.clone()));
        }
        let prefix = below(rel);
        st.virt.range(prefix.clone()..).next().filter(|(k, _)| k.starts_with(&prefix)).map(|_| Kind::VDir)
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

    fn node_for(st: &mut State, parent: u64, name: &[u8]) -> u64 {
        if let Some(id) = st.children.get(&(parent, name.to_vec())) {
            let id = *id;
            if let Some(n) = st.nodes.get_mut(&id) {
                n.nlookup += 1;
            }
            return id;
        }
        let id = st.next;
        st.next += 1;
        st.nodes.insert(id, Node { parent, name: name.to_vec(), nlookup: 1, kids: 0, cached: 0, backing: None });
        st.children.insert((parent, name.to_vec()), id);
        if let Some(p) = st.nodes.get_mut(&parent) {
            p.kids += 1;
        }
        id
    }

    /// The name no longer leads to the node it led to (removed, replaced):
    /// the next lookup gets a new node.
    fn detach(&self, st: &mut State, parent: u64, name: &[u8]) {
        st.children.remove(&(parent, name.to_vec()));
    }

    fn entry(&self, st: &mut State, parent: u64, name: &[u8], kind: &Kind) -> Entry {
        let id = Self::node_for(st, parent, name);
        Entry { nodeid: id, generation: 0, attr: self.attr(id, kind), entry_ttl: TTL, attr_ttl: TTL }
    }

    fn make_parents(&self, rel: &[u8]) -> Res<()> {
        if let Some(i) = rel.iter().rposition(|&b| b == b'/') {
            std::fs::create_dir_all(self.real(&rel[..i])).map_err(errno)?;
        }
        Ok(())
    }

    /// Turns the virtual file at `rel` into a real one, with its content
    /// unless the caller is about to truncate it.
    fn make_real(&self, rel: &[u8], v: &VFile, with_content: bool) -> Res<()> {
        self.make_parents(rel)?;
        let dst = self.real(rel);
        let mode = if v.exec { 0o755 } else { 0o644 };
        if with_content && v.size > 0 {
            let src = (self.fetch)(&v.hash, v.size).map_err(errno)?;
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
        self.state.lock().unwrap().virt.remove(rel);
        self.dirty.store(true, Ordering::SeqCst);
        self.made_real.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Registers an open file. `real` is the real file's path when the
    /// kernel may do the I/O itself (passthrough).
    fn open_handle(&self, node: u64, file: File, real: Option<&Path>) -> Opened {
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        let mut st = self.state.lock().unwrap();
        let mut backing_id = None;
        if let (Some(n), Some(path), Some(s)) = (st.nodes.get_mut(&node), real, self.session.get()) {
            if n.cached == 0 && s.caps().passthrough {
                match &mut n.backing {
                    Some((id, refs)) => {
                        *refs += 1;
                        backing_id = Some(*id);
                    }
                    None => {
                        // Opened for everything a later open may want:
                        // all opens of the file share this one.
                        let both = std::fs::OpenOptions::new().read(true).write(true).open(path);
                        let fd = both.or_else(|_| File::open(path));
                        if let Some(id) = fd.ok().and_then(|f| s.backing_open(f.as_raw_fd()).ok()) {
                            n.backing = Some((id, 1));
                            backing_id = Some(id);
                        }
                    }
                }
            }
        }
        if backing_id.is_none() {
            if let Some(n) = st.nodes.get_mut(&node) {
                n.cached += 1;
            }
        }
        drop(st);
        self.handles.lock().unwrap().insert(fh, Handle { file, node, through: backing_id.is_some() });
        match backing_id {
            Some(id) => Opened { fh, flags: FOPEN_PASSTHROUGH, backing_id: id },
            None => Opened { fh, flags: FOPEN_KEEP_CACHE, backing_id: 0 },
        }
    }

    fn inval(&self, parent: u64, name: &[u8]) {
        if let Some(s) = self.session.get() {
            let _ = s.notify_inval_entry(parent, name);
        }
    }

    /// The node of the directory `rel`, if the kernel knows it.
    fn known_dir(st: &State, rel: &[u8]) -> Option<u64> {
        let mut id = ROOT;
        if rel.is_empty() {
            return Some(id);
        }
        for part in rel.split(|&b| b == b'/') {
            id = *st.children.get(&(id, part.to_vec()))?;
        }
        Some(id)
    }

    /// The name at `rel` now means something else: forget what the kernel
    /// remembers of it.
    fn changed(&self, rel: &[u8]) {
        let (dir, name) = match rel.iter().rposition(|&b| b == b'/') {
            Some(i) => (&rel[..i], &rel[i + 1..]),
            None => (&b""[..], rel),
        };
        let parent = {
            let mut st = self.state.lock().unwrap();
            let Some(parent) = Self::known_dir(&st, dir) else { return };
            self.detach(&mut st, parent, name);
            parent
        };
        self.inval(parent, name);
    }

    // -- what the server delivers -----------------------------------------

    /// A file the server built: it appears at `rel` now, its content
    /// stays where it is until something opens it.
    pub fn deliver(&self, rel: &[u8], v: VFile) {
        self.sweep_beside(rel);
        {
            let st = self.state.lock().unwrap();
            if st.virt.get(rel).is_some_and(|old| old.hash == v.hash && old.exec == v.exec) && std::fs::symlink_metadata(self.real(rel)).is_err() {
                return;
            }
        }
        // A real file of that name is what an earlier build left: replaced.
        match std::fs::symlink_metadata(self.real(rel)) {
            Ok(md) if md.is_dir() => {
                let _ = std::fs::remove_dir_all(self.real(rel));
            }
            Ok(_) => {
                let _ = std::fs::remove_file(self.real(rel));
            }
            Err(_) => {}
        }
        self.state.lock().unwrap().virt.insert(rel.to_vec(), v);
        self.dirty.store(true, Ordering::SeqCst);
        self.changed(rel);
    }

    /// A new delivery begins: directories are swept once per delivery.
    pub fn begin_delivery(&self) {
        self.swept.lock().unwrap().clear();
    }

    /// The first time a delivery puts something into a directory: removes
    /// the temporaries that copying clients which no longer exist left in
    /// it (`.rargo-tmp-<pid>-...`).
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
            // SAFETY: signal 0 only checks for existence.
            let alive = unsafe { libc::kill(pid, 0) } == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
            if !alive && std::fs::remove_file(e.path()).is_ok() {
                self.changed(&join(&dir, name.as_bytes()));
            }
        }
    }

    /// Whether `rel` already shows this content.
    pub fn shows(&self, rel: &[u8], hash: &Digest) -> bool {
        let st = self.state.lock().unwrap();
        st.virt.get(rel).is_some_and(|v| v.hash == *hash) && std::fs::symlink_metadata(self.real(rel)).is_err()
    }

    /// Removes whatever is at `rel`, real or delivered.
    pub fn remove(&self, rel: &[u8]) {
        match std::fs::symlink_metadata(self.real(rel)) {
            Ok(md) if md.is_dir() => {
                let _ = std::fs::remove_dir_all(self.real(rel));
            }
            Ok(_) => {
                let _ = std::fs::remove_file(self.real(rel));
            }
            Err(_) => {}
        }
        {
            let mut st = self.state.lock().unwrap();
            let prefix = below(rel);
            let doomed: Vec<Vec<u8>> = st.virt.range(rel.to_vec()..).take_while(|(k, _)| *k == rel || k.starts_with(&prefix)).map(|(k, _)| k.clone()).collect();
            for k in doomed {
                st.virt.remove(&k);
            }
        }
        self.dirty.store(true, Ordering::SeqCst);
        self.changed(rel);
    }

    /// The real path for something the daemon writes itself (a small
    /// generated file, a symlink, a directory); the caller writes there and
    /// then calls [`OutFs::wrote`].
    pub fn real_for_write(&self, rel: &[u8]) -> std::io::Result<PathBuf> {
        if let Some(i) = rel.iter().rposition(|&b| b == b'/') {
            std::fs::create_dir_all(self.real(&rel[..i]))?;
        }
        Ok(self.real(rel))
    }

    pub fn wrote(&self, rel: &[u8]) {
        if self.state.lock().unwrap().virt.remove(rel).is_some() {
            self.dirty.store(true, Ordering::SeqCst);
        }
        self.changed(rel);
    }
}

impl Filesystem for OutFs {
    fn lookup(&self, _ctx: &Ctx, parent: u64, name: &[u8]) -> Res<Entry> {
        let mut st = self.state.lock().unwrap();
        let rel = join(&Self::path_of(&st, parent)?, name);
        let kind = self.kind(&st, &rel).ok_or(Errno(libc::ENOENT))?;
        Ok(self.entry(&mut st, parent, name, &kind))
    }

    fn forget(&self, nodeid: u64, nlookup: u64) {
        let mut st = self.state.lock().unwrap();
        let mut id = nodeid;
        let mut drop_by = nlookup;
        // A node goes when the kernel has forgotten it and nothing below
        // it is known any more; its parent may then go too.
        while id != ROOT {
            let Some(n) = st.nodes.get_mut(&id) else { break };
            n.nlookup = n.nlookup.saturating_sub(drop_by);
            if n.nlookup > 0 || n.kids > 0 {
                break;
            }
            let (parent, name) = (n.parent, n.name.clone());
            st.nodes.remove(&id);
            if st.children.get(&(parent, name.clone())) == Some(&id) {
                st.children.remove(&(parent, name));
            }
            if let Some(p) = st.nodes.get_mut(&parent) {
                p.kids = p.kids.saturating_sub(1);
            }
            id = parent;
            drop_by = 0;
        }
    }

    fn getattr(&self, _ctx: &Ctx, nodeid: u64, _fh: Option<u64>) -> Res<(fuse_attr, Duration)> {
        let st = self.state.lock().unwrap();
        let rel = Self::path_of(&st, nodeid)?;
        let kind = self.kind(&st, &rel).ok_or(Errno(libc::ENOENT))?;
        Ok((self.attr(nodeid, &kind), TTL))
    }

    fn setattr(&self, ctx: &Ctx, nodeid: u64, set: &SetAttr) -> Res<(fuse_attr, Duration)> {
        let (rel, kind) = {
            let st = self.state.lock().unwrap();
            let rel = Self::path_of(&st, nodeid)?;
            let kind = self.kind(&st, &rel).ok_or(Errno(libc::ENOENT))?;
            (rel, kind)
        };
        match kind {
            Kind::Virtual(mut v) if set.size.is_none() => {
                // Times and the executable bit are kept in the table.
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
                self.state.lock().unwrap().virt.insert(rel, v.clone());
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
            // SAFETY: valid path and a two-element timespec array.
            if unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
                return Err(Errno::last());
            }
        }
        self.getattr(ctx, nodeid, None)
    }

    fn readlink(&self, _ctx: &Ctx, nodeid: u64) -> Res<Vec<u8>> {
        let rel = Self::path_of(&self.state.lock().unwrap(), nodeid)?;
        std::fs::read_link(self.real(&rel)).map(|t| t.as_os_str().as_bytes().to_vec()).map_err(errno)
    }

    fn symlink(&self, _ctx: &Ctx, parent: u64, name: &[u8], target: &[u8]) -> Res<Entry> {
        let rel = join(&Self::path_of(&self.state.lock().unwrap(), parent)?, name);
        if self.state.lock().unwrap().virt.contains_key(&rel) {
            return Err(Errno(libc::EEXIST));
        }
        self.make_parents(&rel)?;
        std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(target), self.real(&rel)).map_err(errno)?;
        let mut st = self.state.lock().unwrap();
        let kind = self.kind(&st, &rel).ok_or(Errno(libc::EIO))?;
        Ok(self.entry(&mut st, parent, name, &kind))
    }

    fn mknod(&self, _ctx: &Ctx, parent: u64, name: &[u8], mode: u32, _rdev: u32) -> Res<Entry> {
        if mode & libc::S_IFMT != libc::S_IFREG {
            return Err(Errno(libc::EPERM));
        }
        let rel = join(&Self::path_of(&self.state.lock().unwrap(), parent)?, name);
        if self.state.lock().unwrap().virt.contains_key(&rel) {
            return Err(Errno(libc::EEXIST));
        }
        self.make_parents(&rel)?;
        std::fs::OpenOptions::new().write(true).create_new(true).mode(mode & 0o7777).open(self.real(&rel)).map_err(errno)?;
        let mut st = self.state.lock().unwrap();
        let kind = self.kind(&st, &rel).ok_or(Errno(libc::EIO))?;
        Ok(self.entry(&mut st, parent, name, &kind))
    }

    fn mkdir(&self, _ctx: &Ctx, parent: u64, name: &[u8], mode: u32) -> Res<Entry> {
        let rel = {
            let st = self.state.lock().unwrap();
            let rel = join(&Self::path_of(&st, parent)?, name);
            if self.kind(&st, &rel).is_some() {
                return Err(Errno(libc::EEXIST));
            }
            rel
        };
        self.make_parents(&rel)?;
        std::fs::DirBuilder::new().mode(mode & 0o7777).create(self.real(&rel)).map_err(errno)?;
        let mut st = self.state.lock().unwrap();
        let kind = self.kind(&st, &rel).ok_or(Errno(libc::EIO))?;
        Ok(self.entry(&mut st, parent, name, &kind))
    }

    fn unlink(&self, _ctx: &Ctx, parent: u64, name: &[u8]) -> Res<()> {
        let mut st = self.state.lock().unwrap();
        let rel = join(&Self::path_of(&st, parent)?, name);
        let real = std::fs::remove_file(self.real(&rel));
        let virt = st.virt.remove(&rel).is_some();
        if virt {
            self.dirty.store(true, Ordering::SeqCst);
        }
        match real {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && virt => {}
            Err(e) => return Err(errno(e)),
        }
        self.detach(&mut st, parent, name);
        Ok(())
    }

    fn rmdir(&self, _ctx: &Ctx, parent: u64, name: &[u8]) -> Res<()> {
        let mut st = self.state.lock().unwrap();
        let rel = join(&Self::path_of(&st, parent)?, name);
        let prefix = below(&rel);
        if st.virt.range(prefix.clone()..).next().is_some_and(|(k, _)| k.starts_with(&prefix)) {
            return Err(Errno(libc::ENOTEMPTY));
        }
        match std::fs::remove_dir(self.real(&rel)) {
            Ok(()) => {}
            // It was a directory only by the delivered files below it,
            // and those are gone.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(errno(e)),
        }
        self.detach(&mut st, parent, name);
        Ok(())
    }

    fn rename(&self, _ctx: &Ctx, parent: u64, name: &[u8], newparent: u64, newname: &[u8], flags: u32) -> Res<()> {
        const NOREPLACE: u32 = 1;
        if flags & !NOREPLACE != 0 {
            return Err(Errno(libc::EINVAL));
        }
        let mut st = self.state.lock().unwrap();
        let from = join(&Self::path_of(&st, parent)?, name);
        let to = join(&Self::path_of(&st, newparent)?, newname);
        let src = self.kind(&st, &from).ok_or(Errno(libc::ENOENT))?;
        let dst = self.kind(&st, &to);
        if flags & NOREPLACE != 0 && dst.is_some() {
            return Err(Errno(libc::EEXIST));
        }
        let (from_below, to_below) = (below(&from), below(&to));
        let dst_has_virtual_below = st.virt.range(to_below.clone()..).next().is_some_and(|(k, _)| k.starts_with(&to_below));
        match &src {
            Kind::Real(md) => {
                if md.is_dir() && dst_has_virtual_below {
                    return Err(Errno(libc::ENOTEMPTY));
                }
                self.make_parents(&to)?;
                std::fs::rename(self.real(&from), self.real(&to)).map_err(errno)?;
                st.virt.remove(&to);
            }
            Kind::Virtual(v) => {
                match &dst {
                    Some(Kind::Real(md)) if md.is_dir() => return Err(Errno(libc::EISDIR)),
                    Some(Kind::VDir) => return Err(Errno(libc::EISDIR)),
                    Some(Kind::Real(_)) => std::fs::remove_file(self.real(&to)).map_err(errno)?,
                    _ => {}
                }
                st.virt.remove(&from);
                st.virt.insert(to.clone(), v.clone());
            }
            Kind::VDir => {
                match &dst {
                    Some(Kind::Real(md)) if md.is_dir() => std::fs::remove_dir(self.real(&to)).map_err(errno)?,
                    Some(Kind::Real(_)) | Some(Kind::Virtual(_)) => return Err(Errno(libc::ENOTDIR)),
                    Some(Kind::VDir) => return Err(Errno(libc::ENOTEMPTY)),
                    None => {}
                }
            }
        }
        // Delivered files below a renamed directory move with it.
        let moved: Vec<Vec<u8>> = st.virt.range(from_below.clone()..).take_while(|(k, _)| k.starts_with(&from_below)).map(|(k, _)| k.clone()).collect();
        for k in moved {
            if let Some(v) = st.virt.remove(&k) {
                let mut nk = to_below.clone();
                nk.extend_from_slice(&k[from_below.len()..]);
                st.virt.insert(nk, v);
            }
        }
        self.dirty.store(true, Ordering::SeqCst);
        // The node moves with the name; what was at the new name is gone.
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
        let mut st = self.state.lock().unwrap();
        let from = Self::path_of(&st, nodeid)?;
        let to = join(&Self::path_of(&st, newparent)?, newname);
        if self.kind(&st, &to).is_some() {
            return Err(Errno(libc::EEXIST));
        }
        match self.kind(&st, &from).ok_or(Errno(libc::ENOENT))? {
            Kind::Real(_) => {
                self.make_parents(&to)?;
                std::fs::hard_link(self.real(&from), self.real(&to)).map_err(errno)?;
            }
            // A second name for the same content.
            Kind::Virtual(v) => {
                st.virt.insert(to.clone(), v);
                self.dirty.store(true, Ordering::SeqCst);
            }
            Kind::VDir => return Err(Errno(libc::EPERM)),
        }
        let kind = self.kind(&st, &to).ok_or(Errno(libc::EIO))?;
        Ok(self.entry(&mut st, newparent, newname, &kind))
    }

    fn open(&self, _ctx: &Ctx, nodeid: u64, flags: u32) -> Res<Opened> {
        let (rel, kind) = {
            let st = self.state.lock().unwrap();
            let rel = Self::path_of(&st, nodeid)?;
            let kind = self.kind(&st, &rel).ok_or(Errno(libc::ENOENT))?;
            (rel, kind)
        };
        let acc = flags as i32 & libc::O_ACCMODE;
        let writing = acc != libc::O_RDONLY || flags as i32 & libc::O_TRUNC != 0;
        if let Kind::Virtual(v) = &kind {
            if !writing {
                // Reading: straight from the client's cache.
                self.opened_virtual.fetch_add(1, Ordering::Relaxed);
                let path = if v.size == 0 {
                    PathBuf::from("/dev/null")
                } else {
                    (self.fetch)(&v.hash, v.size).map_err(|e| {
                        eprintln!("rargo daemon: target/{}: its content could not be fetched: {e}", String::from_utf8_lossy(&rel));
                        errno(e)
                    })?
                };
                let file = File::open(path).map_err(errno)?;
                return Ok(self.open_handle(nodeid, file, None));
            }
            self.make_real(&rel, v, flags as i32 & libc::O_TRUNC == 0)?;
        }
        let keep = libc::O_TRUNC | libc::O_APPEND | libc::O_NOFOLLOW;
        let file = std::fs::OpenOptions::new()
            .read(acc != libc::O_WRONLY)
            .write(acc != libc::O_RDONLY)
            .custom_flags(flags as i32 & keep)
            .open(self.real(&rel))
            .map_err(errno)?;
        Ok(self.open_handle(nodeid, file, Some(&self.real(&rel))))
    }

    fn create(&self, _ctx: &Ctx, parent: u64, name: &[u8], mode: u32, flags: u32) -> Res<(Entry, Opened)> {
        let rel = join(&Self::path_of(&self.state.lock().unwrap(), parent)?, name);
        let had_virtual = self.state.lock().unwrap().virt.contains_key(&rel);
        if had_virtual && flags as i32 & libc::O_EXCL != 0 {
            return Err(Errno(libc::EEXIST));
        }
        self.make_parents(&rel)?;
        let acc = flags as i32 & libc::O_ACCMODE;
        let keep = libc::O_TRUNC | libc::O_APPEND | libc::O_EXCL | libc::O_NOFOLLOW;
        let file = std::fs::OpenOptions::new()
            .read(acc != libc::O_WRONLY)
            .write(true)
            .create(true)
            .mode(mode & 0o7777)
            .custom_flags(flags as i32 & keep)
            .open(self.real(&rel))
            .map_err(errno)?;
        let mut st = self.state.lock().unwrap();
        if st.virt.remove(&rel).is_some() {
            // The delivered file of that name is replaced: a new node.
            self.dirty.store(true, Ordering::SeqCst);
            self.detach(&mut st, parent, name);
        }
        let kind = self.kind(&st, &rel).ok_or(Errno(libc::EIO))?;
        let entry = self.entry(&mut st, parent, name, &kind);
        drop(st);
        Ok((entry, self.open_handle(entry.nodeid, file, Some(&self.real(&rel)))))
    }

    fn read(&self, _ctx: &Ctx, _nodeid: u64, fh: u64, offset: u64, size: u32, out: &mut Vec<u8>) -> Res<()> {
        let file = self.handles.lock().unwrap().get(&fh).map(|h| h.file.try_clone()).ok_or(Errno(libc::EBADF))?.map_err(errno)?;
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
        let file = self.handles.lock().unwrap().get(&fh).map(|h| h.file.try_clone()).ok_or(Errno(libc::EBADF))?.map_err(errno)?;
        file.write_all_at(data, offset).map_err(errno)?;
        Ok(data.len() as u32)
    }

    fn release(&self, _nodeid: u64, fh: u64, _flags: u32) {
        let Some(h) = self.handles.lock().unwrap().remove(&fh) else { return };
        let mut st = self.state.lock().unwrap();
        if let Some(n) = st.nodes.get_mut(&h.node) {
            if !h.through {
                n.cached = n.cached.saturating_sub(1);
            } else if let Some((id, refs)) = &mut n.backing {
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
        let file = self.handles.lock().unwrap().get(&fh).map(|h| h.file.try_clone()).ok_or(Errno(libc::EBADF))?.map_err(errno)?;
        if datasync { file.sync_data() } else { file.sync_all() }.map_err(errno)
    }

    fn readdir(&self, _ctx: &Ctx, nodeid: u64, _fh: u64, offset: u64, sink: &mut DirSink) -> Res<()> {
        let st = self.state.lock().unwrap();
        let rel = Self::path_of(&st, nodeid)?;
        // Real names, then delivered ones that are not also real.
        let mut names: BTreeMap<Vec<u8>, u32> = BTreeMap::new();
        if let Ok(rd) = std::fs::read_dir(self.real(&rel)) {
            for e in rd.flatten() {
                let t = match e.file_type() {
                    Ok(t) if t.is_dir() => libc::DT_DIR,
                    Ok(t) if t.is_symlink() => libc::DT_LNK,
                    _ => libc::DT_REG,
                };
                names.insert(e.file_name().as_bytes().to_vec(), t as u32);
            }
        }
        let prefix = if rel.is_empty() { Vec::new() } else { below(&rel) };
        for (k, _) in st.virt.range(prefix.clone()..).take_while(|(k, _)| k.starts_with(&prefix)) {
            let rest = &k[prefix.len()..];
            let (name, t) = match rest.iter().position(|&b| b == b'/') {
                Some(i) => (&rest[..i], libc::DT_DIR),
                None => (rest, libc::DT_REG),
            };
            names.entry(name.to_vec()).or_insert(t as u32);
        }
        // A listing is read in several calls while other processes add
        // and remove files (compilers and their temporaries, in parallel).
        // The place in the listing is therefore not a position, which
        // every removal before it would shift, but a number that belongs
        // to the name: names are listed in the order of a hash of
        // themselves, and a call continues after the hash it was given.
        // A name that is there throughout is listed exactly once.
        let mut listed: Vec<(u64, &Vec<u8>, u32)> = names.iter().map(|(name, t)| (name_cookie(name), name, *t)).collect();
        listed.sort();
        for (cookie, name, t) in listed {
            if cookie <= offset {
                continue;
            }
            let ino = st.children.get(&(nodeid, name.clone())).copied().unwrap_or(u64::MAX - 1);
            if !sink.push(ino, cookie, t, name) {
                break;
            }
        }
        Ok(())
    }

    fn statfs(&self, _ctx: &Ctx, _nodeid: u64) -> Res<fuse_kstatfs> {
        let c = std::ffi::CString::new(self.under.as_os_str().as_bytes()).map_err(|_| Errno(libc::EINVAL))?;
        // SAFETY: valid path and a zeroed statvfs to fill.
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
}
