use crate::db::{Db, Table};
use crate::hash::Hash;
use crate::view::{DType, FileInfo, FileSrc, HostSig, Listing, Looked, Owner};
use anyhow::{Context, Result};
use dashmap::DashMap;
use rargo_proto::tree::{self, Node, Tree};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

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

pub fn load(db: &Db, name: &[u8]) -> Option<StoredKind> {
    let v = db.get(Table::Meta, &meta_key(name))?;
    bincode::deserialize(&v).ok()
}

fn save(db: &Db, name: &[u8], kind: &StoredKind) -> Result<()> {
    db.put(Table::Meta, &meta_key(name), &bincode::serialize(kind)?);
    Ok(())
}

struct Walked {
    root: Hash,
    trees: Vec<(Hash, Vec<u8>)>,
    files: u64,
    bytes: u64,
}

fn walk_dir(path: &Path, parallel: bool) -> io::Result<Walked> {
    let mut files: Vec<(Vec<u8>, PathBuf, bool)> = Vec::new();
    let mut dirs: Vec<(Vec<u8>, PathBuf)> = Vec::new();
    let mut links: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for ent in fs::read_dir(path)? {
        let ent = ent?;
        let name = ent.file_name().as_bytes().to_vec();
        if name.is_empty()
            || name.contains(&b'/')
            || name.contains(&0)
            || name == b"."
            || name == b".."
        {
            continue;
        }
        let ft = ent.file_type()?;
        if ft.is_symlink() {
            let target = fs::read_link(ent.path())?;
            links.push((name, target.as_os_str().as_bytes().to_vec()));
        } else if ft.is_dir() {
            dirs.push((name, ent.path()));
        } else if ft.is_file() {
            let exec = fs::symlink_metadata(ent.path())?.mode() & 0o111 != 0;
            files.push((name, ent.path(), exec));
        }
    }
    let hash_file =
        |name: &Vec<u8>, path: &Path, exec: &bool| -> io::Result<(Vec<u8>, Node, u64)> {
            let (h, size) = Hash::of_path(path)?;
            Ok((
                name.clone(),
                Node::File {
                    hash: h.0,
                    size,
                    exec: *exec,
                },
                size,
            ))
        };
    let hashed: Vec<io::Result<(Vec<u8>, Node, u64)>> = if parallel {
        files
            .par_iter()
            .map(|(name, path, exec)| hash_file(name, path, exec))
            .collect()
    } else {
        files
            .iter()
            .map(|(name, path, exec)| hash_file(name, path, exec))
            .collect()
    };
    let children: Vec<io::Result<(Vec<u8>, Walked)>> = if parallel {
        dirs.par_iter()
            .map(|(name, path)| Ok((name.clone(), walk_dir(path, true)?)))
            .collect()
    } else {
        dirs.iter()
            .map(|(name, path)| Ok((name.clone(), walk_dir(path, false)?)))
            .collect()
    };

    let mut entries: Vec<(Vec<u8>, Node)> = Vec::new();
    let mut files_n = 0u64;
    let mut bytes = 0u64;
    let mut trees = Vec::new();
    for item in hashed {
        let (name, node, size) = item?;
        files_n += 1;
        bytes += size;
        entries.push((name, node));
    }
    for (name, target) in links {
        entries.push((name, Node::Symlink(target)));
    }
    for item in children {
        let (name, walked) = item?;
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

struct Fresh {
    kind: StoredKind,
    files: u64,
    bytes: u64,
}

fn hash_path(path: &Path) -> io::Result<Fresh> {
    hash_path_mode(path, true)
}

fn hash_path_mode(path: &Path, parallel: bool) -> io::Result<Fresh> {
    let meta = fs::symlink_metadata(path)?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        let target = fs::read_link(path)?;
        return Ok(Fresh {
            kind: StoredKind::Link(target.as_os_str().as_bytes().to_vec()),
            files: 0,
            bytes: 0,
        });
    }
    if ft.is_file() {
        let (hash, size) = Hash::of_path(path)?;
        let exec = meta.mode() & 0o111 != 0;
        return Ok(Fresh {
            kind: StoredKind::File { hash, size, exec },
            files: 1,
            bytes: size,
        });
    }
    if ft.is_dir() {
        let walked = walk_dir(path, parallel)?;
        return Ok(Fresh {
            kind: StoredKind::Dir {
                root: walked.root,
                trees: walked.trees,
            },
            files: walked.files,
            bytes: walked.bytes,
        });
    }
    Err(io::Error::other(format!(
        "{} is not a file, directory or symlink",
        path.display()
    )))
}

pub fn ensure(db: &Db, store: &Path, names: &[String]) -> Result<Prepared> {
    prepare(db, store, names, None, &mut HashSet::new())
}

pub fn prepare(
    db: &Db,
    store: &Path,
    names: &[String],
    hosts: Option<&DashMap<Hash, FileSrc>>,
    seen: &mut HashSet<String>,
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
                let t = Instant::now();
                let fresh = hash_path(&path).with_context(|| format!("hashing {name}"))?;
                cost.millis += t.elapsed().as_millis() as u64;
                save(db, name.as_bytes(), &fresh.kind)?;
                cost.new_paths += 1;
                cost.hashed_files += fresh.files;
                cost.hashed_bytes += fresh.bytes;
                (fresh.kind, false)
            }
        };
        let kind = adopt(
            db, name, &path, kind, was_cached, hosts, seen, &mut cost, true,
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
    db: &Arc<Db>,
    store: &Path,
    names: &[String],
    hosts: Option<&Arc<DashMap<Hash, FileSrc>>>,
    seen: &Arc<parking_lot::Mutex<HashSet<String>>>,
) -> Result<Prepared> {
    let mut cost = Cost::default();
    let mut roots = Vec::with_capacity(names.len());
    let mut pending = Vec::new();
    let host_map = hosts.map(|m| m.as_ref());
    for name in names {
        let path = store.join(name);
        if let Some(kind) = load(db, name.as_bytes()) {
            cost.reused_paths += 1;
            let mut guard = seen.lock();
            let kind = adopt(
                db, name, &path, kind, true, host_map, &mut guard, &mut cost, false,
            )?;
            roots.push(identity(&kind));
        } else {
            roots.push(Hash::ZERO);
            pending.push(name.clone());
        }
    }
    if !pending.is_empty() {
        enqueue(Job {
            db: Arc::clone(db),
            store: store.to_path_buf(),
            names: pending,
            hosts: hosts.cloned(),
            seen: Arc::clone(seen),
        });
    }
    Ok(Prepared { roots, cost })
}

fn adopt(
    db: &Db,
    name: &str,
    path: &Path,
    mut kind: StoredKind,
    was_cached: bool,
    hosts: Option<&DashMap<Hash, FileSrc>>,
    seen: &mut HashSet<String>,
    cost: &mut Cost,
    parallel: bool,
) -> Result<StoredKind> {
    if let Some(hosts) = hosts.filter(|_| !seen.contains(name)) {
        if let Err(e) = attach_kind(&kind, path, hosts) {
            eprintln!(
                "rargod: nix path {name} manifest does not match the files ({e}); hashing it again"
            );
            db.delete(Table::Meta, &meta_key(name.as_bytes()));
            let t = Instant::now();
            let fresh =
                hash_path_mode(path, parallel).with_context(|| format!("hashing {name} again"))?;
            cost.millis += t.elapsed().as_millis() as u64;
            save(db, name.as_bytes(), &fresh.kind)?;
            if was_cached {
                cost.reused_paths = cost.reused_paths.saturating_sub(1);
                cost.new_paths += 1;
            }
            cost.hashed_files += fresh.files;
            cost.hashed_bytes += fresh.bytes;
            attach_kind(&fresh.kind, path, hosts).with_context(|| format!("remembering {name}"))?;
            kind = fresh.kind;
        }
        seen.insert(name.to_string());
    }
    Ok(kind)
}

struct Job {
    db: Arc<Db>,
    store: PathBuf,
    names: Vec<String>,
    hosts: Option<Arc<DashMap<Hash, FileSrc>>>,
    seen: Arc<parking_lot::Mutex<HashSet<String>>>,
}

fn enqueue(job: Job) {
    static TX: OnceLock<std::sync::mpsc::Sender<Job>> = OnceLock::new();
    let tx = TX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("nix-hash".into())
            .spawn(move || {
                idle_priority();
                while let Ok(job) = rx.recv() {
                    hash_job(job);
                }
            })
            .expect("starting nix hash thread");
        tx
    });
    let _ = tx.send(job);
}

fn idle_priority() {
    let tid = unsafe { libc::syscall(libc::SYS_gettid) };
    if tid > 0 {
        unsafe {
            libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, 19);
        }
    }
}

struct Idle {
    depth: parking_lot::Mutex<usize>,
    cv: parking_lot::Condvar,
}

fn idle() -> &'static Idle {
    static IDLE: OnceLock<Idle> = OnceLock::new();
    IDLE.get_or_init(|| Idle {
        depth: parking_lot::Mutex::new(0),
        cv: parking_lot::Condvar::new(),
    })
}

pub struct BuildHold {
    _private: (),
}

pub fn hold_build() -> BuildHold {
    *idle().depth.lock() += 1;
    BuildHold { _private: () }
}

impl Drop for BuildHold {
    fn drop(&mut self) {
        let gate = idle();
        let mut depth = gate.depth.lock();
        *depth = depth.saturating_sub(1);
        if *depth == 0 {
            gate.cv.notify_all();
        }
    }
}

fn one_core() -> bool {
    static ONE: OnceLock<bool> = OnceLock::new();
    *ONE.get_or_init(quota_is_one_core)
}

fn quota_is_one_core() -> bool {
    let Ok(text) = fs::read_to_string("/proc/self/cgroup") else {
        return false;
    };
    let Some(rest) = text.lines().find_map(|line| line.strip_prefix("0::")) else {
        return false;
    };
    let rel = format!("/sys/fs/cgroup{rest}/cpu.max");
    let max = match fs::read_to_string(&rel) {
        Ok(max) => max,
        Err(_) => match fs::read_to_string(format!("/proc/1/root{rel}")) {
            Ok(max) => max,
            Err(_) => return false,
        },
    };
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

#[cfg(test)]
static FORCE_ONE_CORE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn defer_until_idle() -> bool {
    #[cfg(test)]
    if FORCE_ONE_CORE.load(std::sync::atomic::Ordering::Relaxed) {
        return true;
    }
    one_core()
}

fn wait_until_the_build_is_done() {
    if !defer_until_idle() {
        return;
    }
    let gate = idle();
    let mut depth = gate.depth.lock();
    while *depth > 0 {
        gate.cv.wait(&mut depth);
    }
}

fn hash_job(job: Job) {
    let mut cost = Cost::default();
    for name in job.names {
        wait_until_the_build_is_done();
        let path = job.store.join(&name);
        if let Some(kind) = load(&job.db, name.as_bytes()) {
            let mut seen = job.seen.lock();
            if let Err(e) = adopt(
                &job.db,
                &name,
                &path,
                kind,
                true,
                job.hosts.as_deref(),
                &mut seen,
                &mut cost,
                false,
            ) {
                eprintln!("rargod: nix path {name}: {e:#}");
            }
            continue;
        }
        let t = Instant::now();
        let fresh = match hash_path_mode(&path, false) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("rargod: nix path {name}: {e}");
                continue;
            }
        };
        cost.millis += t.elapsed().as_millis() as u64;
        if let Some(hosts) = job.hosts.as_deref() {
            let mut seen = job.seen.lock();
            if !seen.contains(&name) {
                if let Err(e) = attach_kind(&fresh.kind, &path, hosts) {
                    eprintln!("rargod: nix path {name} manifest does not match the files ({e})");
                    continue;
                }
                seen.insert(name.clone());
            }
        }
        if let Err(e) = save(&job.db, name.as_bytes(), &fresh.kind) {
            eprintln!("rargod: nix path {name}: {e:#}");
            continue;
        }
        cost.new_paths += 1;
        cost.hashed_files += fresh.files;
        cost.hashed_bytes += fresh.bytes;
    }
    log_cost(&cost);
}

fn attach_kind(kind: &StoredKind, path: &Path, hosts: &DashMap<Hash, FileSrc>) -> io::Result<()> {
    match kind {
        StoredKind::Dir { root, trees } => {
            let installed = PathTrees::decode(*root, trees)?;
            attach_tree(&installed, *root, path, hosts)
        }
        StoredKind::File { hash, size, .. } => remember_file(path, *hash, *size, hosts),
        StoredKind::Link(_) => {
            if fs::symlink_metadata(path)?.file_type().is_symlink() {
                Ok(())
            } else {
                Err(io::Error::other("not a symlink"))
            }
        }
    }
}

fn attach_tree(
    trees: &PathTrees,
    digest: Hash,
    dir: &Path,
    hosts: &DashMap<Hash, FileSrc>,
) -> io::Result<()> {
    let Some(tree) = trees.trees.get(&digest) else {
        return Err(io::Error::other("missing directory tree"));
    };
    for (name, node) in &tree.entries {
        let path = dir.join(std::ffi::OsStr::from_bytes(name));
        match node {
            Node::Dir(d) => attach_tree(trees, Hash(*d), &path, hosts)?,
            Node::File { hash, size, .. } => remember_file(&path, Hash(*hash), *size, hosts)?,
            Node::Symlink(_) => {
                if !fs::symlink_metadata(&path)?.file_type().is_symlink() {
                    return Err(io::Error::other(format!(
                        "{} is not a symlink",
                        path.display()
                    )));
                }
            }
        }
    }
    Ok(())
}

fn remember_file(
    path: &Path,
    hash: Hash,
    size: u64,
    hosts: &DashMap<Hash, FileSrc>,
) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.file_type().is_file() || meta.len() != size {
        return Err(io::Error::other(format!(
            "{} is not the manifested file",
            path.display()
        )));
    }
    hosts.entry(hash).or_insert_with(|| FileSrc::Host {
        path: Arc::from(path.to_path_buf()),
        sig: HostSig::of(&meta),
    });
    Ok(())
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
    trees: HashMap<Hash, Tree>,
}

impl PathTrees {
    fn decode(root: Hash, encoded: &[(Hash, Vec<u8>)]) -> io::Result<PathTrees> {
        let mut trees = HashMap::with_capacity(encoded.len());
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
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    fn tree(store: &Path) {
        fs::create_dir_all(store.join("aaa/bin")).unwrap();
        fs::create_dir_all(store.join("aaa/lib")).unwrap();
        fs::create_dir_all(store.join("bbb")).unwrap();
        fs::create_dir_all(store.join("ccc")).unwrap();
        let tool = store.join("aaa/bin/tool");
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).mode(0o555);
        opts.open(&tool).unwrap().write_all(b"tool-bytes").unwrap();
        fs::write(store.join("aaa/lib/note"), b"note").unwrap();
        std::os::unix::fs::symlink("../bin/tool", store.join("aaa/lib/to-tool")).unwrap();
        fs::write(store.join("bbb/readme"), b"bee").unwrap();
        fs::write(store.join("ccc/only"), b"cee").unwrap();
        fs::write(store.join("fff"), b"file-path").unwrap();
        std::os::unix::fs::symlink("fff", store.join("lll")).unwrap();
    }

    #[test]
    fn a_shared_store_path_is_manifested_once() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store");
        tree(&store);
        let db = Db::open(&tmp.path().join("db.redb")).unwrap();
        let first = ensure(&db, &store, &["aaa".into(), "bbb".into()]).unwrap();
        assert_eq!(first.cost.new_paths, 2);
        assert_eq!(first.cost.reused_paths, 0);
        assert_eq!(first.cost.hashed_files, 3);
        let second = ensure(&db, &store, &["bbb".into(), "ccc".into()]).unwrap();
        assert_eq!(
            second.cost.reused_paths, 1,
            "bbb was hashed with the first closure"
        );
        assert_eq!(second.cost.new_paths, 1);
        assert_eq!(second.cost.hashed_files, 1, "only ccc's file is new");
        assert_eq!(second.roots[0], first.roots[1], "bbb keeps its tree");
        let third = ensure(&db, &store, &["aaa".into(), "bbb".into()]).unwrap();
        assert_eq!(third.cost.hashed_files, 0);
        assert_eq!(third.cost.reused_paths, 2);
        assert_eq!(third.roots, first.roots);
    }

    #[test]
    fn hashing_a_store_path_does_not_block_the_caller() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store");
        tree(&store);
        let db = Arc::new(Db::open(&tmp.path().join("db.redb")).unwrap());
        let seen = Arc::new(parking_lot::Mutex::new(HashSet::new()));
        let first = schedule(&db, &store, &["aaa".into(), "bbb".into()], None, &seen).unwrap();
        assert_eq!(first.roots, vec![Hash::ZERO, Hash::ZERO]);
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while load(&db, b"aaa").is_none() || load(&db, b"bbb").is_none() {
            assert!(Instant::now() < deadline, "the paths were not hashed");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let second = schedule(&db, &store, &["bbb".into(), "ccc".into()], None, &seen).unwrap();
        assert_ne!(second.roots[0], Hash::ZERO);
        assert_eq!(second.roots[0], identity(&load(&db, b"bbb").unwrap()));
        assert_eq!(second.cost.reused_paths, 1);
        assert_eq!(second.roots[1], Hash::ZERO);
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while load(&db, b"ccc").is_none() {
            assert!(Instant::now() < deadline, "ccc was not hashed");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let third = schedule(&db, &store, &["aaa".into(), "bbb".into()], None, &seen).unwrap();
        assert_eq!(third.cost.reused_paths, 2);
        assert_eq!(third.cost.new_paths, 0);
        assert!(third.roots.iter().all(|r| *r != Hash::ZERO));
        assert_eq!(third.roots[1], second.roots[0]);
    }

    #[test]
    fn hashing_records_files_in_the_caller_map() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store");
        tree(&store);
        let db = Arc::new(Db::open(&tmp.path().join("db-hosts.redb")).unwrap());
        let seen = Arc::new(parking_lot::Mutex::new(HashSet::new()));
        let hosts = Arc::new(DashMap::new());
        schedule(&db, &store, &["aaa".into()], Some(&hosts), &seen).unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while load(&db, b"aaa").is_none() {
            assert!(Instant::now() < deadline, "aaa was not hashed");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!hosts.is_empty(), "the server map received the file locations");
    }

    #[test]
    fn a_busy_one_core_server_hashes_after_the_build() {
        struct Clear;
        impl Drop for Clear {
            fn drop(&mut self) {
                FORCE_ONE_CORE.store(false, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let _clear = Clear;
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store");
        tree(&store);
        let db = Arc::new(Db::open(&tmp.path().join("db-idle.redb")).unwrap());
        let seen = Arc::new(parking_lot::Mutex::new(HashSet::new()));
        FORCE_ONE_CORE.store(true, std::sync::atomic::Ordering::Relaxed);
        let hold = hold_build();
        let first = schedule(&db, &store, &["aaa".into()], None, &seen).unwrap();
        assert_eq!(first.roots, vec![Hash::ZERO]);
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            load(&db, b"aaa").is_none(),
            "hashing waits while the build holds the only core"
        );
        drop(hold);
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while load(&db, b"aaa").is_none() {
            assert!(
                Instant::now() < deadline,
                "the path was not hashed after the build"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn lookups_follow_the_closure_and_the_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store");
        tree(&store);
        let db = Db::open(&tmp.path().join("db.redb")).unwrap();
        let names = [
            "aaa".into(),
            "bbb".into(),
            "ccc".into(),
            "fff".into(),
            "lll".into(),
        ];
        let prepared = ensure(&db, &store, &names).unwrap();
        let mut have: HashMap<Vec<u8>, Arc<Installed>> = HashMap::new();
        for name in &names {
            let kind = load(&db, name.as_bytes()).unwrap();
            have.insert(
                name.as_bytes().to_vec(),
                Arc::new(Installed::from_kind(&kind).unwrap()),
            );
        }
        let mut closure = vec![b"aaa".to_vec(), b"bbb".to_vec()];
        closure.sort();
        let get = |n: &[u8]| have.get(n).cloned();
        let missing = |_: &[u8]| None;
        assert!(lookup_at(missing, &closure, b"/nix/store", b"aaa").is_none());
        assert!(lookup_at(missing, &closure, b"/nix/store/aaa/bin", b"tool").is_none());
        assert!(matches!(
            lookup_at(missing, &closure, b"/nix/store", b"ccc"),
            Some(Looked::Absent)
        ));
        assert!(list_at(missing, &closure, b"/nix/store/aaa").is_none());
        assert!(matches!(
            lookup_at(get, &closure, b"/nix/store", b"aaa"),
            Some(Looked::Dir(_))
        ));
        assert!(matches!(
            lookup_at(get, &closure, b"/nix/store", b"ccc"),
            Some(Looked::Absent)
        ));
        assert!(lookup_at(get, &closure, b"/usr", b"bin").is_none());
        let (want, size) = Hash::of_path(&store.join("aaa/bin/tool")).unwrap();
        match lookup_at(get, &closure, b"/nix/store/aaa/bin", b"tool") {
            Some(Looked::File(f)) => {
                assert!(f.exec());
                assert_eq!(f.size, size);
                match f.src {
                    FileSrc::Blob(h) => assert_eq!(h, want),
                    _ => panic!("a store file is named by its content hash"),
                }
            }
            other => panic!("tool: {other:?}"),
        }
        match lookup_at(get, &closure, b"/nix/store/aaa/lib", b"to-tool") {
            Some(Looked::Symlink(t)) => assert_eq!(t, b"../bin/tool"),
            other => panic!("symlink: {other:?}"),
        }
        match lookup_at(get, &closure, b"/nix/store/aaa/lib", b"missing") {
            Some(Looked::Absent) => {}
            other => panic!("missing: {other:?}"),
        }
        let listing = list_at(get, &closure, b"/nix/store/aaa").unwrap().unwrap();
        let mut names: Vec<_> = listing.into_iter().map(|(n, _)| n).collect();
        names.sort();
        assert_eq!(names, [b"bin".to_vec(), b"lib".to_vec()]);
        let top = list_at(get, &closure, b"/nix/store").unwrap().unwrap();
        assert_eq!(top.len(), 2);
        assert!(top.iter().any(|(n, t)| n == b"aaa" && *t == DType::Dir));
        assert!(top.iter().all(|(n, _)| n != b"ccc"));
        let file_closure = vec![b"fff".to_vec(), b"lll".to_vec()];
        match lookup_at(get, &file_closure, b"/nix/store", b"fff") {
            Some(Looked::File(f)) => assert_eq!(f.size, b"file-path".len() as u64),
            other => panic!("file path: {other:?}"),
        }
        match lookup_at(get, &file_closure, b"/nix/store", b"lll") {
            Some(Looked::Symlink(t)) => assert_eq!(t, b"fff"),
            other => panic!("link path: {other:?}"),
        }
        let _ = prepared;
    }

    #[test]
    fn remembering_a_file_does_not_copy_its_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("store");
        tree(&store);
        let db = Db::open(&tmp.path().join("db.redb")).unwrap();
        let hosts: DashMap<Hash, FileSrc> = DashMap::new();
        let mut seen = HashSet::new();
        prepare(&db, &store, &["aaa".into()], Some(&hosts), &mut seen).unwrap();
        let (h, _) = Hash::of_path(&store.join("aaa/bin/tool")).unwrap();
        match hosts.get(&h).map(|e| e.clone()) {
            Some(FileSrc::Host { path, .. }) => {
                assert_eq!(path.as_ref(), store.join("aaa/bin/tool").as_path())
            }
            other => panic!("host file: {other:?}"),
        }
        assert!(db.get(Table::Meta, &meta_key(b"aaa")).is_some());
        assert!(db.get(Table::Blobs, &h.0).is_none());
    }
}
