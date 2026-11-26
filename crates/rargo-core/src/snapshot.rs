use crate::hash::Hash;
use crate::store::Store;
use crate::trees_state::{TreesState, Upload, Walk};
use crate::view::*;
use anyhow::{Context, Result, bail};
use dashmap::DashMap;
use parking_lot::Mutex;
use rargo_proto::tree::{Node, Tree};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub trait Uploader: Send + Sync {
    fn trees(&self, want: Vec<Hash>) -> Result<Vec<Vec<u8>>>;
    fn blobs(&self, want: Vec<Hash>) -> Result<()>;
}

#[derive(Default)]
pub struct SyncStats {
    pub trees_received: AtomicU64,
    pub blobs_requested: AtomicU64,
    pub bytes_received: AtomicU64,
    pub snapshots: AtomicU64,
    pub shared_uploads: AtomicU64,
    pub offers: AtomicU64,
    pub offered_bytes: AtomicU64,
}

pub struct Trees {
    pub store: Arc<Store>,
    decoded: DashMap<Hash, Arc<Tree>>,
    state: Mutex<Shared>,
    recent: Mutex<HashMap<String, Arc<Snapshot>>>,
    pub stats: SyncStats,
}

#[derive(Default)]
struct Shared {
    core: TreesState,
    done: BTreeMap<Upload, Arc<crate::io::Notify>>,
}

const SYNC_ATTEMPTS: usize = 8;
const _: () = assert!(SYNC_ATTEMPTS >= 1);

impl Trees {
    pub fn new(store: Arc<Store>) -> Arc<Trees> {
        Arc::new(Trees {
            store,
            decoded: DashMap::new(),
            state: Mutex::new(Shared::default()),
            recent: Mutex::new(HashMap::new()),
            stats: SyncStats::default(),
        })
    }

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

    pub fn put(&self, encoded: &[u8]) -> Result<Hash> {
        let tree = Tree::decode(encoded).context("the client sent a malformed directory manifest")?;
        let digest = self.store.put_bytes(encoded)?;
        self.decoded.insert(digest, Arc::new(tree));
        Ok(digest)
    }

    pub fn put_tree(&self, tree: &Tree) -> Result<Hash> {
        self.put(&tree.encode())
    }

    fn is_complete(&self, d: &Hash, generation: u64) -> bool {
        self.state.lock().core.is_complete(d, generation)
    }

    pub fn sync(self: &Arc<Self>, root: Hash, from: &dyn Uploader) -> Result<Arc<Snapshot>> {
        for _ in 0..SYNC_ATTEMPTS {
            let generation = self.store.generation();
            if self.is_complete(&root, generation)
                && let Some(s) = self.hold(root)
            {
                return Ok(s);
            }
            let mut walk = Walk::new(root);
            while let Some(lacking) =
                walk.advance(&|d| self.is_complete(d, generation), &|d| self.get(d), &|h| self.store.has(h))
            {
                let got = from.trees(lacking.clone())?;
                self.stats.trees_received.fetch_add(got.len() as u64, Ordering::Relaxed);
                let this = self.clone();
                let stored: Result<HashSet<Hash>> = crate::io::cpu(move || got.iter().map(|enc| this.put(enc)).collect());
                let stored = stored?;
                if let Some(d) = lacking.iter().find(|d| !stored.contains(d)) {
                    bail!("the client did not send directory manifest {d}");
                }
            }
            assert_eq!(walk.walked.first(), Some(&root), "the walk starts at the root");
            for h in &walk.present {
                self.store.touch(h);
            }
            if !walk.missing.is_empty() {
                self.upload(walk.missing.iter().copied().collect(), from)?;
            }
            self.state.lock().core.completed(&walk.walked, generation, self.store.generation());
            if let Some(s) = self.hold(root) {
                self.stats.snapshots.fetch_add(1, Ordering::Relaxed);
                return Ok(s);
            }
        }
        bail!(
            "content of the worktree was evicted from the store {SYNC_ATTEMPTS} times while it was being synced (is the store's budget smaller than the worktree?)"
        )
    }

    fn hold(self: &Arc<Self>, root: Hash) -> Option<Arc<Snapshot>> {
        let mut all: Vec<Hash> = Vec::new();
        let mut seen: HashSet<Hash> = HashSet::new();
        let mut todo = vec![root];
        seen.insert(root);
        while let Some(d) = todo.pop() {
            let tree = self.get(&d)?;
            if !self.store.has(&d) {
                match self.store.put_bytes(&tree.encode()) {
                    Ok(stored) if stored == d => {}
                    _ => return None,
                }
            }
            all.push(d);
            for (_, node) in &tree.entries {
                match node {
                    Node::File { hash, .. } => {
                        let h = Hash(*hash);
                        if seen.insert(h) {
                            all.push(h);
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
        assert_eq!(all.first(), Some(&root));
        debug_assert_eq!(all.iter().collect::<HashSet<_>>().len(), all.len(), "each hash is pinned once");
        if !self.store.pin_all(&all) {
            return None;
        }
        Some(Arc::new(Snapshot { trees: self.clone(), id: root, dirs: DashMap::new(), pinned: all }))
    }

    pub fn lacking(&self, root: Hash) -> (Vec<Hash>, Vec<Hash>) {
        let generation = self.store.generation();
        let (trees, blobs, walked) =
            crate::trees_state::missing(root, &|d| self.is_complete(d, generation), &|d| self.get(d), &|h| {
                self.store.has(h)
            });
        if trees.is_empty() && blobs.is_empty() {
            self.state.lock().core.completed(&walked, generation, self.store.generation());
        }
        (trees, blobs)
    }

    fn upload(&self, missing: Vec<Hash>, from: &dyn Uploader) -> Result<()> {
        let mut pending = missing;
        while !pending.is_empty() {
            let (claims, claimed) = {
                let mut st = self.state.lock();
                let up = st.core.begin_upload();
                st.done.insert(up, Arc::new(crate::io::Notify::new()));
                let claimed = st.core.claim(up, &pending, &|h| self.store.has(h));
                (Claims { trees: self, up }, claimed)
            };
            pending.clear();
            let mine = claimed.mine;
            let result = if mine.is_empty() {
                Ok(())
            } else {
                self.stats.blobs_requested.fetch_add(mine.len() as u64, Ordering::Relaxed);
                from.blobs(mine.clone())
            };
            drop(claims);
            result?;
            if let Some(h) = mine.iter().find(|h| !self.store.has(h)) {
                bail!("the client did not upload file content {h}");
            }
            self.stats.shared_uploads.fetch_add(claimed.theirs.len() as u64, Ordering::Relaxed);
            for (h, other) in claimed.theirs {
                let Some(n) = self.state.lock().done.get(&other).cloned() else {
                    if !self.store.has(&h) {
                        pending.push(h);
                    }
                    continue;
                };
                loop {
                    if self.store.has(&h) {
                        break;
                    }
                    if self.state.lock().core.holder(&h) != Some(other) {
                        if !self.store.has(&h) {
                            pending.push(h);
                        }
                        break;
                    }
                    let _ = crate::io::timeout(std::time::Duration::from_millis(200), || n.notified());
                }
            }
        }
        Ok(())
    }
}

struct Claims<'a> {
    trees: &'a Trees,
    up: Upload,
}

impl Drop for Claims<'_> {
    fn drop(&mut self) {
        let mut st = self.trees.state.lock();
        st.core.release(self.up);
        if let Some(n) = st.done.remove(&self.up) {
            n.notify_waiters();
        }
    }
}

impl Trees {
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

pub struct Snapshot {
    trees: Arc<Trees>,
    pub id: Hash,
    dirs: DashMap<Vec<u8>, Option<Arc<Tree>>>,
    pinned: Vec<Hash>,
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        if !self.pinned.is_empty() {
            self.trees.store.unpin_all(&self.pinned);
        }
    }
}

impl Snapshot {
    pub fn new(trees: Arc<Trees>, root: Hash) -> Snapshot {
        Snapshot { trees, id: root, dirs: DashMap::new(), pinned: Vec::new() }
    }

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

    pub fn node(&self, rel: &[u8]) -> Option<Node> {
        let mut abs = b"/".to_vec();
        abs.extend_from_slice(rel);
        let (dir, name) = vsplit(&abs)?;
        self.dir(dir)?.get(name).cloned()
    }

    pub fn read(&self, rel: &[u8]) -> Option<Vec<u8>> {
        match self.node(rel)? {
            Node::File { hash, .. } => self.trees.store.read(&Hash(hash)).ok(),
            _ => None,
        }
    }

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

    pub fn materialize(&self, dest: &Path, uid: u32, gid: u32, keep: &dyn Fn(&[u8], &Node) -> bool) -> Result<()> {
        let store = self.trees.store.clone();
        crate::io::file::create_dir_all(dest)?;
        chown(dest, uid, gid);
        let mut failure: Option<anyhow::Error> = None;
        self.walk(&mut |rel, node| {
            if failure.is_some() || !keep(rel, node) {
                return false;
            }
            let path = dest.join(std::ffi::OsStr::from_bytes(rel));
            let done: Result<()> = (|| {
                match node {
                    Node::Dir(_) => crate::io::file::create_dir_all(&path)?,
                    Node::Symlink(t) => crate::io::file::symlink(std::ffi::OsStr::from_bytes(t), &path)?,
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
        unsafe { libc::lchown(c.as_ptr(), uid, gid) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rargo_machine::StateMachine;
    use crate::db::Db;

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
        fn trees(&self, want: Vec<Hash>) -> Result<Vec<Vec<u8>>> {
            self.asked_trees.fetch_add(want.len() as u64, Ordering::Relaxed);
            Ok(want.iter().filter_map(|d| self.trees.lock().get(d).cloned()).collect())
        }
        fn blobs(&self, want: Vec<Hash>) -> Result<()> {
            self.asked_blobs.fetch_add(want.len() as u64, Ordering::Relaxed);
            for h in want {
                if let Some(b) = self.blobs.lock().get(&h) {
                    self.store.as_ref().unwrap().put_bytes(b)?;
                }
            }
            Ok(())
        }
    }

    fn setup() -> (tempfile::TempDir, Arc<Trees>, Fake) {
        let d = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::open(&d.path().join("db.redb")).unwrap());
        let store = Arc::new(Store::open(&d.path().join("store"), db).unwrap());
        let fake = Fake { store: Some(store.clone()), ..Default::default() };
        (d, Trees::new(store), fake)
    }

    struct Stalled;

    impl Uploader for Stalled {
        fn trees(&self, _want: Vec<Hash>) -> Result<Vec<Vec<u8>>> {
            Ok(Vec::new())
        }
        fn blobs(&self, _want: Vec<Hash>) -> Result<()> {
            loop {
                crate::io::sleep(std::time::Duration::from_secs(3600));
            }
        }
    }

    #[test]
    fn dropping_a_sync_while_it_uploads_releases_its_claims_for_the_next_sync() {
        crate::io::drive(1, || {
            let (_d, trees, fake) = setup();
            let v = fake.dir(vec![("a", fake.file(b"content a")), ("b", fake.file(b"content b"))]);
            for enc in fake.trees.lock().values() {
                trees.put(enc).unwrap();
            }
            let blobs = [Hash::of(b"content a"), Hash::of(b"content b")];
            let (first_trees, root) = (trees.clone(), root_of(&v));
            let first = crate::io::spawn(move || first_trees.sync(root, &Stalled));
            crate::io::yield_now();
            assert!(!first.is_finished(), "the first sync waits on its client's upload");
            let up = trees.state.lock().core.holder(&blobs[0]).expect("the first sync claims what it uploads");
            assert_eq!(trees.state.lock().core.holder(&blobs[1]), Some(up));
            first.abort();
            for h in &blobs {
                assert_eq!(trees.state.lock().core.holder(h), None, "a dropped sync leaves no claim behind");
            }
            assert!(!trees.state.lock().done.contains_key(&up), "and nothing waits on it");
            let second = crate::io::timeout(std::time::Duration::from_secs(5), || trees.sync(root, &fake))
                .expect("the next sync of the same content does not wait on the dropped one");
            match second {
                Ok(s) => assert_eq!(s.read(b"b").unwrap(), b"content b"),
                Err(e) => panic!("the next sync of the same content fails: {e}"),
            }
            trees.state.lock().core.invariants();
        });
    }

    fn root_of(n: &Node) -> Hash {
        match n {
            Node::Dir(d) => Hash(*d),
            _ => unreachable!(),
        }
    }

    #[test]
    fn syncs_only_what_is_missing() {
        crate::io::drive(1, || {
        let (_d, trees, fake) = setup();
        let src = fake.dir(vec![("lib.rs", fake.file(b"pub fn a() {}")), ("util.rs", fake.file(b"// util"))]);
        let v1 = fake.dir(vec![
            ("Cargo.toml", fake.file(b"[package]")),
            ("src", src.clone()),
            ("link", Node::Symlink(b"src".to_vec())),
        ]);
        let s1 = trees.sync(root_of(&v1), &fake).unwrap();
        assert_eq!(fake.asked_trees.load(Ordering::Relaxed), 2);
        assert_eq!(fake.asked_blobs.load(Ordering::Relaxed), 3);
        assert!(matches!(s1.lookup(b"/", b"src"), Looked::Dir(_)));
        assert!(matches!(s1.lookup(b"/", b"link"), Looked::Symlink(t) if t == b"src"));
        assert!(matches!(s1.lookup(b"/", b"nope"), Looked::Absent));
        assert!(matches!(s1.lookup(b"/src", b"lib.rs"), Looked::File(f) if f.size == 13));
        assert_eq!(s1.read(b"src/util.rs").unwrap(), b"// util");
        assert_eq!(s1.list(b"/").unwrap().len(), 3);
        assert!(s1.list(b"/missing").is_err());

        trees.sync(root_of(&v1), &fake).unwrap();
        assert_eq!(fake.asked_trees.load(Ordering::Relaxed), 2);

        let src2 = fake.dir(vec![("lib.rs", fake.file(b"pub fn b() {}")), ("util.rs", fake.file(b"// util"))]);
        let v2 = fake.dir(vec![
            ("Cargo.toml", fake.file(b"[package]")),
            ("src", src2),
            ("link", Node::Symlink(b"src".to_vec())),
        ]);
        let s2 = trees.sync(root_of(&v2), &fake).unwrap();
        assert_eq!(fake.asked_trees.load(Ordering::Relaxed), 4);
        assert_eq!(fake.asked_blobs.load(Ordering::Relaxed), 4);
        assert_ne!(s1.id, s2.id);
        assert_eq!(s2.read(b"src/lib.rs").unwrap(), b"pub fn b() {}");
        });
    }

    struct ChurningUpload<'a> {
        source: &'a Fake,
    }

    impl Uploader for ChurningUpload<'_> {
        fn trees(&self, want: Vec<Hash>) -> Result<Vec<Vec<u8>>> {
            self.source.trees(want)
        }

        fn blobs(&self, want: Vec<Hash>) -> Result<()> {
            self.source.blobs(want)?;
            let store = self.source.store.as_ref().unwrap();
            let unrelated = format!("someone else's output {}", store.generation());
            let hash = store.put_bytes(unrelated.as_bytes())?;
            store.remove(&hash);
            Ok(())
        }
    }

    #[test]
    fn a_store_that_evicts_other_content_all_the_time_fails_no_sync() {
        crate::io::drive(1, || {
            let (_d, trees, fake) = setup();
            let names: Vec<String> = (0..512).map(|i| format!("f{i:04}")).collect();
            let mut files: Vec<(&str, Node)> = names.iter().map(|n| (n.as_str(), fake.file(n.as_bytes()))).collect();
            files.push(("b", fake.file(b"content b")));
            let v = fake.dir(vec![("a", fake.file(b"content a")), ("sub", fake.dir(files))]);
            let source = ChurningUpload { source: &fake };
            for i in 0..12 {
                let before = trees.store.generation();
                let extra = format!("edit {i}");
                let w = fake.dir(vec![("tree", v.clone()), ("edit", fake.file(extra.as_bytes()))]);
                let s = trees.sync(root_of(&w), &source).unwrap_or_else(|e| panic!("sync {i} failed: {e:#}"));
                assert!(trees.store.generation() > before);
                assert_eq!(s.read(b"tree/sub/b").unwrap(), b"content b");
                assert_eq!(s.read(b"edit").unwrap(), extra.as_bytes());
            }
        });
    }

    #[test]
    fn a_synced_worktree_survives_eviction_to_a_small_budget_while_it_is_held() {
        crate::io::drive(1, || {
            let (_d, trees, fake) = setup();
            let names: Vec<String> = (0..300).map(|i| format!("f{i:03}")).collect();
            let mut last = None;
            for round in 0..3 {
                let files: Vec<(&str, Node)> =
                    names.iter().map(|n| (n.as_str(), fake.file(format!("{n} in round {round}").as_bytes()))).collect();
                let v = fake.dir(vec![("src", fake.dir(files)), ("top", fake.file(format!("top {round}").as_bytes()))]);
                let s = trees.sync(root_of(&v), &fake).unwrap_or_else(|e| panic!("sync {round} failed: {e:#}"));
                let mut outputs = Vec::new();
                for i in 0..32 {
                    outputs.push(trees.store.put_bytes(format!("output {round}/{i} {}", "x".repeat(4000)).as_bytes()).unwrap());
                }
                assert!(trees.store.evict_to(1000, &|_| false) > 100_000);
                assert!(outputs.iter().all(|h| !trees.store.has(h)));
                for n in &names {
                    let path = format!("src/{n}");
                    assert_eq!(
                        s.read(path.as_bytes())
                            .unwrap_or_else(|| panic!("round {round}: {path} was evicted while its snapshot was held")),
                        format!("{n} in round {round}").as_bytes()
                    );
                }
                assert_eq!(trees.lacking(s.id), (Vec::new(), Vec::new()));
                last = Some((s.id, Hash::of(format!("f000 in round {round}").as_bytes())));
            }
            let (root, a_file) = last.unwrap();
            trees.store.evict_to(0, &|_| false);
            assert!(!trees.store.has(&a_file) && !trees.store.has(&root));
            assert!(!trees.store.is_pinned(&a_file));
        });
    }

    #[test]
    fn a_client_that_withholds_content_fails_the_sync() {
        crate::io::drive(1, || {
        let (_d, trees, fake) = setup();
        let v = fake.dir(vec![("a", fake.file(b"content a"))]);
        fake.blobs.lock().clear();
        assert!(trees.sync(root_of(&v), &fake).is_err());
        let ghost = Hash::of(b"no such tree");
        assert!(trees.sync(ghost, &fake).is_err());
        });
    }

    #[test]
    fn derived_snapshot_and_materialize() {
        crate::io::drive(1, || {
        let (d, trees, fake) = setup();
        let v = fake
            .dir(vec![("Cargo.toml", fake.file(b"[package]")), ("src", fake.dir(vec![("lib.rs", fake.file(b"x"))]))]);
        let s = trees.sync(root_of(&v), &fake).unwrap();
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
        std::fs::write(out.join("Cargo.toml"), b"changed").unwrap();
        assert_eq!(s2.read(b"Cargo.toml").unwrap(), b"[package]");
        let mut live = HashSet::new();
        trees.remember("wt", &s2);
        trees.mark(&mut live);
        assert!(live.contains(&lock) && live.contains(&s2.id));
        });
    }
}
