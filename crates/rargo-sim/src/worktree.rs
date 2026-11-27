//! A client's worktree in memory, which rargo's scan core reads through its questions, and the
//! client's `target/`, which the output filesystem's table holds.

use rargo::out_tree::OutputTree;
use rargo::scan::{self, Answer, Ask, Index, Kind, Meta, Scan};
use rargo_core::hash::Hash;
use rargo_proto::Digest;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::{self, ErrorKind};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

pub const ROOT: &str = "/work";

struct File {
    bytes: Vec<u8>,
    mtime_ns: i64,
    ctime_ns: i64,
    ino: u64,
}

enum Node<'a> {
    Dir,
    File(&'a File),
    Target(u64),
}

pub struct Worktree {
    files: BTreeMap<Vec<u8>, File>,
    next_ino: u64,
    /// The directories written in since the last scan, as the daemon's watch reports them.
    changed: BTreeSet<PathBuf>,
}

/// Where source `index` lives: spread over nested directories, so that a scan lists and reuses
/// directories and an edit changes a path of them.
pub fn path(index: usize) -> Vec<u8> {
    format!("crates/c{}/src/m{}/s{index}.rs", index % 3, index % 2).into_bytes()
}

/// Where the output of the action with this id lands in `target/`.
pub fn target_path(action: u64) -> Vec<u8> {
    format!("debug/deps/out-{action}").into_bytes()
}

fn under(dir: &[u8], rel: &[u8]) -> bool {
    dir.is_empty() || (rel.len() > dir.len() && rel.starts_with(dir) && rel[dir.len()] == b'/')
}

fn child_of<'a>(dir: &[u8], rel: &'a [u8]) -> &'a [u8] {
    let rest = if dir.is_empty() { rel } else { &rel[dir.len() + 1..] };
    rest.split(|&b| b == b'/').next().expect("a path below a directory names a child")
}

/// Besides the sources, a worktree holds these, which a scan takes.
pub const OTHER_FILES: u64 = 1;

impl Default for Worktree {
    fn default() -> Worktree {
        let mut wt = Worktree { files: BTreeMap::new(), next_ino: 1, changed: BTreeSet::new() };
        wt.write(b"Cargo.toml", b"[workspace]\n".to_vec(), 0);
        // The scan leaves out git's own directory, as it does the root's `target/`.
        wt.write(b".git/HEAD", b"ref: refs/heads/main\n".to_vec(), 0);
        wt
    }
}

impl Worktree {

    /// Writes a file in place, as an editor that keeps the inode does; a new file gets a new inode.
    pub fn write(&mut self, rel: &[u8], bytes: Vec<u8>, now_ns: i64) {
        let ino = self.files.get(rel).map_or_else(
            || {
                self.next_ino += 1;
                self.next_ino
            },
            |f| f.ino,
        );
        self.files.insert(rel.to_vec(), File { bytes, mtime_ns: now_ns, ctime_ns: now_ns, ino });
        self.touched(rel);
    }

    pub fn remove(&mut self, rel: &[u8]) {
        if self.files.remove(rel).is_some() {
            self.touched(rel);
        }
    }

    pub fn read(&self, rel: &[u8]) -> Option<&[u8]> {
        self.files.get(rel).map(|f| f.bytes.as_slice())
    }

    /// Forgets which directories were written in, as a watch that missed its events would.
    pub fn forget_changes(&mut self) {
        self.changed.clear();
    }

    fn touched(&mut self, rel: &[u8]) {
        let dir = rel.iter().rposition(|&b| b == b'/').map_or(&rel[..0], |i| &rel[..i]);
        self.changed.insert(abs(dir));
    }

    /// Scans with the daemon's last scan when it has one, reading again only the directories
    /// written in since, and otherwise all of it with the index the daemon saved.
    pub fn scan(&mut self, target: &OutputTree, previous: Option<Scan>, saved: &Index, now_ns: i64) -> io::Result<Scan> {
        let root = Path::new(ROOT);
        let changed = std::mem::take(&mut self.changed);
        let answer = |asks: &[Ask]| asks.iter().map(|ask| self.answer(target, ask, now_ns)).collect();
        match previous {
            Some(prev) => scan::rescan_answered(root, prev, &changed, &[], None, answer),
            None => scan::scan_answered(root, saved, &[], None, answer),
        }
    }

    fn node<'a>(&'a self, target: &'a OutputTree, rel: &[u8]) -> Option<Node<'a>> {
        if let Some(f) = self.files.get(rel) {
            return Some(Node::File(f));
        }
        if let Some(inner) = rel.strip_prefix(b"target/")
            && let Some(f) = target.files.get(inner)
        {
            return Some(Node::Target(f.size));
        }
        let dir = rel == b"target" || self.files.keys().any(|k| under(rel, k));
        let in_target = rel.strip_prefix(b"target/").is_some_and(|inner| target.has_below(inner));
        (dir || in_target).then_some(Node::Dir)
    }

    fn list(&self, target: &OutputTree, rel: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        if !matches!(self.node(target, rel), Some(Node::Dir)) {
            return Err(io::Error::new(ErrorKind::NotFound, "no such directory"));
        }
        let mut names: BTreeSet<Vec<u8>> = self.files.keys().filter(|k| under(rel, k)).map(|k| child_of(rel, k).to_vec()).collect();
        if rel.is_empty() {
            names.insert(b"target".to_vec());
        }
        if let Some(inner) = rel.strip_prefix(b"target").map(|r| r.strip_prefix(b"/").unwrap_or(r)) {
            names.extend(target.files.keys().filter(|k| under(inner, k)).map(|k| child_of(inner, k).to_vec()));
        }
        // The kernel lists a directory in no particular order.
        Ok(names.into_iter().rev().collect())
    }

    fn answer(&self, target: &OutputTree, ask: &Ask, now_ns: i64) -> Answer {
        let missing = || io::Error::new(ErrorKind::NotFound, "no such file");
        match ask {
            Ask::List { rel } => Answer::List(self.list(target, rel)),
            Ask::Meta { rel } => Answer::Meta(match self.node(target, rel) {
                None => Err(missing()),
                Some(Node::Dir) => Ok(Meta { kind: Kind::Dir, size: 0, mtime_ns: 0, ctime_ns: 0, ino: 0, exec: false }),
                Some(Node::File(f)) => Ok(Meta {
                    kind: Kind::File,
                    size: f.bytes.len() as u64,
                    mtime_ns: f.mtime_ns,
                    ctime_ns: f.ctime_ns,
                    ino: f.ino,
                    exec: false,
                }),
                Some(Node::Target(size)) => Ok(Meta { kind: Kind::File, size, mtime_ns: 0, ctime_ns: 0, ino: 0, exec: false }),
            }),
            Ask::Cache { .. } => Answer::Cache(None),
            Ask::Link { .. } => Answer::Link(None),
            Ask::Content { rel, .. } => Answer::Content {
                at_ns: now_ns,
                hash: match self.node(target, rel) {
                    Some(Node::File(f)) => Ok(digest(&f.bytes)),
                    _ => Err(missing()),
                },
            },
        }
    }
}

fn digest(bytes: &[u8]) -> Digest {
    Hash::of(bytes).0
}

fn abs(rel: &[u8]) -> PathBuf {
    let root = Path::new(ROOT);
    if rel.is_empty() { root.to_path_buf() } else { root.join(OsStr::from_bytes(rel)) }
}
