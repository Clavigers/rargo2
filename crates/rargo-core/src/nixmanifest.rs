#[cfg(test)]
use rargo_machine::StateMachine;
use crate::db::{Table, Tables};
use crate::hash::Hash;
use crate::view::{DType, FileInfo, FileSrc, HostSig, Listing, Looked, Owner};
use anyhow::{Context, Result};
use dashmap::DashMap;
use rargo_proto::tree::{self, Node, Tree};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

const NIX_DIR: Owner = Owner {
    uid: 0,
    gid: 0,
    mode: 0o555,
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cost {
    pub hashed_files: u64,
    pub hashed_bytes: u64,
    pub millis: u64,
    pub reused_paths: u64,
    pub new_paths: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prepared {
    pub roots: Vec<Hash>,
    pub cost: Cost,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum StoredKind {
    Dir {
        root: Hash,
        trees: Vec<(Hash, Vec<u8>)>,
    },
    File {
        hash: Hash,
        size: u64,
        exec: bool,
    },
    Link(Vec<u8>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WirePath {
    pub name: Vec<u8>,
    pub kind: StoredKind,
}

pub fn identity(kind: &StoredKind) -> Hash {
    match kind {
        StoredKind::Dir { root, .. } => *root,
        StoredKind::File { hash, .. } => *hash,
        StoredKind::Link(target) => Hash::of(target),
    }
}

fn meta_key(name: &[u8]) -> Vec<u8> {
    let mut k = b"nixpath/".to_vec();
    k.extend_from_slice(name);
    k
}

pub fn load(db: &dyn Tables, name: &[u8]) -> Option<StoredKind> {
    let v = db.get(Table::Meta, &meta_key(name))?;
    bincode::deserialize(&v).ok()
}

fn save(db: &dyn Tables, name: &[u8], kind: &StoredKind) -> Result<()> {
    db.put(Table::Meta, &meta_key(name), &bincode::serialize(kind)?);
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    File { exec: bool },
    Dir,
    Link(Vec<u8>),
    Other,
}

pub trait StoreFs: Send + Sync {
    fn stat(&self, path: &Path) -> io::Result<Kind>;
    fn list(&self, dir: &Path) -> io::Result<Vec<(Vec<u8>, Kind)>>;
    fn hash(&self, path: &Path) -> io::Result<(Hash, u64)>;
    fn host(&self, path: &Path) -> io::Result<(Kind, HostSig)>;
}

pub struct Disk;

/// Where the files of hashed store paths are recorded, by content hash.
pub trait HostFiles {
    /// Records the store file at `path` as holding the bytes of `hash`, unless a holder is recorded already.
    fn record(&self, hash: Hash, path: PathBuf, sig: HostSig);
}

impl HostFiles for DashMap<Hash, (Arc<Path>, HostSig)> {
    fn record(&self, hash: Hash, path: PathBuf, sig: HostSig) {
        self.entry(hash).or_insert_with(|| (Arc::from(path), sig));
    }
}

/// The store paths whose files are recorded already.
#[derive(Default)]
pub struct Attached(parking_lot::Mutex<BTreeSet<String>>);

impl Attached {
    fn lock(&self) -> parking_lot::MutexGuard<'_, BTreeSet<String>> {
        self.0.lock()
    }
}

fn kind_of(path: &Path, meta: &crate::io::file::Stat) -> io::Result<Kind> {
    let ft = meta.file_type();
    Ok(if ft.is_symlink() {
        Kind::Link(crate::io::file::read_link(path)?.as_os_str().as_bytes().to_vec())
    } else if ft.is_dir() {
        Kind::Dir
    } else if ft.is_file() {
        Kind::File { exec: meta.mode() & 0o111 != 0 }
    } else {
        Kind::Other
    })
}

impl StoreFs for Disk {
    fn stat(&self, path: &Path) -> io::Result<Kind> {
        kind_of(path, &crate::io::file::symlink_metadata(path)?)
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<(Vec<u8>, Kind)>> {
        let mut out = Vec::new();
        for ent in crate::io::file::read_dir(dir)? {
            let ent = ent?;
            let path = ent.path();
            out.push((ent.file_name().as_bytes().to_vec(), kind_of(&path, &crate::io::file::symlink_metadata(&path)?)?));
        }
        Ok(out)
    }

    fn hash(&self, path: &Path) -> io::Result<(Hash, u64)> {
        Hash::of_path(path)
    }

    fn host(&self, path: &Path) -> io::Result<(Kind, HostSig)> {
        let meta = crate::io::file::symlink_metadata(path)?;
        Ok((kind_of(path, &meta)?, HostSig::of(&meta)))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Ask {
    Stat(PathBuf),
    List(PathBuf),
    Hash(PathBuf),
}

impl Ask {
    fn path(&self) -> &Path {
        match self {
            Ask::Stat(p) | Ask::List(p) | Ask::Hash(p) => p,
        }
    }
}

#[derive(Debug)]
pub enum Answer {
    Stat(Kind),
    List(Vec<(Vec<u8>, Kind)>),
    Hash(Hash, u64),
}

fn plain_name(name: &[u8]) -> bool {
    !name.is_empty() && !name.contains(&b'/') && !name.contains(&0) && name != b"." && name != b".."
}

pub struct Walk {
    root: PathBuf,
    want: BTreeSet<Ask>,
    asked: BTreeSet<Ask>,
    top: Option<Kind>,
    listed: BTreeMap<PathBuf, Vec<(Vec<u8>, Kind)>>,
    hashed: BTreeMap<PathBuf, (Hash, u64)>,
    failed: Option<String>,
}

impl Walk {
    pub fn new(root: &Path) -> Walk {
        Walk {
            root: root.to_path_buf(),
            want: BTreeSet::from([Ask::Stat(root.to_path_buf())]),
            asked: BTreeSet::new(),
            top: None,
            listed: BTreeMap::new(),
            hashed: BTreeMap::new(),
            failed: None,
        }
    }

    pub fn wants(&self) -> Vec<Ask> {
        self.want.iter().cloned().collect()
    }

    pub fn saw(&mut self, ask: Ask, answer: io::Result<Answer>) {
        if !self.want.remove(&ask) {
            return;
        }
        self.asked.insert(ask.clone());
        let answer = match answer {
            Ok(a) => a,
            Err(e) => {
                self.failed = Some(format!("{}: {e}", ask.path().display()));
                self.want.clear();
                return;
            }
        };
        match (ask, answer) {
            (Ask::Stat(p), Answer::Stat(kind)) => {
                match &kind {
                    Kind::File { .. } => {
                        self.want.insert(Ask::Hash(p));
                    }
                    Kind::Dir => {
                        self.want.insert(Ask::List(p));
                    }
                    Kind::Link(_) => {}
                    Kind::Other => {
                        self.failed = Some(format!("{} is not a file, directory or symlink", p.display()));
                    }
                }
                self.top = Some(kind);
            }
            (Ask::List(p), Answer::List(entries)) => {
                let entries: Vec<(Vec<u8>, Kind)> = entries.into_iter().filter(|(n, _)| plain_name(n)).collect();
                for (name, kind) in &entries {
                    let child = p.join(std::ffi::OsStr::from_bytes(name));
                    match kind {
                        Kind::Dir => {
                            self.want.insert(Ask::List(child));
                        }
                        Kind::File { .. } => {
                            self.want.insert(Ask::Hash(child));
                        }
                        _ => {}
                    }
                }
                self.listed.insert(p, entries);
            }
            (Ask::Hash(p), Answer::Hash(h, size)) => {
                self.hashed.insert(p, (h, size));
            }
            (ask, answer) => {
                self.failed = Some(format!("{}: {answer:?} does not answer {ask:?}", ask.path().display()));
                self.want.clear();
            }
        }
    }

    fn tree(&self, dir: &Path) -> io::Result<Walked> {
        let mut hashed = Vec::new();
        let mut links = Vec::new();
        let mut children = Vec::new();
        for (name, kind) in self.listed.get(dir).ok_or_else(|| io::Error::other(format!("{} was not listed", dir.display())))? {
            let child = dir.join(std::ffi::OsStr::from_bytes(name));
            match kind {
                Kind::File { exec } => {
                    let (h, size) = self.hashed.get(&child).ok_or_else(|| io::Error::other(format!("{} was not hashed", child.display())))?;
                    hashed.push((name.clone(), Node::File { hash: h.0, size: *size, exec: *exec }, *size));
                }
                Kind::Link(target) => links.push((name.clone(), target.clone())),
                Kind::Dir => children.push((name.clone(), self.tree(&child)?)),
                Kind::Other => {}
            }
        }
        manifest(dir, hashed, links, children)
    }

    pub fn finish(self) -> io::Result<Fresh> {
        if let Some(e) = self.failed {
            return Err(io::Error::other(e));
        }
        if !self.want.is_empty() {
            return Err(io::Error::other(format!("{} is not walked yet", self.root.display())));
        }
        match self.top.as_ref() {
            Some(Kind::Link(target)) => Ok(Fresh { kind: StoredKind::Link(target.clone()), files: 0, bytes: 0 }),
            Some(Kind::File { exec }) => {
                let (hash, size) = *self.hashed.get(&self.root).ok_or_else(|| io::Error::other("the file was not hashed"))?;
                Ok(Fresh { kind: StoredKind::File { hash, size, exec: *exec }, files: 1, bytes: size })
            }
            Some(Kind::Dir) => {
                let walked = self.tree(&self.root)?;
                Ok(Fresh { kind: StoredKind::Dir { root: walked.root, trees: walked.trees }, files: walked.files, bytes: walked.bytes })
            }
            _ => Err(io::Error::other(format!("{} was not looked at", self.root.display()))),
        }
    }
}

#[cfg(test)]
impl StateMachine for Walk {
    fn invariants(&self) {
        let inside = |a: &Ask| a.path().starts_with(&self.root);
        let from_listing = |a: &Ask| {
            let p = a.path();
            if p == self.root {
                return true;
            }
            let (Some(dir), Some(name)) = (p.parent(), p.file_name()) else { return false };
            self.listed.get(dir).is_some_and(|entries| {
                entries.iter().any(|(n, k)| {
                    n.as_slice() == name.as_bytes()
                        && match a {
                            Ask::List(_) => *k == Kind::Dir,
                            Ask::Hash(_) => matches!(k, Kind::File { .. }),
                            Ask::Stat(_) => false,
                        }
                })
            })
        };
        for a in self.want.iter().chain(&self.asked) {
            assert!(inside(a), "{a:?} is inside the root");
            assert!(from_listing(a), "{a:?} is the root or named by a listing as what it asks");
        }
        assert!(self.want.is_disjoint(&self.asked), "nothing wanted has been asked already");
        assert!(self.listed.keys().all(|d| self.asked.contains(&Ask::List(d.clone()))), "every listing was asked for");
        assert!(self.hashed.keys().all(|f| self.asked.contains(&Ask::Hash(f.clone()))), "every hash was asked for");
        assert!(self.listed.values().flatten().all(|(n, _)| plain_name(n)), "every listed name is a plain name");
        assert!(self.failed.is_none() || self.want.is_empty(), "a failed walk wants nothing more");
        assert!(self.top.is_some() || self.asked.is_empty() || self.failed.is_some(), "a walk that asked has a top or a failure");
    }
}

pub struct Walked {
    root: Hash,
    trees: Vec<(Hash, Vec<u8>)>,
    files: u64,
    bytes: u64,
}

fn manifest(
    path: &Path,
    hashed: Vec<(Vec<u8>, Node, u64)>,
    links: Vec<(Vec<u8>, Vec<u8>)>,
    mut children: Vec<(Vec<u8>, Walked)>,
) -> io::Result<Walked> {
    children.sort_by(|a, b| a.0.cmp(&b.0));
    let mut entries: Vec<(Vec<u8>, Node)> = Vec::new();
    let mut files_n = 0u64;
    let mut bytes = 0u64;
    let mut trees = Vec::new();
    for (name, node, size) in hashed {
        files_n += 1;
        bytes += size;
        entries.push((name, node));
    }
    for (name, target) in links {
        entries.push((name, Node::Symlink(target)));
    }
    for (name, walked) in children {
        entries.push((name, Node::Dir(walked.root.0)));
        files_n += walked.files;
        bytes += walked.bytes;
        trees.extend(walked.trees);
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    if entries.windows(2).any(|w| w[0].0 == w[1].0) {
        return Err(io::Error::other(format!(
            "{} has two entries of the same name",
            path.display()
        )));
    }
    let tree = Tree { entries };
    let encoded = tree.encode();
    let root = Hash(tree::digest(&encoded));
    trees.push((root, encoded));
    Ok(Walked {
        root,
        trees,
        files: files_n,
        bytes,
    })
}

pub struct Fresh {
    kind: StoredKind,
    files: u64,
    bytes: u64,
}

fn answer(fs: &dyn StoreFs, ask: &Ask) -> io::Result<Answer> {
    match ask {
        Ask::Stat(p) => fs.stat(p).map(Answer::Stat),
        Ask::List(p) => fs.list(p).map(Answer::List),
        Ask::Hash(p) => fs.hash(p).map(|(h, size)| Answer::Hash(h, size)),
    }
}

fn hash_path_mode(fs: &dyn StoreFs, path: &Path, parallel: bool) -> io::Result<Fresh> {
    let mut walk = Walk::new(path);
    loop {
        let wants = walk.wants();
        if wants.is_empty() {
            break;
        }
        let answers: Vec<(Ask, io::Result<Answer>)> = if parallel {
            wants.into_par_iter().map(|a| { let r = answer(fs, &a); (a, r) }).collect()
        } else {
            wants.into_iter().map(|a| { let r = answer(fs, &a); (a, r) }).collect()
        };
        for (ask, got) in answers {
            walk.saw(ask, got);
        }
    }
    walk.finish()
}

pub fn ensure(db: &dyn Tables, fs: &dyn StoreFs, store: &Path, names: &[String]) -> Result<Prepared> {
    prepare(db, fs, store, names, None, &mut BTreeSet::new())
}

pub fn prepare(
    db: &dyn Tables,
    fs: &dyn StoreFs,
    store: &Path,
    names: &[String],
    hosts: Option<&dyn HostFiles>,
    seen: &mut BTreeSet<String>,
) -> Result<Prepared> {
    let mut cost = Cost::default();
    let mut roots = Vec::with_capacity(names.len());
    for name in names {
        let path = store.join(name);
        let (kind, was_cached) = match load(db, name.as_bytes()) {
            Some(k) => {
                cost.reused_paths += 1;
                (k, true)
            }
            None => {
                let t = crate::io::now();
                let fresh = hash_path_mode(fs, &path, true).with_context(|| format!("hashing {name}"))?;
                cost.millis += crate::io::now().saturating_duration_since(t).as_millis() as u64;
                save(db, name.as_bytes(), &fresh.kind)?;
                cost.new_paths += 1;
                cost.hashed_files += fresh.files;
                cost.hashed_bytes += fresh.bytes;
                (fresh.kind, false)
            }
        };
        let kind = adopt(
            db, fs, name, &path, kind, was_cached, hosts, seen, &mut cost, true,
        )?;
        roots.push(identity(&kind));
    }
    Ok(Prepared { roots, cost })
}

pub fn log_cost(cost: &Cost) {
    if cost.hashed_files == 0 && cost.new_paths == 0 {
        return;
    }
    eprintln!(
        "nix closure: hashed {} files, {} bytes, {} ms; reused {} paths",
        cost.hashed_files, cost.hashed_bytes, cost.millis, cost.reused_paths
    );
}

pub fn schedule(
    db: &Arc<dyn Tables>,
    fs: &Arc<dyn StoreFs>,
    store: &Path,
    names: &[String],
    hosts: Option<&Arc<DashMap<Hash, (Arc<Path>, HostSig)>>>,
    seen: &Arc<Attached>,
) -> Result<Prepared> {
    let (prepared, pending) = plan(db.as_ref(), fs.as_ref(), store, names, hosts.map(|m| m.as_ref() as &dyn HostFiles), seen)?;
    if !pending.is_empty() {
        let job = Arc::new(Job {
            db: Arc::clone(db),
            fs: Arc::clone(fs),
            store: store.to_path_buf(),
            hosts: hosts.cloned(),
            seen: Arc::clone(seen),
            cost: parking_lot::Mutex::new(Cost::default()),
            left: std::sync::atomic::AtomicUsize::new(pending.len()),
        });
        enqueue(pending.into_iter().map(|name| (Arc::clone(&job), name)));
    }
    Ok(prepared)
}

fn plan(
    db: &dyn Tables,
    fs: &dyn StoreFs,
    store: &Path,
    names: &[String],
    hosts: Option<&dyn HostFiles>,
    seen: &Attached,
) -> Result<(Prepared, Vec<String>)> {
    let mut cost = Cost::default();
    let mut roots = Vec::with_capacity(names.len());
    let mut pending = Vec::new();
    for name in names {
        let path = store.join(name);
        if let Some(kind) = load(db, name.as_bytes()) {
            cost.reused_paths += 1;
            let mut guard = seen.lock();
            let kind = adopt(db, fs, name, &path, kind, true, hosts, &mut guard, &mut cost, false)?;
            roots.push(identity(&kind));
        } else {
            roots.push(Hash::ZERO);
            pending.push(name.clone());
        }
    }
    Ok((Prepared { roots, cost }, pending))
}

fn adopt(
    db: &dyn Tables,
    fs: &dyn StoreFs,
    name: &str,
    path: &Path,
    mut kind: StoredKind,
    was_cached: bool,
    hosts: Option<&dyn HostFiles>,
    seen: &mut BTreeSet<String>,
    cost: &mut Cost,
    parallel: bool,
) -> Result<StoredKind> {
    if let Some(hosts) = hosts.filter(|_| !seen.contains(name)) {
        if let Err(e) = attach_kind(fs, &kind, path, hosts) {
            eprintln!(
                "rargod: nix path {name} manifest does not match the files ({e}); hashing it again"
            );
            db.delete(Table::Meta, &meta_key(name.as_bytes()));
            let t = crate::io::now();
            let fresh =
                hash_path_mode(fs, path, parallel).with_context(|| format!("hashing {name} again"))?;
            cost.millis += crate::io::now().saturating_duration_since(t).as_millis() as u64;
            save(db, name.as_bytes(), &fresh.kind)?;
            if was_cached {
                cost.reused_paths = cost.reused_paths.saturating_sub(1);
                cost.new_paths += 1;
            }
            cost.hashed_files += fresh.files;
            cost.hashed_bytes += fresh.bytes;
            attach_kind(fs, &fresh.kind, path, hosts).map_err(anyhow::Error::msg).with_context(|| format!("remembering {name}"))?;
            kind = fresh.kind;
        }
        seen.insert(name.to_string());
    }
    Ok(kind)
}

pub struct Queue<T> {
    waiting: std::collections::VecDeque<(u64, T)>,
    builds: usize,
    one_core: bool,
    pushed: u64,
    handed: u64,
}

impl<T> Queue<T> {
    pub fn new(one_core: bool) -> Queue<T> {
        Queue { waiting: std::collections::VecDeque::new(), builds: 0, one_core, pushed: 0, handed: 0 }
    }

    pub fn push(&mut self, items: impl IntoIterator<Item = T>) {
        for item in items {
            self.waiting.push_back((self.pushed, item));
            self.pushed += 1;
        }
    }

    pub fn hold(&mut self) {
        self.builds += 1;
    }

    pub fn release(&mut self) -> bool {
        self.builds = self.builds.saturating_sub(1);
        self.builds == 0
    }

    pub fn take(&mut self) -> Option<T> {
        if self.one_core && self.builds > 0 {
            return None;
        }
        let (_, item) = self.waiting.pop_front()?;
        self.handed += 1;
        Some(item)
    }
}

#[cfg(test)]
impl<T> StateMachine for Queue<T> {
    fn invariants(&self) {
        assert_eq!(self.handed + self.waiting.len() as u64, self.pushed, "everything pushed is handed out or waiting");
        assert!(self.waiting.iter().zip(self.handed..).all(|((n, _), want)| *n == want), "the waiting entries are the next ones in order");
    }
}

struct Job {
    db: Arc<dyn Tables>,
    fs: Arc<dyn StoreFs>,
    store: PathBuf,
    hosts: Option<Arc<DashMap<Hash, (Arc<Path>, HostSig)>>>,
    seen: Arc<Attached>,
    cost: parking_lot::Mutex<Cost>,
    left: std::sync::atomic::AtomicUsize,
}

type Hashing = (parking_lot::Mutex<Queue<(Arc<Job>, String)>>, parking_lot::Condvar);

fn hashing() -> &'static Hashing {
    static Q: OnceLock<Hashing> = OnceLock::new();
    static WORKER: OnceLock<()> = OnceLock::new();
    let q = Q.get_or_init(|| (parking_lot::Mutex::new(Queue::new(one_core())), parking_lot::Condvar::new()));
    WORKER.get_or_init(|| {
        crate::io::thread::spawn_niced("nix-hash", IDLE, || {
            let (queue, cv) = hashing();
            loop {
                let next = {
                    let mut q = queue.lock();
                    loop {
                        match q.take() {
                            Some(item) => break item,
                            None => cv.wait(&mut q),
                        }
                    }
                };
                let (job, name) = next;
                {
                    let mut cost = job.cost.lock();
                    let hosts = job.hosts.as_deref().map(|m| m as &dyn HostFiles);
                    hash_one(job.db.as_ref(), job.fs.as_ref(), &job.store, hosts, &job.seen, &name, &mut cost);
                }
                if job.left.fetch_sub(1, std::sync::atomic::Ordering::AcqRel) == 1 {
                    log_cost(&job.cost.lock());
                }
            }
        })
        .expect("starting nix hash thread");
    });
    q
}

fn enqueue(items: impl IntoIterator<Item = (Arc<Job>, String)>) {
    let (queue, cv) = hashing();
    queue.lock().push(items);
    cv.notify_all();
}

/// The scheduling priority of the hashing thread: the lowest there is.
const IDLE: i32 = 19;

pub struct BuildHold {
    _private: (),
}

pub fn hold_build() -> BuildHold {
    hashing().0.lock().hold();
    BuildHold { _private: () }
}

impl Drop for BuildHold {
    fn drop(&mut self) {
        let (queue, cv) = hashing();
        if queue.lock().release() {
            cv.notify_all();
        }
    }
}

fn one_core() -> bool {
    let Ok(text) = crate::io::file::read_to_string("/proc/self/cgroup") else {
        return false;
    };
    let Some(rest) = text.lines().find_map(|line| line.strip_prefix("0::")) else {
        return false;
    };
    let rel = format!("/sys/fs/cgroup{rest}/cpu.max");
    let max = match crate::io::file::read_to_string(&rel) {
        Ok(max) => max,
        Err(_) => match crate::io::file::read_to_string(format!("/proc/1/root{rel}")) {
            Ok(max) => max,
            Err(_) => return false,
        },
    };
    quota_is_one_core(&max)
}

fn quota_is_one_core(max: &str) -> bool {
    let mut parts = max.split_whitespace();
    let (Some(quota), Some(period)) = (parts.next(), parts.next()) else {
        return false;
    };
    if quota == "max" {
        return false;
    }
    matches!(
        (quota.parse::<u64>(), period.parse::<u64>()),
        (Ok(q), Ok(p)) if p > 0 && q <= p
    )
}

fn hash_one(db: &dyn Tables, fs: &dyn StoreFs, store: &Path, hosts: Option<&dyn HostFiles>, seen: &Attached, name: &str, cost: &mut Cost) {
    let path = store.join(name);
    if let Some(kind) = load(db, name.as_bytes()) {
        let mut seen = seen.lock();
        if let Err(e) = adopt(db, fs, name, &path, kind, true, hosts, &mut seen, cost, false) {
            eprintln!("rargod: nix path {name}: {e:#}");
        }
        return;
    }
    let t = crate::io::now();
    let fresh = match hash_path_mode(fs, &path, false) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("rargod: nix path {name}: {e}");
            return;
        }
    };
    cost.millis += crate::io::now().saturating_duration_since(t).as_millis() as u64;
    if let Some(hosts) = hosts {
        let mut seen = seen.lock();
        if !seen.contains(name) {
            if let Err(e) = attach_kind(fs, &fresh.kind, &path, hosts) {
                eprintln!("rargod: nix path {name} manifest does not match the files ({e})");
                return;
            }
            seen.insert(name.to_string());
        }
    }
    if let Err(e) = save(db, name.as_bytes(), &fresh.kind) {
        eprintln!("rargod: nix path {name}: {e:#}");
        return;
    }
    cost.new_paths += 1;
    cost.hashed_files += fresh.files;
    cost.hashed_bytes += fresh.bytes;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expect {
    Link,
    File { hash: Hash, size: u64 },
}

pub fn expectations(kind: &StoredKind, path: &Path) -> Result<Vec<(PathBuf, Expect)>, String> {
    match kind {
        StoredKind::Dir { root, trees } => {
            let installed = PathTrees::decode(*root, trees).map_err(|e| e.to_string())?;
            let mut out = Vec::new();
            expect_tree(&installed, *root, path, &mut out)?;
            Ok(out)
        }
        StoredKind::File { hash, size, .. } => Ok(vec![(path.to_path_buf(), Expect::File { hash: *hash, size: *size })]),
        StoredKind::Link(_) => Ok(vec![(path.to_path_buf(), Expect::Link)]),
    }
}

fn expect_tree(trees: &PathTrees, digest: Hash, dir: &Path, out: &mut Vec<(PathBuf, Expect)>) -> Result<(), String> {
    let tree = trees.trees.get(&digest).ok_or("missing directory tree")?;
    for (name, node) in &tree.entries {
        let path = dir.join(std::ffi::OsStr::from_bytes(name));
        match node {
            Node::Dir(d) => expect_tree(trees, Hash(*d), &path, out)?,
            Node::File { hash, size, .. } => out.push((path, Expect::File { hash: Hash(*hash), size: *size })),
            Node::Symlink(_) => out.push((path, Expect::Link)),
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq)]
pub enum Verdict {
    Matches(Vec<(Hash, PathBuf, HostSig)>),
    Stale(String),
}

pub fn judge(expected: Vec<(PathBuf, Expect)>, seen: Vec<io::Result<(Kind, HostSig)>>) -> Verdict {
    if expected.len() != seen.len() {
        return Verdict::Stale(format!("{} answers for {} paths", seen.len(), expected.len()));
    }
    let mut files = Vec::new();
    for ((path, want), got) in expected.into_iter().zip(seen) {
        let (kind, sig) = match got {
            Ok(k) => k,
            Err(e) => return Verdict::Stale(e.to_string()),
        };
        match want {
            Expect::Link if matches!(kind, Kind::Link(_)) => {}
            Expect::Link => return Verdict::Stale(format!("{} is not a symlink", path.display())),
            Expect::File { hash, size } if matches!(kind, Kind::File { .. }) && sig.size == size => files.push((hash, path, sig)),
            Expect::File { .. } => return Verdict::Stale(format!("{} is not the manifested file", path.display())),
        }
    }
    Verdict::Matches(files)
}

fn attach_kind(fs: &dyn StoreFs, kind: &StoredKind, path: &Path, hosts: &dyn HostFiles) -> Result<(), String> {
    let expected = expectations(kind, path)?;
    let seen = expected.iter().map(|(p, _)| fs.host(p)).collect();
    match judge(expected, seen) {
        Verdict::Matches(files) => {
            for (hash, path, sig) in files {
                hosts.record(hash, path, sig);
            }
            Ok(())
        }
        Verdict::Stale(why) => Err(why),
    }
}

pub enum Place {
    Root,
    Inside { name: Vec<u8>, sub: Vec<u8> },
}

pub fn place(dir: &[u8]) -> Option<Place> {
    if dir == b"/nix/store" {
        return Some(Place::Root);
    }
    let rest = dir.strip_prefix(b"/nix/store/")?;
    if rest.is_empty() {
        return Some(Place::Root);
    }
    match rest.iter().position(|&c| c == b'/') {
        None => Some(Place::Inside {
            name: rest.to_vec(),
            sub: b"/".to_vec(),
        }),
        Some(i) => Some(Place::Inside {
            name: rest[..i].to_vec(),
            sub: rest[i..].to_vec(),
        }),
    }
}

pub struct PathTrees {
    root: Hash,
    trees: BTreeMap<Hash, Tree>,
}

impl PathTrees {
    fn decode(root: Hash, encoded: &[(Hash, Vec<u8>)]) -> io::Result<PathTrees> {
        let mut trees = BTreeMap::new();
        for (h, bytes) in encoded {
            if Hash(tree::digest(bytes)) != *h {
                return Err(io::Error::other("tree digest mismatch"));
            }
            trees.insert(*h, Tree::decode(bytes)?);
        }
        if !trees.contains_key(&root) {
            return Err(io::Error::other("root tree is missing"));
        }
        Ok(PathTrees { root, trees })
    }

    fn dir<'a>(&'a self, sub: &[u8]) -> Option<&'a Tree> {
        let mut cur = self.root;
        let rel = sub.strip_prefix(b"/").unwrap_or(sub);
        if !rel.is_empty() {
            for comp in rel.split(|&c| c == b'/') {
                if comp.is_empty() {
                    continue;
                }
                let tree = self.trees.get(&cur)?;
                match tree.get(comp)? {
                    Node::Dir(d) => cur = Hash(*d),
                    _ => return None,
                }
            }
        }
        self.trees.get(&cur)
    }

    fn lookup(&self, sub: &[u8], name: &[u8]) -> Looked {
        let Some(tree) = self.dir(sub) else {
            return Looked::Absent;
        };
        match tree.get(name) {
            None => Looked::Absent,
            Some(Node::Dir(_)) => Looked::Dir(NIX_DIR),
            Some(Node::Symlink(t)) => Looked::Symlink(t.clone()),
            Some(Node::File { hash, size, exec }) => Looked::File(FileInfo {
                size: *size,
                owner: Owner {
                    uid: 0,
                    gid: 0,
                    mode: if *exec { 0o555 } else { 0o444 },
                },
                src: FileSrc::Blob(Hash(*hash)),
                sem: None,
            }),
        }
    }

    fn list(&self, sub: &[u8]) -> io::Result<Listing> {
        let tree = self
            .dir(sub)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
        Ok(tree
            .entries
            .iter()
            .map(|(n, node)| {
                let t = match node {
                    Node::Dir(_) => DType::Dir,
                    Node::File { .. } => DType::File,
                    Node::Symlink(_) => DType::Symlink,
                };
                (n.clone(), t)
            })
            .collect())
    }
}

pub enum Installed {
    Dir(PathTrees),
    File { hash: Hash, size: u64, exec: bool },
    Link(Vec<u8>),
}

impl Installed {
    pub fn from_kind(kind: &StoredKind) -> io::Result<Installed> {
        match kind {
            StoredKind::Dir { root, trees } => Ok(Installed::Dir(PathTrees::decode(*root, trees)?)),
            StoredKind::File { hash, size, exec } => Ok(Installed::File {
                hash: *hash,
                size: *size,
                exec: *exec,
            }),
            StoredKind::Link(t) => Ok(Installed::Link(t.clone())),
        }
    }

    fn at_root(&self) -> Looked {
        match self {
            Installed::Dir(_) => Looked::Dir(NIX_DIR),
            Installed::File { hash, size, exec } => Looked::File(FileInfo {
                size: *size,
                owner: Owner {
                    uid: 0,
                    gid: 0,
                    mode: if *exec { 0o555 } else { 0o444 },
                },
                src: FileSrc::Blob(*hash),
                sem: None,
            }),
            Installed::Link(t) => Looked::Symlink(t.clone()),
        }
    }

    fn dtype(&self) -> DType {
        match self {
            Installed::Dir(_) => DType::Dir,
            Installed::File { .. } => DType::File,
            Installed::Link(_) => DType::Symlink,
        }
    }
}

fn in_closure(closure: &[Vec<u8>], name: &[u8]) -> bool {
    closure.binary_search_by(|n| n.as_slice().cmp(name)).is_ok()
}

pub fn lookup_at(
    get: impl Fn(&[u8]) -> Option<Arc<Installed>>,
    closure: &[Vec<u8>],
    dir: &[u8],
    name: &[u8],
) -> Option<Looked> {
    let place = place(dir)?;
    if closure.is_empty() {
        return None;
    }
    match place {
        Place::Root => {
            if !in_closure(closure, name) {
                return Some(Looked::Absent);
            }
            match get(name) {
                Some(g) => Some(g.at_root()),
                None => None,
            }
        }
        Place::Inside {
            name: store_name,
            sub,
        } => {
            if !in_closure(closure, &store_name) {
                return Some(Looked::Absent);
            }
            match get(&store_name).as_deref() {
                Some(Installed::Dir(t)) => Some(t.lookup(&sub, name)),
                Some(_) => Some(Looked::Absent),
                None => None,
            }
        }
    }
}

pub fn list_at(
    get: impl Fn(&[u8]) -> Option<Arc<Installed>>,
    closure: &[Vec<u8>],
    dir: &[u8],
) -> Option<io::Result<Listing>> {
    let place = place(dir)?;
    if closure.is_empty() {
        return None;
    }
    match place {
        Place::Root => Some(Ok(closure
            .iter()
            .map(|n| {
                let t = get(n).as_deref().map(|g| g.dtype()).unwrap_or(DType::Dir);
                (n.clone(), t)
            })
            .collect())),
        Place::Inside { name, sub } => {
            if !in_closure(closure, &name) {
                return Some(Err(io::Error::from_raw_os_error(libc::ENOENT)));
            }
            match get(&name).as_deref() {
                Some(Installed::Dir(t)) => Some(t.list(&sub)),
                Some(_) => Some(Err(io::Error::from_raw_os_error(libc::ENOENT))),
                None => None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Transcript;
    use expect_test::expect;
    use crate::db::MemTables;
    use std::cell::RefCell;

    #[derive(Clone, Debug)]
    enum Ent {
        File(Vec<u8>, bool),
        Dir,
        Link(Vec<u8>),
        Other,
    }

    #[derive(Default)]
    struct FakeStore {
        ents: BTreeMap<PathBuf, Ent>,
        odd: BTreeMap<PathBuf, Vec<Vec<u8>>>,
        broken: BTreeSet<PathBuf>,
    }

    impl FakeStore {
        fn put(&mut self, path: &str, e: Ent) {
            let path = PathBuf::from(path);
            for a in path.ancestors().skip(1) {
                if a.as_os_str().is_empty() || a == Path::new("/") {
                    break;
                }
                self.ents.entry(a.to_path_buf()).or_insert(Ent::Dir);
            }
            self.ents.insert(path, e);
        }

        /// The store with only the entries under `dir`: asking it about anything else fails.
        fn only(&self, dir: &str) -> FakeStore {
            let ents = self.ents.iter().filter(|(p, _)| p.starts_with(dir)).map(|(p, e)| (p.clone(), e.clone())).collect();
            FakeStore { ents, ..FakeStore::default() }
        }

        fn kind(e: &Ent) -> Kind {
            match e {
                Ent::File(_, exec) => Kind::File { exec: *exec },
                Ent::Dir => Kind::Dir,
                Ent::Link(t) => Kind::Link(t.clone()),
                Ent::Other => Kind::Other,
            }
        }
    }

    fn missing(path: &Path) -> io::Error {
        io::Error::new(io::ErrorKind::NotFound, format!("{} is missing", path.display()))
    }

    impl StoreFs for FakeStore {
        fn stat(&self, path: &Path) -> io::Result<Kind> {
            self.ents.get(path).map(FakeStore::kind).ok_or_else(|| missing(path))
        }

        fn list(&self, dir: &Path) -> io::Result<Vec<(Vec<u8>, Kind)>> {
            if self.broken.contains(dir) {
                return Err(io::Error::other("permission denied"));
            }
            if !matches!(self.ents.get(dir), Some(Ent::Dir)) {
                return Err(missing(dir));
            }
            let mut out: Vec<(Vec<u8>, Kind)> = self
                .ents
                .iter()
                .filter(|(p, _)| p.parent() == Some(dir))
                .map(|(p, e)| (p.file_name().unwrap().as_bytes().to_vec(), FakeStore::kind(e)))
                .collect();
            for name in self.odd.get(dir).into_iter().flatten() {
                out.push((name.clone(), Kind::File { exec: false }));
            }
            out.reverse();
            Ok(out)
        }

        fn hash(&self, path: &Path) -> io::Result<(Hash, u64)> {
            match self.ents.get(path) {
                Some(Ent::File(data, _)) => Ok((Hash::of(data), data.len() as u64)),
                _ => Err(missing(path)),
            }
        }

        fn host(&self, path: &Path) -> io::Result<(Kind, HostSig)> {
            let e = self.ents.get(path).ok_or_else(|| missing(path))?;
            let size = if let Ent::File(d, _) = e { d.len() as u64 } else { 0 };
            let ino = self.ents.keys().position(|p| p == path).unwrap() as u64;
            Ok((FakeStore::kind(e), HostSig { dev: 1, ino, size, mtime_ns: 0, ctime_ns: 0 }))
        }
    }

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
    }

    fn model(fs: &FakeStore, dir: &Path) -> (Hash, u64, u64) {
        let mut entries = Vec::new();
        let (mut files, mut bytes) = (0, 0);
        for (p, e) in fs.ents.iter().filter(|(p, _)| p.parent() == Some(dir)) {
            let name = p.file_name().unwrap().as_bytes().to_vec();
            match e {
                Ent::File(data, exec) => {
                    files += 1;
                    bytes += data.len() as u64;
                    entries.push((name, Node::File { hash: Hash::of(data).0, size: data.len() as u64, exec: *exec }));
                }
                Ent::Link(t) => entries.push((name, Node::Symlink(t.clone()))),
                Ent::Dir => {
                    let (h, f, b) = model(fs, p);
                    files += f;
                    bytes += b;
                    entries.push((name, Node::Dir(h.0)));
                }
                Ent::Other => {}
            }
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        (Hash(tree::digest(&Tree { entries }.encode())), files, bytes)
    }

    fn walk_shuffled(fs: &FakeStore, root: &Path, rng: &mut Rng) -> io::Result<Fresh> {
        let mut walk = Walk::new(root);
        walk.invariants();
        let mut rounds = 0;
        loop {
            let mut wants = walk.wants();
            if wants.is_empty() {
                break;
            }
            rounds += 1;
            assert!(rounds < 10_000);
            let take = 1 + rng.below(wants.len() as u64) as usize;
            for _ in 0..take {
                let ask = wants.remove(rng.below(wants.len() as u64) as usize);
                let got = answer(fs, &ask);
                walk.saw(ask.clone(), got);
                rargo_machine::check(&walk, format_args!("after {ask:?}"));
                walk.saw(ask, Ok(Answer::Stat(Kind::Dir)));
                rargo_machine::check(&walk, "an answer to nothing asked is ignored");
            }
        }
        walk.finish()
    }

    #[test]
    fn generated_store_trees_walk_to_the_manifest_of_their_listing_in_any_order() {
        let names: [&[u8]; 8] = [b"bin", b"lib", b"share", b"a.so", b"x", b"y y", b"\xff", b"-dash"];
        for seed in 1..=16u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let mut fs = FakeStore::default();
            let root = "/nix/store/0000000000000000000000000000000a-pkg";
            fs.put(root, Ent::Dir);
            let mut dirs = vec![PathBuf::from(root)];
            for _ in 0..rng.below(40) {
                let parent = dirs[rng.below(dirs.len() as u64) as usize].clone();
                let name = std::ffi::OsStr::from_bytes(names[rng.below(names.len() as u64) as usize]);
                let path = parent.join(name);
                if fs.ents.contains_key(&path) {
                    continue;
                }
                let e = match rng.below(10) {
                    0..=4 => Ent::File(vec![rng.below(256) as u8; rng.below(5) as usize], rng.below(2) == 0),
                    5..=7 => {
                        dirs.push(path.clone());
                        Ent::Dir
                    }
                    8 => Ent::Link(b"../lib".to_vec()),
                    _ => Ent::Other,
                };
                fs.ents.insert(path, e);
            }
            if rng.below(3) == 0 {
                fs.odd.insert(PathBuf::from(root), vec![b".".to_vec(), b"..".to_vec(), Vec::new(), b"a/b".to_vec(), b"nul\0".to_vec()]);
            }
            let (want, files, bytes) = model(&fs, Path::new(root));
            for order in 0..3 {
                let fresh = walk_shuffled(&fs, Path::new(root), &mut rng).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
                assert_eq!(identity(&fresh.kind), want, "seed {seed} order {order}: the walk gives the listing's tree");
                assert_eq!((fresh.files, fresh.bytes), (files, bytes), "seed {seed}");
                let StoredKind::Dir { root: r, trees } = &fresh.kind else { panic!("a directory") };
                assert!(PathTrees::decode(*r, trees).is_ok(), "seed {seed}: every tree is carried");
            }
            assert_eq!(identity(&hash_path_mode(&fs, Path::new(root), false).unwrap().kind), want, "seed {seed}: the shell's serial walk agrees");
        }
    }

    #[test]
    fn single_files_links_and_failures_are_walked_as_their_kind_says() {
        let mut fs = FakeStore::default();
        fs.put("/s/f", Ent::File(b"file-path".to_vec(), true));
        fs.put("/s/l", Ent::Link(b"f".to_vec()));
        fs.put("/s/p", Ent::Other);
        fs.put("/s/d/sub/x", Ent::File(b"x".to_vec(), false));
        fs.broken.insert(PathBuf::from("/s/d/sub"));
        let mut rng = Rng(7);
        let mut outcomes = Transcript::default();
        for path in ["/s/f", "/s/l", "/s/p", "/s/d", "/s/none"] {
            match walk_shuffled(&fs, Path::new(path), &mut rng) {
                Ok(fresh) => outcomes.record(path, (fresh.kind, fresh.files, fresh.bytes)),
                Err(err) => outcomes.record(path, err.to_string()),
            }
        }
        let mut walk = Walk::new(Path::new("/s/f"));
        walk.saw(Ask::Stat("/s/f".into()), Ok(Answer::Hash(Hash::ZERO, 0)));
        walk.invariants();
        outcomes.record("wrong answer shape", walk.finish().err().expect("the walk failed").to_string());
        outcomes.record("unfinished walk", Walk::new(Path::new("/s/f")).finish().err().expect("the walk failed").to_string());
        expect![[r#"
            /s/f: (File { hash: a42be855d08d82d7, size: 9, exec: true }, 1, 9)
            /s/l: (Link([102]), 0, 0)
            /s/p: "/s/p is not a file, directory or symlink"
            /s/d: "/s/d/sub: permission denied"
            /s/none: "/s/none: /s/none is missing"
            wrong answer shape: "/s/f: Hash(0000000000000000, 0) does not answer Stat(\"/s/f\")"
            unfinished walk: "/s/f is not walked yet"
        "#]].assert_eq(&outcomes.0);
    }

    #[test]
    fn generated_store_listings_have_a_stable_manifest() {
        for count in 0..32 {
            let build = |reverse| {
                let directories: Vec<_> = (0..count).map(|i| {
                    let child = manifest(Path::new("child"),
                        vec![(b"bin".to_vec(), Node::File { hash: [i as u8; 32], size: 7, exec: true }, 7)], vec![], vec![]).unwrap();
                    (format!("dir-{i}").into_bytes(), child)
                }).collect();
                let mut directories = directories;
                if reverse { directories.reverse(); }
                manifest(Path::new("store-path"), vec![], vec![(b"link".to_vec(), b"dir-0/bin".to_vec())], directories).unwrap()
            };
            let a = build(false);
            let b = build(true);
            assert_eq!(a.root, b.root);
            assert_eq!(a.trees, b.trees);
            assert_eq!((a.files, a.bytes), (count, count * 7));
            assert_eq!(a.trees.last().unwrap().0, a.root);
        }
    }

    fn tree() -> FakeStore {
        let mut fs = FakeStore::default();
        fs.put("/store/aaa/bin/tool", Ent::File(b"tool-bytes".to_vec(), true));
        fs.put("/store/aaa/lib/note", Ent::File(b"note".to_vec(), false));
        fs.put("/store/aaa/lib/to-tool", Ent::Link(b"../bin/tool".to_vec()));
        fs.put("/store/bbb/readme", Ent::File(b"bee".to_vec(), false));
        fs.put("/store/ccc/only", Ent::File(b"cee".to_vec(), false));
        fs.put("/store/fff", Ent::File(b"file-path".to_vec(), false));
        fs.put("/store/lll", Ent::Link(b"fff".to_vec()));
        fs
    }

    /// The files a run recorded, held by the test alone.
    #[derive(Default)]
    struct Recorded(RefCell<BTreeMap<Hash, PathBuf>>);

    impl HostFiles for Recorded {
        fn record(&self, hash: Hash, path: PathBuf, _: HostSig) {
            self.0.borrow_mut().entry(hash).or_insert(path);
        }
    }

    /// What the hashing thread works on: the saved manifests, the store, and the paths already recorded.
    struct Parts {
        db: MemTables,
        fs: FakeStore,
        store: PathBuf,
        seen: Attached,
    }

    fn parts(fs: FakeStore) -> Parts {
        Parts { db: MemTables::default(), fs, store: PathBuf::from("/store"), seen: Attached::default() }
    }

    fn plan_of(parts: &Parts, fs: &FakeStore, hosts: Option<&dyn HostFiles>, names: &[&str]) -> (Prepared, Vec<String>) {
        let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        plan(&parts.db, fs, &parts.store, &names, hosts, &parts.seen).unwrap()
    }

    fn hash(parts: &Parts, hosts: Option<&dyn HostFiles>, name: &str, cost: &mut Cost) {
        hash_one(&parts.db, &parts.fs, &parts.store, hosts, &parts.seen, name, cost);
    }

    #[test]
    fn a_shared_store_path_is_manifested_once() {
        let fs = tree();
        let db = MemTables::default();
        let store = Path::new("/store");
        let first = ensure(&db, &fs, store, &["aaa".into(), "bbb".into()]).unwrap();
        assert_eq!(first.cost.new_paths, 2);
        assert_eq!(first.cost.reused_paths, 0);
        assert_eq!(first.cost.hashed_files, 3);
        let second = ensure(&db, &fs.only("/store/ccc"), store, &["bbb".into(), "ccc".into()]).unwrap();
        assert_eq!(second.cost.reused_paths, 1, "bbb was hashed with the first closure");
        assert_eq!(second.cost.new_paths, 1);
        assert_eq!(second.cost.hashed_files, 1, "only ccc's file is new");
        assert_eq!(second.roots[0], first.roots[1], "bbb keeps its tree");
        let third = ensure(&db, &FakeStore::default(), store, &["aaa".into(), "bbb".into()]).unwrap();
        assert_eq!(third.cost.hashed_files, 0);
        assert_eq!(third.cost.reused_paths, 2);
        assert_eq!(third.roots, first.roots);
        assert_eq!(first.roots[0], model(&fs, Path::new("/store/aaa")).0);
    }

    #[test]
    fn scheduling_answers_from_saved_manifests_and_leaves_the_rest_to_the_hash_queue() {
        let parts = parts(tree());
        let nothing_to_read = FakeStore::default();
        let (first, pending) = plan_of(&parts, &nothing_to_read, None, &["aaa", "bbb"]);
        assert_eq!(first.roots, vec![Hash::ZERO, Hash::ZERO]);
        assert_eq!(pending, ["aaa", "bbb"], "planning hashes nothing: it does not read the store");
        let mut cost = Cost::default();
        for name in &pending {
            hash(&parts, None, name, &mut cost);
        }
        assert_eq!((cost.new_paths, cost.hashed_files), (2, 3));
        let (second, pending) = plan_of(&parts, &nothing_to_read, None, &["bbb", "ccc"]);
        assert_eq!(second.roots[0], identity(&load(&parts.db, b"bbb").unwrap()));
        assert_eq!(second.roots[0], model(&parts.fs, Path::new("/store/bbb")).0);
        assert_eq!((second.cost.reused_paths, second.roots[1]), (1, Hash::ZERO));
        assert_eq!(pending, ["ccc"]);
        hash(&parts, None, "ccc", &mut cost);
        hash(&parts, None, "ccc", &mut cost);
        assert_eq!(cost.new_paths, 3, "a path saved meanwhile is not hashed again");
        let (third, pending) = plan_of(&parts, &nothing_to_read, None, &["aaa", "bbb"]);
        assert!(pending.is_empty() && third.cost.reused_paths == 2 && third.roots.iter().all(|r| *r != Hash::ZERO));
        let mut nothing = Cost::default();
        hash(&parts, None, "gone", &mut nothing);
        assert!(load(&parts.db, b"gone").is_none() && nothing.new_paths == 0, "a path that cannot be hashed is skipped");
    }

    #[test]
    fn hashing_records_files_in_the_caller_map() {
        let hosts = Recorded::default();
        let parts = parts(tree());
        let (_, pending) = plan_of(&parts, &parts.fs, Some(&hosts), &["aaa"]);
        hash(&parts, Some(&hosts), &pending[0], &mut Cost::default());
        let recorded = hosts.0.borrow();
        assert_eq!(recorded.len(), 2);
        assert_eq!(recorded.get(&Hash::of(b"tool-bytes")).map(PathBuf::as_path), Some(Path::new("/store/aaa/bin/tool")));
        assert!(recorded.contains_key(&Hash::of(b"note")));
        assert!(parts.seen.lock().contains("aaa"));
    }

    #[test]
    fn a_busy_one_core_server_hashes_after_the_build() {
        for seed in 1..=300u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let one_core = rng.below(2) == 0;
            let mut q = Queue::new(one_core);
            let (mut builds, mut next_in, mut next_out) = (0usize, 0u32, 0u32);
            for _ in 0..60 {
                match rng.below(4) {
                    0 => {
                        let n = rng.below(3) as u32;
                        q.push(next_in..next_in + n);
                        next_in += n;
                    }
                    1 => {
                        q.hold();
                        builds += 1;
                    }
                    2 if builds > 0 => {
                        builds -= 1;
                        assert_eq!(q.release(), builds == 0, "seed {seed}: the last build to finish wakes the queue");
                    }
                    _ => match q.take() {
                        Some(item) => {
                            assert!(!(one_core && builds > 0), "seed {seed}: nothing is hashed while a build holds the only core");
                            assert_eq!(item, next_out, "seed {seed}: paths are hashed in the order they were scheduled");
                            next_out += 1;
                        }
                        None => assert!((one_core && builds > 0) || next_out == next_in, "seed {seed}: the queue is idle only when empty or held"),
                    },
                }
                rargo_machine::check(&q, format_args!("seed {seed}"));
            }
        }
        assert!(quota_is_one_core("100000 100000\n") && quota_is_one_core("50000 100000"));
        assert!(!quota_is_one_core("max 100000") && !quota_is_one_core("200000 100000") && !quota_is_one_core("1 0") && !quota_is_one_core(""));
    }

    #[test]
    fn a_saved_manifest_matches_the_store_only_while_every_link_and_file_size_does() {
        for seed in 1..=300u64 {
            let mut rng = Rng(seed.wrapping_mul(0x2545_f491_4f6c_dd1d) | 1);
            let mut fs = FakeStore::default();
            let root = "/store/0000000000000000000000000000000b-pkg";
            fs.put(root, Ent::Dir);
            let mut dirs = vec![PathBuf::from(root)];
            for i in 0..1 + rng.below(25) {
                let parent = dirs[rng.below(dirs.len() as u64) as usize].clone();
                let path = parent.join(format!("e{i}"));
                let e = match rng.below(6) {
                    0..=2 => Ent::File(vec![i as u8; rng.below(4) as usize], rng.below(2) == 0),
                    3 | 4 => {
                        dirs.push(path.clone());
                        Ent::Dir
                    }
                    _ => Ent::Link(b"x".to_vec()),
                };
                fs.ents.insert(path, e);
            }
            let kind = hash_path_mode(&fs, Path::new(root), false).unwrap().kind;
            let expected = expectations(&kind, Path::new(root)).unwrap();
            let files: Vec<_> = fs.ents.iter().filter(|(_, e)| matches!(e, Ent::File(..))).map(|(p, _)| p.clone()).collect();
            let links = fs.ents.values().filter(|e| matches!(e, Ent::Link(_))).count();
            assert_eq!(expected.len(), files.len() + links, "seed {seed}: every file and link is checked");
            let answers = |fs: &FakeStore| expected.iter().map(|(p, _)| fs.host(p)).collect();
            match judge(expected.clone(), answers(&fs)) {
                Verdict::Matches(found) => {
                    assert_eq!(found.len(), files.len(), "seed {seed}");
                    for (hash, path, sig) in &found {
                        let Some(Ent::File(data, _)) = fs.ents.get(path) else { panic!("seed {seed}: {path:?} is a file") };
                        assert_eq!((*hash, sig.size), (Hash::of(data), data.len() as u64), "seed {seed}");
                    }
                }
                other => panic!("seed {seed}: {other:?}"),
            }
            if expected.is_empty() {
                continue;
            }
            let mut changed = fs;
            let victim = expected[rng.below(expected.len() as u64) as usize].0.clone();
            let why = match (changed.ents.get(&victim).cloned().unwrap(), rng.below(3)) {
                (Ent::File(mut data, exec), 0) => {
                    data.iter_mut().for_each(|b| *b ^= 0xff);
                    changed.ents.insert(victim.clone(), Ent::File(data, exec));
                    None
                }
                (Ent::File(mut data, exec), 1) => {
                    data.push(7);
                    changed.ents.insert(victim.clone(), Ent::File(data, exec));
                    Some(format!("{} is not the manifested file", victim.display()))
                }
                (Ent::Link(_), 0) => {
                    changed.ents.insert(victim.clone(), Ent::File(Vec::new(), false));
                    Some(format!("{} is not a symlink", victim.display()))
                }
                _ => {
                    changed.ents.remove(&victim);
                    Some(format!("{} is missing", victim.display()))
                }
            };
            match (judge(expected.clone(), answers(&changed)), why) {
                (Verdict::Stale(got), Some(want)) => assert_eq!(got, want, "seed {seed}"),
                (Verdict::Matches(_), None) => {}
                (got, want) => panic!("seed {seed}: {got:?}, wanted {want:?}"),
            }
        }
        let link = StoredKind::Link(b"x".to_vec());
        assert_eq!(expectations(&link, Path::new("/s/l")).unwrap(), [(PathBuf::from("/s/l"), Expect::Link)]);
        assert!(matches!(judge(vec![(PathBuf::from("/s/l"), Expect::Link)], Vec::new()), Verdict::Stale(_)), "every path needs an answer");
        let broken = StoredKind::Dir { root: Hash::of(b"nothing"), trees: Vec::new() };
        assert!(expectations(&broken, Path::new("/s/d")).is_err());
    }

    #[test]
    fn a_manifest_that_no_longer_matches_the_files_is_hashed_again() {
        let mut fs = tree();
        let db = MemTables::default();
        let hosts = Recorded::default();
        let store = Path::new("/store");
        ensure(&db, &fs, store, &["bbb".into()]).unwrap();
        fs.put("/store/bbb/readme", Ent::File(b"changed".to_vec(), false));
        let p = prepare(&db, &fs, store, &["bbb".into()], Some(&hosts), &mut BTreeSet::new()).unwrap();
        assert_eq!((p.cost.reused_paths, p.cost.new_paths, p.cost.hashed_files), (0, 1, 1));
        assert_eq!(p.roots[0], model(&fs, Path::new("/store/bbb")).0);
        assert!(hosts.0.borrow().contains_key(&Hash::of(b"changed")));
    }

    #[test]
    fn lookups_follow_the_closure_and_the_tree() {
        let fs = tree();
        let db = MemTables::default();
        let names = ["aaa".into(), "bbb".into(), "ccc".into(), "fff".into(), "lll".into()];
        ensure(&db, &fs, Path::new("/store"), &names).unwrap();
        let mut have: BTreeMap<Vec<u8>, Arc<Installed>> = BTreeMap::new();
        for name in &names {
            let kind = load(&db, name.as_bytes()).unwrap();
            have.insert(name.as_bytes().to_vec(), Arc::new(Installed::from_kind(&kind).unwrap()));
        }
        let closure = vec![b"aaa".to_vec(), b"bbb".to_vec()];
        let get = |n: &[u8]| have.get(n).cloned();
        let missing = |_: &[u8]| None;
        assert!(lookup_at(missing, &closure, b"/nix/store", b"aaa").is_none());
        assert!(lookup_at(missing, &closure, b"/nix/store/aaa/bin", b"tool").is_none());
        assert!(matches!(lookup_at(missing, &closure, b"/nix/store", b"ccc"), Some(Looked::Absent)));
        assert!(list_at(missing, &closure, b"/nix/store/aaa").is_none());
        assert!(matches!(lookup_at(get, &closure, b"/nix/store", b"aaa"), Some(Looked::Dir(_))));
        assert!(matches!(lookup_at(get, &closure, b"/nix/store", b"ccc"), Some(Looked::Absent)));
        assert!(lookup_at(get, &closure, b"/usr", b"bin").is_none());
        match lookup_at(get, &closure, b"/nix/store/aaa/bin", b"tool") {
            Some(Looked::File(f)) => {
                assert!(f.exec());
                assert_eq!(f.size, 10);
                assert!(matches!(f.src, FileSrc::Blob(h) if h == Hash::of(b"tool-bytes")), "a store file is named by its content hash");
            }
            other => panic!("tool: {other:?}"),
        }
        assert!(matches!(lookup_at(get, &closure, b"/nix/store/aaa/lib", b"to-tool"), Some(Looked::Symlink(t)) if t == b"../bin/tool"));
        assert!(matches!(lookup_at(get, &closure, b"/nix/store/aaa/lib", b"missing"), Some(Looked::Absent)));
        let listing = list_at(get, &closure, b"/nix/store/aaa").unwrap().unwrap();
        let mut listed: Vec<_> = listing.into_iter().map(|(n, _)| n).collect();
        listed.sort();
        assert_eq!(listed, [b"bin".to_vec(), b"lib".to_vec()]);
        let top = list_at(get, &closure, b"/nix/store").unwrap().unwrap();
        assert_eq!(top.len(), 2);
        assert!(top.iter().any(|(n, t)| n == b"aaa" && *t == DType::Dir));
        assert!(top.iter().all(|(n, _)| n != b"ccc"));
        let file_closure = vec![b"fff".to_vec(), b"lll".to_vec()];
        assert!(matches!(lookup_at(get, &file_closure, b"/nix/store", b"fff"), Some(Looked::File(f)) if f.size == 9));
        assert!(matches!(lookup_at(get, &file_closure, b"/nix/store", b"lll"), Some(Looked::Symlink(t)) if t == b"fff"));
    }

    #[test]
    fn remembering_a_file_does_not_copy_its_bytes() {
        let fs = tree();
        let db = MemTables::default();
        let hosts = Recorded::default();
        prepare(&db, &fs, Path::new("/store"), &["aaa".into()], Some(&hosts), &mut BTreeSet::new()).unwrap();
        let h = Hash::of(b"tool-bytes");
        assert_eq!(hosts.0.borrow().get(&h).map(PathBuf::as_path), Some(Path::new("/store/aaa/bin/tool")), "the file is recorded where it lies in the store");
        assert!(db.get(Table::Meta, &meta_key(b"aaa")).is_some());
        assert!(db.get(Table::Blobs, &h.0).is_none());
    }
}
