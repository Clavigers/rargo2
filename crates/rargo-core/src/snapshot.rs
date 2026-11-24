//! Worktree snapshots: content-addressed trees synced from clients.
//!
//! The server never sees a client's worktree. A client describes it as a
//! tree of directory manifests ([`Tree`]) whose leaves are content hashes,
//! and uploads whatever manifests and file contents the server's store does
//! not hold. A [`Snapshot`] is one such tree, identified by the digest of its
//! root: immutable, and identical for identical source no matter which
//! machine, user or directory it came from.
//!
//! Manifests are kept in the content store like any other blob, under the
//! hash of their encoding, so they are deduplicated, evicted and collected
//! by the same rules as file contents; a client is simply asked again for
//! whatever has gone.

use crate::hash::Hash;
use crate::store::Store;
use crate::view::*;
use anyhow::{Context, Result, bail};
use dashmap::DashMap;
use parking_lot::Mutex;
use rargo_proto::tree::{Node, Tree};
use std::collections::{HashMap, HashSet};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// What a snapshot needs from the client it came from while it is being
/// brought into the store.
pub trait Uploader: Send + Sync {
    /// The encoded trees with these digests, in any order.
    fn trees<'a>(&'a self, want: Vec<Hash>) -> futures::BoxFuture<'a, Result<Vec<Vec<u8>>>>;
    /// Uploads these file contents into the store; returns when all of them
    /// are there.
    fn blobs<'a>(&'a self, want: Vec<Hash>) -> futures::BoxFuture<'a, Result<()>>;
}

pub mod futures {
    pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;
}

#[derive(Default)]
pub struct SyncStats {
    pub trees_received: AtomicU64,
    pub blobs_requested: AtomicU64,
    pub bytes_received: AtomicU64,
    pub snapshots: AtomicU64,
    /// Blobs one client waited for while another was already uploading them.
    pub shared_uploads: AtomicU64,
    /// Changes clients pushed without being asked, and their size.
    pub offers: AtomicU64,
    pub offered_bytes: AtomicU64,
}

/// All trees the server knows, and the bookkeeping for syncing new ones.
pub struct Trees {
    pub store: Arc<Store>,
    decoded: DashMap<Hash, Arc<Tree>>,
    /// Trees whose whole subtree (manifests and file contents) was in the
    /// store at the given store generation.
    complete: DashMap<Hash, u64>,
    /// File contents some client is uploading right now.
    uploading: Mutex<HashMap<Hash, Arc<tokio::sync::Notify>>>,
    /// The newest snapshot of each worktree: kept from garbage collection.
    recent: Mutex<HashMap<String, Arc<Snapshot>>>,
    pub stats: SyncStats,
}

/// How often a sync starts over because content of its own tree was
/// evicted while it ran.
const SYNC_ATTEMPTS: usize = 8;

impl Trees {
    pub fn new(store: Arc<Store>) -> Arc<Trees> {
        Arc::new(Trees {
            store,
            decoded: DashMap::new(),
            complete: DashMap::new(),
            uploading: Mutex::new(HashMap::new()),
            recent: Mutex::new(HashMap::new()),
            stats: SyncStats::default(),
        })
    }

    /// The tree with this digest, if the store has it.
    pub fn get(&self, digest: &Hash) -> Option<Arc<Tree>> {
        if let Some(t) = self.decoded.get(digest) {
            return Some(t.clone());
        }
        if !self.store.has(digest) {
            return None;
        }
        let bytes = self.store.read(digest).ok()?;
        let tree = Arc::new(Tree::decode(&bytes).ok()?);
        if self.decoded.len() > 200_000 {
            self.decoded.clear();
        }
        self.decoded.insert(*digest, tree.clone());
        Some(tree)
    }

    /// Stores an encoded tree and returns its digest.
    pub fn put(&self, encoded: &[u8]) -> Result<Hash> {
        let tree = Tree::decode(encoded)?;
        let digest = self.store.put_bytes(encoded)?;
        self.decoded.insert(digest, Arc::new(tree));
        Ok(digest)
    }

    /// Stores a tree built on the server.
    pub fn put_tree(&self, tree: &Tree) -> Result<Hash> {
        self.put(&tree.encode())
    }

    /// Brings the tree rooted at `root` completely into the store, asking
    /// `from` for what is missing, and returns it as a snapshot.
    ///
    /// The store may be evicting while this goes on (a server at its budget
    /// does so all the time). That is no reason to start over, and never a
    /// reason to fail, unless content of this very tree went missing: when
    /// the store has changed during the walk, the whole tree is checked
    /// once more against the store as it is, and if all of it is there the
    /// sync is done.
    pub async fn sync(self: &Arc<Self>, root: Hash, from: &dyn Uploader) -> Result<Arc<Snapshot>> {
        for _ in 0..SYNC_ATTEMPTS {
            let generation = self.store.generation();
            if self.complete.get(&root).is_some_and(|g| *g == generation) {
                return Ok(Arc::new(Snapshot::new(self.clone(), root)));
            }
            // Walk level by level: fetch the manifests this level lacks,
            // then descend into every directory not known to be complete.
            let mut visited: Vec<Hash> = Vec::new();
            let mut seen: HashSet<Hash> = HashSet::new();
            let mut missing_blobs: HashSet<Hash> = HashSet::new();
            let mut level = vec![root];
            seen.insert(root);
            while !level.is_empty() {
                let lacking: Vec<Hash> = level.iter().filter(|d| self.get(d).is_none()).copied().collect();
                if !lacking.is_empty() {
                    let got = from.trees(lacking.clone()).await?;
                    self.stats.trees_received.fetch_add(got.len() as u64, Ordering::Relaxed);
                    let this = self.clone();
                    let stored: Result<HashSet<Hash>> = tokio::task::spawn_blocking(move || {
                        got.iter().map(|enc| this.put(enc)).collect()
                    })
                    .await?;
                    let stored = stored.context("the client sent a malformed directory manifest")?;
                    if let Some(d) = lacking.iter().find(|d| !stored.contains(d)) {
                        bail!("the client did not send directory manifest {d}");
                    }
                }
                let mut next = Vec::new();
                for d in &level {
                    let tree = self.get(d).context("a directory manifest vanished from the store during sync")?;
                    visited.push(*d);
                    for (_, node) in &tree.entries {
                        match node {
                            Node::File { hash, .. } => {
                                let h = Hash(*hash);
                                if self.store.has(&h) {
                                    self.store.touch(&h);
                                } else {
                                    missing_blobs.insert(h);
                                }
                            }
                            Node::Dir(sub) => {
                                let sub = Hash(*sub);
                                if seen.insert(sub) && !self.complete.get(&sub).is_some_and(|g| *g == generation) {
                                    next.push(sub);
                                }
                            }
                            Node::Symlink(_) => {}
                        }
                    }
                }
                level = next;
            }
            if !missing_blobs.is_empty() {
                self.upload(missing_blobs.into_iter().collect(), from).await?;
            }
            // Something was evicted while we were looking. Was any of it
            // ours? (What the walk found present was marked as just used,
            // so an eviction by age takes other things first.)
            if self.store.generation() != generation {
                let (trees, blobs) = self.lacking(root);
                if trees.is_empty() && blobs.is_empty() {
                    self.stats.snapshots.fetch_add(1, Ordering::Relaxed);
                    return Ok(Arc::new(Snapshot::new(self.clone(), root)));
                }
                continue;
            }
            for d in visited {
                self.complete.insert(d, generation);
            }
            if self.complete.len() > 2_000_000 {
                self.complete.retain(|_, g| *g == generation);
            }
            self.stats.snapshots.fetch_add(1, Ordering::Relaxed);
            return Ok(Arc::new(Snapshot::new(self.clone(), root)));
        }
        bail!("content of the worktree was evicted from the store {SYNC_ATTEMPTS} times while it was being synced (is the store's budget smaller than the worktree?)")
    }

    /// What of the tree rooted at `root` the store does not hold: directory
    /// manifests (nothing below a missing one can be known) and file
    /// contents. Asks nobody.
    pub fn lacking(&self, root: Hash) -> (Vec<Hash>, Vec<Hash>) {
        let generation = self.store.generation();
        let mut trees: Vec<Hash> = Vec::new();
        let mut blobs: HashSet<Hash> = HashSet::new();
        let mut visited: Vec<Hash> = Vec::new();
        let mut seen: HashSet<Hash> = HashSet::new();
        let mut todo = vec![root];
        seen.insert(root);
        while let Some(d) = todo.pop() {
            if self.complete.get(&d).is_some_and(|g| *g == generation) {
                continue;
            }
            let Some(tree) = self.get(&d) else {
                trees.push(d);
                continue;
            };
            visited.push(d);
            for (_, node) in &tree.entries {
                match node {
                    Node::File { hash, .. } => {
                        let h = Hash(*hash);
                        if !self.store.has(&h) {
                            blobs.insert(h);
                        }
                    }
                    Node::Dir(sub) => {
                        let sub = Hash(*sub);
                        if seen.insert(sub) {
                            todo.push(sub);
                        }
                    }
                    Node::Symlink(_) => {}
                }
            }
        }
        if trees.is_empty() && blobs.is_empty() && self.store.generation() == generation {
            for d in visited {
                self.complete.insert(d, generation);
            }
        }
        (trees, blobs.into_iter().collect())
    }

    /// Gets `missing` into the store. A blob another client is already
    /// uploading is waited for rather than uploaded twice.
    async fn upload(&self, missing: Vec<Hash>, from: &dyn Uploader) -> Result<()> {
        let mut pending = missing;
        while !pending.is_empty() {
            let mut mine = Vec::new();
            let mut theirs: Vec<(Hash, Arc<tokio::sync::Notify>)> = Vec::new();
            {
                let mut up = self.uploading.lock();
                for h in pending.drain(..) {
                    if self.store.has(&h) {
                        continue;
                    }
                    match up.get(&h) {
                        Some(n) => theirs.push((h, n.clone())),
                        None => {
                            up.insert(h, Arc::new(tokio::sync::Notify::new()));
                            mine.push(h);
                        }
                    }
                }
            }
            let result = if mine.is_empty() {
                Ok(())
            } else {
                self.stats.blobs_requested.fetch_add(mine.len() as u64, Ordering::Relaxed);
                from.blobs(mine.clone()).await
            };
            {
                let mut up = self.uploading.lock();
                for h in &mine {
                    if let Some(n) = up.remove(h) {
                        n.notify_waiters();
                    }
                }
            }
            result?;
            if let Some(h) = mine.iter().find(|h| !self.store.has(h)) {
                bail!("the client did not upload file content {h}");
            }
            // Wait for the other uploaders; whatever they failed to bring is
            // ours to ask for on the next round.
            self.stats.shared_uploads.fetch_add(theirs.len() as u64, Ordering::Relaxed);
            for (h, n) in theirs {
                loop {
                    let notified = n.notified();
                    if self.store.has(&h) {
                        break;
                    }
                    let gone = !self.uploading.lock().get(&h).is_some_and(|cur| Arc::ptr_eq(cur, &n));
                    if gone {
                        if !self.store.has(&h) {
                            pending.push(h);
                        }
                        break;
                    }
                    let _ = tokio::time::timeout(std::time::Duration::from_millis(200), notified).await;
                }
            }
        }
        Ok(())
    }

    /// Remembers `snap` as the newest state of `worktree`.
    pub fn remember(&self, worktree: &str, snap: &Arc<Snapshot>) {
        let mut r = self.recent.lock();
        if r.len() > 4096 {
            r.clear();
        }
        r.insert(worktree.to_string(), snap.clone());
    }

    pub fn recent(&self) -> Vec<(String, Arc<Snapshot>)> {
        self.recent.lock().iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    /// Adds everything the remembered snapshots consist of to `live`.
    pub fn mark(&self, live: &mut HashSet<Hash>) {
        let roots: Vec<Hash> = self.recent.lock().values().map(|s| s.id).collect();
        let mut stack = roots;
        while let Some(d) = stack.pop() {
            if !live.insert(d) {
                continue;
            }
            let Some(tree) = self.get(&d) else { continue };
            for (_, node) in &tree.entries {
                match node {
                    Node::File { hash, .. } => {
                        live.insert(Hash(*hash));
                    }
                    Node::Dir(sub) => stack.push(Hash(*sub)),
                    Node::Symlink(_) => {}
                }
            }
        }
    }
}

const OWN_FILE: Owner = Owner::own(0o644);
const OWN_EXEC: Owner = Owner::own(0o755);
const OWN_DIR: Owner = Owner::own(0o755);

/// An immutable view of a worktree.
pub struct Snapshot {
    trees: Arc<Trees>,
    /// The root tree's digest: identifies the snapshot's content.
    pub id: Hash,
    dirs: DashMap<Vec<u8>, Option<Arc<Tree>>>,
}

impl Snapshot {
    pub fn new(trees: Arc<Trees>, root: Hash) -> Snapshot {
        Snapshot { trees, id: root, dirs: DashMap::new() }
    }

    /// `rel_dir` is `/` for the worktree root or `/a/b`.
    fn dir(&self, rel_dir: &[u8]) -> Option<Arc<Tree>> {
        if rel_dir == b"/" || rel_dir.is_empty() {
            return self.trees.get(&self.id);
        }
        if let Some(t) = self.dirs.get(rel_dir) {
            return t.clone();
        }
        let (parent, name) = vsplit(rel_dir)?;
        let t = self.dir(parent).and_then(|p| match p.get(name) {
            Some(Node::Dir(d)) => self.trees.get(&Hash(*d)),
            _ => None,
        });
        self.dirs.insert(rel_dir.to_vec(), t.clone());
        t
    }

    pub fn lookup(&self, rel_dir: &[u8], name: &[u8]) -> Looked {
        let Some(tree) = self.dir(rel_dir) else { return Looked::Absent };
        match tree.get(name) {
            None => Looked::Absent,
            Some(Node::Dir(_)) => Looked::Dir(OWN_DIR),
            Some(Node::Symlink(t)) => Looked::Symlink(t.clone()),
            Some(Node::File { hash, size, exec }) => Looked::File(FileInfo {
                size: *size,
                owner: if *exec { OWN_EXEC } else { OWN_FILE },
                src: FileSrc::Blob(Hash(*hash)),
                sem: None,
            }),
        }
    }

    pub fn list(&self, rel_dir: &[u8]) -> io::Result<Listing> {
        let tree = self.dir(rel_dir).ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
        Ok(tree
            .entries
            .iter()
            .map(|(name, node)| {
                let t = match node {
                    Node::Dir(_) => DType::Dir,
                    Node::Symlink(_) => DType::Symlink,
                    Node::File { .. } => DType::File,
                };
                (name.clone(), t)
            })
            .collect())
    }

    /// The node at a path relative to the root (`a/b/c`).
    pub fn node(&self, rel: &[u8]) -> Option<Node> {
        let mut abs = b"/".to_vec();
        abs.extend_from_slice(rel);
        let (dir, name) = vsplit(&abs)?;
        self.dir(dir)?.get(name).cloned()
    }

    /// The content of the file at a path relative to the root.
    pub fn read(&self, rel: &[u8]) -> Option<Vec<u8>> {
        match self.node(rel)? {
            Node::File { hash, .. } => self.trees.store.read(&Hash(hash)).ok(),
            _ => None,
        }
    }

    /// Calls `f` with every entry, parents before children; `f` returns
    /// whether to descend into a directory.
    pub fn walk(&self, f: &mut dyn FnMut(&[u8], &Node) -> bool) {
        fn go(s: &Snapshot, tree: &Tree, prefix: &[u8], f: &mut dyn FnMut(&[u8], &Node) -> bool) {
            for (name, node) in &tree.entries {
                let mut path = prefix.to_vec();
                if !path.is_empty() {
                    path.push(b'/');
                }
                path.extend_from_slice(name);
                let descend = f(&path, node);
                if let (Node::Dir(d), true) = (node, descend) {
                    if let Some(sub) = s.trees.get(&Hash(*d)) {
                        go(s, &sub, &path, f);
                    }
                }
            }
        }
        if let Some(root) = self.trees.get(&self.id) {
            go(self, &root, b"", f);
        }
    }

    /// Writes the snapshot out as ordinary files owned by `uid`/`gid`:
    /// for the few things that need a real directory (evaluating a flake,
    /// fetching dependencies, the drift check's stock cargo build).
    /// `keep` chooses what to write; a directory it rejects is skipped whole.
    pub fn materialize(&self, dest: &Path, uid: u32, gid: u32, keep: &dyn Fn(&[u8], &Node) -> bool) -> Result<()> {
        let store = self.trees.store.clone();
        std::fs::create_dir_all(dest)?;
        chown(dest, uid, gid);
        let mut failure: Option<anyhow::Error> = None;
        self.walk(&mut |rel, node| {
            if failure.is_some() || !keep(rel, node) {
                return false;
            }
            let path = dest.join(std::ffi::OsStr::from_bytes(rel));
            let done: Result<()> = (|| {
                match node {
                    Node::Dir(_) => std::fs::create_dir_all(&path)?,
                    Node::Symlink(t) => std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(t), &path)?,
                    Node::File { hash, exec, .. } => {
                        store.materialize_owned(&Hash(*hash), *exec, &path)?;
                    }
                }
                chown(&path, uid, gid);
                Ok(())
            })();
            if let Err(e) = done {
                failure = Some(e.context(format!("writing {}", path.display())));
            }
            true
        });
        match failure {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// A snapshot that differs from this one in one file.
    pub fn with_file(&self, rel: &[u8], node: Option<Node>) -> Result<Arc<Snapshot>> {
        fn set(trees: &Trees, tree: &Tree, comps: &[&[u8]], node: Option<Node>) -> Result<Tree> {
            let mut out = tree.clone();
            let pos = out.entries.binary_search_by(|(n, _)| n.as_slice().cmp(comps[0]));
            let new = if comps.len() == 1 {
                node
            } else {
                let sub = match pos.ok().map(|i| &out.entries[i].1) {
                    Some(Node::Dir(d)) => (*trees.get(&Hash(*d)).context("subtree missing")?).clone(),
                    _ => Tree::default(),
                };
                let sub = set(trees, &sub, &comps[1..], node)?;
                Some(Node::Dir(trees.put_tree(&sub)?.0))
            };
            match (pos, new) {
                (Ok(i), Some(n)) => out.entries[i].1 = n,
                (Ok(i), None) => {
                    out.entries.remove(i);
                }
                (Err(i), Some(n)) => out.entries.insert(i, (comps[0].to_vec(), n)),
                (Err(_), None) => {}
            }
            Ok(out)
        }
        let comps: Vec<&[u8]> = rel.split(|&c| c == b'/').filter(|c| !c.is_empty()).collect();
        if comps.is_empty() {
            bail!("empty path");
        }
        let root = self.trees.get(&self.id).context("root tree missing")?;
        let new_root = set(&self.trees, &root, &comps, node)?;
        let id = self.trees.put_tree(&new_root)?;
        Ok(Arc::new(Snapshot::new(self.trees.clone(), id)))
    }
}

pub fn chown(path: &Path, uid: u32, gid: u32) {
    if unsafe { libc::geteuid() } != 0 {
        return;
    }
    if let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) {
        // SAFETY: valid C string.
        unsafe { libc::lchown(c.as_ptr(), uid, gid) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    /// A client: trees and blobs by digest.
    #[derive(Default)]
    struct Fake {
        trees: Mutex<HashMap<Hash, Vec<u8>>>,
        blobs: Mutex<HashMap<Hash, Vec<u8>>>,
        store: Option<Arc<Store>>,
        asked_trees: AtomicU64,
        asked_blobs: AtomicU64,
    }

    impl Fake {
        fn file(&self, content: &[u8]) -> Node {
            let h = Hash::of(content);
            self.blobs.lock().insert(h, content.to_vec());
            Node::File { hash: h.0, size: content.len() as u64, exec: false }
        }
        fn dir(&self, mut entries: Vec<(&str, Node)>) -> Node {
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let t = Tree { entries: entries.into_iter().map(|(n, e)| (n.as_bytes().to_vec(), e)).collect() };
            let enc = t.encode();
            let d = Hash::of(&enc);
            self.trees.lock().insert(d, enc);
            Node::Dir(d.0)
        }
    }

    impl Uploader for Fake {
        fn trees<'a>(&'a self, want: Vec<Hash>) -> futures::BoxFuture<'a, Result<Vec<Vec<u8>>>> {
            Box::pin(async move {
                self.asked_trees.fetch_add(want.len() as u64, Ordering::Relaxed);
                Ok(want.iter().filter_map(|d| self.trees.lock().get(d).cloned()).collect())
            })
        }
        fn blobs<'a>(&'a self, want: Vec<Hash>) -> futures::BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                self.asked_blobs.fetch_add(want.len() as u64, Ordering::Relaxed);
                for h in want {
                    if let Some(b) = self.blobs.lock().get(&h) {
                        self.store.as_ref().unwrap().put_bytes(b)?;
                    }
                }
                Ok(())
            })
        }
    }

    fn setup() -> (tempfile::TempDir, Arc<Trees>, Fake) {
        let d = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::open(&d.path().join("db.redb")).unwrap());
        let store = Arc::new(Store::open(&d.path().join("store"), db).unwrap());
        let fake = Fake { store: Some(store.clone()), ..Default::default() };
        (d, Trees::new(store), fake)
    }

    fn root_of(n: &Node) -> Hash {
        match n {
            Node::Dir(d) => Hash(*d),
            _ => unreachable!(),
        }
    }

    #[tokio::test]
    async fn syncs_only_what_is_missing() {
        let (_d, trees, fake) = setup();
        let src = fake.dir(vec![("lib.rs", fake.file(b"pub fn a() {}")), ("util.rs", fake.file(b"// util"))]);
        let v1 = fake.dir(vec![
            ("Cargo.toml", fake.file(b"[package]")),
            ("src", src.clone()),
            ("link", Node::Symlink(b"src".to_vec())),
        ]);
        let s1 = trees.sync(root_of(&v1), &fake).await.unwrap();
        assert_eq!(fake.asked_trees.load(Ordering::Relaxed), 2);
        assert_eq!(fake.asked_blobs.load(Ordering::Relaxed), 3);
        assert!(matches!(s1.lookup(b"/", b"src"), Looked::Dir(_)));
        assert!(matches!(s1.lookup(b"/", b"link"), Looked::Symlink(t) if t == b"src"));
        assert!(matches!(s1.lookup(b"/", b"nope"), Looked::Absent));
        assert!(matches!(s1.lookup(b"/src", b"lib.rs"), Looked::File(f) if f.size == 13));
        assert_eq!(s1.read(b"src/util.rs").unwrap(), b"// util");
        assert_eq!(s1.list(b"/").unwrap().len(), 3);
        assert!(s1.list(b"/missing").is_err());

        // The same tree again: nothing is asked.
        trees.sync(root_of(&v1), &fake).await.unwrap();
        assert_eq!(fake.asked_trees.load(Ordering::Relaxed), 2);

        // One file edited: the two manifests on its path and one blob.
        let src2 = fake.dir(vec![("lib.rs", fake.file(b"pub fn b() {}")), ("util.rs", fake.file(b"// util"))]);
        let v2 = fake.dir(vec![
            ("Cargo.toml", fake.file(b"[package]")),
            ("src", src2),
            ("link", Node::Symlink(b"src".to_vec())),
        ]);
        let s2 = trees.sync(root_of(&v2), &fake).await.unwrap();
        assert_eq!(fake.asked_trees.load(Ordering::Relaxed), 4);
        assert_eq!(fake.asked_blobs.load(Ordering::Relaxed), 4);
        assert_ne!(s1.id, s2.id);
        assert_eq!(s2.read(b"src/lib.rs").unwrap(), b"pub fn b() {}");
    }

    #[tokio::test]
    async fn a_store_that_evicts_other_content_all_the_time_fails_no_sync() {
        let (_d, trees, fake) = setup();
        // A tree large enough that walking it takes longer than the store
        // stays unchanged: every walk sees a change.
        let names: Vec<String> = (0..4000).map(|i| format!("f{i:04}")).collect();
        let mut files: Vec<(&str, Node)> = names.iter().map(|n| (n.as_str(), fake.file(n.as_bytes()))).collect();
        files.push(("b", fake.file(b"content b")));
        let v = fake.dir(vec![("a", fake.file(b"content a")), ("sub", fake.dir(files))]);
        // Another build's content comes and goes without pause.
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let churn = {
            let (store, stop) = (trees.store.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut n = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let junk = format!("someone else's output {n}");
                    store.put_bytes(junk.as_bytes()).unwrap();
                    store.remove(&Hash::of(junk.as_bytes()));
                    n += 1;
                }
                n
            })
        };
        let before = trees.store.generation();
        for i in 0..40 {
            // (A changed tree each time, so that every sync walks.)
            let extra = format!("edit {i}");
            let w = fake.dir(vec![("tree", v.clone()), ("edit", fake.file(extra.as_bytes()))]);
            let s = trees.sync(root_of(&w), &fake).await.unwrap_or_else(|e| panic!("sync {i} failed: {e:#}"));
            assert_eq!(s.read(b"tree/sub/b").unwrap(), b"content b");
            assert_eq!(s.read(b"edit").unwrap(), extra.as_bytes());
        }
        stop.store(true, Ordering::Relaxed);
        let churned = churn.join().unwrap();
        assert!(trees.store.generation() > before + 400, "the store did change throughout ({churned} rounds)");
    }

    #[tokio::test]
    async fn evicted_content_is_asked_for_again() {
        let (_d, trees, fake) = setup();
        let v = fake.dir(vec![("a", fake.file(b"content a")), ("b", fake.file(b"content b"))]);
        trees.sync(root_of(&v), &fake).await.unwrap();
        trees.store.remove(&Hash::of(b"content a"));
        let s = trees.sync(root_of(&v), &fake).await.unwrap();
        assert_eq!(fake.asked_blobs.load(Ordering::Relaxed), 3);
        assert_eq!(s.read(b"a").unwrap(), b"content a");
    }

    #[tokio::test]
    async fn a_client_that_withholds_content_fails_the_sync() {
        let (_d, trees, fake) = setup();
        let v = fake.dir(vec![("a", fake.file(b"content a"))]);
        fake.blobs.lock().clear();
        assert!(trees.sync(root_of(&v), &fake).await.is_err());
        let ghost = Hash::of(b"no such tree");
        assert!(trees.sync(ghost, &fake).await.is_err());
    }

    #[tokio::test]
    async fn derived_snapshot_and_materialize() {
        let (d, trees, fake) = setup();
        let v = fake.dir(vec![("Cargo.toml", fake.file(b"[package]")), ("src", fake.dir(vec![("lib.rs", fake.file(b"x"))]))]);
        let s = trees.sync(root_of(&v), &fake).await.unwrap();
        let lock = trees.store.put_bytes(b"# lock").unwrap();
        let s2 = s.with_file(b"sub/Cargo.lock", Some(Node::File { hash: lock.0, size: 6, exec: false })).unwrap();
        assert_eq!(s2.read(b"sub/Cargo.lock").unwrap(), b"# lock");
        assert_eq!(s2.read(b"src/lib.rs").unwrap(), b"x");
        let s3 = s2.with_file(b"src/lib.rs", None).unwrap();
        assert!(s3.node(b"src/lib.rs").is_none());
        let out = d.path().join("out");
        s2.materialize(&out, 0, 0, &|rel, _| rel != b"src").unwrap();
        assert_eq!(std::fs::read(out.join("sub/Cargo.lock")).unwrap(), b"# lock");
        assert!(!out.join("src").exists());
        // An ordinary writable file, not a link into the store.
        std::fs::write(out.join("Cargo.toml"), b"changed").unwrap();
        assert_eq!(s2.read(b"Cargo.toml").unwrap(), b"[package]");
        let mut live = HashSet::new();
        trees.remember("wt", &s2);
        trees.mark(&mut live);
        assert!(live.contains(&lock) && live.contains(&s2.id));
    }
}
