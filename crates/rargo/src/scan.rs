//! Describing a worktree as a content-addressed tree, quickly.
//!
//! Every command scans the whole worktree: one `lstat` per file, in
//! parallel. A file is only read when its size, times or inode differ from
//! what the index remembered when it was last hashed, so a scan costs time
//! proportional to the number of files plus the bytes that changed.
//!
//! What is left out: `.git` (anywhere), cargo target directories (any
//! directory holding a `CACHEDIR.TAG`, and `target` at the root), anything
//! the project's policy excludes, and things that are neither file,
//! directory nor symlink. Everything else is synced, whether git ignores it
//! or not (an ignored `Cargo.lock` or generated source is as much an input
//! to the build as a tracked file), unless the policy says
//! `gitignored = false`: then what git ignores is left out too, except what
//! the policy's `include` names ([`Ignored`]).

use rargo_proto::Digest;
use rargo_proto::tree::{Node, Tree};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// What the index remembers about a file it has hashed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub ino: u64,
    pub hash: Digest,
    /// When the file was hashed. A file modified this close to being hashed
    /// may have been modified again afterwards without its timestamps
    /// moving, so it is hashed once more on the next scan.
    pub hashed_at_ns: i64,
}

/// Timestamps closer than this to the moment of hashing are not trusted.
const RACY_NS: i64 = 50_000_000;

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Index {
    /// By path relative to the root.
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
    /// Encoded trees by digest.
    pub trees: HashMap<Digest, Vec<u8>>,
    /// Where to read each distinct file content from, and its size.
    pub files: HashMap<Digest, (PathBuf, u64)>,
    /// How many files of the worktree have each content (`files` names
    /// one of them).
    refs: HashMap<Digest, u32>,
    /// What `graft_git` added to `files`.
    grafted: Vec<Digest>,
    /// Every directory of the worktree that is synced (what a watcher has
    /// to watch).
    pub dirs: Vec<PathBuf>,
    /// The tree of each of those directories, by its path from the root
    /// ("" for the root): what [`rescan`] reuses.
    pub dir_trees: HashMap<Vec<u8>, Digest>,
    /// The worktree's root tree, before `graft_git` adds `.git` to it.
    worktree_root: Vec<u8>,
    /// How many directories were listed (the others were taken as they
    /// were).
    pub listed_dirs: u64,
    pub index: Index,
    /// The index differs from the one the scan started with.
    pub index_changed: bool,
    pub file_count: u64,
    pub hashed_files: u64,
    pub hashed_bytes: u64,
}

/// What a scan builds, behind locks for the directories scanned in
/// parallel. A rescan starts from the scan before and edits it.
#[derive(Default)]
struct Maps {
    trees: Mutex<HashMap<Digest, Vec<u8>>>,
    files: Mutex<HashMap<Digest, (PathBuf, u64)>>,
    refs: Mutex<HashMap<Digest, u32>>,
    dir_trees: Mutex<HashMap<Vec<u8>, Digest>>,
    index: Mutex<HashMap<Vec<u8>, Entry>>,
    /// Contents whose recorded path went while other files have it.
    relink: Mutex<HashSet<Digest>>,
}

struct Ctx<'a> {
    root: &'a Path,
    /// A whole scan: the index of an earlier one, whose unchanged entries
    /// spare reading files. A rescan uses its own index for that.
    old: Option<&'a Index>,
    /// A rescan: the directories that may have changed, every directory
    /// above them and the root. Others are taken as they were.
    on_path: Option<HashSet<Vec<u8>>>,
    /// A rescan: the root's tree before.
    root_before: Vec<u8>,
    exclude: &'a [String],
    ignored: Option<&'a Ignored>,
    m: Maps,
    listed_dirs: AtomicU64,
    removed: AtomicU64,
    hashed_files: AtomicU64,
    hashed_bytes: AtomicU64,
}

impl Maps {
    fn add_file(&self, hash: Digest, path: PathBuf, size: u64) {
        *self.refs.lock().unwrap().entry(hash).or_insert(0) += 1;
        self.files.lock().unwrap().entry(hash).or_insert((path, size));
    }

    fn drop_file(&self, hash: Digest, path: &Path) {
        let mut refs = self.refs.lock().unwrap();
        let Some(n) = refs.get_mut(&hash) else { return };
        assert!(*n > 0, "a counted content has a reference");
        *n -= 1;
        let mut files = self.files.lock().unwrap();
        assert!(files.contains_key(&hash), "a counted content has a file");
        if *n == 0 {
            refs.remove(&hash);
            files.remove(&hash);
        } else if files.get(&hash).is_some_and(|(p, _)| p == path) {
            self.relink.lock().unwrap().insert(hash);
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

/// Whether a policy pattern excludes `rel`: a pattern with a slash (also
/// just a leading one) is a path from the root, one without matches that
/// name at any depth.
fn excluded(patterns: &[String], rel: &[u8], name: &[u8]) -> bool {
    patterns.iter().any(|p| {
        let anchored = p.trim_end_matches('/').contains('/');
        let p = p.trim_matches('/').as_bytes();
        if anchored { p == rel } else { p == name }
    })
}

/// What git ignores in a worktree, as git itself says (`.gitignore` files,
/// `info/exclude`, the user's excludes file; a tracked file is never
/// ignored), less what the policy's `include` takes back. An ignored
/// directory is one entry and is not looked into, unless a path in
/// `include` lies inside it: then only the way to that path is.
#[derive(Debug, Default, PartialEq)]
pub struct Ignored {
    /// Paths from the root; a directory's without its trailing slash.
    paths: HashSet<Vec<u8>>,
    include: Vec<String>,
    /// The directories that lead to a path `include` names.
    on_way: HashSet<Vec<u8>>,
}

/// What the scan does with one entry.
#[derive(Debug, PartialEq)]
enum Take {
    Skip,
    Keep,
    /// Kept, but only the way to an included path inside it is.
    OnlyTheWay,
}

impl Ignored {
    /// Asks git; `None` where git cannot say (no repository, no git).
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
        Some(Ignored::new(out.stdout.split(|&b| b == 0).filter(|p| !p.is_empty()), include))
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

    /// As if git had listed `listed` (directories with a trailing slash).
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

    /// Whether the scan left `rel` (a path from the root) out because git
    /// ignores it.
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

impl Ctx<'_> {
    /// What the index knows about `rel`.
    fn cached(&self, rel: &[u8]) -> Option<Entry> {
        match self.old {
            Some(old) => old.files.get(rel).copied(),
            None => self.m.index.lock().unwrap().get(rel).copied(),
        }
    }

    /// Scans one directory; returns its tree's digest. `inside`: the
    /// directory is one git ignores, entered only on the way to a path the
    /// policy includes.
    fn dir(&self, rel: &[u8], inside: bool) -> io::Result<Digest> {
        let abs = if rel.is_empty() { self.root.to_path_buf() } else { self.root.join(OsStr::from_bytes(rel)) };
        // A rescan: the directory as it was, if nothing below it may have
        // changed; else what it held, to tell what went.
        let mut before: Option<Tree> = None;
        if let Some(on_path) = &self.on_path {
            let was = self.m.dir_trees.lock().unwrap().get(rel).copied();
            if let Some(d) = was {
                if !on_path.contains(rel) {
                    return Ok(d);
                }
                before = Some(if rel.is_empty() {
                    Tree::decode(&self.root_before)?
                } else {
                    let trees = self.m.trees.lock().unwrap();
                    Tree::decode(trees.get(&d).ok_or_else(|| io::Error::other("a tree of the last scan is missing"))?)?
                });
            }
        }
        self.listed_dirs.fetch_add(1, Ordering::Relaxed);
        let mut names: Vec<Vec<u8>> = Vec::new();
        for e in std::fs::read_dir(&abs)? {
            names.push(e?.file_name().as_bytes().to_vec());
        }
        let nodes: Vec<io::Result<Option<(Vec<u8>, Node, Option<Entry>)>>> = names
            .into_par_iter()
            .map(|name| -> io::Result<Option<(Vec<u8>, Node, Option<Entry>)>> {
                let child_rel = join(rel, &name);
                if name == b".git" || excluded(self.exclude, &child_rel, &name) || (rel.is_empty() && name == b"target") {
                    return Ok(None);
                }
                let take = self.ignored.map(|i| i.take(&child_rel, &name, inside)).unwrap_or(Take::Keep);
                if take == Take::Skip {
                    return Ok(None);
                }
                let child_abs = abs.join(OsStr::from_bytes(&name));
                let md = match std::fs::symlink_metadata(&child_abs) {
                    Ok(m) => m,
                    // Removed since the directory was listed.
                    Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
                    Err(e) => return Err(e),
                };
                let ft = md.file_type();
                if ft.is_dir() {
                    if is_cache_dir(&child_abs) {
                        return Ok(None);
                    }
                    return match self.dir(&child_rel, take == Take::OnlyTheWay) {
                        Ok(d) => Ok(Some((name, Node::Dir(d), None))),
                        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                        Err(e) => Err(e),
                    };
                }
                if ft.is_symlink() {
                    return match std::fs::read_link(&child_abs) {
                        Ok(t) => Ok(Some((name, Node::Symlink(t.as_os_str().as_bytes().to_vec()), None))),
                        Err(_) => Ok(None),
                    };
                }
                if !ft.is_file() || take == Take::OnlyTheWay {
                    return Ok(None);
                }
                let exec = md.mode() & 0o111 != 0;
                let (mtime_ns, ctime_ns) =
                    (md.mtime() * 1_000_000_000 + md.mtime_nsec(), md.ctime() * 1_000_000_000 + md.ctime_nsec());
                if let Some(old) = self.cached(&child_rel) {
                    let same = old.size == md.len() && old.mtime_ns == mtime_ns && old.ctime_ns == ctime_ns && old.ino == md.ino();
                    let settled = mtime_ns.max(ctime_ns) + RACY_NS < old.hashed_at_ns;
                    if same && settled {
                        return Ok(Some((name, Node::File { hash: old.hash, size: old.size, exec }, Some(old))));
                    }
                }
                let hashed_at_ns = now_ns();
                let hash = match hash_file(&child_abs, md.len()) {
                    Ok(h) => h,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
                    Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", child_abs.display()))),
                };
                self.hashed_files.fetch_add(1, Ordering::Relaxed);
                self.hashed_bytes.fetch_add(md.len(), Ordering::Relaxed);
                let entry = Entry { size: md.len(), mtime_ns, ctime_ns, ino: md.ino(), hash, hashed_at_ns };
                Ok(Some((name, Node::File { hash, size: md.len(), exec }, Some(entry))))
            })
            .collect();
        let mut entries = Vec::with_capacity(nodes.len());
        let mut index_add = Vec::new();
        for n in nodes {
            let Some((name, node, entry)) = n? else { continue };
            if let (Node::File { hash, size, .. }, Some(e)) = (&node, entry) {
                index_add.push((name.clone(), *hash, *size, e));
            }
            entries.push((name, node));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        assert!(entries.windows(2).all(|w| w[0].0 < w[1].0), "names in a directory are unique");
        let old: HashMap<&[u8], &Node> = before.iter().flat_map(|t| t.entries.iter().map(|(n, x)| (n.as_slice(), x))).collect();
        // What was here and is not, or is no longer what it was.
        for (name, was) in &old {
            let now = entries.binary_search_by(|(n, _)| n.as_slice().cmp(name)).ok().map(|i| &entries[i].1);
            let child = join(rel, name);
            match (was, now) {
                (Node::File { hash, .. }, Some(Node::File { hash: h, .. })) if h == hash => {}
                (Node::File { hash, .. }, _) => {
                    self.m.drop_file(*hash, &abs.join(OsStr::from_bytes(name)));
                    self.m.index.lock().unwrap().remove(&child);
                    self.removed.fetch_add(1, Ordering::Relaxed);
                }
                (Node::Dir(_), Some(Node::Dir(_))) => {}
                (Node::Dir(d), _) => self.forget(&child, *d)?,
                (Node::Symlink(_), _) => {}
            }
        }
        // What is here and was not.
        let mut index = Vec::with_capacity(index_add.len());
        for (name, hash, size, e) in index_add {
            let new = !matches!(old.get(name.as_slice()), Some(Node::File { hash: h, .. }) if *h == hash);
            if new {
                self.m.add_file(hash, abs.join(OsStr::from_bytes(&name)), size);
            }
            index.push((join(rel, &name), e));
        }
        let encoded = Tree { entries }.encode();
        debug_assert!(Tree::decode(&encoded).is_ok());
        let digest = rargo_proto::tree::digest(&encoded);
        self.m.trees.lock().unwrap().insert(digest, encoded);
        self.m.dir_trees.lock().unwrap().insert(rel.to_vec(), digest);
        self.m.index.lock().unwrap().extend(index);
        Ok(digest)
    }

    /// Removes directory `rel` of the scan before, and all below it.
    fn forget(&self, rel: &[u8], digest: Digest) -> io::Result<()> {
        let tree = {
            let trees = self.m.trees.lock().unwrap();
            Tree::decode(trees.get(&digest).ok_or_else(|| io::Error::other("a tree of the last scan is missing"))?)?
        };
        let abs = self.root.join(OsStr::from_bytes(rel));
        for (name, node) in tree.entries {
            let child = join(rel, &name);
            match node {
                Node::Dir(d) => self.forget(&child, d)?,
                Node::File { hash, .. } => {
                    self.m.drop_file(hash, &abs.join(OsStr::from_bytes(&name)));
                    self.m.index.lock().unwrap().remove(&child);
                    self.removed.fetch_add(1, Ordering::Relaxed);
                }
                Node::Symlink(_) => {}
            }
        }
        self.m.dir_trees.lock().unwrap().remove(rel);
        Ok(())
    }
}

/// Scans the worktree at `root`, leaving out what `ignored` says git
/// ignores.
pub fn scan(root: &Path, old: &Index, exclude: &[String], ignored: Option<&Ignored>) -> io::Result<Scan> {
    scan_dir(root, Some(old), None, exclude, ignored)
}

/// Scans the worktree again, looking only at the directories in `changed`
/// (absolute paths), those above them and the root: every other directory
/// of `prev` is taken as it was, without being looked at. `prev` must have
/// been made with the same `exclude` and `ignored`; it becomes the result.
pub fn rescan(root: &Path, prev: Scan, changed: &HashSet<PathBuf>, exclude: &[String], ignored: Option<&Ignored>) -> io::Result<Scan> {
    // The root is always listed: `graft_git` replaced its tree.
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

/// Scans a directory the client keeps itself (the history of
/// [`crate::git::history`]): everything in it, against its own index.
pub fn scan_store(dir: &Path, old: &Index) -> io::Result<Scan> {
    scan_dir(dir, Some(old), None, &[], None)
}

/// Shows `git` (see `git::thin`) as the worktree's `.git`, with what
/// `history` holds (a [`scan_store`] of `git::history`'s packs) in its
/// `objects` directory beside its own.
pub fn graft_git(scan: &mut Scan, root: &Path, git: &Path, history: Option<Scan>) -> io::Result<()> {
    // Small and immutable: scanned from scratch, grafted into the root.
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
        // A linked worktree: `.git` is a file that names the git
        // directory, and it is shown as one. Build scripts tell the two
        // apart (rust-analyzer's asks cargo to rerun it when
        // `.git/HEAD` changes, and finds no such file in a linked
        // worktree); showing a directory made every commit rerun the
        // script and recompile the crate where stock cargo does not.
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
    // The root without `.git` is not part of the tree any more (no
    // directory below the root can equal it).
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

/// The directory with the entries of both `a` and `b`, directories of the
/// same name merged, `b`'s entry taken where one of them is a file.
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
    let (m, on_path, root_before, files_before) = match prev {
        Some((mut p, on_path)) => {
            // Not the worktree's: `graft_git` adds them again.
            for h in p.grafted.drain(..) {
                if !p.refs.contains_key(&h) {
                    p.files.remove(&h);
                }
            }
            let n = p.index.files.len();
            let m = Maps {
                trees: Mutex::new(p.trees),
                files: Mutex::new(p.files),
                refs: Mutex::new(p.refs),
                dir_trees: Mutex::new(p.dir_trees),
                index: Mutex::new(p.index.files),
                relink: Mutex::new(HashSet::new()),
            };
            (m, Some(on_path), p.worktree_root, n)
        }
        None => (Maps::default(), None, Vec::new(), old.map(|o| o.files.len()).unwrap_or(0)),
    };
    let ctx = Ctx {
        root,
        old,
        on_path,
        root_before,
        exclude,
        ignored,
        m,
        listed_dirs: AtomicU64::new(0),
        removed: AtomicU64::new(0),
        hashed_files: AtomicU64::new(0),
        hashed_bytes: AtomicU64::new(0),
    };
    let root_digest = ctx.dir(b"", false)?;
    let m = ctx.m;
    let mut files = m.files.into_inner().unwrap();
    let index = m.index.into_inner().unwrap();
    let relink = m.relink.into_inner().unwrap();
    if !relink.is_empty() {
        // Another file with the same content stands in for the one gone.
        for (rel, e) in &index {
            if relink.contains(&e.hash) {
                files.insert(e.hash, (root.join(OsStr::from_bytes(rel)), e.size));
            }
        }
    }
    let dir_trees = m.dir_trees.into_inner().unwrap();
    let mut trees = m.trees.into_inner().unwrap();
    if trees.len() > 2 * dir_trees.len() + 256 {
        // Trees of earlier scans that no directory has any more.
        let live: HashSet<&Digest> = dir_trees.values().collect();
        trees.retain(|d, _| live.contains(d));
    }
    let worktree_root = trees.get(&root_digest).cloned().unwrap_or_default();
    let hashed_files = ctx.hashed_files.into_inner();
    // Unchanged when nothing was read and no path came or went.
    let index_changed = hashed_files > 0 || ctx.removed.into_inner() > 0 || index.len() != files_before;
    Ok(Scan {
        root: root_digest,
        trees,
        files,
        refs: m.refs.into_inner().unwrap(),
        grafted: Vec::new(),
        dirs: dir_trees.keys().map(|rel| if rel.is_empty() { root.to_path_buf() } else { root.join(OsStr::from_bytes(rel)) }).collect(),
        dir_trees,
        worktree_root,
        listed_dirs: ctx.listed_dirs.into_inner(),
        file_count: index.len() as u64,
        index: Index { files: index },
        index_changed,
        hashed_files,
        hashed_bytes: ctx.hashed_bytes.into_inner(),
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
        // Equal to a whole scan in everything the daemon uses.
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
        // Past the window in which a file just written is read again.
        std::thread::sleep(std::time::Duration::from_millis(120));
        let s0 = scan(r, &Index::default(), &exclude, None).unwrap();
        // Only what is synced is a directory to watch.
        let rel: HashSet<&[u8]> = s0.dirs.iter().map(|d| d.strip_prefix(r).unwrap().as_os_str().as_bytes()).collect();
        assert_eq!(rel, [&b""[..], b"src", b"src/deep", b"docs", b"docs/old"].into_iter().collect());
        assert_eq!(s0.listed_dirs, 5);

        // A file changed, a directory made and one removed, all in places
        // the rescan is told about.
        write(r, "src/deep/x.rs", "changed");
        write(r, "src/deep/new/y.rs", "y");
        std::fs::remove_dir_all(r.join("docs/old")).unwrap();
        let told: HashSet<PathBuf> = [r.join("src/deep"), r.join("docs"), r.join("docs/old")].into_iter().collect();
        let s1 = rescan(r, s0, &told, &exclude, None).unwrap();
        same(&s1, &whole());
        // The root, src and docs on the way, src/deep and docs told, the
        // new directory below src/deep.
        assert_eq!(s1.listed_dirs, 5);
        assert_eq!(s1.hashed_files, 2, "only the changed and the new file are read");
        assert!(s1.index_changed);

        // A change where the rescan is not told is not looked at.
        write(r, "docs/a.md", "unseen");
        let s2 = rescan(r, s1.clone(), &[r.join("src")].into_iter().collect(), &exclude, None).unwrap();
        assert_eq!(s2.root, s1.root);
        assert_eq!(s2.listed_dirs, 2, "the root and src; src/deep and src/deep/new are taken as they were");
        assert!(!s2.index_changed);
        // Told, it is.
        let s3 = rescan(r, s2, &[r.join("docs")].into_iter().collect(), &exclude, None).unwrap();
        same(&s3, &whole());

        // Of two files with the same content, the one recorded goes: the
        // other stands in. Then the other goes too: so does the content.
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

        // With nothing told, only the root is looked at.
        let s6 = rescan(r, s5.clone(), &HashSet::new(), &exclude, None).unwrap();
        same(&s6, &s5);
        assert_eq!(s6.listed_dirs, 1);
        // Also after `.git` was grafted onto the root: what the graft
        // added is not the worktree's.
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

        // Without the option nothing changes: everything is synced.
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

        // Without `include`, an ignored directory is one entry, left out.
        let ignored = Ignored::from_git(r, &[]).unwrap();
        let s = scan(r, &Index::default(), &[], Some(&ignored)).unwrap();
        assert_eq!(names(&s, &s.root), [".gitignore", "kept.log", "src"]);
        // Outside a repository git cannot say.
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
        // A checkout with a git directory is shown with a directory.
        let mut s = scan(&main, &Index::default(), &[], None).unwrap();
        graft_git(&mut s, &main, &stand_in, None).unwrap();
        let top = tree(&s, &s.root);
        assert!(matches!(top.get(b".git"), Some(Node::Dir(_))));
        assert!(top.get(rargo_proto::GIT_SIDE.as_bytes()).is_none());
        // History's packs, beside the stand-in's own.
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
        // A linked worktree with a file that names the directory beside it.
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
        // A directory grafted in as `.git`.
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

    #[test]
    fn unchanged_files_are_not_read_again() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        write(r, "a.rs", "one");
        write(r, "d/b.rs", "two");
        let s1 = scan(r, &Index::default(), &[], None).unwrap();
        // Hashed just after being written: not trusted yet.
        let s2 = scan(r, &s1.index, &[], None).unwrap();
        assert_eq!(s2.hashed_files, 2);
        assert_eq!(s2.root, s1.root);
        std::thread::sleep(std::time::Duration::from_millis(120));
        let s3 = scan(r, &s2.index, &[], None).unwrap();
        assert_eq!(s3.hashed_files, 2, "hashed within the racy window of the write");
        let s4 = scan(r, &s3.index, &[], None).unwrap();
        assert_eq!(s4.hashed_files, 0);
        assert!(!s4.index_changed);
        assert_eq!(s4.root, s1.root);
        // An edit changes the root and is the only thing read.
        write(r, "d/b.rs", "two!");
        let s5 = scan(r, &s4.index, &[], None).unwrap();
        assert_eq!(s5.hashed_files, 1);
        assert_ne!(s5.root, s1.root);
        // Same size, same content hash path: a removed file leaves the index.
        std::fs::remove_file(r.join("a.rs")).unwrap();
        let s6 = scan(r, &s5.index, &[], None).unwrap();
        assert!(s6.index_changed);
        assert_eq!(s6.index.files.len(), 1);
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
