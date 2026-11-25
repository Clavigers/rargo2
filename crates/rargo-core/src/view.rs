use crate::hash::Hash;
use crate::store::Store;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use crate::io::file::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub type VPath = Arc<[u8]>;

pub fn vjoin(dir: &[u8], name: &[u8]) -> Vec<u8> {
    assert!(dir.starts_with(b"/"), "sandbox paths are absolute: {}", show(dir));
    assert!(dir == b"/" || !dir.ends_with(b"/"), "sandbox paths are normalised: {}", show(dir));
    assert!(!name.is_empty());
    let mut v = Vec::with_capacity(dir.len() + 1 + name.len());
    if dir != b"/" {
        v.extend_from_slice(dir);
    }
    v.push(b'/');
    v.extend_from_slice(name);
    assert!(vpath_starts_with(&v, dir));
    v
}

pub fn vsplit(path: &[u8]) -> Option<(&[u8], &[u8])> {
    if path == b"/" || path.is_empty() {
        return None;
    }
    let i = path.iter().rposition(|&c| c == b'/')?;
    let parent = if i == 0 { &b"/"[..] } else { &path[..i] };
    let name = &path[i + 1..];
    assert!(!name.contains(&b'/'));
    assert!(parent.len() < path.len());
    Some((parent, name))
}

pub fn vpath_starts_with(path: &[u8], prefix: &[u8]) -> bool {
    if prefix == b"/" {
        return true;
    }
    path.starts_with(prefix) && (path.len() == prefix.len() || path[prefix.len()] == b'/')
}

pub fn to_os_path(p: &[u8]) -> &Path {
    Path::new(std::ffi::OsStr::from_bytes(p))
}

pub fn show(p: &[u8]) -> String {
    String::from_utf8_lossy(p).into_owned()
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Obs {
    Absent,
    Dir,
    Listing(Hash),
    Symlink(Vec<u8>),
    Meta { size: u64, exec: bool },
    IsFile { exec: bool },
    File { hash: Hash, exec: bool },
    Other,
    Code { executed: Hash, rest: Hash },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HostSig {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

impl HostSig {
    pub fn of(m: &crate::io::file::Stat) -> HostSig {
        HostSig {
            dev: m.dev(),
            ino: m.ino(),
            size: m.len(),
            mtime_ns: m.mtime() * 1_000_000_000 + m.mtime_nsec(),
            ctime_ns: m.ctime() * 1_000_000_000 + m.ctime_nsec(),
        }
    }

    fn key(&self) -> [u8; 40] {
        let mut k = [0u8; 40];
        k[0..8].copy_from_slice(&self.dev.to_le_bytes());
        k[8..16].copy_from_slice(&self.ino.to_le_bytes());
        k[16..24].copy_from_slice(&self.size.to_le_bytes());
        k[24..32].copy_from_slice(&self.mtime_ns.to_le_bytes());
        k[32..40].copy_from_slice(&self.ctime_ns.to_le_bytes());
        const _: () = assert!(std::mem::size_of::<HostSig>() == 40);
        k
    }
}

pub trait LazyBlob: Send + Sync {
    fn materialize(&self, store: &Store) -> io::Result<Hash>;
}

#[derive(Clone)]
pub enum FileSrc {
    Blob(Hash),
    Host { path: Arc<Path>, sig: HostSig },
    Lazy(Arc<dyn LazyBlob>),
}

impl std::fmt::Debug for FileSrc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FileSrc::Blob(h) => write!(f, "Blob({h:?})"),
            FileSrc::Host { path, .. } => write!(f, "Host({})", path.display()),
            FileSrc::Lazy(_) => write!(f, "Lazy"),
        }
    }
}

pub const OWNER_SELF: u32 = u32::MAX;

#[derive(Clone, Copy, Debug)]
pub struct Owner {
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
}

impl Owner {
    pub const fn own(mode: u32) -> Owner {
        Owner { uid: OWNER_SELF, gid: OWNER_SELF, mode }
    }
}

#[derive(Clone, Debug)]
pub struct FileInfo {
    pub size: u64,
    pub owner: Owner,
    pub src: FileSrc,
    pub sem: Option<Hash>,
}

impl FileInfo {
    pub fn exec(&self) -> bool {
        self.owner.mode & 0o111 != 0
    }
}

#[derive(Clone, Debug)]
pub enum Looked {
    Absent,
    Dir(Owner),
    Symlink(Vec<u8>),
    File(FileInfo),
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DType {
    Dir,
    File,
    Symlink,
    Other,
}

impl DType {
    pub fn as_dirent(self) -> u32 {
        match self {
            DType::Dir => libc::DT_DIR as u32,
            DType::File => libc::DT_REG as u32,
            DType::Symlink => libc::DT_LNK as u32,
            DType::Other => libc::DT_UNKNOWN as u32,
        }
    }
    fn tag(self) -> u8 {
        match self {
            DType::Dir => b'd',
            DType::File => b'f',
            DType::Symlink => b'l',
            DType::Other => b'o',
        }
    }
}

pub type Listing = Vec<(Vec<u8>, DType)>;

pub fn listing_hash(entries: &Listing) -> Hash {
    let mut sorted: Vec<&(Vec<u8>, DType)> = entries.iter().collect();
    sorted.sort();
    let mut h = blake3::Hasher::new();
    for (name, t) in sorted {
        h.update(&(name.len() as u32).to_le_bytes());
        h.update(name);
        h.update(&[t.tag()]);
    }
    Hash(*h.finalize().as_bytes())
}

pub trait View: Send + Sync {
    fn lookup(&self, dir: &[u8], name: &[u8]) -> Looked;
    fn list(&self, dir: &[u8]) -> io::Result<Listing>;
    fn traced(&self, _dir: &[u8]) -> bool {
        true
    }
    fn hints(&self) -> Vec<Vec<u8>> {
        Vec::new()
    }
    fn nix_closure(&self) -> &[(Vec<u8>, Hash)] {
        &[]
    }
    fn stat(&self, path: &[u8]) -> Looked {
        match vsplit(path) {
            None => Looked::Dir(Owner { uid: 0, gid: 0, mode: 0o755 }),
            Some((dir, name)) => self.lookup(dir, name),
        }
    }

    fn read_bytes(&self, _path: &[u8]) -> Option<&[u8]> {
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BackingKey {
    Blob(Hash),
    Host(HostSig),
}

pub type Fetcher = Arc<dyn Fn(&Hash) -> io::Result<()> + Send + Sync>;

pub struct Content {
    pub store: Arc<Store>,
    host_hashes: DashMap<HostSig, Hash>,
    fetcher: std::sync::OnceLock<Fetcher>,
    meta_ids: DashMap<Hash, Option<Hash>>,
}

impl Content {
    pub fn new(store: Arc<Store>) -> Content {
        Content {
            store,
            host_hashes: DashMap::new(),
            fetcher: std::sync::OnceLock::new(),
            meta_ids: DashMap::new(),
        }
    }

    pub fn metadata_identity(&self, blob: &Hash) -> Option<Hash> {
        if let Some(v) = self.meta_ids.get(blob) {
            return *v;
        }
        let id = (|| {
            use std::io::Read;
            let mut f = self.store.open_blob(blob).ok()?;
            let mut head = [0u8; 40];
            f.read_exact(&mut head).ok()?;
            if &head[..7] != b"rust\0\0\x01" {
                return None;
            }
            let mut h = crate::hash::KeyHasher::new("rmeta-identity");
            h.field(&head[7..8]).field(&head[24..40]);
            Some(h.finish())
        })();
        self.meta_ids.insert(*blob, id);
        id
    }

    fn derived_key(kind: &[u8], blob: &Hash) -> Vec<u8> {
        let mut k = kind.to_vec();
        k.extend_from_slice(&blob.0);
        k
    }

    pub fn code_table(&self, binary: &Hash) -> Option<crate::fn_code::CodeTable> {
        let key = Content::derived_key(b"code-table:", binary);
        match self.db().get(crate::db::Table::Meta, &key) {
            Some(v) if v.is_empty() => return None,
            Some(v) => {
                let kept = Hash(v[..].try_into().ok()?);
                if let Some(table) = self.store.read(&kept).ok().and_then(|b| bincode::deserialize(&b).ok()) {
                    return Some(table);
                }
            }
            None => {}
        }
        let f = self.store.open_blob(binary).ok()?;
        let table = crate::fn_code::table_by(&|at, len| {
            let mut buf = vec![0u8; len];
            f.read_exact_at(&mut buf, at).ok()?;
            Some(buf)
        });
        let kept = match &table {
            Some(t) => self.store.put_bytes(&bincode::serialize(t).expect("a table serialises")).ok()?.0.to_vec(),
            None => Vec::new(),
        };
        self.db().put(crate::db::Table::Meta, &key, &kept);
        table
    }

    pub fn linked_tracked(&self, input: &Hash) -> bool {
        let key = Content::derived_key(b"code-tracked:", input);
        if let Some(v) = self.db().get(crate::db::Table::Meta, &key) {
            return v.first() == Some(&1);
        }
        let Ok(bytes) = self.store.read(input) else { return false };
        let tracked = crate::fn_code::linked_tracked(&bytes);
        self.db().put(crate::db::Table::Meta, &key, &[tracked as u8]);
        tracked
    }

    pub fn code_rest(&self, binary: &Hash) -> Option<Hash> {
        let v = self.db().get(crate::db::Table::Meta, &Content::derived_key(b"code-rest:", binary))?;
        Some(Hash(v[..].try_into().ok()?))
    }

    pub fn record_code_rest(&self, binary: &Hash, rest: &Hash) {
        self.db().put(crate::db::Table::Meta, &Content::derived_key(b"code-rest:", binary), &rest.0);
    }

    pub fn code_verdict(&self, binary: &Hash, executed: &Hash, rest: &Hash) -> crate::fn_code::Verdict {
        let table = self.code_table(binary);
        let now = table.as_ref().zip(self.code_rest(binary));
        let blob = self.store.read(executed).ok();
        crate::fn_code::verdict(blob.as_deref(), rest, now)
    }

    pub fn set_fetcher(&self, f: Fetcher) {
        let _ = self.fetcher.set(f);
    }

    fn open_blob(&self, h: &Hash) -> io::Result<File> {
        if !self.store.has(h) {
            if let Some(fetch) = self.fetcher.get() {
                fetch(h)?;
            }
        }
        self.store.open_blob(h)
    }

    fn db(&self) -> &crate::db::Db {
        self.store.db()
    }

    pub fn open(&self, src: &FileSrc) -> io::Result<(File, BackingKey, FileSrc)> {
        match src {
            FileSrc::Blob(h) => Ok((self.open_blob(h)?, BackingKey::Blob(*h), src.clone())),
            FileSrc::Host { path, .. } => {
                let f = crate::io::file::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path)?;
                let sig = HostSig::of(&f.metadata()?);
                Ok((f, BackingKey::Host(sig), FileSrc::Host { path: path.clone(), sig }))
            }
            FileSrc::Lazy(_) => {
                assert!(self.known_hash(src).is_none(), "lazy content has no hash before it is fetched");
                let h = self.hash(src)?;
                Ok((self.open_blob(&h)?, BackingKey::Blob(h), FileSrc::Blob(h)))
            }
        }
    }

    pub fn known_hash(&self, src: &FileSrc) -> Option<Hash> {
        match src {
            FileSrc::Blob(h) => Some(*h),
            FileSrc::Host { sig, .. } => {
                if let Some(h) = self.host_hashes.get(sig) {
                    return Some(*h);
                }
                let v = self.db().get(crate::db::Table::HostSigs, &sig.key())?;
                let h = Hash(v[..].try_into().ok()?);
                self.host_hashes.insert(*sig, h);
                Some(h)
            }
            FileSrc::Lazy(_) => None,
        }
    }

    pub fn hash(&self, src: &FileSrc) -> io::Result<Hash> {
        if let Some(h) = self.known_hash(src) {
            return Ok(h);
        }
        match src {
            FileSrc::Blob(h) => Ok(*h),
            FileSrc::Host { path, sig } => {
                let f = crate::io::file::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path)?;
                let before = HostSig::of(&f.metadata()?);
                if before != *sig {
                    return Err(io::Error::from_raw_os_error(libc::ESTALE));
                }
                assert_eq!(before.size, sig.size);
                let (h, _) = Hash::of_file(&f)?;
                let after = HostSig::of(&f.metadata()?);
                if after != before {
                    return Err(io::Error::from_raw_os_error(libc::ESTALE));
                }
                self.record_host(*sig, h);
                Ok(h)
            }
            FileSrc::Lazy(blob) => blob.materialize(&self.store),
        }
    }

    pub fn record_host(&self, sig: HostSig, h: Hash) {
        self.host_hashes.insert(sig, h);
        self.db().put(crate::db::Table::HostSigs, &sig.key(), &h.0);
    }
}

pub(crate) enum CheckObservation {
    Known(bool),
    Hash(FileSrc, Hash),
}

impl Obs {
    pub(crate) fn check(&self, looked: Looked) -> CheckObservation {
        let matches = match (self, looked) {
            (Obs::Absent, Looked::Absent) | (Obs::Dir, Looked::Dir(_)) | (Obs::Other, Looked::Other) => true,
            (Obs::Symlink(a), Looked::Symlink(b)) => *a == b,
            (Obs::Meta { size, exec }, Looked::File(f)) => f.size == *size && f.exec() == *exec,
            (Obs::IsFile { exec }, Looked::File(f)) => f.exec() == *exec,
            (Obs::File { hash, exec }, Looked::File(f)) if f.exec() == *exec => {
                if let Some(sem) = f.sem {
                    sem == *hash
                } else {
                    return CheckObservation::Hash(f.src, *hash);
                }
            }
            _ => false,
        };
        CheckObservation::Known(matches)
    }
}

pub fn obs_holds(view: &dyn View, hash_of: &dyn Fn(&FileSrc) -> Option<Hash>, path: &[u8], obs: &Obs) -> bool {
    if let Obs::Listing(want) = obs {
        return view.list(path).is_ok_and(|entries| listing_hash(&entries) == *want);
    }
    match obs.check(view.stat(path)) {
        CheckObservation::Known(matches) => matches,
        CheckObservation::Hash(src, hash) => hash_of(&src).is_some_and(|got| got == hash),
    }
}

pub(crate) fn merge_listings<K: Ord>(
    base: impl IntoIterator<Item = (K, DType)>,
    overlay: impl IntoIterator<Item = (K, Option<DType>)>,
) -> Vec<(K, DType)> {
    let mut names: std::collections::BTreeMap<_, _> = base.into_iter().collect();
    for (name, kind) in overlay {
        if let Some(kind) = kind {
            names.insert(name, kind);
        } else {
            names.remove(&name);
        }
    }
    names.into_iter().collect()
}

pub struct HostRegion {
    pub real: PathBuf,
}

impl HostRegion {
    pub fn new(real: impl Into<PathBuf>) -> HostRegion {
        HostRegion { real: real.into() }
    }

    pub fn real_path(&self, rel_dir: &[u8], name: &[u8]) -> PathBuf {
        let mut p = self.real.clone();
        let rel = rel_dir.strip_prefix(b"/").unwrap_or(rel_dir);
        if !rel.is_empty() {
            p.push(to_os_path(rel));
        }
        if !name.is_empty() {
            p.push(to_os_path(name));
        }
        p
    }
}

#[derive(Clone)]
enum MemNode {
    Dir(Owner),
    File { info: FileInfo, bytes: Vec<u8> },
    Symlink(Vec<u8>),
    Other,
}

#[derive(Clone)]
pub struct MemView {
    nodes: std::collections::BTreeMap<Vec<u8>, MemNode>,
}

impl MemView {
    pub fn new() -> MemView {
        let mut nodes = std::collections::BTreeMap::new();
        nodes.insert(b"/".to_vec(), MemNode::Dir(Owner { uid: 0, gid: 0, mode: 0o755 }));
        MemView { nodes }
    }

    fn insert(&mut self, path: &[u8], node: MemNode) {
        let (parent, name) = vsplit(path).expect("an entry has a parent");
        assert!(!name.is_empty());
        assert!(self.is_dir(parent), "parent is a directory: {}", show(parent));
        self.nodes.insert(path.to_vec(), node);
    }

    pub fn dir(&mut self, path: &[u8]) {
        self.insert(path, MemNode::Dir(Owner { uid: 0, gid: 0, mode: 0o755 }));
    }

    pub fn file(&mut self, path: &[u8], bytes: &[u8], exec: bool, sem: Option<Hash>) {
        let mode = if exec { 0o755 } else { 0o644 };
        let info = FileInfo {
            size: bytes.len() as u64,
            owner: Owner::own(mode),
            src: FileSrc::Blob(Hash::of(bytes)),
            sem,
        };
        self.insert(path, MemNode::File { info, bytes: bytes.to_vec() });
    }

    pub fn symlink(&mut self, path: &[u8], target: &[u8]) {
        self.insert(path, MemNode::Symlink(target.to_vec()));
    }

    pub fn other(&mut self, path: &[u8]) {
        self.insert(path, MemNode::Other);
    }

    fn is_dir(&self, path: &[u8]) -> bool {
        path == b"/" || matches!(self.nodes.get(path), Some(MemNode::Dir(_)))
    }
}

#[cfg(test)]
impl MemView {
    pub(crate) fn remove_tree(&mut self, path: &[u8]) {
        assert_ne!(path, b"/");
        self.nodes.retain(|p, _| p.as_slice() != path && !vpath_starts_with(p, path));
    }

    pub(crate) fn rewrite(&mut self, path: &[u8], bytes: &[u8]) {
        let Some(MemNode::File { info, bytes: buf }) = self.nodes.get_mut(path) else {
            panic!("rewrite expects a file");
        };
        info.size = bytes.len() as u64;
        info.src = FileSrc::Blob(Hash::of(bytes));
        *buf = bytes.to_vec();
    }
}

impl View for MemView {
    fn lookup(&self, dir: &[u8], name: &[u8]) -> Looked {
        if !self.is_dir(dir) {
            return Looked::Absent;
        }
        match self.nodes.get(&vjoin(dir, name)) {
            None => Looked::Absent,
            Some(MemNode::Dir(owner)) => Looked::Dir(*owner),
            Some(MemNode::File { info, .. }) => Looked::File(info.clone()),
            Some(MemNode::Symlink(target)) => Looked::Symlink(target.clone()),
            Some(MemNode::Other) => Looked::Other,
        }
    }

    fn list(&self, dir: &[u8]) -> io::Result<Listing> {
        if !self.is_dir(dir) {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }
        let mut out = Vec::new();
        for (path, node) in &self.nodes {
            let Some((parent, name)) = vsplit(path) else { continue };
            if parent != dir {
                continue;
            }
            let kind = match node {
                MemNode::Dir(_) => DType::Dir,
                MemNode::File { .. } => DType::File,
                MemNode::Symlink(_) => DType::Symlink,
                MemNode::Other => DType::Other,
            };
            out.push((name.to_vec(), kind));
        }
        Ok(out)
    }

    fn read_bytes(&self, path: &[u8]) -> Option<&[u8]> {
        match self.nodes.get(path) {
            Some(MemNode::File { bytes, .. }) => Some(bytes.as_slice()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(vjoin(b"/", b"a"), b"/a");
        assert_eq!(vjoin(b"/a", b"b"), b"/a/b");
        assert_eq!(vsplit(b"/a/b"), Some((&b"/a"[..], &b"b"[..])));
        assert_eq!(vsplit(b"/a"), Some((&b"/"[..], &b"a"[..])));
        assert_eq!(vsplit(b"/"), None);
        assert!(vpath_starts_with(b"/w/target", b"/w"));
        assert!(!vpath_starts_with(b"/work", b"/w"));
        assert!(vpath_starts_with(b"/w", b"/w"));
    }

    #[test]
    fn listing_hash_is_order_independent() {
        let a = vec![(b"x".to_vec(), DType::File), (b"y".to_vec(), DType::Dir)];
        let b = vec![(b"y".to_vec(), DType::Dir), (b"x".to_vec(), DType::File)];
        assert_eq!(listing_hash(&a), listing_hash(&b));
        let c = vec![(b"x".to_vec(), DType::Dir), (b"y".to_vec(), DType::Dir)];
        assert_ne!(listing_hash(&a), listing_hash(&c));
    }
}
