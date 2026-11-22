//! Worktree snapshots: HEAD tree plus a dirty overlay.
//!
//! A [`Snapshot`] is an immutable description of every file in a worktree at
//! one moment. Clean tracked files are identified by their git object id and
//! never touched. Everything that differs from HEAD (modified, untracked,
//! deleted, files behind a smudge filter) is captured into the content store
//! when the snapshot is taken. Ignored directories and submodules are not
//! captured; they are shown live, like host files.
//!
//! A [`Worktree`] keeps the overlay current from inotify events, so taking a
//! snapshot costs time proportional to what changed since the last one.

use crate::hash::Hash;
use crate::store::Store;
use crate::view::*;
use anyhow::{Context, Result, bail};
use dashmap::DashMap;
use parking_lot::{Condvar, Mutex};
use sha1::{Digest, Sha1};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub type Oid = [u8; 20];

#[derive(Clone, Copy, Debug)]
pub struct TreeEnt {
    pub mode: u32,
    pub oid: Oid,
}

impl TreeEnt {
    fn is_tree(&self) -> bool {
        self.mode == 0o040000
    }
    fn is_link(&self) -> bool {
        self.mode == 0o120000
    }
    fn is_gitlink(&self) -> bool {
        self.mode == 0o160000
    }
    fn exec(&self) -> bool {
        self.mode & 0o111 != 0
    }
}

pub type TreeMap = HashMap<Box<[u8]>, TreeEnt>;

/// Shared object access for one git repository (all of its worktrees).
pub struct GitRepo {
    repo: Mutex<git2::Repository>,
    trees: DashMap<Oid, Arc<TreeMap>>,
    sizes: DashMap<Oid, u64>,
    links: DashMap<Oid, Arc<[u8]>>,
}

fn oid_of(o: git2::Oid) -> Oid {
    let mut out = [0u8; 20];
    out.copy_from_slice(o.as_bytes());
    out
}

fn git_err(e: git2::Error) -> io::Error {
    io::Error::other(e.message().to_string())
}

impl GitRepo {
    pub fn open(path: &Path) -> Result<Arc<GitRepo>> {
        let repo = git2::Repository::open(path)
            .with_context(|| format!("{} is not a git worktree", path.display()))?;
        Ok(Arc::new(GitRepo {
            repo: Mutex::new(repo),
            trees: DashMap::new(),
            sizes: DashMap::new(),
            links: DashMap::new(),
        }))
    }

    pub fn tree(&self, oid: &Oid) -> io::Result<Arc<TreeMap>> {
        if let Some(t) = self.trees.get(oid) {
            return Ok(t.clone());
        }
        let repo = self.repo.lock();
        let tree = repo.find_tree(git2::Oid::from_bytes(oid).map_err(git_err)?).map_err(git_err)?;
        let mut map = TreeMap::with_capacity(tree.len());
        for e in tree.iter() {
            map.insert(e.name_bytes().into(), TreeEnt { mode: e.filemode() as u32, oid: oid_of(e.id()) });
        }
        let map = Arc::new(map);
        self.trees.insert(*oid, map.clone());
        Ok(map)
    }

    pub fn blob_size(&self, oid: &Oid) -> io::Result<u64> {
        if let Some(s) = self.sizes.get(oid) {
            return Ok(*s);
        }
        let repo = self.repo.lock();
        let odb = repo.odb().map_err(git_err)?;
        let (size, _) = odb.read_header(git2::Oid::from_bytes(oid).map_err(git_err)?).map_err(git_err)?;
        self.sizes.insert(*oid, size as u64);
        Ok(size as u64)
    }

    pub fn blob(&self, oid: &Oid) -> io::Result<Vec<u8>> {
        let repo = self.repo.lock();
        let blob = repo.find_blob(git2::Oid::from_bytes(oid).map_err(git_err)?).map_err(git_err)?;
        Ok(blob.content().to_vec())
    }

    fn link_target(&self, oid: &Oid) -> io::Result<Arc<[u8]>> {
        if let Some(t) = self.links.get(oid) {
            return Ok(t.clone());
        }
        let t: Arc<[u8]> = self.blob(oid)?.into();
        self.links.insert(*oid, t.clone());
        Ok(t)
    }

    /// Paths (files only) that differ between two trees.
    pub fn diff_trees(&self, old: &Oid, new: &Oid) -> io::Result<Vec<Vec<u8>>> {
        let repo = self.repo.lock();
        let find = |o: &Oid| -> Result<Option<git2::Tree<'_>>, git2::Error> {
            if *o == [0u8; 20] { Ok(None) } else { Ok(Some(repo.find_tree(git2::Oid::from_bytes(o)?)?)) }
        };
        let a = find(old).map_err(git_err)?;
        let b = find(new).map_err(git_err)?;
        let diff = repo.diff_tree_to_tree(a.as_ref(), b.as_ref(), None).map_err(git_err)?;
        let mut out = Vec::new();
        for d in diff.deltas() {
            for f in [d.old_file(), d.new_file()] {
                if let Some(p) = f.path_bytes() {
                    out.push(p.to_vec());
                }
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }
}

struct GitBlob {
    repo: Arc<GitRepo>,
    oid: Oid,
}

impl LazyBlob for GitBlob {
    fn materialize(&self, store: &Store) -> io::Result<Hash> {
        let bytes = self.repo.blob(&self.oid)?;
        store.put_bytes(&bytes)
    }
}

/// One overlay entry.
#[derive(Clone, Debug)]
pub enum Ov {
    File { hash: Hash, exec: bool, size: u64 },
    Symlink(Vec<u8>),
    /// A path HEAD has that the worktree does not.
    Deleted,
    Dir(Arc<OvDir>),
    /// A directory shown live from the host (ignored directory, submodule).
    Live(Arc<Path>),
}

#[derive(Clone, Debug, Default)]
pub struct OvDir {
    /// The directory exists in the worktree even if HEAD has nothing here.
    pub entries: BTreeMap<Vec<u8>, Ov>,
}

#[derive(Clone)]
struct DirState {
    tree: Option<Arc<TreeMap>>,
    ov: Option<Arc<OvDir>>,
    live: Option<(Arc<Path>, Vec<u8>)>,
}

/// An immutable view of a worktree.
pub struct Snapshot {
    pub repo: Arc<GitRepo>,
    /// HEAD tree, or zeros when HEAD is unborn.
    pub tree: Oid,
    pub overlay: Arc<OvDir>,
    /// Identifies the snapshot's content.
    pub id: Hash,
    dirs: DashMap<Vec<u8>, Option<DirState>>,
    /// Paths (relative, no leading slash) that are hidden, e.g. `.git`.
    hidden: Vec<Vec<u8>>,
}

const OWN_FILE: Owner = Owner::own(0o644);
const OWN_EXEC: Owner = Owner::own(0o755);
const OWN_DIR: Owner = Owner::own(0o755);

impl Snapshot {
    pub fn new(repo: Arc<GitRepo>, tree: Oid, overlay: Arc<OvDir>) -> Snapshot {
        let mut h = crate::hash::KeyHasher::new("snapshot");
        h.field(&tree);
        hash_ov(&overlay, &mut h);
        Snapshot { repo, tree, overlay, id: h.finish(), dirs: DashMap::new(), hidden: vec![b".git".to_vec()] }
    }

    fn root_state(&self) -> Option<DirState> {
        let tree = if self.tree == [0u8; 20] { None } else { self.repo.tree(&self.tree).ok() };
        Some(DirState { tree, ov: Some(self.overlay.clone()), live: None })
    }

    /// `rel_dir` is `/` for the worktree root or `/a/b`.
    fn dir_state(&self, rel_dir: &[u8]) -> Option<DirState> {
        if rel_dir == b"/" || rel_dir.is_empty() {
            return self.root_state();
        }
        if let Some(s) = self.dirs.get(rel_dir) {
            return s.clone();
        }
        let (parent, name) = vsplit(rel_dir)?;
        let st = self.dir_state(parent).and_then(|p| self.child_dir(&p, name));
        self.dirs.insert(rel_dir.to_vec(), st.clone());
        st
    }

    fn child_dir(&self, p: &DirState, name: &[u8]) -> Option<DirState> {
        if let Some((real, rel)) = &p.live {
            return Some(DirState { tree: None, ov: None, live: Some((real.clone(), vjoin(rel, name))) });
        }
        let tree_child = || -> Option<Arc<TreeMap>> {
            let e = p.tree.as_ref()?.get(name)?;
            if e.is_tree() { self.repo.tree(&e.oid).ok() } else { None }
        };
        match p.ov.as_ref().and_then(|o| o.entries.get(name)) {
            Some(Ov::Dir(d)) => Some(DirState { tree: tree_child(), ov: Some(d.clone()), live: None }),
            Some(Ov::Live(real)) => Some(DirState { tree: None, ov: None, live: Some((real.clone(), b"/".to_vec())) }),
            Some(_) => None,
            None => tree_child().map(|t| DirState { tree: Some(t), ov: None, live: None }),
        }
    }

    pub fn lookup(&self, rel_dir: &[u8], name: &[u8]) -> Looked {
        if (rel_dir == b"/" || rel_dir.is_empty()) && self.hidden.iter().any(|h| h == name) {
            return Looked::Absent;
        }
        let Some(st) = self.dir_state(rel_dir) else { return Looked::Absent };
        if let Some((real, rel)) = &st.live {
            return HostRegion::new(real.to_path_buf()).lookup(rel, name);
        }
        if let Some(e) = st.ov.as_ref().and_then(|o| o.entries.get(name)) {
            return match e {
                Ov::File { hash, exec, size } => Looked::File(FileInfo {
                    size: *size,
                    owner: if *exec { OWN_EXEC } else { OWN_FILE },
                    src: FileSrc::Blob(*hash),
                }),
                Ov::Symlink(t) => Looked::Symlink(t.clone()),
                Ov::Deleted => Looked::Absent,
                Ov::Dir(_) | Ov::Live(_) => Looked::Dir(OWN_DIR),
            };
        }
        let Some(e) = st.tree.as_ref().and_then(|t| t.get(name)) else { return Looked::Absent };
        if e.is_tree() || e.is_gitlink() {
            Looked::Dir(OWN_DIR)
        } else if e.is_link() {
            match self.repo.link_target(&e.oid) {
                Ok(t) => Looked::Symlink(t.to_vec()),
                Err(_) => Looked::Absent,
            }
        } else {
            let Ok(size) = self.repo.blob_size(&e.oid) else { return Looked::Absent };
            Looked::File(FileInfo {
                size,
                owner: if e.exec() { OWN_EXEC } else { OWN_FILE },
                src: FileSrc::Git { oid: e.oid, blob: Arc::new(GitBlob { repo: self.repo.clone(), oid: e.oid }) },
            })
        }
    }

    pub fn list(&self, rel_dir: &[u8]) -> io::Result<Listing> {
        let st = self.dir_state(rel_dir).ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
        if let Some((real, rel)) = &st.live {
            return HostRegion::new(real.to_path_buf()).list(rel);
        }
        let mut names: BTreeMap<Vec<u8>, DType> = BTreeMap::new();
        if let Some(t) = &st.tree {
            for (name, e) in t.iter() {
                let dt = if e.is_tree() || e.is_gitlink() {
                    DType::Dir
                } else if e.is_link() {
                    DType::Symlink
                } else {
                    DType::File
                };
                names.insert(name.to_vec(), dt);
            }
        }
        if let Some(o) = &st.ov {
            for (name, e) in &o.entries {
                match e {
                    Ov::Deleted => {
                        names.remove(name);
                    }
                    Ov::File { .. } => {
                        names.insert(name.clone(), DType::File);
                    }
                    Ov::Symlink(_) => {
                        names.insert(name.clone(), DType::Symlink);
                    }
                    Ov::Dir(_) | Ov::Live(_) => {
                        names.insert(name.clone(), DType::Dir);
                    }
                }
            }
        }
        if rel_dir == b"/" || rel_dir.is_empty() {
            for h in &self.hidden {
                names.remove(h);
            }
        }
        Ok(names.into_iter().collect())
    }
}

fn hash_ov(d: &OvDir, h: &mut crate::hash::KeyHasher) {
    h.u64(d.entries.len() as u64);
    for (name, e) in &d.entries {
        h.field(name);
        match e {
            Ov::File { hash, exec, .. } => {
                h.field(b"f").field(&hash.0).u64(*exec as u64);
            }
            Ov::Symlink(t) => {
                h.field(b"l").field(t);
            }
            Ov::Deleted => {
                h.field(b"x");
            }
            Ov::Dir(d) => {
                h.field(b"d");
                hash_ov(d, h);
            }
            Ov::Live(p) => {
                h.field(b"L").field(p.as_os_str().as_bytes());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Worktree: keeps the overlay current
// ---------------------------------------------------------------------------

/// Flat overlay entry, keyed by path relative to the worktree root.
#[derive(Clone, Debug, PartialEq)]
enum Change {
    File { hash: Hash, exec: bool, size: u64 },
    Symlink(Vec<u8>),
    Deleted,
    /// A directory that exists in the worktree. Only recorded when HEAD has
    /// no directory there.
    Dir,
    Live,
}

struct WtState {
    tree: Oid,
    changes: BTreeMap<Vec<u8>, Change>,
    /// Directories being watched, relative paths (`""` is the root).
    watched: HashMap<Vec<u8>, i32>,
    /// Paths behind a smudge filter or eol conversion: never taken from git.
    filtered: HashSet<Vec<u8>>,
    snapshot: Option<Arc<Snapshot>>,
    scanned: bool,
}

pub struct Worktree {
    pub root: PathBuf,
    /// The worktree's own git directory (where HEAD and index live).
    pub git_dir: PathBuf,
    pub repo: Arc<GitRepo>,
    content: Arc<Content>,
    watcher: Arc<Watcher>,
    id: u64,
    st: Mutex<WtState>,
    suspects: Mutex<HashSet<Vec<u8>>>,
    rescan: AtomicBool,
    /// Counters for tests and status output.
    pub full_scans: AtomicU64,
    pub refreshed_paths: AtomicU64,
}

fn rel_join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    if dir.is_empty() {
        name.to_vec()
    } else {
        let mut v = dir.to_vec();
        v.push(b'/');
        v.extend_from_slice(name);
        v
    }
}

fn rel_parent(p: &[u8]) -> &[u8] {
    match p.iter().rposition(|&c| c == b'/') {
        Some(i) => &p[..i],
        None => b"",
    }
}

fn git_cmd(root: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(root).arg("--no-optional-locks");
    // The daemon may run as a different user than the repository owner.
    c.env("GIT_CONFIG_COUNT", "1").env("GIT_CONFIG_KEY_0", "safe.directory").env("GIT_CONFIG_VALUE_0", "*");
    c
}

impl Worktree {
    pub fn open(root: &Path, content: Arc<Content>, watcher: Arc<Watcher>, repos: &RepoCache) -> Result<Arc<Worktree>> {
        let root = root.canonicalize()?;
        let probe = git2::Repository::open(&root)
            .with_context(|| format!("{} is not the root of a git worktree", root.display()))?;
        let workdir = probe.workdir().map(|p| p.canonicalize()).transpose()?;
        if workdir.as_deref() != Some(&root) {
            bail!("{} is not the root of a git worktree", root.display());
        }
        let git_dir = probe.path().canonicalize()?;
        let common = probe.commondir().canonicalize()?;
        drop(probe);
        let repo = repos.get(&common, &root)?;
        let wt = Arc::new(Worktree {
            root,
            git_dir,
            repo,
            content,
            watcher: watcher.clone(),
            id: watcher.next_id.fetch_add(1, Ordering::Relaxed),
            st: Mutex::new(WtState {
                tree: [0u8; 20],
                changes: BTreeMap::new(),
                watched: HashMap::new(),
                filtered: HashSet::new(),
                snapshot: None,
                scanned: false,
            }),
            suspects: Mutex::new(HashSet::new()),
            rescan: AtomicBool::new(false),
            full_scans: AtomicU64::new(0),
            refreshed_paths: AtomicU64::new(0),
        });
        watcher.register(&wt);
        Ok(wt)
    }

    fn head_tree(&self) -> Result<Oid> {
        let repo = git2::Repository::open(&self.root)?;
        let head = match repo.head() {
            Ok(h) => h,
            Err(e) if e.code() == git2::ErrorCode::UnbornBranch || e.code() == git2::ErrorCode::NotFound => {
                return Ok([0u8; 20]);
            }
            Err(e) => return Err(e.into()),
        };
        Ok(oid_of(head.peel_to_tree()?.id()))
    }

    /// Returns a snapshot of the worktree as it is now.
    pub fn snapshot(self: &Arc<Self>) -> Result<Arc<Snapshot>> {
        let mut st = self.st.lock();
        // Everything that happened before this point has been delivered once
        // the cookie comes back.
        let overflowed = self.watcher.sync(self)?;
        let tree = self.head_tree()?;
        let mut changed = false;
        if !st.scanned || overflowed || self.rescan.swap(false, Ordering::SeqCst) {
            self.full_scan(&mut st, tree)?;
            changed = true;
        } else {
            let mut suspects: HashSet<Vec<u8>> = std::mem::take(&mut *self.suspects.lock());
            if tree != st.tree {
                for p in self.repo.diff_trees(&st.tree, &tree)? {
                    suspects.insert(p);
                }
                // Entries recorded against the old tree may be redundant or
                // meaningless against the new one.
                suspects.extend(st.changes.keys().cloned());
                st.tree = tree;
                changed = true;
            }
            if suspects.iter().any(|p| {
                let name = p.rsplit(|&c| c == b'/').next().unwrap_or(p);
                name == b".gitattributes" || name == b".gitignore"
            }) {
                self.full_scan(&mut st, tree)?;
                changed = true;
            } else if !suspects.is_empty() {
                let mut list: Vec<Vec<u8>> = suspects.into_iter().collect();
                list.sort();
                for p in list {
                    changed |= self.refresh(&mut st, &p)?;
                }
            }
        }
        if changed || st.snapshot.is_none() {
            let overlay = build_overlay(&self.root, &st.changes);
            let reuse = st.snapshot.as_ref().filter(|s| s.tree == st.tree);
            let snap = Snapshot::new(self.repo.clone(), st.tree, Arc::new(overlay));
            // Keep the same object (and its memoised directory states) when
            // nothing actually changed.
            match reuse {
                Some(old) if old.id == snap.id => {}
                _ => st.snapshot = Some(Arc::new(snap)),
            }
        }
        Ok(st.snapshot.clone().unwrap())
    }

    fn tree_entry(&self, tree: &Oid, rel: &[u8]) -> Option<TreeEnt> {
        if *tree == [0u8; 20] {
            return None;
        }
        let mut cur = self.repo.tree(tree).ok()?;
        let mut comps = rel.split(|&c| c == b'/').peekable();
        while let Some(c) = comps.next() {
            let e = *cur.get(c)?;
            if comps.peek().is_none() {
                return Some(e);
            }
            if !e.is_tree() {
                return None;
            }
            cur = self.repo.tree(&e.oid).ok()?;
        }
        None
    }

    fn real(&self, rel: &[u8]) -> PathBuf {
        if rel.is_empty() { self.root.clone() } else { self.root.join(OsStr::from_bytes(rel)) }
    }

    fn in_live(&self, st: &WtState, rel: &[u8]) -> bool {
        let mut p = rel;
        loop {
            if matches!(st.changes.get(p), Some(Change::Live)) {
                return true;
            }
            if p.is_empty() {
                return false;
            }
            p = rel_parent(p);
        }
    }

    fn remove_under(st: &mut WtState, rel: &[u8]) -> bool {
        let mut prefix = rel.to_vec();
        prefix.push(b'/');
        let doomed: Vec<Vec<u8>> = st
            .changes
            .range(prefix.clone()..)
            .take_while(|(k, _)| k.starts_with(&prefix))
            .map(|(k, _)| k.clone())
            .collect();
        let any = !doomed.is_empty();
        for k in doomed {
            st.changes.remove(&k);
        }
        any
    }

    fn set(st: &mut WtState, rel: &[u8], c: Option<Change>) -> bool {
        match c {
            Some(c) => st.changes.insert(rel.to_vec(), c.clone()) != Some(c),
            None => st.changes.remove(rel).is_some(),
        }
    }

    /// Brings the overlay entry for one path in line with the worktree.
    /// Returns whether the overlay changed.
    fn refresh(&self, st: &mut WtState, rel: &[u8]) -> Result<bool> {
        if rel.is_empty() || rel == b".git" || rel.starts_with(b".git/") {
            return Ok(false);
        }
        // A path below a live directory is served from disk directly.
        if self.in_live(st, rel_parent(rel)) {
            return Ok(false);
        }
        self.refreshed_paths.fetch_add(1, Ordering::Relaxed);
        let real = self.real(rel);
        let head = self.tree_entry(&st.tree, rel);
        let meta = match std::fs::symlink_metadata(&real) {
            Ok(m) => m,
            Err(_) => {
                let mut changed = Self::remove_under(st, rel);
                changed |= Self::set(st, rel, head.map(|_| Change::Deleted));
                return Ok(changed);
            }
        };
        let ft = meta.file_type();
        if ft.is_dir() {
            if matches!(st.changes.get(rel), Some(Change::Live)) {
                return Ok(false);
            }
            let known = st.watched.contains_key(rel);
            let mut changed = false;
            let head_is_dir = head.map(|e| e.is_tree()).unwrap_or(false);
            if head.map(|e| e.is_gitlink()).unwrap_or(false) || (!known && self.is_ignored(rel)) {
                changed |= Self::remove_under(st, rel);
                changed |= Self::set(st, rel, Some(Change::Live));
                return Ok(changed);
            }
            changed |= Self::set(st, rel, (!head_is_dir).then_some(Change::Dir));
            if !known {
                // New directory: watch first, then look inside, so nothing
                // created in between is missed.
                self.watch_dir(st, rel);
                for e in std::fs::read_dir(&real).into_iter().flatten().flatten() {
                    let child = rel_join(rel, e.file_name().as_bytes());
                    changed |= self.refresh(st, &child)?;
                }
            }
            return Ok(changed);
        }
        // Not a directory any more: forget anything recorded below.
        let mut changed = Self::remove_under(st, rel);
        if let Some(wd) = st.watched.remove(rel) {
            self.watcher.unwatch(wd);
        }
        if ft.is_symlink() {
            let target = std::fs::read_link(&real)?.into_os_string().into_encoded_bytes();
            let same = head.map(|e| e.is_link() && self.repo.link_target(&e.oid).map(|t| &t[..] == &target[..]).unwrap_or(false)).unwrap_or(false);
            changed |= Self::set(st, rel, (!same).then_some(Change::Symlink(target)));
            return Ok(changed);
        }
        if !ft.is_file() {
            // Sockets, fifos, devices: not part of a snapshot.
            changed |= Self::set(st, rel, head.map(|_| Change::Deleted));
            return Ok(changed);
        }
        let exec = meta.mode() & 0o111 != 0;
        let (hash, oid, size) = self.capture(&real)?;
        let clean = head
            .map(|e| !e.is_tree() && !e.is_link() && !e.is_gitlink() && e.oid == oid && e.exec() == exec)
            .unwrap_or(false)
            && !st.filtered.contains(rel);
        changed |= Self::set(st, rel, (!clean).then_some(Change::File { hash, exec, size }));
        Ok(changed)
    }

    /// Copies a worktree file into the store, computing its blake3 and git
    /// blob id. Memoised by file identity.
    fn capture(&self, real: &Path) -> Result<(Hash, Oid, u64)> {
        for _ in 0..5 {
            let mut f = std::fs::File::open(real)?;
            let sig = HostSig::of(&f.metadata()?);
            let memo_key = capture_key(&sig);
            if let Some(v) = self.content.store.db().get(crate::db::Table::Meta, &memo_key) {
                if v.len() == 52 {
                    let hash = Hash(v[..32].try_into().unwrap());
                    if self.content.store.has(&hash) {
                        self.content.store.touch(&hash);
                        return Ok((hash, v[32..52].try_into().unwrap(), sig.size));
                    }
                }
            }
            let mut b3 = blake3::Hasher::new();
            let mut s1 = Sha1::new();
            s1.update(format!("blob {}\0", sig.size).as_bytes());
            let tmp = self.content.store.tmp_path();
            let mut out = std::fs::File::create(&tmp)?;
            let mut buf = vec![0u8; 256 * 1024];
            let mut total = 0u64;
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                b3.update(&buf[..n]);
                s1.update(&buf[..n]);
                out.write_all(&buf[..n])?;
                total += n as u64;
            }
            drop(out);
            // The file must not have changed while it was being read.
            if total != sig.size || HostSig::of(&f.metadata()?) != sig {
                let _ = std::fs::remove_file(&tmp);
                continue;
            }
            let hash = Hash(*b3.finalize().as_bytes());
            let oid: Oid = s1.finalize().into();
            self.content.store.install_hashed(&tmp, hash, total)?;
            self.content.record_git(oid, hash);
            let mut v = Vec::with_capacity(52);
            v.extend_from_slice(&hash.0);
            v.extend_from_slice(&oid);
            self.content.store.db().put(crate::db::Table::Meta, &memo_key, &v);
            return Ok((hash, oid, total));
        }
        bail!("{} keeps changing while being captured", real.display())
    }

    fn is_ignored(&self, rel: &[u8]) -> bool {
        git_cmd(&self.root)
            .arg("check-ignore")
            .arg("-q")
            .arg("--")
            .arg(OsStr::from_bytes(rel))
            .status()
            .map(|s| s.code() == Some(0))
            .unwrap_or(false)
    }

    fn watch_dir(&self, st: &mut WtState, rel: &[u8]) {
        if st.watched.contains_key(rel) {
            return;
        }
        if let Some(wd) = self.watcher.watch(self.id, &self.real(rel), rel) {
            st.watched.insert(rel.to_vec(), wd);
        }
    }

    /// Rebuilds the overlay from `git status` and a directory walk.
    fn full_scan(&self, st: &mut WtState, tree: Oid) -> Result<()> {
        self.full_scans.fetch_add(1, Ordering::Relaxed);
        // Events for anything that changes from here on will be seen by the
        // next snapshot; drop what is queued, the scan supersedes it.
        self.suspects.lock().clear();
        st.tree = tree;
        st.changes.clear();
        st.scanned = true;
        self.watch_dir(st, b"");

        let out = git_cmd(&self.root)
            .args(["status", "--porcelain=v2", "-z", "--untracked-files=all", "--ignored=matching", "--no-renames"])
            .output()
            .context("running git status")?;
        if !out.status.success() {
            bail!("git status failed: {}", String::from_utf8_lossy(&out.stderr));
        }
        let mut suspects: Vec<Vec<u8>> = Vec::new();
        let mut ignored_dirs: Vec<Vec<u8>> = Vec::new();
        for rec in out.stdout.split(|&c| c == 0) {
            if rec.len() < 3 {
                continue;
            }
            let path = match rec[0] {
                b'1' => rec.splitn(9, |&c| c == b' ').nth(8),
                b'u' => rec.splitn(11, |&c| c == b' ').nth(10),
                b'?' => Some(&rec[2..]),
                b'!' => {
                    let p = &rec[2..];
                    if let Some(dir) = p.strip_suffix(b"/") {
                        ignored_dirs.push(dir.to_vec());
                        None
                    } else {
                        Some(p)
                    }
                }
                _ => None,
            };
            if let Some(p) = path {
                suspects.push(p.to_vec());
            }
        }
        for d in &ignored_dirs {
            st.changes.insert(d.clone(), Change::Live);
        }
        st.filtered = self.filtered_paths()?;
        suspects.extend(st.filtered.iter().cloned());

        // Walk every directory that is not live: set up watches and find
        // directories HEAD does not know (git does not report empty ones).
        let mut seen_dirs: HashSet<Vec<u8>> = HashSet::new();
        let mut stack: Vec<Vec<u8>> = vec![Vec::new()];
        while let Some(dir) = stack.pop() {
            self.watch_dir(st, &dir);
            seen_dirs.insert(dir.clone());
            let Ok(rd) = std::fs::read_dir(self.real(&dir)) else { continue };
            for e in rd.flatten() {
                let Ok(ft) = e.file_type() else { continue };
                if !ft.is_dir() {
                    continue;
                }
                let child = rel_join(&dir, e.file_name().as_bytes());
                if child == b".git" || matches!(st.changes.get(&child), Some(Change::Live)) {
                    continue;
                }
                match self.tree_entry(&tree, &child) {
                    Some(te) if te.is_gitlink() => {
                        st.changes.insert(child, Change::Live);
                        continue;
                    }
                    Some(te) if te.is_tree() => {}
                    _ => {
                        st.changes.insert(child.clone(), Change::Dir);
                    }
                }
                stack.push(child);
            }
        }
        // Drop watches for directories that are gone.
        let gone: Vec<Vec<u8>> = st.watched.keys().filter(|k| !seen_dirs.contains(*k)).cloned().collect();
        for k in gone {
            if let Some(wd) = st.watched.remove(&k) {
                self.watcher.unwatch(wd);
            }
        }
        // Tracked directories missing from disk: mark the topmost one.
        self.mark_missing_dirs(st, &tree, b"", &seen_dirs);

        suspects.sort();
        suspects.dedup();
        for p in suspects {
            self.refresh(st, &p)?;
        }
        Ok(())
    }

    fn mark_missing_dirs(&self, st: &mut WtState, tree: &Oid, dir: &[u8], seen: &HashSet<Vec<u8>>) {
        let Ok(t) = self.repo.tree(tree) else { return };
        for (name, e) in t.iter() {
            if !e.is_tree() {
                continue;
            }
            let child = rel_join(dir, name);
            if seen.contains(&child) {
                self.mark_missing_dirs(st, &e.oid, &child, seen);
            } else if !st.changes.contains_key(&child) && std::fs::symlink_metadata(self.real(&child)).is_err() {
                st.changes.insert(child, Change::Deleted);
            }
        }
    }

    /// Tracked paths whose worktree bytes are not the blob's bytes.
    fn filtered_paths(&self) -> Result<HashSet<Vec<u8>>> {
        let mut out = HashSet::new();
        // Without any attributes file and without autocrlf nothing is filtered.
        let cfg = git_cmd(&self.root).args(["config", "--get", "core.autocrlf"]).output()?;
        let autocrlf = String::from_utf8_lossy(&cfg.stdout).trim() == "true";
        let ls = git_cmd(&self.root).args(["ls-files", "-z"]).output()?;
        if !ls.status.success() {
            return Ok(out);
        }
        if autocrlf {
            for p in ls.stdout.split(|&c| c == 0).filter(|p| !p.is_empty()) {
                out.insert(p.to_vec());
            }
            return Ok(out);
        }
        let has_attrs = ls.stdout.split(|&c| c == 0).any(|p| p == b".gitattributes" || p.ends_with(b"/.gitattributes"))
            || self.git_dir.join("info/attributes").exists()
            || self.root.join(".gitattributes").exists();
        if !has_attrs {
            return Ok(out);
        }
        let mut child = git_cmd(&self.root)
            .args(["check-attr", "-z", "--stdin", "filter", "eol", "working-tree-encoding", "ident"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()?;
        let mut stdin = child.stdin.take().unwrap();
        let input = ls.stdout;
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
        let mut res = Vec::new();
        child.stdout.take().unwrap().read_to_end(&mut res)?;
        let _ = writer.join();
        let _ = child.wait();
        let mut it = res.split(|&c| c == 0);
        while let (Some(path), Some(attr), Some(value)) = (it.next(), it.next(), it.next()) {
            let risky = match attr {
                b"eol" => value == b"crlf",
                b"filter" | b"working-tree-encoding" | b"ident" => {
                    value != b"unspecified" && value != b"unset"
                }
                _ => false,
            };
            if risky {
                out.insert(path.to_vec());
            }
        }
        Ok(out)
    }

    /// Number of overlay entries (for status and tests).
    pub fn dirty_count(&self) -> usize {
        self.st.lock().changes.len()
    }
}

fn capture_key(sig: &HostSig) -> Vec<u8> {
    let mut k = b"cap:".to_vec();
    k.extend_from_slice(&bincode::serialize(sig).unwrap());
    k
}

fn build_overlay(root: &Path, changes: &BTreeMap<Vec<u8>, Change>) -> OvDir {
    #[derive(Default)]
    struct B {
        leaf: Option<Change>,
        kids: BTreeMap<Vec<u8>, B>,
    }
    let mut top = B::default();
    for (path, c) in changes {
        let mut cur = &mut top;
        for comp in path.split(|&c| c == b'/') {
            cur = cur.kids.entry(comp.to_vec()).or_default();
        }
        cur.leaf = Some(c.clone());
    }
    fn conv(root: &Path, rel: &[u8], b: &B) -> OvDir {
        let mut d = OvDir::default();
        for (name, kid) in &b.kids {
            let child_rel = rel_join(rel, name);
            let ov = match &kid.leaf {
                Some(Change::File { hash, exec, size }) => Ov::File { hash: *hash, exec: *exec, size: *size },
                Some(Change::Symlink(t)) => Ov::Symlink(t.clone()),
                Some(Change::Deleted) => Ov::Deleted,
                Some(Change::Live) => Ov::Live(Arc::from(root.join(OsStr::from_bytes(&child_rel)))),
                Some(Change::Dir) | None => Ov::Dir(Arc::new(conv(root, &child_rel, kid))),
            };
            d.entries.insert(name.clone(), ov);
        }
        d
    }
    conv(root, b"", &top)
}

/// Caches one [`GitRepo`] per git common directory.
#[derive(Default)]
pub struct RepoCache {
    repos: Mutex<HashMap<PathBuf, Arc<GitRepo>>>,
}

impl RepoCache {
    fn get(&self, common: &Path, worktree: &Path) -> Result<Arc<GitRepo>> {
        let mut m = self.repos.lock();
        if let Some(r) = m.get(common) {
            return Ok(r.clone());
        }
        let r = GitRepo::open(worktree)?;
        m.insert(common.to_path_buf(), r.clone());
        Ok(r)
    }
}

// ---------------------------------------------------------------------------
// inotify watcher
// ---------------------------------------------------------------------------

struct Watch {
    wt: u64,
    rel: Vec<u8>,
}

/// One inotify instance for all worktrees.
pub struct Watcher {
    fd: OwnedFd,
    watches: Mutex<HashMap<i32, Watch>>,
    worktrees: Mutex<HashMap<u64, std::sync::Weak<Worktree>>>,
    next_id: AtomicU64,
    cookie_seq: AtomicU64,
    cookies_seen: Mutex<HashSet<u64>>,
    cookie_cv: Condvar,
    /// Bumped on every queue overflow.
    overflows: AtomicU64,
    seen_overflow: Mutex<HashMap<u64, u64>>,
    cookie_dir: PathBuf,
    cookie_wd: i32,
}

const WATCH_MASK: u32 = libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_MODIFY
    | libc::IN_CLOSE_WRITE
    | libc::IN_MOVED_FROM
    | libc::IN_MOVED_TO
    | libc::IN_ATTRIB
    | libc::IN_DONT_FOLLOW
    | libc::IN_ONLYDIR;

impl Watcher {
    /// `cookie_dir` is a private directory used to synchronise with the
    /// event stream.
    pub fn start(cookie_dir: &Path) -> Result<Arc<Watcher>> {
        std::fs::create_dir_all(cookie_dir)?;
        // SAFETY: plain syscall.
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: fd was just created.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let c = std::ffi::CString::new(cookie_dir.as_os_str().as_bytes())?;
        // SAFETY: valid fd and C string.
        let cookie_wd = unsafe { libc::inotify_add_watch(fd.as_raw_fd(), c.as_ptr(), libc::IN_CREATE | libc::IN_ONLYDIR) };
        if cookie_wd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let w = Arc::new(Watcher {
            fd,
            watches: Mutex::new(HashMap::new()),
            worktrees: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            cookie_seq: AtomicU64::new(1),
            cookies_seen: Mutex::new(HashSet::new()),
            cookie_cv: Condvar::new(),
            overflows: AtomicU64::new(0),
            seen_overflow: Mutex::new(HashMap::new()),
            cookie_dir: cookie_dir.to_path_buf(),
            cookie_wd,
        });
        let w2 = Arc::downgrade(&w);
        std::thread::Builder::new().name("inotify".into()).spawn(move || watcher_loop(w2))?;
        Ok(w)
    }

    fn register(&self, wt: &Arc<Worktree>) {
        self.worktrees.lock().insert(wt.id, Arc::downgrade(wt));
    }

    fn watch(&self, wt: u64, real: &Path, rel: &[u8]) -> Option<i32> {
        let c = std::ffi::CString::new(real.as_os_str().as_bytes()).ok()?;
        // Hold the map lock across the syscall so the reader thread cannot
        // see an event for a descriptor it does not know yet.
        let mut watches = self.watches.lock();
        // SAFETY: valid fd and C string.
        let wd = unsafe { libc::inotify_add_watch(self.fd.as_raw_fd(), c.as_ptr(), WATCH_MASK) };
        if wd < 0 {
            return None;
        }
        watches.insert(wd, Watch { wt, rel: rel.to_vec() });
        Some(wd)
    }

    fn unwatch(&self, wd: i32) {
        self.watches.lock().remove(&wd);
        // SAFETY: valid fd; removing a stale descriptor is harmless.
        unsafe { libc::inotify_rm_watch(self.fd.as_raw_fd(), wd) };
    }

    /// Waits until every event queued before this call has been processed.
    /// Returns true if events were lost since this worktree last synced.
    fn sync(&self, wt: &Worktree) -> Result<bool> {
        let n = self.cookie_seq.fetch_add(1, Ordering::Relaxed);
        let path = self.cookie_dir.join(format!("c{n}"));
        std::fs::File::create(&path)?;
        let mut seen = self.cookies_seen.lock();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !seen.remove(&n) {
            if self.cookie_cv.wait_until(&mut seen, deadline).timed_out() {
                drop(seen);
                let _ = std::fs::remove_file(&path);
                bail!("file watcher did not respond");
            }
        }
        drop(seen);
        let _ = std::fs::remove_file(&path);
        let now = self.overflows.load(Ordering::SeqCst);
        let mut so = self.seen_overflow.lock();
        let prev = so.insert(wt.id, now).unwrap_or(0);
        Ok(prev != now)
    }
}

fn watcher_loop(w: std::sync::Weak<Watcher>) {
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let Some(this) = w.upgrade() else { return };
        let fd = this.fd.as_raw_fd();
        drop(this);
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // Wake periodically so the thread notices the watcher being dropped.
        // SAFETY: valid pollfd.
        let r = unsafe { libc::poll(&mut pfd, 1, 1000) };
        if r <= 0 {
            continue;
        }
        let Some(this) = w.upgrade() else { return };
        // SAFETY: reading into a valid buffer.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) };
        if n <= 0 {
            continue;
        }
        let mut off = 0usize;
        let n = n as usize;
        let hdr = std::mem::size_of::<libc::inotify_event>();
        while off + hdr <= n {
            // SAFETY: the kernel wrote a complete inotify_event at this offset.
            let ev: libc::inotify_event = unsafe { std::ptr::read_unaligned(buf.as_ptr().add(off) as *const _) };
            let name_bytes = &buf[off + hdr..off + hdr + ev.len as usize];
            let name = match name_bytes.iter().position(|&c| c == 0) {
                Some(i) => &name_bytes[..i],
                None => name_bytes,
            };
            off += hdr + ev.len as usize;
            if ev.mask & libc::IN_Q_OVERFLOW != 0 {
                this.overflows.fetch_add(1, Ordering::SeqCst);
                continue;
            }
            if ev.wd == this.cookie_wd {
                if let Some(n) = std::str::from_utf8(name).ok().and_then(|s| s.strip_prefix('c')).and_then(|s| s.parse().ok()) {
                    this.cookies_seen.lock().insert(n);
                    this.cookie_cv.notify_all();
                }
                continue;
            }
            if ev.mask & libc::IN_IGNORED != 0 {
                this.watches.lock().remove(&ev.wd);
                continue;
            }
            let target = {
                let watches = this.watches.lock();
                watches.get(&ev.wd).map(|wa| (wa.wt, rel_join(&wa.rel, name)))
            };
            let Some((wt_id, rel)) = target else { continue };
            let wt = this.worktrees.lock().get(&wt_id).and_then(|w| w.upgrade());
            if let Some(wt) = wt {
                wt.suspects.lock().insert(rel);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    struct Fx {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        content: Arc<Content>,
        watcher: Arc<Watcher>,
        repos: RepoCache,
    }

    fn fixture() -> Fx {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        fs::create_dir_all(root.join("src/deep")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        fs::write(root.join("src/deep/x.rs"), "x\n").unwrap();
        fs::write(root.join("run.sh"), "#!/bin/sh\n").unwrap();
        fs::write(root.join(".gitignore"), "/target\n*.log\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("src/lib.rs", root.join("link")).unwrap();
        git(tmp.path(), &["init", "-q", "-b", "main", "repo"]);
        git(&root, &["add", "-A"]);
        git(&root, &["commit", "-q", "-m", "init"]);
        let db = Arc::new(crate::db::Db::open(&tmp.path().join("db.redb")).unwrap());
        let store = Arc::new(Store::open(&tmp.path().join("store"), db).unwrap());
        let content = Arc::new(Content::new(store));
        let watcher = Watcher::start(&tmp.path().join("cookies")).unwrap();
        Fx { root, content, watcher, repos: RepoCache::default(), _tmp: tmp }
    }

    fn read(fx: &Fx, s: &Snapshot, dir: &[u8], name: &[u8]) -> Option<Vec<u8>> {
        match s.lookup(dir, name) {
            Looked::File(f) => {
                let h = fx.content.hash(&f.src).unwrap();
                let bytes = fx.content.store.read(&h).unwrap();
                assert_eq!(bytes.len() as u64, f.size);
                Some(bytes)
            }
            _ => None,
        }
    }

    #[test]
    fn clean_tree_is_served_from_git() {
        let fx = fixture();
        let wt = Worktree::open(&fx.root, fx.content.clone(), fx.watcher.clone(), &fx.repos).unwrap();
        let s = wt.snapshot().unwrap();
        assert_eq!(wt.dirty_count(), 0);
        assert_eq!(read(&fx, &s, b"/src", b"lib.rs").unwrap(), b"pub fn a() {}\n");
        assert_eq!(read(&fx, &s, b"/src/deep", b"x.rs").unwrap(), b"x\n");
        assert!(matches!(s.lookup(b"/", b"link"), Looked::Symlink(t) if t == b"src/lib.rs"));
        assert!(matches!(s.lookup(b"/", b"run.sh"), Looked::File(f) if f.exec()));
        assert!(matches!(s.lookup(b"/", b"nope"), Looked::Absent));
        assert!(matches!(s.lookup(b"/", b".git"), Looked::Absent));
        let mut names: Vec<Vec<u8>> = s.list(b"/").unwrap().into_iter().map(|e| e.0).collect();
        names.sort();
        assert_eq!(names, vec![b".gitignore".to_vec(), b"link".to_vec(), b"run.sh".to_vec(), b"src".to_vec()]);
        // A second snapshot with nothing changed is the same object and costs
        // no scan.
        let s2 = wt.snapshot().unwrap();
        assert!(Arc::ptr_eq(&s, &s2));
        assert_eq!(wt.full_scans.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn edits_are_picked_up_from_events() {
        let fx = fixture();
        let wt = Worktree::open(&fx.root, fx.content.clone(), fx.watcher.clone(), &fx.repos).unwrap();
        let s1 = wt.snapshot().unwrap();
        fs::write(fx.root.join("src/lib.rs"), "pub fn b() {}\n").unwrap();
        fs::write(fx.root.join("src/new.rs"), "new\n").unwrap();
        fs::remove_file(fx.root.join("src/deep/x.rs")).unwrap();
        fs::create_dir_all(fx.root.join("fresh/inner")).unwrap();
        fs::write(fx.root.join("fresh/inner/f.txt"), "f\n").unwrap();
        fs::create_dir(fx.root.join("emptydir")).unwrap();
        let s2 = wt.snapshot().unwrap();
        assert_eq!(wt.full_scans.load(Ordering::Relaxed), 1, "no rescan needed");
        assert_ne!(s1.id, s2.id);
        // The old snapshot is unchanged.
        assert_eq!(read(&fx, &s1, b"/src", b"lib.rs").unwrap(), b"pub fn a() {}\n");
        assert_eq!(read(&fx, &s2, b"/src", b"lib.rs").unwrap(), b"pub fn b() {}\n");
        assert_eq!(read(&fx, &s2, b"/src", b"new.rs").unwrap(), b"new\n");
        assert!(matches!(s2.lookup(b"/src/deep", b"x.rs"), Looked::Absent));
        assert_eq!(read(&fx, &s2, b"/fresh/inner", b"f.txt").unwrap(), b"f\n");
        assert!(matches!(s2.lookup(b"/", b"emptydir"), Looked::Dir(_)));
        assert!(s2.list(b"/emptydir").unwrap().is_empty());
        // Reverting an edit makes the path clean again.
        fs::write(fx.root.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        fs::remove_file(fx.root.join("src/new.rs")).unwrap();
        let s3 = wt.snapshot().unwrap();
        assert!(matches!(s3.lookup(b"/src", b"lib.rs"), Looked::File(f) if matches!(f.src, FileSrc::Git { .. })));
        assert!(matches!(s3.lookup(b"/src", b"new.rs"), Looked::Absent));
    }

    #[test]
    fn head_moves_without_touching_files() {
        let fx = fixture();
        let wt = Worktree::open(&fx.root, fx.content.clone(), fx.watcher.clone(), &fx.repos).unwrap();
        fs::write(fx.root.join("src/lib.rs"), "pub fn c() {}\n").unwrap();
        let s1 = wt.snapshot().unwrap();
        assert_eq!(wt.dirty_count(), 1);
        git(&fx.root, &["commit", "-q", "-am", "second"]);
        let s2 = wt.snapshot().unwrap();
        assert_eq!(wt.dirty_count(), 0, "committed file is clean again");
        assert_ne!(s1.tree, s2.tree);
        assert_eq!(read(&fx, &s2, b"/src", b"lib.rs").unwrap(), b"pub fn c() {}\n");
        // Soft reset: HEAD moves back, the file on disk stays.
        git(&fx.root, &["reset", "-q", "--soft", "HEAD~1"]);
        let s3 = wt.snapshot().unwrap();
        assert_eq!(wt.dirty_count(), 1);
        assert_eq!(read(&fx, &s3, b"/src", b"lib.rs").unwrap(), b"pub fn c() {}\n");
        assert_eq!(s3.id, s1.id, "same tree and same overlay give the same snapshot id");
    }

    #[test]
    fn ignored_dirs_are_live_and_ignored_files_are_captured() {
        let fx = fixture();
        fs::create_dir_all(fx.root.join("target/debug")).unwrap();
        fs::write(fx.root.join("target/debug/out"), "artifact").unwrap();
        fs::write(fx.root.join("build.log"), "log line\n").unwrap();
        let wt = Worktree::open(&fx.root, fx.content.clone(), fx.watcher.clone(), &fx.repos).unwrap();
        let s = wt.snapshot().unwrap();
        assert!(matches!(s.lookup(b"/", b"target"), Looked::Dir(_)));
        assert!(matches!(s.lookup(b"/target/debug", b"out"), Looked::File(f) if matches!(f.src, FileSrc::Host { .. })));
        assert_eq!(read(&fx, &s, b"/", b"build.log").unwrap(), b"log line\n");
        // Changes inside the live directory do not make the snapshot new.
        fs::write(fx.root.join("target/debug/out2"), "more").unwrap();
        let s2 = wt.snapshot().unwrap();
        assert!(Arc::ptr_eq(&s, &s2));
    }

    #[test]
    fn two_worktrees_share_identity() {
        let fx = fixture();
        let other = fx.root.parent().unwrap().join("wt2");
        git(&fx.root, &["worktree", "add", "-q", other.to_str().unwrap(), "-b", "other"]);
        let a = Worktree::open(&fx.root, fx.content.clone(), fx.watcher.clone(), &fx.repos).unwrap();
        let b = Worktree::open(&other, fx.content.clone(), fx.watcher.clone(), &fx.repos).unwrap();
        assert!(Arc::ptr_eq(&a.repo, &b.repo));
        let sa = a.snapshot().unwrap();
        let sb = b.snapshot().unwrap();
        assert_eq!(sa.id, sb.id);
        // The same edit in both gives the same snapshot again.
        fs::write(fx.root.join("src/lib.rs"), "edited\n").unwrap();
        fs::write(other.join("src/lib.rs"), "edited\n").unwrap();
        assert_eq!(a.snapshot().unwrap().id, b.snapshot().unwrap().id);
        // Dirty in one, committed in the other: content hashes agree.
        git(&other, &["commit", "-q", "-am", "edit"]);
        let sb = b.snapshot().unwrap();
        let sa = a.snapshot().unwrap();
        let (Looked::File(fa), Looked::File(fb)) = (sa.lookup(b"/src", b"lib.rs"), sb.lookup(b"/src", b"lib.rs")) else {
            panic!()
        };
        assert!(matches!(fb.src, FileSrc::Git { .. }));
        assert_eq!(fx.content.known_hash(&fa.src), fx.content.known_hash(&fb.src));
    }
}
