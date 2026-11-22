//! Views: the immutable filesystem an action is shown, and observations made
//! against it.
//!
//! A view answers three questions about a sandbox path: what is here
//! (`lookup`), what does this directory contain (`list`), and where do this
//! file's bytes come from (`FileSrc`). The sandbox filesystem asks them on
//! behalf of a running action and records the answers; the trace cache asks
//! them again later to decide whether a recorded trace still holds.

use crate::hash::Hash;
use crate::store::Store;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// An absolute, normalised sandbox path as bytes (`/`, `/usr/lib`).
pub type VPath = Arc<[u8]>;

pub fn vjoin(dir: &[u8], name: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(dir.len() + 1 + name.len());
    if dir != b"/" {
        v.extend_from_slice(dir);
    }
    v.push(b'/');
    v.extend_from_slice(name);
    v
}

/// Splits `/a/b` into (`/a`, `b`). The root has no parent.
pub fn vsplit(path: &[u8]) -> Option<(&[u8], &[u8])> {
    if path == b"/" || path.is_empty() {
        return None;
    }
    let i = path.iter().rposition(|&c| c == b'/')?;
    let parent = if i == 0 { &b"/"[..] } else { &path[..i] };
    Some((parent, &path[i + 1..]))
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

/// What an action saw at a path.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Obs {
    Absent,
    Dir,
    /// Hash of the sorted `(name, type)` entries.
    Listing(Hash),
    Symlink(Vec<u8>),
    /// A file that was looked up but never opened.
    Meta { size: u64, exec: bool },
    /// A file that was looked up but never opened, by an action known not
    /// to act on file sizes (the planner): only its existence and
    /// executable bit were observed.
    IsFile { exec: bool },
    /// A file whose content was read.
    File { hash: Hash, exec: bool },
    /// Something that is neither file, directory nor symlink.
    Other,
}

/// Identity of a host file at the moment it was looked at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HostSig {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

impl HostSig {
    pub fn of(m: &std::fs::Metadata) -> HostSig {
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
        k
    }
}

/// A blob that can be produced on demand (a git object).
pub trait LazyBlob: Send + Sync {
    /// Writes the content into the store and returns its hash.
    fn materialize(&self, store: &Store) -> io::Result<Hash>;
}

/// Where a file's bytes come from.
#[derive(Clone)]
pub enum FileSrc {
    /// Already in the store.
    Blob(Hash),
    /// A file on the host, read in place.
    Host { path: Arc<Path>, sig: HostSig },
    /// A git blob, identified by object id, extracted on first use.
    Git { oid: [u8; 20], blob: Arc<dyn LazyBlob> },
}

impl std::fmt::Debug for FileSrc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FileSrc::Blob(h) => write!(f, "Blob({h:?})"),
            FileSrc::Host { path, .. } => write!(f, "Host({})", path.display()),
            FileSrc::Git { oid, .. } => write!(f, "Git({:02x}{:02x}..)", oid[0], oid[1]),
        }
    }
}

/// `uid`/`gid` value meaning "the user the action runs as".
pub const OWNER_SELF: u32 = u32::MAX;

#[derive(Clone, Copy, Debug)]
pub struct Owner {
    pub uid: u32,
    pub gid: u32,
    /// Permission bits only.
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
    /// Exists but is not representable (device, socket, fifo).
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

/// Canonical hash of a directory listing.
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
    /// What is `name` in directory `dir`. `dir` is known to be a directory
    /// in this view.
    fn lookup(&self, dir: &[u8], name: &[u8]) -> Looked;
    /// Entries of directory `dir`.
    fn list(&self, dir: &[u8]) -> io::Result<Listing>;
    /// Whether observations under this directory belong in a trace.
    fn traced(&self, _dir: &[u8]) -> bool {
        true
    }
    /// Looks up a full path. Every parent must be a real directory.
    fn stat(&self, path: &[u8]) -> Looked {
        match vsplit(path) {
            None => Looked::Dir(Owner { uid: 0, gid: 0, mode: 0o755 }),
            Some((dir, name)) => self.lookup(dir, name),
        }
    }
}

/// Key under which a passthrough backing file can be shared between actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BackingKey {
    Blob(Hash),
    Host(HostSig),
}

/// Resolves [`FileSrc`]s to bytes and hashes.
pub struct Content {
    pub store: Arc<Store>,
    host_hashes: DashMap<HostSig, Hash>,
    git_hashes: DashMap<[u8; 20], Hash>,
}

impl Content {
    pub fn new(store: Arc<Store>) -> Content {
        Content { store, host_hashes: DashMap::new(), git_hashes: DashMap::new() }
    }

    fn db(&self) -> &crate::db::Db {
        self.store.db()
    }

    /// Opens the file for reading. For host files the returned source carries
    /// the signature of the file actually opened.
    pub fn open(&self, src: &FileSrc) -> io::Result<(File, BackingKey, FileSrc)> {
        match src {
            FileSrc::Blob(h) => Ok((self.store.open_blob(h)?, BackingKey::Blob(*h), src.clone())),
            FileSrc::Host { path, .. } => {
                let f = std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(path)?;
                let sig = HostSig::of(&f.metadata()?);
                Ok((f, BackingKey::Host(sig), FileSrc::Host { path: path.clone(), sig }))
            }
            FileSrc::Git { .. } => {
                let h = self.hash(src)?;
                Ok((self.store.open_blob(&h)?, BackingKey::Blob(h), FileSrc::Blob(h)))
            }
        }
    }

    /// The content hash if it is known without reading the file.
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
            FileSrc::Git { oid, .. } => {
                if let Some(h) = self.git_hashes.get(oid) {
                    return Some(*h);
                }
                let v = self.db().get(crate::db::Table::GitOids, oid)?;
                let h = Hash(v[..].try_into().ok()?);
                self.git_hashes.insert(*oid, h);
                Some(h)
            }
        }
    }

    /// The content hash, reading the file if needed. For host files this
    /// fails with `ESTALE` if the file no longer matches the signature the
    /// source was created with.
    pub fn hash(&self, src: &FileSrc) -> io::Result<Hash> {
        if let Some(h) = self.known_hash(src) {
            if let FileSrc::Git { .. } = src {
                if !self.store.has(&h) {
                    return self.materialize_git(src);
                }
            }
            return Ok(h);
        }
        match src {
            FileSrc::Blob(h) => Ok(*h),
            FileSrc::Host { path, sig } => {
                let f = std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(path)?;
                let before = HostSig::of(&f.metadata()?);
                if before != *sig {
                    return Err(io::Error::from_raw_os_error(libc::ESTALE));
                }
                let (h, _) = Hash::of_file(&f)?;
                let after = HostSig::of(&f.metadata()?);
                if after != before {
                    return Err(io::Error::from_raw_os_error(libc::ESTALE));
                }
                self.record_host(*sig, h);
                Ok(h)
            }
            FileSrc::Git { .. } => self.materialize_git(src),
        }
    }

    fn materialize_git(&self, src: &FileSrc) -> io::Result<Hash> {
        let FileSrc::Git { oid, blob } = src else { unreachable!() };
        let h = blob.materialize(&self.store)?;
        self.record_git(*oid, h);
        Ok(h)
    }

    pub fn record_host(&self, sig: HostSig, h: Hash) {
        self.host_hashes.insert(sig, h);
        self.db().put(crate::db::Table::HostSigs, &sig.key(), &h.0);
    }

    /// Remembers that git blob `oid` has content hash `h`.
    pub fn record_git(&self, oid: [u8; 20], h: Hash) {
        if self.git_hashes.insert(oid, h).is_none() {
            self.db().put(crate::db::Table::GitOids, &oid, &h.0);
        }
    }
}

/// Decides whether an observation still holds in `view`.
pub fn obs_holds(view: &dyn View, content: &Content, path: &[u8], obs: &Obs) -> bool {
    if let Obs::Listing(want) = obs {
        return match view.list(path) {
            Ok(l) => listing_hash(&l) == *want,
            Err(_) => false,
        };
    }
    let looked = view.stat(path);
    match (obs, looked) {
        (Obs::Absent, Looked::Absent) => true,
        (Obs::Dir, Looked::Dir(_)) => true,
        (Obs::Other, Looked::Other) => true,
        (Obs::Symlink(a), Looked::Symlink(b)) => *a == b,
        (Obs::Meta { size, exec }, Looked::File(f)) => f.size == *size && f.exec() == *exec,
        (Obs::IsFile { exec }, Looked::File(f)) => f.exec() == *exec,
        (Obs::File { hash, exec }, Looked::File(f)) => {
            if f.exec() != *exec {
                return false;
            }
            match content.known_hash(&f.src) {
                Some(h) => h == *hash,
                None => match &f.src {
                    // An unknown host file has to be read to be compared.
                    FileSrc::Host { .. } => content.hash(&f.src).map(|h| h == *hash).unwrap_or(false),
                    // A git blob nobody has ever materialised cannot be the
                    // content some action read: every blob that entered a
                    // trace has its object id recorded.
                    _ => false,
                },
            }
        }
        _ => false,
    }
}

/// A directory tree on the host shown at some sandbox path.
pub struct HostRegion {
    pub real: PathBuf,
}

impl HostRegion {
    pub fn new(real: impl Into<PathBuf>) -> HostRegion {
        HostRegion { real: real.into() }
    }

    fn real_path(&self, rel_dir: &[u8], name: &[u8]) -> PathBuf {
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

    /// `rel_dir` is the directory relative to the region root (`/` or `/a/b`).
    pub fn lookup(&self, rel_dir: &[u8], name: &[u8]) -> Looked {
        let p = self.real_path(rel_dir, name);
        let Ok(m) = std::fs::symlink_metadata(&p) else { return Looked::Absent };
        let owner = Owner { uid: m.uid(), gid: m.gid(), mode: m.mode() & 0o777 };
        let ft = m.file_type();
        if ft.is_dir() {
            Looked::Dir(owner)
        } else if ft.is_file() {
            Looked::File(FileInfo {
                size: m.len(),
                owner,
                src: FileSrc::Host { path: Arc::from(p), sig: HostSig::of(&m) },
            })
        } else if ft.is_symlink() {
            match std::fs::read_link(&p) {
                Ok(t) => Looked::Symlink(t.as_os_str().as_bytes().to_vec()),
                Err(_) => Looked::Absent,
            }
        } else {
            Looked::Other
        }
    }

    pub fn list(&self, rel_dir: &[u8]) -> io::Result<Listing> {
        let p = self.real_path(rel_dir, b"");
        let mut out = Vec::new();
        for e in std::fs::read_dir(&p)? {
            let e = e?;
            let ft = e.file_type()?;
            let t = if ft.is_dir() {
                DType::Dir
            } else if ft.is_file() {
                DType::File
            } else if ft.is_symlink() {
                DType::Symlink
            } else {
                DType::Other
            };
            out.push((e.file_name().as_bytes().to_vec(), t));
        }
        Ok(out)
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
