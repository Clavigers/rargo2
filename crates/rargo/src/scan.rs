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

#[derive(Default, Serialize, Deserialize)]
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

pub struct Scan {
    pub root: Digest,
    /// Encoded trees by digest.
    pub trees: HashMap<Digest, Vec<u8>>,
    /// Where to read each distinct file content from, and its size.
    pub files: HashMap<Digest, (PathBuf, u64)>,
    /// Every directory of the worktree that was scanned (what a watcher
    /// has to watch).
    pub dirs: Vec<PathBuf>,
    pub index: Index,
    /// The index differs from the one the scan started with.
    pub index_changed: bool,
    pub file_count: u64,
    pub hashed_files: u64,
    pub hashed_bytes: u64,
}

struct Ctx<'a> {
    root: &'a Path,
    old: &'a Index,
    exclude: &'a [String],
    ignored: Option<&'a Ignored>,
    trees: Mutex<HashMap<Digest, Vec<u8>>>,
    files: Mutex<HashMap<Digest, (PathBuf, u64)>>,
    dirs: Mutex<Vec<PathBuf>>,
    index: Mutex<HashMap<Vec<u8>, Entry>>,
    hashed_files: AtomicU64,
    hashed_bytes: AtomicU64,
    file_count: AtomicU64,
    reused: AtomicU64,
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
    let mut v = Vec::with_capacity(rel.len() + 1 + name.len());
    if !rel.is_empty() {
        v.extend_from_slice(rel);
        v.push(b'/');
    }
    v.extend_from_slice(name);
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
#[derive(Debug, Default)]
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
    /// Scans one directory; returns its tree's digest. `inside`: the
    /// directory is one git ignores, entered only on the way to a path the
    /// policy includes.
    fn dir(&self, rel: &[u8], inside: bool) -> io::Result<Digest> {
        let abs = if rel.is_empty() { self.root.to_path_buf() } else { self.root.join(OsStr::from_bytes(rel)) };
        let mut names: Vec<Vec<u8>> = Vec::new();
        self.dirs.lock().unwrap().push(abs.clone());
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
                self.file_count.fetch_add(1, Ordering::Relaxed);
                let exec = md.mode() & 0o111 != 0;
                let (mtime_ns, ctime_ns) =
                    (md.mtime() * 1_000_000_000 + md.mtime_nsec(), md.ctime() * 1_000_000_000 + md.ctime_nsec());
                if let Some(old) = self.old.files.get(&child_rel) {
                    let same = old.size == md.len() && old.mtime_ns == mtime_ns && old.ctime_ns == ctime_ns && old.ino == md.ino();
                    let settled = mtime_ns.max(ctime_ns) + RACY_NS < old.hashed_at_ns;
                    if same && settled {
                        self.reused.fetch_add(1, Ordering::Relaxed);
                        return Ok(Some((name, Node::File { hash: old.hash, size: old.size, exec }, Some(*old))));
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
        let mut files_add = Vec::new();
        for n in nodes {
            let Some((name, node, entry)) = n? else { continue };
            if let (Node::File { hash, size, .. }, Some(e)) = (&node, entry) {
                let child_rel = join(rel, &name);
                files_add.push((*hash, (abs.join(OsStr::from_bytes(&name)), *size)));
                index_add.push((child_rel, e));
            }
            entries.push((name, node));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let encoded = Tree { entries }.encode();
        let digest = rargo_proto::tree::digest(&encoded);
        self.trees.lock().unwrap().insert(digest, encoded);
        if !index_add.is_empty() {
            self.files.lock().unwrap().extend(files_add);
            self.index.lock().unwrap().extend(index_add);
        }
        Ok(digest)
    }
}

/// Scans the worktree at `root`, leaving out what `ignored` says git
/// ignores.
pub fn scan(root: &Path, old: &Index, exclude: &[String], ignored: Option<&Ignored>) -> io::Result<Scan> {
    scan_dir(root, old, exclude, ignored)
}

/// Scans a directory the client keeps itself (the history of
/// [`crate::git::history`]): everything in it, against its own index.
pub fn scan_store(dir: &Path, old: &Index) -> io::Result<Scan> {
    scan_dir(dir, old, &[], None)
}

/// Shows `git` (see `git::thin`) as the worktree's `.git`, with what
/// `history` holds (a [`scan_store`] of `git::history`'s packs) in its
/// `objects` directory beside its own.
pub fn graft_git(scan: &mut Scan, root: &Path, git: &Path, history: Option<Scan>) -> io::Result<()> {
    // Small and immutable: scanned from scratch, grafted into the root.
    let mut sub = scan_dir(git, &Index::default(), &[], None)?;
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
    let encoded = out.encode();
    let digest = rargo_proto::tree::digest(&encoded);
    trees.insert(digest, encoded);
    Ok(digest)
}

fn scan_dir(root: &Path, old: &Index, exclude: &[String], ignored: Option<&Ignored>) -> io::Result<Scan> {
    let ctx = Ctx {
        root,
        old,
        exclude,
        ignored,
        trees: Mutex::new(HashMap::new()),
        files: Mutex::new(HashMap::new()),
        dirs: Mutex::new(Vec::new()),
        index: Mutex::new(HashMap::with_capacity(old.files.len())),
        hashed_files: AtomicU64::new(0),
        hashed_bytes: AtomicU64::new(0),
        file_count: AtomicU64::new(0),
        reused: AtomicU64::new(0),
    };
    let root_digest = ctx.dir(b"", false)?;
    let files = ctx.index.into_inner().unwrap();
    let hashed_files = ctx.hashed_files.into_inner();
    // Unchanged when nothing was hashed and no path came or went.
    let index_changed = hashed_files > 0 || files.len() != old.files.len();
    Ok(Scan {
        root: root_digest,
        trees: ctx.trees.into_inner().unwrap(),
        files: ctx.files.into_inner().unwrap(),
        dirs: ctx.dirs.into_inner().unwrap(),
        index: Index { files },
        index_changed,
        file_count: ctx.file_count.into_inner(),
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
