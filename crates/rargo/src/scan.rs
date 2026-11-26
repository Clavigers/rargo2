use rargo_machine::StateMachine;
use rargo_net::io::file::PathRing as _;
use rargo_proto::Digest;
use rargo_proto::tree::{Node, Tree};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::{self, ErrorKind, Read};
use std::os::unix::ffi::OsStrExt;
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
    pub files: BTreeMap<Vec<u8>, Entry>,
}

fn index_from_bytes(raw: Option<&[u8]>) -> Index {
    let Some(raw) = raw else { return Index::default() };
    let Ok(plain) = zstd::stream::decode_all(raw) else { return Index::default() };
    bincode::deserialize(&plain).unwrap_or_default()
}

fn index_bytes(index: &Index) -> io::Result<Vec<u8>> {
    let plain = bincode::serialize(index).map_err(io::Error::other)?;
    zstd::stream::encode_all(&plain[..], 1)
}

impl Index {
    pub fn load(path: &Path) -> Index {
        index_from_bytes(rargo_net::io::file::read(path).ok().as_deref())
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        crate::write_atomic(path, &index_bytes(self)?)
    }
}

#[derive(Clone)]
pub struct Scan {
    pub root: Digest,
    pub trees: BTreeMap<Digest, Vec<u8>>,
    pub files: BTreeMap<Digest, (PathBuf, u64)>,
    refs: BTreeMap<Digest, u32>,
    grafted: Vec<Digest>,
    pub dirs: Vec<PathBuf>,
    pub dir_trees: BTreeMap<Vec<u8>, Digest>,
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
    trees: BTreeMap<Digest, Vec<u8>>,
    files: BTreeMap<Digest, (PathBuf, u64)>,
    refs: BTreeMap<Digest, u32>,
    dir_trees: BTreeMap<Vec<u8>, Digest>,
    index: BTreeMap<Vec<u8>, Entry>,
    relink: BTreeSet<Digest>,
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
        let mut f = rargo_net::io::file::File::open(path)?;
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
    paths: BTreeSet<Vec<u8>>,
    include: Vec<String>,
    on_way: BTreeSet<Vec<u8>>,
}

#[derive(Debug, PartialEq)]
enum Take {
    Skip,
    Keep,
    OnlyTheWay,
}

impl Ignored {
    pub fn from_git(root: &Path, include: &[String]) -> Option<Ignored> {
        let listed = crate::git::ignored(root)?;
        Some(Ignored::new(listed.iter().map(|p| p.as_slice()), include))
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
    on_path: Option<&'a BTreeSet<Vec<u8>>>,
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
    drive(ScanCore::whole(root, Some(old), exclude, ignored))
}

pub fn rescan(root: &Path, prev: Scan, changed: &BTreeSet<PathBuf>, exclude: &[String], ignored: Option<&Ignored>) -> io::Result<Scan> {
    drive(ScanCore::rescan(root, prev, changed, exclude, ignored))
}

pub fn scan_store(dir: &Path, old: &Index) -> io::Result<Scan> {
    drive(ScanCore::whole(dir, Some(old), &[], None))
}

pub fn graft_git(scan: &mut Scan, root: &Path, git: &Path, history: Option<Scan>) -> io::Result<()> {
    let fresh = Index::default();
    let sub = drive(ScanCore::whole(git, Some(&fresh), &[], None))?;
    let gitfile = if root.join(".git").ring_is_file() {
        let text = format!("gitdir: {}\n", rargo_proto::GIT_SIDE);
        let path = git.parent().unwrap_or(git).join("gitfile-v1");
        if rargo_net::io::file::read(&path).ok().as_deref() != Some(text.as_bytes()) {
            crate::write_atomic(&path, text.as_bytes())?;
        }
        Some((path, text))
    } else {
        None
    };
    graft_into(scan, sub, history, gitfile)
}

fn graft_into(scan: &mut Scan, mut sub: Scan, history: Option<Scan>, gitfile: Option<(PathBuf, String)>) -> io::Result<()> {
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
    if let Some((path, text)) = gitfile {
        let hash = *blake3::hash(text.as_bytes()).as_bytes();
        scan.files.entry(hash).or_insert((path, text.len() as u64));
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

fn merge(trees: &mut BTreeMap<Digest, Vec<u8>>, a: Digest, b: Digest) -> io::Result<Digest> {
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
        let live: BTreeSet<_> = state.dir_trees.values().collect();
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

enum Ask {
    List { rel: Vec<u8> },
    Meta { rel: Vec<u8> },
    Cache { rel: Vec<u8> },
    Link { rel: Vec<u8> },
    Content { rel: Vec<u8>, size: u64 },
}

enum Answer {
    List(io::Result<Vec<Vec<u8>>>),
    Meta(io::Result<Meta>),
    Cache(Option<Vec<u8>>),
    Link(Option<Vec<u8>>),
    Content { at_ns: i64, hash: io::Result<Digest> },
}

struct Meta {
    kind: Kind,
    size: u64,
    mtime_ns: i64,
    ctime_ns: i64,
    ino: u64,
    exec: bool,
}

enum Kind {
    Dir,
    Symlink,
    File,
    Other,
}

enum Step {
    Ask(Vec<Ask>),
    Done,
}

struct DirBuild {
    inside: bool,
    phase: Phase,
}

enum Phase {
    NeedList,
    Listing(Vec<Row>),
    Done(Directory),
    Folded,
    Gone,
}

enum Row {
    Meta { name: Vec<u8>, child: Vec<u8>, take: Take },
    Cache { name: Vec<u8>, child: Vec<u8>, inside: bool },
    Link { name: Vec<u8>, child: Vec<u8> },
    Content { name: Vec<u8>, child: Vec<u8>, stamp: Stamp, exec: bool },
    Ready { name: Vec<u8>, observed: Observed },
    Omit { name: Vec<u8> },
    Child { name: Vec<u8>, child: Vec<u8> },
}

struct Pending {
    rel: Vec<u8>,
    kind: Pend,
}

enum Pend {
    List,
    Row(usize),
}

struct ScanCore<'a> {
    root_path: &'a Path,
    old: Option<&'a Index>,
    previous: Option<Scan>,
    on_path: Option<BTreeSet<Vec<u8>>>,
    exclude: &'a [String],
    ignored: Option<&'a Ignored>,
    dirs: BTreeMap<Vec<u8>, DirBuild>,
    pending: Vec<Pending>,
    outcome: Option<Scan>,
}

impl<'a> ScanCore<'a> {
    fn whole(root_path: &'a Path, old: Option<&'a Index>, exclude: &'a [String], ignored: Option<&'a Ignored>) -> Self {
        Self::open(root_path, old, None, None, exclude, ignored)
    }

    fn rescan(root_path: &'a Path, previous: Scan, changed: &BTreeSet<PathBuf>, exclude: &'a [String], ignored: Option<&'a Ignored>) -> Self {
        let on_path = Some(changed_directories(root_path, changed));
        Self::open(root_path, None, Some(previous), on_path, exclude, ignored)
    }

    fn open(
        root_path: &'a Path,
        old: Option<&'a Index>,
        previous: Option<Scan>,
        on_path: Option<BTreeSet<Vec<u8>>>,
        exclude: &'a [String],
        ignored: Option<&'a Ignored>,
    ) -> Self {
        let mut dirs = BTreeMap::new();
        dirs.insert(Vec::new(), DirBuild { inside: false, phase: Phase::NeedList });
        Self { root_path, old, previous, on_path, exclude, ignored, dirs, pending: Vec::new(), outcome: None }
    }

    fn plan(&self) -> ScanPlan<'_> {
        ScanPlan {
            old: self.old,
            previous: self.previous.as_ref(),
            on_path: self.on_path.as_ref(),
            exclude: self.exclude,
            ignored: self.ignored,
        }
    }

    fn advance(&mut self, answers: Vec<Answer>) -> io::Result<Step> {
        self.apply(answers)?;
        self.fold();
        if self.root_done() {
            self.outcome = Some(self.finish()?);
            return Ok(Step::Done);
        }
        let asks = self.collect_asks();
        if asks.is_empty() {
            return Err(io::Error::other("the scan stopped before the tree was built"));
        }
        Ok(Step::Ask(asks))
    }

    fn apply(&mut self, answers: Vec<Answer>) -> io::Result<()> {
        if answers.len() != self.pending.len() {
            return Err(io::Error::other("a scan step answered the wrong number of requests"));
        }
        let pending = std::mem::take(&mut self.pending);
        for (pend, answer) in pending.into_iter().zip(answers) {
            self.apply_one(pend, answer)?;
        }
        Ok(())
    }

    fn apply_one(&mut self, pend: Pending, answer: Answer) -> io::Result<()> {
        match pend.kind {
            Pend::List => self.apply_list(pend.rel, answer),
            Pend::Row(index) => self.apply_row(&pend.rel, index, answer),
        }
    }

    fn apply_list(&mut self, rel: Vec<u8>, answer: Answer) -> io::Result<()> {
        let Answer::List(names) = answer else { return Err(io::Error::other("a scan answer does not match its request")) };
        let names = match names {
            Ok(names) => names,
            Err(e) if e.kind() == ErrorKind::NotFound && !rel.is_empty() => {
                if let Some(dir) = self.dirs.get_mut(&rel) {
                    dir.phase = Phase::Gone;
                }
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        let inside = self.dirs.get(&rel).ok_or_else(|| io::Error::other("a listing arrived for an unknown directory"))?.inside;
        if !matches!(self.dirs[&rel].phase, Phase::NeedList) {
            return Err(io::Error::other("a listing arrived twice"));
        }
        let mut names = names;
        names.sort();
        let mut rows = Vec::new();
        {
            let plan = self.plan();
            for name in names {
                if let Some((child, take)) = plan.take(&rel, &name, inside) {
                    rows.push(Row::Meta { name, child, take });
                }
            }
        }
        self.dirs.get_mut(&rel).unwrap().phase = Phase::Listing(rows);
        Ok(())
    }

    fn apply_row(&mut self, rel: &[u8], index: usize, answer: Answer) -> io::Result<()> {
        let row = {
            let dir = self.dirs.get_mut(rel).ok_or_else(|| io::Error::other("a scan answer names an unknown directory"))?;
            let Phase::Listing(rows) = &mut dir.phase else { return Err(io::Error::other("a scan answer arrived after its directory closed")) };
            if index >= rows.len() {
                return Err(io::Error::other("a scan answer names an unknown entry"));
            }
            std::mem::replace(&mut rows[index], Row::Omit { name: Vec::new() })
        };
        let next = self.reduce(row, answer)?;
        let Phase::Listing(rows) = &mut self.dirs.get_mut(rel).unwrap().phase else { unreachable!("the directory is still listing") };
        rows[index] = next;
        Ok(())
    }

    fn reduce(&mut self, row: Row, answer: Answer) -> io::Result<Row> {
        match (row, answer) {
            (Row::Meta { name, child, take }, Answer::Meta(meta)) => self.reduce_meta(name, child, take, meta),
            (Row::Cache { name, child, inside }, Answer::Cache(bytes)) => Ok(self.reduce_cache(name, child, inside, bytes.as_deref())),
            (Row::Link { name, .. }, Answer::Link(target)) => Ok(match target {
                Some(target) => Row::Ready { name, observed: Observed::Symlink(target) },
                None => Row::Omit { name },
            }),
            (Row::Content { name, child, stamp, exec }, Answer::Content { at_ns, hash }) => self.reduce_content(name, child, stamp, exec, at_ns, hash),
            _ => Err(io::Error::other("a scan answer does not match its request")),
        }
    }

    fn reduce_meta(&self, name: Vec<u8>, child: Vec<u8>, take: Take, meta: io::Result<Meta>) -> io::Result<Row> {
        let meta = match meta {
            Ok(meta) => meta,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Row::Omit { name }),
            Err(e) => return Err(e),
        };
        match meta.kind {
            Kind::Dir => Ok(Row::Cache { name, child, inside: take == Take::OnlyTheWay }),
            Kind::Symlink => Ok(Row::Link { name, child }),
            Kind::File if take != Take::OnlyTheWay => {
                let stamp = Stamp { size: meta.size, mtime_ns: meta.mtime_ns, ctime_ns: meta.ctime_ns, ino: meta.ino };
                if let Some(entry) = self.plan().reuse_file(&child, stamp) {
                    Ok(Row::Ready { name, observed: Observed::File { entry, exec: meta.exec, hashed: false } })
                } else {
                    Ok(Row::Content { name, child, stamp, exec: meta.exec })
                }
            }
            Kind::File | Kind::Other => Ok(Row::Omit { name }),
        }
    }

    fn reduce_cache(&mut self, name: Vec<u8>, child: Vec<u8>, inside: bool, bytes: Option<&[u8]>) -> Row {
        if bytes.is_some_and(|b| b == rargo_proto::CACHEDIR_SIGNATURE) {
            return Row::Omit { name };
        }
        if let Some(digest) = self.plan().reuse(&child) {
            return Row::Ready { name, observed: Observed::Directory(Directory::Reuse(digest)) };
        }
        self.dirs.insert(child.clone(), DirBuild { inside, phase: Phase::NeedList });
        Row::Child { name, child }
    }

    fn reduce_content(&self, name: Vec<u8>, child: Vec<u8>, stamp: Stamp, exec: bool, at_ns: i64, hash: io::Result<Digest>) -> io::Result<Row> {
        let hash = match hash {
            Ok(hash) => hash,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Row::Omit { name }),
            Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", abs(self.root_path, &child).display()))),
        };
        let entry = Entry { size: stamp.size, mtime_ns: stamp.mtime_ns, ctime_ns: stamp.ctime_ns, ino: stamp.ino, hash, hashed_at_ns: at_ns };
        Ok(Row::Ready { name, observed: Observed::File { entry, exec, hashed: true } })
    }

    fn fold(&mut self) {
        let rels: Vec<Vec<u8>> = self.dirs.keys().cloned().collect();
        for _ in 0..rels.len().saturating_add(1) {
            let mut progressed = false;
            for rel in &rels {
                if self.close_dir(rel) {
                    progressed = true;
                }
            }
            if !progressed {
                return;
            }
        }
        panic!("a scan fold did not finish");
    }

    fn close_dir(&mut self, rel: &[u8]) -> bool {
        let Phase::Listing(_) = &self.dirs[rel].phase else { return false };
        let ready: Vec<(usize, Vec<u8>, Vec<u8>, bool)> = {
            let Phase::Listing(rows) = &self.dirs[rel].phase else { return false };
            rows.iter().enumerate().filter_map(|(i, row)| {
                let Row::Child { name, child } = row else { return None };
                let done = match self.dirs.get(child).map(|dir| &dir.phase) {
                    Some(Phase::Done(_)) => true,
                    Some(Phase::Gone) => false,
                    _ => return None,
                };
                Some((i, name.clone(), child.clone(), done))
            }).collect()
        };
        let mut changed = false;
        for (index, name, child, done) in ready {
            let phase = std::mem::replace(&mut self.dirs.get_mut(&child).unwrap().phase, Phase::Folded);
            let directory = if done {
                let Phase::Done(directory) = phase else { unreachable!("the child was done") };
                Some(directory)
            } else {
                None
            };
            let Phase::Listing(rows) = &mut self.dirs.get_mut(rel).unwrap().phase else { unreachable!("the parent is listing") };
            rows[index] = match directory {
                Some(directory) => Row::Ready { name, observed: Observed::Directory(directory) },
                None => Row::Omit { name },
            };
            changed = true;
        }
        let finished = match &self.dirs[rel].phase {
            Phase::Listing(rows) => rows.iter().all(|row| matches!(row, Row::Ready { .. } | Row::Omit { .. })),
            _ => false,
        };
        if !finished {
            return changed;
        }
        let Phase::Listing(rows) = std::mem::replace(&mut self.dirs.get_mut(rel).unwrap().phase, Phase::Folded) else { unreachable!("the parent is listing") };
        let listing = rows.into_iter().filter_map(|row| match row {
            Row::Ready { name, observed } => Some((name, observed)),
            Row::Omit { .. } => None,
            Row::Meta { .. } | Row::Cache { .. } | Row::Link { .. } | Row::Content { .. } | Row::Child { .. } => None,
        }).collect();
        self.dirs.get_mut(rel).unwrap().phase = Phase::Done(Directory::Listing(listing));
        true
    }

    fn root_done(&self) -> bool {
        matches!(self.dirs.get(b"".as_slice()).map(|dir| &dir.phase), Some(Phase::Done(_)))
    }

    fn finish(&mut self) -> io::Result<Scan> {
        let Some(DirBuild { phase: Phase::Done(directory), .. }) = self.dirs.remove(b"".as_slice()) else {
            return Err(io::Error::other("the scan finished without a root"));
        };
        finish_scan(self.root_path, self.old, self.previous.take(), directory)
    }

    fn collect_asks(&mut self) -> Vec<Ask> {
        let mut asks = Vec::new();
        let mut pending = Vec::new();
        for (rel, dir) in &self.dirs {
            match &dir.phase {
                Phase::NeedList => {
                    asks.push(Ask::List { rel: rel.clone() });
                    pending.push(Pending { rel: rel.clone(), kind: Pend::List });
                }
                Phase::Listing(rows) => {
                    for (i, row) in rows.iter().enumerate() {
                        let ask = match row {
                            Row::Meta { child, .. } => Some(Ask::Meta { rel: child.clone() }),
                            Row::Cache { child, .. } => Some(Ask::Cache { rel: child.clone() }),
                            Row::Link { child, .. } => Some(Ask::Link { rel: child.clone() }),
                            Row::Content { child, stamp, .. } => Some(Ask::Content { rel: child.clone(), size: stamp.size }),
                            Row::Ready { .. } | Row::Omit { .. } | Row::Child { .. } => None,
                        };
                        if let Some(ask) = ask {
                            asks.push(ask);
                            pending.push(Pending { rel: rel.clone(), kind: Pend::Row(i) });
                        }
                    }
                }
                Phase::Done(_) | Phase::Folded | Phase::Gone => {}
            }
        }
        self.pending = pending;
        asks
    }

    fn pending_ok(&self, pend: &Pending) -> bool {
        let Some(dir) = self.dirs.get(&pend.rel) else { return false };
        match pend.kind {
            Pend::List => matches!(dir.phase, Phase::NeedList),
            Pend::Row(i) => match &dir.phase {
                Phase::Listing(rows) => rows.get(i).is_some_and(|row| {
                    matches!(row, Row::Meta { .. } | Row::Cache { .. } | Row::Link { .. } | Row::Content { .. })
                }),
                _ => false,
            },
        }
    }
}

impl<'a> StateMachine for ScanCore<'a> {
    fn invariants(&self) {
        assert!(
            self.on_path.as_ref().is_none_or(|paths| paths.contains(b"".as_slice())),
            "the directories on the path include the root"
        );
        assert!(self.pending.iter().all(|pend| self.pending_ok(pend)), "every pending look is one the scan can still use");
        for (rel, dir) in &self.dirs {
            let at = String::from_utf8_lossy(rel);
            assert!(rel_well_formed(rel), "{at:?} is a well-formed relative path");
            match &dir.phase {
                Phase::Listing(rows) => assert!(rows_ok(rel, rows, &self.dirs), "the rows listed for {at:?} agree with the directories"),
                Phase::Done(Directory::Listing(listing)) => {
                    assert!(names_increase(listing.iter().map(|(name, _)| name.as_slice())), "the listing of {at:?} is sorted")
                }
                Phase::Done(Directory::Reuse(_)) | Phase::NeedList | Phase::Folded | Phase::Gone => {}
            }
        }
        if self.outcome.is_none() && self.dirs.values().any(|dir| matches!(dir.phase, Phase::Done(_))) {
            assert!(self.root_done(), "once a directory is done the root is done, until the scan finishes");
        }
        if let Some(prev) = &self.previous {
            assert!(
                prev.dir_trees.iter().all(|(rel, hash)| rel.is_empty() || prev.trees.contains_key(hash)),
                "every directory tree of the previous scan is one of its trees"
            );
        }
        assert!(self.outcome.as_ref().is_none_or(|scan| scan_holds(scan, self.root_path)), "the finished scan holds");
    }
}

fn changed_directories(root: &Path, changed: &BTreeSet<PathBuf>) -> BTreeSet<Vec<u8>> {
    let mut on_path = BTreeSet::from([Vec::new()]);
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
    on_path
}

fn drive(mut core: ScanCore) -> io::Result<Scan> {
    let mut answers = Vec::new();
    loop {
        match core.advance(std::mem::take(&mut answers))? {
            Step::Ask(asks) => {
                core.invariants();
                answers = read_host(core.root_path, &asks);
            }
            Step::Done => {
                core.invariants();
                return Ok(core.outcome.take().expect("a finished scan is present"));
            }
        }
    }
}

fn read_host(root: &Path, asks: &[Ask]) -> Vec<Answer> {
    asks.par_iter().map(|ask| host_one(root, ask)).collect()
}

fn host_one(root: &Path, ask: &Ask) -> Answer {
    match ask {
        Ask::List { rel } => Answer::List(list_dir(&abs(root, rel))),
        Ask::Meta { rel } => Answer::Meta(match rargo_net::io::file::symlink_metadata(abs(root, rel)) {
            Ok(md) => Ok(meta_of(&md)),
            Err(e) => Err(e),
        }),
        Ask::Cache { rel } => Answer::Cache(cache_tag(&abs(root, rel))),
        Ask::Link { rel } => Answer::Link(rargo_net::io::file::read_link(abs(root, rel)).ok().map(|target| target.as_os_str().as_bytes().to_vec())),
        Ask::Content { rel, size } => Answer::Content { at_ns: now_ns(), hash: hash_file(&abs(root, rel), *size) },
    }
}

fn list_dir(abs_path: &Path) -> io::Result<Vec<Vec<u8>>> {
    let mut names = Vec::new();
    for entry in rargo_net::io::file::read_dir(abs_path)? {
        names.push(entry?.file_name().as_bytes().to_vec());
    }
    Ok(names)
}

fn meta_of(md: &rargo_net::io::file::Stat) -> Meta {
    let kind = if md.is_symlink() {
        Kind::Symlink
    } else if md.is_dir() {
        Kind::Dir
    } else if md.is_file() {
        Kind::File
    } else {
        Kind::Other
    };
    Meta {
        kind,
        size: md.len(),
        mtime_ns: md.mtime() * 1_000_000_000 + md.mtime_nsec(),
        ctime_ns: md.ctime() * 1_000_000_000 + md.ctime_nsec(),
        ino: md.ino(),
        exec: md.mode() & 0o111 != 0,
    }
}

fn cache_tag(dir: &Path) -> Option<Vec<u8>> {
    let mut head = vec![0u8; rargo_proto::CACHEDIR_SIGNATURE.len()];
    let mut file = rargo_net::io::file::File::open(dir.join("CACHEDIR.TAG")).ok()?;
    file.read_exact(&mut head).ok()?;
    Some(head)
}

fn abs(root: &Path, rel: &[u8]) -> PathBuf {
    if rel.is_empty() { root.to_path_buf() } else { root.join(OsStr::from_bytes(rel)) }
}

fn rel_well_formed(rel: &[u8]) -> bool {
    rel.is_empty() || rel.split(|b| *b == b'/').all(|part| !part.is_empty() && part != b"." && part != b"..")
}

fn child_matches(parent: &[u8], name: &[u8], child: &[u8]) -> bool {
    if parent.is_empty() {
        child == name
    } else {
        child.len() == parent.len() + 1 + name.len() && &child[..parent.len()] == parent && child[parent.len()] == b'/' && &child[parent.len() + 1..] == name
    }
}

fn row_name(row: &Row) -> &[u8] {
    match row {
        Row::Meta { name, .. } | Row::Cache { name, .. } | Row::Link { name, .. } | Row::Content { name, .. }
        | Row::Ready { name, .. } | Row::Omit { name, .. } | Row::Child { name, .. } => name,
    }
}

fn row_child(row: &Row) -> Option<&[u8]> {
    match row {
        Row::Meta { child, .. } | Row::Cache { child, .. } | Row::Link { child, .. } | Row::Content { child, .. } | Row::Child { child, .. } => Some(child),
        Row::Ready { .. } | Row::Omit { .. } => None,
    }
}

fn names_increase<'b>(names: impl Iterator<Item = &'b [u8]>) -> bool {
    let mut prev: Option<&[u8]> = None;
    for name in names {
        if name.is_empty() || name.contains(&b'/') {
            return false;
        }
        if prev.is_some_and(|prev| prev >= name) {
            return false;
        }
        prev = Some(name);
    }
    true
}

fn rows_ok(parent: &[u8], rows: &[Row], dirs: &BTreeMap<Vec<u8>, DirBuild>) -> bool {
    if !names_increase(rows.iter().map(row_name)) {
        return false;
    }
    rows.iter().all(|row| {
        let child_ok = row_child(row).is_none_or(|child| child_matches(parent, row_name(row), child));
        let link_ok = match row {
            Row::Child { child, .. } => matches!(dirs.get(child).map(|dir| &dir.phase), Some(Phase::NeedList | Phase::Listing(_))),
            _ => true,
        };
        child_ok && link_ok
    })
}

fn scan_holds(scan: &Scan, root: &Path) -> bool {
    scan.dir_trees.get(b"".as_slice()) == Some(&scan.root)
        && scan.dir_trees.values().all(|hash| scan.trees.contains_key(hash))
        && scan.refs.values().map(|&n| u64::from(n)).sum::<u64>() == scan.file_count
        && scan.files.iter().all(|(hash, (path, size))| {
            path.strip_prefix(root).ok().and_then(|rel| scan.index.files.get(rel.as_os_str().as_bytes())).is_some_and(|entry| &entry.hash == hash && &entry.size == size)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use expect_test::expect;
    use std::collections::BTreeSet;
    use std::io::ErrorKind;

    const SETTLED: i64 = 1_000_000_000;

    enum Mem {
        Dir(BTreeMap<Vec<u8>, Mem>),
        File { bytes: Vec<u8>, mtime_ns: i64, ctime_ns: i64, ino: u64, exec: bool },
        Link(Vec<u8>),
        Other,
        Vanish,
        Race,
        Blocked { size: u64, mtime_ns: i64, ctime_ns: i64, ino: u64 },
    }

    struct Model {
        root: Mem,
        at_ns: i64,
    }

    fn dir() -> Mem {
        Mem::Dir(BTreeMap::new())
    }

    fn file(bytes: &str, ino: u64) -> Mem {
        Mem::File { bytes: bytes.as_bytes().to_vec(), mtime_ns: 0, ctime_ns: 0, ino, exec: false }
    }

    fn put(node: &mut Mem, rel: &str, child: Mem) {
        let Some((name, rest)) = rel.split_once('/') else {
            let Mem::Dir(map) = node else { panic!("a file is not a directory") };
            map.insert(rel.as_bytes().to_vec(), child);
            return;
        };
        let Mem::Dir(map) = node else { panic!("a file is not a directory") };
        let next = map.entry(name.as_bytes().to_vec()).or_insert_with(dir);
        put(next, rest, child);
    }

    fn drop_at(node: &mut Mem, rel: &str) {
        let Some((name, rest)) = rel.split_once('/') else {
            let Mem::Dir(map) = node else { panic!("a file is not a directory") };
            map.remove(rel.as_bytes());
            return;
        };
        let Mem::Dir(map) = node else { panic!("a file is not a directory") };
        if let Some(next) = map.get_mut(name.as_bytes()) {
            drop_at(next, rest);
        }
    }

    fn get<'a>(node: &'a Mem, rel: &[u8]) -> Option<&'a Mem> {
        if rel.is_empty() {
            return Some(node);
        }
        let mut cur = node;
        for part in rel.split(|b| *b == b'/') {
            match cur {
                Mem::Dir(map) => cur = map.get(part)?,
                _ => return None,
            }
        }
        Some(cur)
    }

    fn mem_mut<'a>(node: &'a mut Mem, rel: &str) -> &'a mut Mem {
        let Some((name, rest)) = rel.split_once('/') else {
            let Mem::Dir(map) = node else { panic!("a file is not a directory") };
            return map.get_mut(rel.as_bytes()).unwrap();
        };
        let Mem::Dir(map) = node else { panic!("a file is not a directory") };
        mem_mut(map.get_mut(name.as_bytes()).unwrap(), rest)
    }

    fn file_bytes<'a>(node: &'a Mem, rel: &[u8]) -> &'a [u8] {
        match get(node, rel) {
            Some(Mem::File { bytes, .. }) => bytes,
            _ => panic!("no file at {}", String::from_utf8_lossy(rel)),
        }
    }

    impl Model {
        fn answer(&self, asks: &[Ask]) -> Vec<Answer> {
            asks.iter().map(|ask| self.one(ask)).collect()
        }

        fn one(&self, ask: &Ask) -> Answer {
            match ask {
                Ask::List { rel } => Answer::List(self.list(rel)),
                Ask::Meta { rel } => Answer::Meta(self.meta(rel)),
                Ask::Cache { rel } => Answer::Cache(self.cache(rel)),
                Ask::Link { rel } => Answer::Link(self.link(rel)),
                Ask::Content { rel, .. } => self.content(rel),
            }
        }

        fn list(&self, rel: &[u8]) -> io::Result<Vec<Vec<u8>>> {
            match get(&self.root, rel) {
                Some(Mem::Dir(map)) => {
                    let mut names: Vec<_> = map.keys().cloned().collect();
                    names.reverse();
                    Ok(names)
                }
                _ => Err(io::Error::new(ErrorKind::NotFound, "missing")),
            }
        }

        fn meta(&self, rel: &[u8]) -> io::Result<Meta> {
            let Some(node) = get(&self.root, rel) else { return Err(io::Error::new(ErrorKind::NotFound, "missing")) };
            let meta = |kind, size, mtime_ns, ctime_ns, ino, exec| Meta { kind, size, mtime_ns, ctime_ns, ino, exec };
            Ok(match node {
                Mem::Dir(_) | Mem::Vanish => meta(Kind::Dir, 0, 0, 0, 0, false),
                Mem::File { bytes, mtime_ns, ctime_ns, ino, exec } => meta(Kind::File, bytes.len() as u64, *mtime_ns, *ctime_ns, *ino, *exec),
                Mem::Link(_) => meta(Kind::Symlink, 0, 0, 0, 0, false),
                Mem::Other => meta(Kind::Other, 0, 0, 0, 0, false),
                Mem::Race => meta(Kind::File, 1, 0, 0, 0, false),
                Mem::Blocked { size, mtime_ns, ctime_ns, ino } => meta(Kind::File, *size, *mtime_ns, *ctime_ns, *ino, false),
            })
        }

        fn cache(&self, rel: &[u8]) -> Option<Vec<u8>> {
            let path = join(rel, b"CACHEDIR.TAG");
            match get(&self.root, &path) {
                Some(Mem::File { bytes, .. }) if bytes.len() >= rargo_proto::CACHEDIR_SIGNATURE.len() => {
                    Some(bytes[..rargo_proto::CACHEDIR_SIGNATURE.len()].to_vec())
                }
                _ => None,
            }
        }

        fn link(&self, rel: &[u8]) -> Option<Vec<u8>> {
            match get(&self.root, rel) {
                Some(Mem::Link(target)) => Some(target.clone()),
                _ => None,
            }
        }

        fn content(&self, rel: &[u8]) -> Answer {
            let hash = match get(&self.root, rel) {
                Some(Mem::File { bytes, .. }) => Ok(*blake3::hash(bytes).as_bytes()),
                None | Some(Mem::Race) => Err(io::Error::new(ErrorKind::NotFound, "missing")),
                Some(Mem::Blocked { .. }) => Err(io::Error::new(ErrorKind::PermissionDenied, "unreadable")),
                Some(_) => Err(io::Error::other("not a file")),
            };
            Answer::Content { at_ns: self.at_ns, hash }
        }
    }

    fn run(core: &mut ScanCore, model: &Model) -> io::Result<Scan> {
        rargo_machine::check(&core, "invariants before the first step");
        let mut answers = Vec::new();
        let mut steps = 0u32;
        loop {
            let step = core.advance(std::mem::take(&mut answers))?;
            steps += 1;
            rargo_machine::check(&core, format_args!("invariants after step {steps}"));
            match step {
                Step::Ask(asks) => answers = model.answer(&asks),
                Step::Done => return Ok(core.outcome.take().expect("a finished scan")),
            }
        }
    }

    fn scan_of(root: &Path, model: &Model, old: &Index, exclude: &[String], ignored: Option<&Ignored>) -> Scan {
        run(&mut ScanCore::whole(root, Some(old), exclude, ignored), model).unwrap()
    }

    fn rescan_of(root: &Path, model: &Model, prev: Scan, changed: &BTreeSet<PathBuf>, exclude: &[String], ignored: Option<&Ignored>) -> Scan {
        run(&mut ScanCore::rescan(root, prev, changed, exclude, ignored), model).unwrap()
    }

    fn tree_of(scan: &Scan, digest: &Digest) -> Tree {
        Tree::decode(&scan.trees[digest]).unwrap()
    }

    fn names(scan: &Scan, digest: &Digest) -> Vec<String> {
        tree_of(scan, digest).entries.iter().map(|(name, _)| String::from_utf8(name.clone()).unwrap()).collect()
    }

    fn sub(scan: &Scan, digest: &Digest, name: &str) -> Digest {
        match tree_of(scan, digest).get(name.as_bytes()) {
            Some(Node::Dir(digest)) => *digest,
            other => panic!("{name}: {other:?}"),
        }
    }

    fn rels(scan: &Scan, root: &Path) -> BTreeSet<Vec<u8>> {
        scan.dirs.iter().map(|dir| dir.strip_prefix(root).unwrap().as_os_str().as_bytes().to_vec()).collect()
    }

    fn rel_of(root: &Path, path: &Path) -> String {
        String::from_utf8(path.strip_prefix(root).unwrap().as_os_str().as_bytes().to_vec()).unwrap()
    }

    fn same(a: &Scan, b: &Scan, root: &Path, model: &Mem) {
        assert_eq!(a.root, b.root);
        assert_eq!(a.dirs, b.dirs);
        assert_eq!(a.dir_trees, b.dir_trees);
        assert_eq!(a.file_count, b.file_count);
        assert_eq!(a.refs, b.refs);
        assert_eq!(a.index.files.len(), b.index.files.len());
        for (path, entry) in &a.index.files {
            let other = &b.index.files[path];
            assert_eq!((entry.hash, entry.size, entry.mtime_ns, entry.ctime_ns, entry.ino), (other.hash, other.size, other.mtime_ns, other.ctime_ns, other.ino));
        }
        assert_eq!(a.files.len(), b.files.len());
        for (hash, (path, size)) in &a.files {
            let bytes = file_bytes(model, path.strip_prefix(root).unwrap().as_os_str().as_bytes());
            assert_eq!(*size, bytes.len() as u64);
            assert_eq!(*hash, *blake3::hash(bytes).as_bytes());
            assert_eq!(b.files.get(hash), Some(&(path.clone(), *size)));
        }
        assert!(a.dir_trees.values().all(|digest| a.trees.contains_key(digest)));
    }

    /// Runs a scan to its end, answering from the model, and lists what it asked at each step and what it found.
    fn transcript(core: &mut ScanCore, model: &Model) -> String {
        let text = |b: &[u8]| format!("{:?}", String::from_utf8_lossy(b));
        let mut out = String::new();
        let mut answers = Vec::new();
        for step in 1.. {
            let next = core.advance(std::mem::take(&mut answers)).unwrap();
            rargo_machine::check(&core, format_args!("invariants after step {step}"));
            let Step::Ask(asks) = next else { break };
            let asked: Vec<String> = asks
                .iter()
                .map(|ask| match ask {
                    Ask::List { rel } => format!("list {}", text(rel)),
                    Ask::Meta { rel } => format!("meta {}", text(rel)),
                    Ask::Cache { rel } => format!("cache tag {}", text(rel)),
                    Ask::Link { rel } => format!("link {}", text(rel)),
                    Ask::Content { rel, size } => format!("content {} ({size} bytes)", text(rel)),
                })
                .collect();
            out += &format!("step {step}: {}\n", asked.join(", "));
            answers = model.answer(&asks);
        }
        let scan = core.outcome.take().expect("a finished scan");
        let mut files: Vec<String> =
            scan.index.files.iter().map(|(path, entry)| format!("{}:{}", text(path), entry.size)).collect();
        files.sort();
        out += &format!(
            "listed {} directories, hashed {} files, index changed: {}; files {}\n",
            scan.listed_dirs,
            scan.hashed_files,
            scan.index_changed,
            files.join(" ")
        );
        core.outcome = Some(scan);
        out
    }

    #[test]
    fn a_scan_asks_for_each_directory_then_its_entries_and_a_rescan_only_for_what_it_is_told() {
        let root = Path::new("/mem");
        let mut tree = dir();
        put(&mut tree, "Cargo.toml", file("[package]", 1));
        put(&mut tree, "src/lib.rs", file("pub fn a() {}", 2));
        put(&mut tree, "src/bin/b.rs", file("fn main() {}", 3));
        put(&mut tree, "target/debug/out", file("o", 4));
        put(&mut tree, "link", Mem::Link(b"src".to_vec()));
        let mut model = Model { root: tree, at_ns: SETTLED };
        let mut whole = ScanCore::whole(root, Some(&Index::default()), &[], None);
        expect![[r#""#]].assert_eq(&transcript(&mut whole, &model));

        let first = whole.outcome.take().unwrap();
        put(&mut model.root, "src/lib.rs", Mem::File { bytes: b"pub fn b() {}".to_vec(), mtime_ns: SETTLED, ctime_ns: SETTLED, ino: 2, exec: false });
        let told: BTreeSet<PathBuf> = [root.join("src")].into_iter().collect();
        let mut rescan = ScanCore::rescan(root, first, &told, &[], None);
        expect![[r#""#]].assert_eq(&transcript(&mut rescan, &model));
    }

    #[test]
    fn rescan_reads_only_the_directories_it_is_told_about() {
        let root = Path::new("/mem");
        let exclude = vec!["node_modules".to_string()];
        let mut tree = dir();
        put(&mut tree, "Cargo.toml", file("[package]", 1));
        put(&mut tree, "src/lib.rs", file("pub fn a() {}", 2));
        put(&mut tree, "src/deep/x.rs", file("x", 3));
        put(&mut tree, "src/deep/same.rs", file("same", 4));
        put(&mut tree, "docs/same.md", file("same", 5));
        put(&mut tree, "docs/a.md", file("a", 6));
        put(&mut tree, "docs/old/b.md", file("b", 7));
        put(&mut tree, "target/debug/out", file("o", 8));
        put(&mut tree, "node_modules/m/i.js", file("m", 9));
        let mut model = Model { root: tree, at_ns: SETTLED };
        let whole = |model: &Model| scan_of(root, model, &Index::default(), &exclude, None);

        let s0 = whole(&model);
        assert_eq!(rels(&s0, root), BTreeSet::from([b"".to_vec(), b"src".to_vec(), b"src/deep".to_vec(), b"docs".to_vec(), b"docs/old".to_vec()]));
        assert_eq!(s0.listed_dirs, 5);

        drop_at(&mut model.root, "src/deep/x.rs");
        put(&mut model.root, "src/deep/x.rs", Mem::File { bytes: b"changed".to_vec(), mtime_ns: SETTLED, ctime_ns: SETTLED, ino: 3, exec: false });
        put(&mut model.root, "src/deep/new/y.rs", file("y", 10));
        drop_at(&mut model.root, "docs/old");
        let told: BTreeSet<PathBuf> = [root.join("src/deep"), root.join("docs"), root.join("docs/old")].into_iter().collect();
        let s1 = rescan_of(root, &model, s0, &told, &exclude, None);
        same(&s1, &whole(&model), root, &model.root);
        assert_eq!(s1.listed_dirs, 5);
        assert_eq!(s1.hashed_files, 2, "only the changed and the new file are read");
        assert!(s1.index_changed);

        let Mem::File { bytes, mtime_ns, ctime_ns, .. } = mem_mut(&mut model.root, "docs/a.md") else { panic!("a.md") };
        *bytes = b"unseen".to_vec();
        *mtime_ns = SETTLED;
        *ctime_ns = SETTLED;
        let s2 = rescan_of(root, &model, s1.clone(), &[root.join("src")].into_iter().collect(), &exclude, None);
        assert_eq!(s2.root, s1.root);
        assert_eq!(s2.listed_dirs, 2, "the root and src; src/deep and src/deep/new stay as they were");
        assert!(!s2.index_changed);
        let s3 = rescan_of(root, &model, s2, &[root.join("docs")].into_iter().collect(), &exclude, None);
        same(&s3, &whole(&model), root, &model.root);

        let shared = *blake3::hash(b"same").as_bytes();
        let recorded = s3.files[&shared].0.clone();
        drop_at(&mut model.root, &rel_of(root, &recorded));
        let s4 = rescan_of(root, &model, s3, &[recorded.parent().unwrap().to_path_buf()].into_iter().collect(), &exclude, None);
        same(&s4, &whole(&model), root, &model.root);
        assert_ne!(s4.files[&shared].0, recorded);
        let other = s4.files[&shared].0.clone();
        drop_at(&mut model.root, &rel_of(root, &other));
        let s5 = rescan_of(root, &model, s4, &[other.parent().unwrap().to_path_buf()].into_iter().collect(), &exclude, None);
        same(&s5, &whole(&model), root, &model.root);
        assert!(!s5.files.contains_key(&shared));

        let s6 = rescan_of(root, &model, s5.clone(), &BTreeSet::new(), &exclude, None);
        same(&s6, &s5, root, &model.root);
        assert_eq!(s6.listed_dirs, 1);
        let outside: BTreeSet<PathBuf> = [PathBuf::from("/elsewhere")].into_iter().collect();
        let skipped = rescan_of(root, &model, s6.clone(), &outside, &exclude, None);
        assert_eq!(skipped.root, s6.root);
        assert_eq!(skipped.listed_dirs, 1);

        let mut git = dir();
        put(&mut git, "HEAD", file("ref: refs/heads/main", 1));
        let git_scan = scan_of(Path::new("/git"), &Model { root: git, at_ns: SETTLED }, &Index::default(), &[], None);
        let mut grafted = s6.clone();
        graft_into(&mut grafted, git_scan, None, None).unwrap();
        assert_ne!(grafted.root, s6.root);
        same(&rescan_of(root, &model, grafted, &BTreeSet::new(), &exclude, None), &s6, root, &model.root);
    }

    #[test]
    fn ignore_rules_keep_included_paths_and_record_symlinks_on_the_way() {
        let root = Path::new("/mem");
        let mut tree = dir();
        put(&mut tree, ".gitignore", file("node_modules/\n*.log\ndata/\n.env\n", 1));
        put(&mut tree, "src/lib.rs", file("pub fn a() {}", 2));
        put(&mut tree, "node_modules/a/b.js", file("x", 3));
        put(&mut tree, "x.log", file("x", 4));
        put(&mut tree, "src/y.log", file("x", 5));
        put(&mut tree, "kept.log", file("tracked although it matches", 6));
        put(&mut tree, "data/big.bin", file("x", 7));
        put(&mut tree, "data/schema.sql", file("create table t;", 8));
        put(&mut tree, "data/sub/z", file("x", 9));
        put(&mut tree, "data/noise.txt", file("x", 10));
        put(&mut tree, ".env", file("A=1", 11));
        put(&mut tree, "linkdir", Mem::Link(b"inner".to_vec()));
        put(&mut tree, "wayfile", file("not descended", 12));
        let model = Model { root: tree, at_ns: SETTLED };
        let listed = ["node_modules", "x.log", "src/y.log", "data", ".env", "linkdir", "wayfile"];
        let open = scan_of(root, &model, &Index::default(), &[], None);
        assert!(names(&open, &open.root).contains(&"node_modules".to_string()));

        let include = ["data/schema.sql".to_string(), ".env".to_string(), "linkdir/kept".to_string(), "wayfile/nope".to_string()];
        let ignored = Ignored::for_test(&listed, &include);
        let scan = scan_of(root, &model, &Index::default(), &[], Some(&ignored));
        assert_eq!(names(&scan, &scan.root), [".env", ".gitignore", "data", "kept.log", "linkdir", "src"]);
        assert_eq!(tree_of(&scan, &scan.root).get(b"linkdir"), Some(&Node::Symlink(b"inner".to_vec())));
        assert_eq!(names(&scan, &sub(&scan, &scan.root, "src")), ["lib.rs"]);
        assert_eq!(names(&scan, &sub(&scan, &scan.root, "data")), ["schema.sql"], "only the way to the included file");
        assert!(!names(&scan, &scan.root).iter().any(|name| name == "wayfile" || name == "node_modules" || name == "x.log"));
        assert!(!scan.dirs.iter().any(|dir| dir.ends_with("node_modules") || dir.ends_with("data/sub")), "not looked into, so not watched");
        for (path, out) in [
            ("node_modules/a/b.js", true),
            ("x.log", true),
            ("src/y.log", true),
            ("data/big.bin", true),
            ("data/sub/z", true),
            ("data/noise.txt", true),
            ("data/schema.sql", false),
            (".env", false),
            ("kept.log", false),
            ("src/lib.rs", false),
        ] {
            assert_eq!(ignored.left_out(path.as_bytes()), out, "{path}");
        }
        assert!(!ignored.left_out(b"data"), "data/ is not left out whole");

        let ignored = Ignored::for_test(&listed, &[]);
        let scan = scan_of(root, &model, &Index::default(), &[], Some(&ignored));
        assert_eq!(names(&scan, &scan.root), [".gitignore", "kept.log", "src"]);
    }

    #[test]
    fn skips_caches_excludes_and_non_files() {
        let root = Path::new("/tree");
        let mut tree = dir();
        put(&mut tree, "Cargo.toml", file("[package]", 1));
        put(&mut tree, "src/lib.rs", file("pub fn a() {}", 2));
        put(&mut tree, ".git/HEAD", file("ref: refs/heads/main", 3));
        put(&mut tree, "target/debug/app", file("binary", 4));
        put(&mut tree, "sub/out/CACHEDIR.TAG", Mem::File { bytes: rargo_proto::CACHEDIR_TAG.as_bytes().to_vec(), mtime_ns: 0, ctime_ns: 0, ino: 5, exec: false });
        put(&mut tree, "sub/out/debug/x", file("binary", 6));
        put(&mut tree, "sub/Cargo.lock", file("ignored by git, synced anyway", 7));
        put(&mut tree, "node_modules/big/file", file("junk", 8));
        put(&mut tree, "docs/node_modules/x", file("junk", 9));
        put(&mut tree, "vendor/skip/me", file("junk", 10));
        put(&mut tree, "vendor/keep/me", Mem::File { bytes: b"kept".to_vec(), mtime_ns: 0, ctime_ns: 0, ino: 11, exec: true });
        put(&mut tree, "cache/x", file("at the root: excluded", 12));
        put(&mut tree, "docs/cache/y", file("deeper: kept", 13));
        put(&mut tree, "link", Mem::Link(b"src".to_vec()));
        put(&mut tree, "empty", dir());
        put(&mut tree, "weird", Mem::Other);
        put(&mut tree, "ghost", Mem::Vanish);
        put(&mut tree, "ghostfile", Mem::Race);
        put(&mut tree, "noted/CACHEDIR.TAG", file("nope", 14));
        put(&mut tree, "noted/a", file("a", 15));
        let model = Model { root: tree, at_ns: SETTLED };
        let exclude = vec!["node_modules".to_string(), "vendor/skip".to_string(), "/cache".to_string()];
        let scan = scan_of(root, &model, &Index::default(), &exclude, None);
        let top = tree_of(&scan, &scan.root);
        assert_eq!(names(&scan, &scan.root), ["Cargo.toml", "docs", "empty", "link", "noted", "src", "sub", "vendor"]);
        assert_eq!(top.get(b"link"), Some(&Node::Symlink(b"src".to_vec())));
        assert_eq!(top.get(b"weird"), None);
        assert_eq!(top.get(b"ghost"), None);
        assert_eq!(top.get(b"ghostfile"), None);
        assert_eq!(tree_of(&scan, &sub(&scan, &scan.root, "sub")).entries.len(), 1);
        assert_eq!(tree_of(&scan, &sub(&scan, &scan.root, "docs")).entries.len(), 1, "only docs/cache");
        assert_eq!(tree_of(&scan, &sub(&scan, &scan.root, "vendor")).entries.len(), 1);
        assert_eq!(names(&scan, &sub(&scan, &scan.root, "noted")), ["CACHEDIR.TAG", "a"]);
        assert!(tree_of(&scan, &sub(&scan, &scan.root, "empty")).entries.is_empty());
        let keep = sub(&scan, &sub(&scan, &scan.root, "vendor"), "keep");
        assert!(matches!(tree_of(&scan, &keep).get(b"me"), Some(Node::File { exec: true, .. })));
        assert_eq!(scan.file_count, 7);
        assert_eq!(scan.hashed_files, 7);
        assert!(scan.index_changed);
        let lib = *blake3::hash(b"pub fn a() {}").as_bytes();
        assert_eq!(scan.files[&lib], (root.join("src/lib.rs"), 13));
        let kept = *blake3::hash(b"kept").as_bytes();
        assert_eq!(scan.files[&kept], (root.join("vendor/keep/me"), 4));
    }

    #[test]
    fn graft_edits_the_tree_without_reading_the_worktree() {
        let root = Path::new("/wt");
        let stand = Path::new("/stand");
        let store = Path::new("/store");
        let mut work = dir();
        put(&mut work, "src/lib.rs", file("pub fn a() {}", 1));
        let work = Model { root: work, at_ns: SETTLED };
        let base = scan_of(root, &work, &Index::default(), &[], None);

        let mut head = dir();
        put(&mut head, "HEAD", file("ref: refs/heads/main\n", 1));
        let head_scan = scan_of(stand, &Model { root: head, at_ns: SETTLED }, &Index::default(), &[], None);
        let mut normal = base.clone();
        graft_into(&mut normal, head_scan, None, None).unwrap();
        let top = tree_of(&normal, &normal.root);
        assert!(matches!(top.get(b".git"), Some(Node::Dir(_))));
        assert!(top.get(rargo_proto::GIT_SIDE.as_bytes()).is_none());
        assert_eq!(top.entries[0].0, b".git");
        let git = sub(&normal, &normal.root, ".git");
        assert_eq!(tree_of(&normal, &git).entries.len(), 1);
        let head_hash = *blake3::hash(b"ref: refs/heads/main\n").as_bytes();
        assert_eq!(normal.files[&head_hash].0, stand.join("HEAD"));

        let mut stand_in = dir();
        put(&mut stand_in, "HEAD", file("ref: refs/heads/main\n", 1));
        put(&mut stand_in, "objects/pack/pack-1.pack", file("pack", 2));
        let mut history = dir();
        put(&mut history, "pack/pack-2.pack", file("history", 3));
        let sub_scan = scan_of(stand, &Model { root: stand_in, at_ns: SETTLED }, &Index::default(), &[], None);
        let history_scan = scan_of(store, &Model { root: history, at_ns: SETTLED }, &Index::default(), &[], None);
        let mut merged = base.clone();
        graft_into(&mut merged, sub_scan, Some(history_scan), None).unwrap();
        let git = sub(&merged, &merged.root, ".git");
        let objects = sub(&merged, &git, "objects");
        let pack = sub(&merged, &objects, "pack");
        let packs = tree_of(&merged, &pack);
        assert!(packs.get(b"pack-1.pack").is_some() && packs.get(b"pack-2.pack").is_some(), "both packs side by side");
        let Some(Node::File { hash, .. }) = packs.get(b"pack-2.pack").cloned() else { panic!("pack-2") };
        assert_eq!(merged.files[&hash].0, store.join("pack/pack-2.pack"), "sent from the store");
        assert_eq!(hash, *blake3::hash(b"history").as_bytes());

        let mut bare = dir();
        put(&mut bare, "HEAD", file("ref: refs/heads/main\n", 1));
        let bare_scan = scan_of(stand, &Model { root: bare, at_ns: SETTLED }, &Index::default(), &[], None);
        let mut history = dir();
        put(&mut history, "pack/pack-2.pack", file("history", 3));
        let history_scan = scan_of(store, &Model { root: history, at_ns: SETTLED }, &Index::default(), &[], None);
        let before = base.root;
        let mut unchanged = base.clone();
        let err = graft_into(&mut unchanged, bare_scan, Some(history_scan), None).unwrap_err();
        assert_eq!(err.to_string(), "the git stand-in has no objects directory");
        assert_eq!(unchanged.root, before);

        let mut side = dir();
        put(&mut side, "HEAD", file("ref: refs/heads/main\n", 1));
        let side_scan = scan_of(stand, &Model { root: side, at_ns: SETTLED }, &Index::default(), &[], None);
        let text = format!("gitdir: {}\n", rargo_proto::GIT_SIDE);
        let gitfile = PathBuf::from("/stand/gitfile-v1");
        let mut linked = base;
        graft_into(&mut linked, side_scan, None, Some((gitfile.clone(), text.clone()))).unwrap();
        let top = tree_of(&linked, &linked.root);
        let Some(Node::File { hash, size, exec }) = top.get(b".git").cloned() else { panic!("`.git` is not a file") };
        assert!(!exec);
        assert_eq!(size, text.len() as u64);
        assert_eq!(hash, *blake3::hash(text.as_bytes()).as_bytes());
        assert_eq!(linked.files[&hash], (gitfile, text.len() as u64));
        let side = sub(&linked, &linked.root, rargo_proto::GIT_SIDE);
        assert!(tree_of(&linked, &side).get(b"HEAD").is_some());
    }

    #[test]
    fn mtime_size_and_ino_decide_reuse() {
        let root = Path::new("/mem");
        let mut tree = dir();
        put(&mut tree, "hello", Mem::File { bytes: b"hello".to_vec(), mtime_ns: 1, ctime_ns: 2, ino: 3, exec: false });
        let boundary = 2 + RACY_NS;
        let mut model = Model { root: tree, at_ns: boundary };
        let first = scan_of(root, &model, &Index::default(), &[], None);
        assert_eq!(first.hashed_files, 1);
        assert_eq!(first.index.files[b"hello".as_slice()].hashed_at_ns, boundary);

        let second = scan_of(root, &model, &first.index, &[], None);
        assert_eq!(second.hashed_files, 1, "a hash taken at the racy boundary is not settled");

        model.at_ns = boundary + 1;
        let third = scan_of(root, &model, &second.index, &[], None);
        assert_eq!(third.hashed_files, 1);
        assert_eq!(third.index.files[b"hello".as_slice()].hashed_at_ns, boundary + 1);
        let fourth = scan_of(root, &model, &third.index, &[], None);
        assert_eq!(fourth.hashed_files, 0);
        assert_eq!(fourth.root, third.root);

        let Mem::File { ino, .. } = mem_mut(&mut model.root, "hello") else { panic!("hello") };
        *ino = 9;
        let fifth = scan_of(root, &model, &fourth.index, &[], None);
        assert_eq!(fifth.hashed_files, 1);
        assert_eq!(fifth.root, fourth.root);
        assert_eq!(fifth.index.files[b"hello".as_slice()].hash, *blake3::hash(b"hello").as_bytes());

        let Mem::File { bytes, .. } = mem_mut(&mut model.root, "hello") else { panic!("hello") };
        bytes.push(b'!');
        let sixth = scan_of(root, &model, &fifth.index, &[], None);
        assert_eq!(sixth.hashed_files, 1);
        assert_eq!(sixth.index.files[b"hello".as_slice()].hash, *blake3::hash(b"hello!").as_bytes());
        assert_ne!(sixth.root, fifth.root);
    }

    #[test]
    fn index_bytes_round_trip_without_a_file() {
        let root = Path::new("/mem");
        let mut tree = dir();
        put(&mut tree, "a", file("x", 1));
        let scan = scan_of(root, &Model { root: tree, at_ns: SETTLED }, &Index::default(), &[], None);
        let bytes = index_bytes(&scan.index).unwrap();
        assert_eq!(index_from_bytes(Some(&bytes)).files, scan.index.files);
        assert!(index_from_bytes(None).files.is_empty());
        assert!(index_from_bytes(Some(b"garbage")).files.is_empty());
    }

    #[test]
    fn a_missing_root_is_an_error_and_an_unreadable_file_names_its_path() {
        let empty = Index::default();
        let mut core = ScanCore::whole(Path::new("/missing"), Some(&empty), &[], None);
        core.invariants();
        let Step::Ask(asks) = core.advance(Vec::new()).unwrap() else { panic!("the root is listed") };
        core.invariants();
        assert!(matches!(asks.as_slice(), [Ask::List { rel }] if rel.is_empty()));
        let err = match core.advance(vec![Answer::List(Err(io::Error::new(ErrorKind::NotFound, "gone")))]) {
            Err(err) => err,
            Ok(_) => panic!("a missing root is an error"),
        };
        assert_eq!(err.kind(), ErrorKind::NotFound);
        core.invariants();

        let root = Path::new("/mem");
        let mut tree = dir();
        put(&mut tree, "secret", Mem::Blocked { size: 4, mtime_ns: 0, ctime_ns: 0, ino: 1 });
        let err = match run(&mut ScanCore::whole(root, Some(&Index::default()), &[], None), &Model { root: tree, at_ns: SETTLED }) {
            Err(err) => err,
            Ok(_) => panic!("an unreadable file fails the scan"),
        };
        assert_eq!(err.kind(), ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("/mem/secret"), "{err}");
    }
}
