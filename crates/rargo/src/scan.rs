use rargo_proto::Digest;
use rargo_proto::tree::{Node, Tree};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub ino: u64,
    pub hash: Digest,
    pub hashed_at_ns: i64,
}

const RACY_NS: i64 = 50_000_000;

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Index {
    pub files: HashMap<Vec<u8>, Entry>,
}

impl Index {
    pub fn load(path: &Path) -> Index {
        (|| -> Option<Index> {
            let raw = std::fs::read(path).ok()?;
            let plain = zstd::stream::decode_all(&raw[..]).ok()?;
            bincode::deserialize(&plain).ok()
        })()
        .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let plain = bincode::serialize(self).map_err(io::Error::other)?;
        let packed = zstd::stream::encode_all(&plain[..], 1)?;
        crate::write_atomic(path, &packed)
    }
}

#[derive(Clone)]
pub struct Scan {
    pub root: Digest,
    pub trees: HashMap<Digest, Vec<u8>>,
    pub files: HashMap<Digest, (PathBuf, u64)>,
    refs: HashMap<Digest, u32>,
    grafted: Vec<Digest>,
    pub dirs: Vec<PathBuf>,
    pub dir_trees: HashMap<Vec<u8>, Digest>,
    worktree_root: Vec<u8>,
    pub listed_dirs: u64,
    pub index: Index,
    pub index_changed: bool,
    pub file_count: u64,
    pub hashed_files: u64,
    pub hashed_bytes: u64,
}

#[derive(Default)]
struct ScanState {
    trees: HashMap<Digest, Vec<u8>>,
    files: HashMap<Digest, (PathBuf, u64)>,
    refs: HashMap<Digest, u32>,
    dir_trees: HashMap<Vec<u8>, Digest>,
    index: HashMap<Vec<u8>, Entry>,
    relink: HashSet<Digest>,
    listed_dirs: u64,
    removed: u64,
    hashed_files: u64,
    hashed_bytes: u64,
}

impl ScanState {
    fn add_file(&mut self, hash: Digest, path: PathBuf, size: u64) {
        *self.refs.entry(hash).or_insert(0) += 1;
        self.files.entry(hash).or_insert((path, size));
    }

    fn drop_file(&mut self, hash: Digest, path: &Path) {
        let Some(n) = self.refs.get_mut(&hash) else { return };
        assert!(*n > 0, "a counted content has a reference");
        *n -= 1;
        assert!(self.files.contains_key(&hash), "a counted content has a file");
        if *n == 0 {
            self.refs.remove(&hash);
            self.files.remove(&hash);
        } else if self.files.get(&hash).is_some_and(|(p, _)| p == path) {
            self.relink.insert(hash);
        }
    }
}

fn now_ns() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as i64).unwrap_or(0)
}

pub fn hash_file(path: &Path, size: u64) -> io::Result<Digest> {
    let mut h = blake3::Hasher::new();
    if size >= 4 << 20 {
        h.update_mmap_rayon(path)?;
    } else {
        let mut f = std::fs::File::open(path)?;
        let mut buf = Vec::with_capacity(size as usize + 1);
        f.read_to_end(&mut buf)?;
        h.update(&buf);
    }
    Ok(*h.finalize().as_bytes())
}

fn join(rel: &[u8], name: &[u8]) -> Vec<u8> {
    assert!(!name.is_empty());
    let mut v = Vec::with_capacity(rel.len() + 1 + name.len());
    if !rel.is_empty() {
        v.extend_from_slice(rel);
        v.push(b'/');
    }
    v.extend_from_slice(name);
    assert_eq!(v.len(), rel.len() + name.len() + usize::from(!rel.is_empty()));
    v
}

fn excluded(patterns: &[String], rel: &[u8], name: &[u8]) -> bool {
    patterns.iter().any(|p| {
        let anchored = p.trim_end_matches('/').contains('/');
        let p = p.trim_matches('/').as_bytes();
        if anchored { p == rel } else { p == name }
    })
}

#[derive(Debug, Default, PartialEq)]
pub struct Ignored {
    paths: HashSet<Vec<u8>>,
    include: Vec<String>,
    on_way: HashSet<Vec<u8>>,
}

#[derive(Debug, PartialEq)]
enum Take {
    Skip,
    Keep,
    OnlyTheWay,
}

impl Ignored {
    pub fn from_git(root: &Path, include: &[String]) -> Option<Ignored> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["--no-optional-locks", "ls-files", "-z", "--others", "--ignored", "--exclude-standard", "--directory"])
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())?;
        Some(Ignored::new(crate::git::ignored_paths(&out.stdout), include))
    }

    fn new<'p>(listed: impl Iterator<Item = &'p [u8]>, include: &[String]) -> Ignored {
        let mut me = Ignored { include: include.to_vec(), ..Ignored::default() };
        for p in listed {
            me.paths.insert(p.strip_suffix(b"/").unwrap_or(p).to_vec());
        }
        for p in include.iter().filter(|p| p.trim_end_matches('/').contains('/')) {
            let mut way = p.trim_matches('/').as_bytes();
            while let Some(i) = way.iter().rposition(|&b| b == b'/') {
                way = &way[..i];
                me.on_way.insert(way.to_vec());
            }
        }
        me
    }

    #[cfg(test)]
    pub fn for_test(listed: &[&str], include: &[String]) -> Ignored {
        Ignored::new(listed.iter().map(|p| p.as_bytes()), include)
    }

    fn included(&self, rel: &[u8], name: &[u8]) -> bool {
        excluded(&self.include, rel, name)
    }

    fn take(&self, rel: &[u8], name: &[u8], inside: bool) -> Take {
        if !inside && !self.paths.contains(rel) {
            return Take::Keep;
        }
        if self.included(rel, name) {
            Take::Keep
        } else if self.on_way.contains(rel) {
            Take::OnlyTheWay
        } else {
            Take::Skip
        }
    }

    pub fn left_out(&self, rel: &[u8]) -> bool {
        let mut inside = false;
        let mut at = 0;
        loop {
            let end = rel[at..].iter().position(|&b| b == b'/').map(|i| at + i).unwrap_or(rel.len());
            let (here, name) = (&rel[..end], &rel[at..end]);
            match self.take(here, name, inside) {
                Take::Skip => return true,
                Take::Keep => inside = false,
                Take::OnlyTheWay => inside = true,
            }
            if end == rel.len() {
                return false;
            }
            at = end + 1;
        }
    }
}

fn is_cache_dir(dir: &Path) -> bool {
    let mut head = [0u8; 43];
    match std::fs::File::open(dir.join("CACHEDIR.TAG")).and_then(|mut f| f.read_exact(&mut head)) {
        Ok(()) => head[..] == *rargo_proto::CACHEDIR_SIGNATURE,
        Err(_) => false,
    }
}

enum Directory {
    Reuse(Digest),
    Listing(Vec<(Vec<u8>, Observed)>),
}

enum Observed {
    Directory(Directory),
    Symlink(Vec<u8>),
    File { entry: Entry, exec: bool, hashed: bool },
}

struct ScanPlan<'a> {
    old: Option<&'a Index>,
    previous: Option<&'a Scan>,
    on_path: Option<&'a HashSet<Vec<u8>>>,
    exclude: &'a [String],
    ignored: Option<&'a Ignored>,
}

impl ScanPlan<'_> {
    fn reuse(&self, rel: &[u8]) -> Option<Digest> {
        if self.on_path?.contains(rel) {
            return None;
        }
        self.previous?.dir_trees.get(rel).copied()
    }

    fn take(&self, dir: &[u8], name: &[u8], inside: bool) -> Option<(Vec<u8>, Take)> {
        let child = join(dir, name);
        if name == b".git" || excluded(self.exclude, &child, name) || (dir.is_empty() && name == b"target") {
            return None;
        }
        let take = self.ignored.map_or(Take::Keep, |i| i.take(&child, name, inside));
        (take != Take::Skip).then_some((child, take))
    }

    fn reuse_file(&self, rel: &[u8], stamp: Stamp) -> Option<Entry> {
        let index = self.old.or_else(|| self.previous.map(|scan| &scan.index))?;
        let old = index.files.get(rel).copied()?;
        let same = old.size == stamp.size && old.mtime_ns == stamp.mtime_ns && old.ctime_ns == stamp.ctime_ns && old.ino == stamp.ino;
        let settled = stamp.mtime_ns.max(stamp.ctime_ns) + RACY_NS < old.hashed_at_ns;
        (same && settled).then_some(old)
    }
}

#[derive(Clone, Copy)]
struct Stamp {
    size: u64,
    mtime_ns: i64,
    ctime_ns: i64,
    ino: u64,
}

fn observe_dir(root: &Path, rel: &[u8], inside: bool, plan: &ScanPlan<'_>) -> io::Result<Directory> {
    if let Some(digest) = plan.reuse(rel) {
        return Ok(Directory::Reuse(digest));
    }
    let abs = if rel.is_empty() { root.to_path_buf() } else { root.join(OsStr::from_bytes(rel)) };
    let mut names = std::fs::read_dir(&abs)?
        .map(|entry| entry.map(|entry| entry.file_name().as_bytes().to_vec()))
        .collect::<io::Result<Vec<_>>>()?;
    names.sort();
    let nodes: Vec<_> = names.into_par_iter().map(|name| {
        observe_entry(root, rel, &name, inside, plan).map(|entry| entry.map(|value| (name, value)))
    }).collect();
    let listing = nodes.into_iter().collect::<io::Result<Vec<_>>>()?.into_iter().flatten().collect();
    Ok(Directory::Listing(listing))
}

fn observe_entry(root: &Path, rel: &[u8], name: &[u8], inside: bool, plan: &ScanPlan<'_>) -> io::Result<Option<Observed>> {
    let Some((child, take)) = plan.take(rel, name, inside) else { return Ok(None) };
    let path = root.join(OsStr::from_bytes(&child));
    let md = match std::fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if md.is_dir() {
        if is_cache_dir(&path) {
            return Ok(None);
        }
        return match observe_dir(root, &child, take == Take::OnlyTheWay, plan) {
            Ok(dir) => Ok(Some(Observed::Directory(dir))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        };
    }
    if md.is_symlink() {
        return Ok(std::fs::read_link(&path).ok().map(|target| Observed::Symlink(target.as_os_str().as_bytes().to_vec())));
    }
    if !md.is_file() || take == Take::OnlyTheWay {
        return Ok(None);
    }
    let exec = md.mode() & 0o111 != 0;
    let stamp = Stamp {
        size: md.len(),
        mtime_ns: md.mtime() * 1_000_000_000 + md.mtime_nsec(),
        ctime_ns: md.ctime() * 1_000_000_000 + md.ctime_nsec(),
        ino: md.ino(),
    };
    if let Some(entry) = plan.reuse_file(&child, stamp) {
        return Ok(Some(Observed::File { entry, exec, hashed: false }));
    }
    let hashed_at_ns = now_ns();
    let hash = match hash_file(&path, stamp.size) {
        Ok(hash) => hash,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", path.display()))),
    };
    let entry = Entry { size: stamp.size, mtime_ns: stamp.mtime_ns, ctime_ns: stamp.ctime_ns, ino: stamp.ino, hash, hashed_at_ns };
    Ok(Some(Observed::File { entry, exec, hashed: true }))
}

impl ScanState {
    fn dir(&mut self, root: &Path, rel: &[u8], directory: Directory, root_before: &[u8]) -> io::Result<Digest> {
        let listing = match directory {
            Directory::Reuse(digest) => return Ok(digest),
            Directory::Listing(listing) => listing,
        };
        self.listed_dirs += 1;
        let before = self.dir_trees.get(rel).map(|d| {
            if rel.is_empty() {
                Tree::decode(root_before)
            } else {
                self.decode(d)
            }
        }).transpose()?;
        let old: BTreeMap<_, _> = before.iter().flat_map(|t| t.entries.iter().map(|(n, x)| (n.as_slice(), x))).collect();
        let mut entries = Vec::with_capacity(listing.len());
        let mut index_add = Vec::new();
        for (name, observed) in listing {
            let child = join(rel, &name);
            let node = match observed {
                Observed::Directory(dir) => Node::Dir(self.dir(root, &child, dir, root_before)?),
                Observed::Symlink(target) => Node::Symlink(target),
                Observed::File { entry, exec, hashed } => {
                    if hashed {
                        self.hashed_files += 1;
                        self.hashed_bytes += entry.size;
                    }
                    index_add.push((name.clone(), child, entry));
                    Node::File { hash: entry.hash, size: entry.size, exec }
                }
            };
            entries.push((name, node));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        assert!(entries.windows(2).all(|w| w[0].0 < w[1].0), "names in a directory are unique");
        for (name, was) in &old {
            let now = entries.binary_search_by(|(n, _)| n.as_slice().cmp(name)).ok().map(|i| &entries[i].1);
            let child = join(rel, name);
            match (was, now) {
                (Node::File { hash, .. }, Some(Node::File { hash: h, .. })) if h == hash => {}
                (Node::File { hash, .. }, _) => self.forget_file(root, &child, *hash),
                (Node::Dir(_), Some(Node::Dir(_))) => {}
                (Node::Dir(d), _) => self.forget(root, &child, *d)?,
                (Node::Symlink(_), _) => {}
            }
        }
        index_add.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        for (name, child, entry) in index_add {
            if !matches!(old.get(name.as_slice()), Some(Node::File { hash, .. }) if *hash == entry.hash) {
                self.add_file(entry.hash, root.join(OsStr::from_bytes(&child)), entry.size);
            }
            self.index.insert(child, entry);
        }
        let encoded = Tree { entries }.encode();
        debug_assert!(Tree::decode(&encoded).is_ok());
        let digest = rargo_proto::tree::digest(&encoded);
        self.trees.insert(digest, encoded);
        self.dir_trees.insert(rel.to_vec(), digest);
        Ok(digest)
    }

    fn decode(&self, digest: &Digest) -> io::Result<Tree> {
        Tree::decode(self.trees.get(digest).ok_or_else(|| io::Error::other("a tree of the last scan is missing"))?)
    }

    fn forget_file(&mut self, root: &Path, child: &[u8], hash: Digest) {
        self.drop_file(hash, &root.join(OsStr::from_bytes(child)));
        self.index.remove(child);
        self.removed += 1;
    }

    fn forget(&mut self, root: &Path, rel: &[u8], digest: Digest) -> io::Result<()> {
        for (name, node) in self.decode(&digest)?.entries {
            let child = join(rel, &name);
            match node {
                Node::Dir(d) => self.forget(root, &child, d)?,
                Node::File { hash, .. } => self.forget_file(root, &child, hash),
                Node::Symlink(_) => {}
            }
        }
        self.dir_trees.remove(rel);
        Ok(())
    }
}

pub fn scan(root: &Path, old: &Index, exclude: &[String], ignored: Option<&Ignored>) -> io::Result<Scan> {
    scan_dir(root, Some(old), None, exclude, ignored)
}

pub fn rescan(root: &Path, prev: Scan, changed: &HashSet<PathBuf>, exclude: &[String], ignored: Option<&Ignored>) -> io::Result<Scan> {
    let mut on_path: HashSet<Vec<u8>> = [Vec::new()].into_iter().collect();
    for dir in changed {
        let Ok(rel) = dir.strip_prefix(root) else { continue };
        let mut rel = rel.as_os_str().as_bytes();
        while !rel.is_empty() && on_path.insert(rel.to_vec()) {
            rel = match rel.iter().rposition(|&c| c == b'/') {
                Some(i) => &rel[..i],
                None => b"",
            };
        }
    }
    scan_dir(root, None, Some((prev, on_path)), exclude, ignored)
}

pub fn scan_store(dir: &Path, old: &Index) -> io::Result<Scan> {
    scan_dir(dir, Some(old), None, &[], None)
}

pub fn graft_git(scan: &mut Scan, root: &Path, git: &Path, history: Option<Scan>) -> io::Result<()> {
    let mut sub = scan_dir(git, Some(&Index::default()), None, &[], None)?;
    let put = |top: &mut Tree, name: &[u8], node: Node| {
        match top.entries.binary_search_by(|(n, _)| n.as_slice().cmp(name)) {
            Ok(i) => top.entries[i].1 = node,
            Err(i) => top.entries.insert(i, (name.to_vec(), node)),
        }
    };
    if let Some(h) = history {
        let mut top = Tree::decode(&sub.trees[&sub.root])?;
        let objects = top.entries.iter().find(|(n, _)| n == b"objects").map(|(_, node)| node.clone());
        let Some(Node::Dir(objects)) = objects else { return Err(io::Error::other("the git stand-in has no objects directory")) };
        sub.trees.extend(h.trees);
        let digest = merge(&mut sub.trees, objects, h.root)?;
        put(&mut top, b"objects", Node::Dir(digest));
        let encoded = top.encode();
        sub.root = rargo_proto::tree::digest(&encoded);
        sub.trees.insert(sub.root, encoded);
        for (hash, at) in h.files {
            sub.files.entry(hash).or_insert(at);
        }
    }
    let mut top = Tree::decode(&scan.trees[&scan.root])?;
    if root.join(".git").is_file() {
        let text = format!("gitdir: {}\n", rargo_proto::GIT_SIDE);
        let gitfile = git.parent().unwrap_or(git).join("gitfile-v1");
        if std::fs::read(&gitfile).ok().as_deref() != Some(text.as_bytes()) {
            crate::write_atomic(&gitfile, text.as_bytes())?;
        }
        let hash = *blake3::hash(text.as_bytes()).as_bytes();
        scan.files.entry(hash).or_insert((gitfile, text.len() as u64));
        scan.grafted.push(hash);
        put(&mut top, b".git", Node::File { hash, size: text.len() as u64, exec: false });
        put(&mut top, rargo_proto::GIT_SIDE.as_bytes(), Node::Dir(sub.root));
    } else {
        put(&mut top, b".git", Node::Dir(sub.root));
    }
    let encoded = top.encode();
    scan.trees.remove(&scan.root);
    scan.root = rargo_proto::tree::digest(&encoded);
    scan.trees.insert(scan.root, encoded);
    scan.trees.extend(sub.trees);
    for (h, at) in sub.files {
        scan.files.entry(h).or_insert(at);
        scan.grafted.push(h);
    }
    Ok(())
}

fn merge(trees: &mut HashMap<Digest, Vec<u8>>, a: Digest, b: Digest) -> io::Result<Digest> {
    let mut out = Tree::decode(&trees[&a])?;
    for (name, node) in Tree::decode(&trees[&b])?.entries {
        let node = match (out.get(&name).cloned(), node) {
            (Some(Node::Dir(x)), Node::Dir(y)) => Node::Dir(merge(trees, x, y)?),
            (_, n) => n,
        };
        match out.entries.binary_search_by(|(n, _)| n.as_slice().cmp(&name)) {
            Ok(i) => out.entries[i].1 = node,
            Err(i) => out.entries.insert(i, (name, node)),
        }
    }
    assert!(out.entries.windows(2).all(|w| w[0].0 < w[1].0), "a merged directory stays sorted");
    let encoded = out.encode();
    let digest = rargo_proto::tree::digest(&encoded);
    trees.insert(digest, encoded);
    Ok(digest)
}

fn scan_dir(
    root: &Path,
    old: Option<&Index>,
    prev: Option<(Scan, HashSet<Vec<u8>>)>,
    exclude: &[String],
    ignored: Option<&Ignored>,
) -> io::Result<Scan> {
    let plan = ScanPlan {
        old,
        previous: prev.as_ref().map(|(scan, _)| scan),
        on_path: prev.as_ref().map(|(_, paths)| paths),
        exclude,
        ignored,
    };
    let observed = observe_dir(root, b"", false, &plan)?;
    finish_scan(root, old, prev.map(|(scan, _)| scan), observed)
}

fn finish_scan(root: &Path, old: Option<&Index>, previous: Option<Scan>, observed: Directory) -> io::Result<Scan> {
    let (mut state, root_before, files_before) = match previous {
        Some(mut scan) => {
            for hash in scan.grafted.drain(..) {
                if !scan.refs.contains_key(&hash) {
                    scan.files.remove(&hash);
                }
            }
            let count = scan.index.files.len();
            let state = ScanState {
                trees: scan.trees,
                files: scan.files,
                refs: scan.refs,
                dir_trees: scan.dir_trees,
                index: scan.index.files,
                ..ScanState::default()
            };
            (state, scan.worktree_root, count)
        }
        None => (ScanState::default(), Vec::new(), old.map_or(0, |index| index.files.len())),
    };
    let root_digest = state.dir(root, b"", observed, &root_before)?;
    if !state.relink.is_empty() {
        let mut candidates: BTreeMap<Digest, (&Vec<u8>, u64)> = BTreeMap::new();
        for (path, entry) in &state.index {
            if state.relink.contains(&entry.hash) {
                let candidate = candidates.entry(entry.hash).or_insert((path, entry.size));
                if path < candidate.0 {
                    *candidate = (path, entry.size);
                }
            }
        }
        for (hash, (path, size)) in candidates {
            state.files.insert(hash, (root.join(OsStr::from_bytes(path)), size));
        }
    }
    let worktree_root = state.trees.get(&root_digest).cloned().unwrap_or_default();
    if state.trees.len() > 2 * state.dir_trees.len() + 256 {
        let live: HashSet<_> = state.dir_trees.values().collect();
        state.trees.retain(|d, _| live.contains(d));
    }
    let mut dirs: Vec<_> = state.dir_trees.keys().map(|rel| if rel.is_empty() { root.to_path_buf() } else { root.join(OsStr::from_bytes(rel)) }).collect();
    dirs.sort_unstable();
    let index_changed = state.hashed_files > 0 || state.removed > 0 || state.index.len() != files_before;
    Ok(Scan {
        root: root_digest,
        trees: state.trees,
        files: state.files,
        refs: state.refs,
        grafted: Vec::new(),
        dirs,
        dir_trees: state.dir_trees,
        worktree_root,
        listed_dirs: state.listed_dirs,
        file_count: state.index.len() as u64,
        index: Index { files: state.index },
        index_changed,
        hashed_files: state.hashed_files,
        hashed_bytes: state.hashed_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    fn tree<'a>(s: &'a Scan, d: &Digest) -> Tree {
        Tree::decode(&s.trees[d]).unwrap()
    }

    #[test]
    fn a_rescan_looks_only_where_told_and_equals_a_whole_scan_there() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        write(r, "Cargo.toml", "[package]");
        write(r, "src/lib.rs", "pub fn a() {}");
        write(r, "src/deep/x.rs", "x");
        write(r, "src/deep/same.rs", "same");
        write(r, "docs/same.md", "same");
        write(r, "docs/a.md", "a");
        write(r, "docs/old/b.md", "b");
        write(r, "target/debug/out", "o");
        write(r, "node_modules/m/i.js", "m");
        let exclude = vec!["node_modules".to_string()];
        let whole = || scan(r, &Index::default(), &exclude, None).unwrap();
        let same = |a: &Scan, b: &Scan| {
            assert_eq!(a.root, b.root);
            let sorted = |s: &Scan| {
                let mut d = s.dirs.clone();
                d.sort();
                d
            };
            assert_eq!(sorted(a), sorted(b));
            assert_eq!(a.dir_trees, b.dir_trees);
            assert_eq!(a.index.files.keys().collect::<HashSet<_>>(), b.index.files.keys().collect::<HashSet<_>>());
            assert_eq!(a.files.keys().collect::<HashSet<_>>(), b.files.keys().collect::<HashSet<_>>());
            for (h, (path, size)) in &a.files {
                assert_eq!(hash_file(path, *size).unwrap(), *h, "{} has the content it is recorded for", path.display());
            }
            assert_eq!(a.refs, b.refs);
            assert_eq!(a.file_count, b.file_count);
            for d in a.dir_trees.values().filter(|d| **d != a.dir_trees[&b""[..]]) {
                assert!(a.trees.contains_key(d), "every directory's tree is at hand");
            }
        };
        std::thread::sleep(std::time::Duration::from_millis(120));
        let s0 = scan(r, &Index::default(), &exclude, None).unwrap();
        let rel: HashSet<&[u8]> = s0.dirs.iter().map(|d| d.strip_prefix(r).unwrap().as_os_str().as_bytes()).collect();
        assert_eq!(rel, [&b""[..], b"src", b"src/deep", b"docs", b"docs/old"].into_iter().collect());
        assert_eq!(s0.listed_dirs, 5);

        write(r, "src/deep/x.rs", "changed");
        write(r, "src/deep/new/y.rs", "y");
        std::fs::remove_dir_all(r.join("docs/old")).unwrap();
        let told: HashSet<PathBuf> = [r.join("src/deep"), r.join("docs"), r.join("docs/old")].into_iter().collect();
        let s1 = rescan(r, s0, &told, &exclude, None).unwrap();
        same(&s1, &whole());
        assert_eq!(s1.listed_dirs, 5);
        assert_eq!(s1.hashed_files, 2, "only the changed and the new file are read");
        assert!(s1.index_changed);

        write(r, "docs/a.md", "unseen");
        let s2 = rescan(r, s1.clone(), &[r.join("src")].into_iter().collect(), &exclude, None).unwrap();
        assert_eq!(s2.root, s1.root);
        assert_eq!(s2.listed_dirs, 2, "the root and src; src/deep and src/deep/new are taken as they were");
        assert!(!s2.index_changed);
        let s3 = rescan(r, s2, &[r.join("docs")].into_iter().collect(), &exclude, None).unwrap();
        same(&s3, &whole());

        let shared = *blake3::hash(b"same").as_bytes();
        let recorded = s3.files[&shared].0.clone();
        std::fs::remove_file(&recorded).unwrap();
        let s4 = rescan(r, s3, &[recorded.parent().unwrap().to_path_buf()].into_iter().collect(), &exclude, None).unwrap();
        same(&s4, &whole());
        assert_ne!(s4.files[&shared].0, recorded);
        let other = s4.files[&shared].0.clone();
        std::fs::remove_file(&other).unwrap();
        let s5 = rescan(r, s4, &[other.parent().unwrap().to_path_buf()].into_iter().collect(), &exclude, None).unwrap();
        same(&s5, &whole());
        assert!(!s5.files.contains_key(&shared));

        let s6 = rescan(r, s5.clone(), &HashSet::new(), &exclude, None).unwrap();
        same(&s6, &s5);
        assert_eq!(s6.listed_dirs, 1);
        let mut grafted = s6;
        let elsewhere = tempfile::tempdir().unwrap();
        write(elsewhere.path(), "HEAD", "ref: refs/heads/main");
        graft_git(&mut grafted, r, elsewhere.path(), None).unwrap();
        assert_ne!(grafted.root, s5.root);
        same(&rescan(r, grafted, &HashSet::new(), &exclude, None).unwrap(), &s5);
    }

    #[test]
    fn what_git_ignores_is_left_out_on_request_but_what_include_names() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(r)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        write(r, ".gitignore", "node_modules/\n*.log\ndata/\n.env\n");
        write(r, "src/lib.rs", "pub fn a() {}");
        write(r, "node_modules/a/b.js", "x");
        write(r, "x.log", "x");
        write(r, "src/y.log", "x");
        write(r, "kept.log", "tracked although it matches");
        write(r, "data/big.bin", "x");
        write(r, "data/schema.sql", "create table t;");
        write(r, "data/sub/z", "x");
        write(r, ".env", "A=1");
        git(&["init", "-q"]);
        git(&["add", "-A"]);
        git(&["add", "-f", "kept.log"]);
        git(&["commit", "-q", "-m", "one"]);
        let names = |s: &Scan, d: &Digest| tree(s, d).entries.iter().map(|(n, _)| String::from_utf8(n.clone()).unwrap()).collect::<Vec<_>>();
        let dir = |s: &Scan, d: &Digest, name: &str| match tree(s, d).get(name.as_bytes()) {
            Some(Node::Dir(x)) => *x,
            other => panic!("{name}: {other:?}"),
        };

        let all = scan(r, &Index::default(), &[], None).unwrap();
        assert!(names(&all, &all.root).contains(&"node_modules".to_string()));

        let include = ["data/schema.sql".to_string(), ".env".to_string()];
        let ignored = Ignored::from_git(r, &include).unwrap();
        let s = scan(r, &Index::default(), &[], Some(&ignored)).unwrap();
        assert_eq!(names(&s, &s.root), [".env", ".gitignore", "data", "kept.log", "src"]);
        assert_eq!(names(&s, &dir(&s, &s.root, "src")), ["lib.rs"]);
        assert_eq!(names(&s, &dir(&s, &s.root, "data")), ["schema.sql"], "only the way to the included file");
        assert!(!s.dirs.iter().any(|d| d.ends_with("node_modules") || d.ends_with("data/sub")), "not looked into, so not watched");
        for (path, out) in [
            ("node_modules/a/b.js", true),
            ("x.log", true),
            ("src/y.log", true),
            ("data/big.bin", true),
            ("data/sub/z", true),
            ("data/schema.sql", false),
            (".env", false),
            ("kept.log", false),
            ("src/lib.rs", false),
        ] {
            assert_eq!(ignored.left_out(path.as_bytes()), out, "{path}");
        }
        assert!(!ignored.left_out(b"data"), "data/ is not left out whole");

        let ignored = Ignored::from_git(r, &[]).unwrap();
        let s = scan(r, &Index::default(), &[], Some(&ignored)).unwrap();
        assert_eq!(names(&s, &s.root), [".gitignore", "kept.log", "src"]);
        let elsewhere = tempfile::tempdir().unwrap();
        assert!(Ignored::from_git(elsewhere.path(), &[]).is_none());
    }

    #[test]
    fn a_linked_worktree_keeps_its_git_file() {
        let tmp = tempfile::tempdir().unwrap();
        let (main, linked, stand_in) = (tmp.path().join("main"), tmp.path().join("linked"), tmp.path().join("cache/git/abc"));
        for r in [&main, &linked] {
            write(r, "src/lib.rs", "pub fn a() {}");
        }
        write(&stand_in, "HEAD", "ref: refs/heads/main\n");
        std::fs::create_dir_all(main.join(".git")).unwrap();
        std::fs::write(linked.join(".git"), "gitdir: /elsewhere/.git/worktrees/linked\n").unwrap();
        let mut s = scan(&main, &Index::default(), &[], None).unwrap();
        graft_git(&mut s, &main, &stand_in, None).unwrap();
        let top = tree(&s, &s.root);
        assert!(matches!(top.get(b".git"), Some(Node::Dir(_))));
        assert!(top.get(rargo_proto::GIT_SIDE.as_bytes()).is_none());
        let store = tmp.path().join("cache/git-history/r/heads/c");
        write(&stand_in, "objects/pack/pack-1.pack", "pack");
        write(&store, "pack/pack-2.pack", "history");
        let h = scan_store(&store, &Index::default()).unwrap();
        let mut s = scan(&main, &Index::default(), &[], None).unwrap();
        graft_git(&mut s, &main, &stand_in, Some(h)).unwrap();
        let Some(Node::Dir(git)) = tree(&s, &s.root).get(b".git").cloned() else { panic!("no .git") };
        let Some(Node::Dir(objects)) = tree(&s, &git).get(b"objects").cloned() else { panic!("no objects") };
        let Some(Node::Dir(pack)) = tree(&s, &objects).get(b"pack").cloned() else { panic!("no pack directory") };
        let packs = tree(&s, &pack);
        assert!(packs.get(b"pack-1.pack").is_some() && packs.get(b"pack-2.pack").is_some(), "both packs side by side");
        let Some(Node::File { hash, .. }) = packs.get(b"pack-2.pack").cloned() else { panic!() };
        assert_eq!(s.files[&hash].0, store.join("pack/pack-2.pack"), "sent from the store");
        let mut s = scan(&linked, &Index::default(), &[], None).unwrap();
        graft_git(&mut s, &linked, &stand_in, None).unwrap();
        let top = tree(&s, &s.root);
        let Some(Node::File { hash, .. }) = top.get(b".git") else { panic!("`.git` is not a file") };
        let (path, _) = &s.files[hash];
        assert_eq!(std::fs::read_to_string(path).unwrap(), format!("gitdir: {}\n", rargo_proto::GIT_SIDE));
        let Some(Node::Dir(side)) = top.get(rargo_proto::GIT_SIDE.as_bytes()) else { panic!("no stand-in beside it") };
        assert!(tree(&s, side).get(b"HEAD").is_some());
    }

    #[test]
    fn scans_and_skips() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        write(r, "Cargo.toml", "[package]");
        write(r, "src/lib.rs", "pub fn a() {}");
        write(r, ".git/HEAD", "ref: refs/heads/main");
        write(r, "target/debug/app", "binary");
        write(r, "sub/out/CACHEDIR.TAG", rargo_proto::CACHEDIR_TAG);
        write(r, "sub/out/debug/x", "binary");
        write(r, "sub/Cargo.lock", "ignored by git, synced anyway");
        write(r, "node_modules/big/file", "junk");
        write(r, "docs/node_modules/x", "junk");
        write(r, "vendor/skip/me", "junk");
        write(r, "vendor/keep/me", "kept");
        write(r, "cache/x", "at the root: excluded");
        write(r, "docs/cache/y", "deeper: kept");
        std::os::unix::fs::symlink("src", r.join("link")).unwrap();
        std::fs::create_dir(r.join("empty")).unwrap();
        let exclude = vec!["node_modules".to_string(), "vendor/skip".to_string(), "/cache".to_string()];
        let s = scan(r, &Index::default(), &exclude, None).unwrap();
        let top = tree(&s, &s.root);
        let names: Vec<&str> = top.entries.iter().map(|(n, _)| std::str::from_utf8(n).unwrap()).collect();
        assert_eq!(names, ["Cargo.toml", "docs", "empty", "link", "src", "sub", "vendor"]);
        assert_eq!(top.get(b"link"), Some(&Node::Symlink(b"src".to_vec())));
        let Some(Node::Dir(sub)) = top.get(b"sub") else { panic!() };
        assert_eq!(tree(&s, sub).entries.len(), 1);
        let Some(Node::Dir(docs)) = top.get(b"docs") else { panic!() };
        assert_eq!(tree(&s, docs).entries.len(), 1, "only docs/cache");
        let Some(Node::Dir(vendor)) = top.get(b"vendor") else { panic!() };
        assert_eq!(tree(&s, vendor).entries.len(), 1);
        assert_eq!(s.file_count, 5);
        assert_eq!(s.hashed_files, 5);
        assert!(s.index_changed);
        let lib = *blake3::hash(b"pub fn a() {}").as_bytes();
        assert_eq!(s.files[&lib], (r.join("src/lib.rs"), 13));
        write(tmp.path(), "thin/HEAD", "ref: refs/heads/main\n");
        let mut g = scan(r, &Index::default(), &exclude, None).unwrap();
        graft_git(&mut g, r, &tmp.path().join("thin"), None).unwrap();
        assert_ne!(g.root, s.root);
        let top = tree(&g, &g.root);
        let Some(Node::Dir(git)) = top.get(b".git") else { panic!() };
        assert_eq!(tree(&g, git).entries.len(), 1);
        assert_eq!(top.entries[0].0, b".git");
        let head = *blake3::hash(b"ref: refs/heads/main\n").as_bytes();
        assert_eq!(g.files[&head].0, tmp.path().join("thin/HEAD"));
    }

    fn invariants(scan: &Scan, root: &Path) -> bool {
        scan.dir_trees.get(b"".as_slice()) == Some(&scan.root)
            && scan.dir_trees.values().all(|hash| scan.trees.contains_key(hash))
            && scan.refs.values().map(|&n| u64::from(n)).sum::<u64>() == scan.file_count
            && scan.files.iter().all(|(hash, (path, size))| {
                path.strip_prefix(root).ok().and_then(|rel| scan.index.files.get(rel.as_os_str().as_bytes()))
                    .is_some_and(|entry| &entry.hash == hash && &entry.size == size)
            })
    }

    #[test]
    fn incremental_model_relinks_identical_content_and_reuses_unvisited_directories() {
        let root = Path::new("/test-tree");
        let entry = Entry { size: 1, mtime_ns: 0, ctime_ns: 0, ino: 1, hash: [4; 32], hashed_at_ns: RACY_NS + 1 };
        let file = |hashed| Observed::File { entry, exec: false, hashed };
        let leaf = b"file".to_vec();
        let observed = Directory::Listing(vec![
            (b"a".to_vec(), Observed::Directory(Directory::Listing(vec![(leaf.clone(), file(true))]))),
            (b"b".to_vec(), Observed::Directory(Directory::Listing(vec![(leaf, file(true))]))),
            (b"link".to_vec(), Observed::Symlink(b"b/file".to_vec())),
        ]);
        let old = finish_scan(root, None, None, observed).unwrap();
        assert!(invariants(&old, root));
        assert_eq!(old.files[&entry.hash].0, root.join("a/file"));
        assert_eq!(old.hashed_files, 2);
        assert_eq!(old.listed_dirs, 3);

        let on_path = HashSet::from([Vec::new(), b"a".to_vec()]);
        let plan = ScanPlan { old: None, previous: Some(&old), on_path: Some(&on_path), exclude: &[], ignored: None };
        let keep = plan.reuse(b"b").unwrap();
        assert_eq!(plan.reuse(b"a"), None);
        let observed = Directory::Listing(vec![
            (b"b".to_vec(), Observed::Directory(Directory::Reuse(keep))),
            (b"link".to_vec(), Observed::Symlink(b"b/file".to_vec())),
        ]);
        let new = finish_scan(root, None, Some(old), observed).unwrap();
        assert!(invariants(&new, root));
        assert_eq!(new.files[&entry.hash].0, root.join("b/file"));
        assert_eq!(new.hashed_files, 0);
        assert_eq!(new.listed_dirs, 1);
        assert_eq!(new.file_count, 1);
        assert!(new.index_changed);
    }

    #[test]
    fn metadata_shortcuts_and_ignore_decisions_are_pure() {
        let stamp = Stamp { size: 5, mtime_ns: 1, ctime_ns: 2, ino: 3 };
        let entry = Entry { size: 5, mtime_ns: 1, ctime_ns: 2, ino: 3, hash: [7; 32], hashed_at_ns: 2 + RACY_NS };
        let mut index = Index { files: HashMap::from([(b"hello".to_vec(), entry)]) };
        let ignored = Ignored::for_test(&["secret", "vendored/"], &["vendored/src".into()]);
        let plan = ScanPlan { old: Some(&index), previous: None, on_path: None, exclude: &[], ignored: Some(&ignored) };
        assert_eq!(plan.reuse_file(b"hello", stamp), None);
        index.files.get_mut(b"hello".as_slice()).unwrap().hashed_at_ns += 1;
        let plan = ScanPlan { old: Some(&index), previous: None, on_path: None, exclude: &[], ignored: Some(&ignored) };
        assert!(plan.reuse_file(b"hello", stamp).is_some());
        assert_eq!(plan.reuse_file(b"hello", Stamp { ino: 6, ..stamp }), None);
        assert_eq!(plan.take(b"", b"secret", false), None);
        assert_eq!(plan.take(b"", b"target", false), None);
        assert_eq!(plan.take(b"", b"vendored", false).unwrap().1, Take::OnlyTheWay);
        assert_eq!(plan.take(b"vendored", b"src", true).unwrap().1, Take::Keep);
    }

    #[test]
    fn index_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "wt/a", "x");
        let s = scan(&tmp.path().join("wt"), &Index::default(), &[], None).unwrap();
        let p = tmp.path().join("cache/index");
        s.index.save(&p).unwrap();
        assert_eq!(Index::load(&p).files, s.index.files);
        assert!(Index::load(&tmp.path().join("missing")).files.is_empty());
    }
}
