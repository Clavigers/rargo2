use crate::db::{Db, Table};
use crate::hash::Hash;
use crate::store::Store;
use crate::trace::{MEntry, Manifest};
use crate::view::vpath_starts_with;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StateMeta {
    pub manifest: Hash,
    pub worktree: String,
    pub commit: Option<[u8; 20]>,
    pub created: u64,
    pub bytes: u64,
}

const KEEP_EXTRA: usize = 2;
const KEEP_WORKTREES: usize = 32;
const _: () = assert!(KEEP_WORKTREES > 0);

#[derive(Default)]
pub struct IncrStats {
    pub saved: AtomicU64,
    pub seeded_same_worktree: AtomicU64,
    pub seeded_ancestor: AtomicU64,
    pub seeded_other: AtomicU64,
    pub unseeded: AtomicU64,
    pub discarded_after_crash: AtomicU64,
}

pub struct IncrStore {
    db: Arc<Db>,
    store: Arc<Store>,
    next: AtomicU64,
    held: std::sync::OnceLock<Box<dyn Fn(&Hash) -> bool + Send + Sync>>,
    pub stats: IncrStats,
}

#[derive(Clone)]
pub struct Seed {
    pub id: u64,
    pub manifest: Arc<Manifest>,
    pub kind: SeedKind,
    pub blobs: Arc<Vec<(Hash, u64)>>,
}

fn key(crate_key: &Hash, id: u64) -> Vec<u8> {
    let mut k = crate_key.0.to_vec();
    k.extend_from_slice(&id.to_be_bytes());
    assert_eq!(k.len(), 40, "list() reads the id at 32..40");
    k
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeedKind {
    SameWorktree,
    Ancestor,
    Other,
}

impl IncrStore {
    pub fn new(store: Arc<Store>, seed: u64) -> IncrStore {
        IncrStore {
            db: store.db().clone(),
            store,
            next: AtomicU64::new(seed),
            held: std::sync::OnceLock::new(),
            stats: IncrStats::default(),
        }
    }

    pub fn set_held(&self, f: Box<dyn Fn(&Hash) -> bool + Send + Sync>) {
        let _ = self.held.set(f);
    }

    fn list(&self, crate_key: &Hash) -> Vec<(u64, StateMeta)> {
        let mut v: Vec<(u64, StateMeta)> = self
            .db
            .scan_prefix(Table::Incr, &crate_key.0)
            .into_iter()
            .filter_map(|(k, val)| {
                let id = u64::from_be_bytes(k.get(32..40)?.try_into().ok()?);
                Some((id, bincode::deserialize(&val).ok()?))
            })
            .collect();
        v.sort_by(|a, b| b.0.cmp(&a.0));
        assert!(v.windows(2).all(|w| w[0].0 > w[1].0), "state ids are unique");
        v
    }

    fn load(&self, meta: &StateMeta) -> Option<(Arc<Manifest>, Arc<Vec<(Hash, u64)>>)> {
        let raw = self.store.read(&meta.manifest).ok()?;
        let m: Manifest = bincode::deserialize(&raw).ok()?;
        let mut blobs: Vec<(Hash, u64)> = Vec::new();
        for (_, e) in &m.entries {
            if let MEntry::File { hash, size, .. } = e {
                if !self.store.has(hash) && !self.held.get().is_some_and(|held| held(hash)) {
                    return None;
                }
                blobs.push((*hash, *size));
            }
        }
        blobs.sort();
        blobs.dedup();
        assert!(blobs.windows(2).all(|w| w[0] < w[1]));
        for (hash, _) in &blobs {
            self.store.touch(hash);
        }
        Some((Arc::new(m), Arc::new(blobs)))
    }

    pub fn pick(
        &self,
        crate_key: &Hash,
        worktree: &str,
        distance: &dyn Fn(&[u8; 20]) -> Option<usize>,
    ) -> Option<Seed> {
        let all = self.list(crate_key);
        let own: Vec<&(u64, StateMeta)> = all.iter().filter(|(_, m)| m.worktree == worktree).collect();
        let mut near: Vec<(usize, &(u64, StateMeta))> = all
            .iter()
            .filter(|(_, m)| m.worktree != worktree)
            .filter_map(|s| s.1.commit.as_ref().and_then(distance).map(|d| (d, s)))
            .collect();
        near.sort_by_key(|(d, _)| *d);
        assert!(own.iter().all(|(_, m)| m.worktree == worktree));
        assert!(near.iter().all(|(_, (_, m))| m.worktree != worktree));
        let own_first = match (own.first(), near.first()) {
            (Some(_), None) => true,
            (Some((_, m)), Some((best, _))) => match &m.commit {
                None => true,
                Some(c) => distance(c).is_some_and(|d| d <= *best),
            },
            (None, _) => false,
        };
        let mut order: Vec<(&(u64, StateMeta), SeedKind)> = Vec::new();
        let ancestors = near.iter().map(|(_, s)| (*s, SeedKind::Ancestor));
        if own_first {
            order.extend(own.iter().map(|s| (*s, SeedKind::SameWorktree)));
            order.extend(ancestors);
        } else {
            order.extend(ancestors);
            order.extend(own.iter().map(|s| (*s, SeedKind::SameWorktree)));
        }
        order.extend(all.iter().map(|s| (s, SeedKind::Other)));
        assert_eq!(order.len(), own.len() + near.len() + all.len());
        for ((id, meta), kind) in order {
            match self.load(meta) {
                Some((manifest, blobs)) => {
                    let counter = match kind {
                        SeedKind::SameWorktree => &self.stats.seeded_same_worktree,
                        SeedKind::Ancestor => &self.stats.seeded_ancestor,
                        SeedKind::Other => &self.stats.seeded_other,
                    };
                    counter.fetch_add(1, Ordering::Relaxed);
                    return Some(Seed { id: *id, manifest, kind, blobs });
                }
                None => self.db.delete(Table::Incr, &key(crate_key, *id)),
            }
        }
        self.stats.unseeded.fetch_add(1, Ordering::Relaxed);
        None
    }

    pub fn save(&self, crate_key: &Hash, worktree: &str, commit: Option<[u8; 20]>, manifest: &Manifest, created: u64) {
        let Ok(raw) = bincode::serialize(manifest) else { return };
        let Ok(h) = self.store.put_bytes(&raw) else { return };
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        assert!(id < u64::MAX, "state id counter does not wrap");
        let meta = StateMeta {
            manifest: h,
            worktree: worktree.to_string(),
            commit,
            created,
            bytes: manifest.total_size(),
        };
        self.db.put(Table::Incr, &key(crate_key, id), &bincode::serialize(&meta).unwrap());
        self.stats.saved.fetch_add(1, Ordering::Relaxed);
        self.prune(crate_key);
    }

    fn prune(&self, crate_key: &Hash) {
        let all = self.list(crate_key);
        let mut seen_worktrees: Vec<&str> = Vec::new();
        let mut extra = 0;
        for (id, meta) in &all {
            if !seen_worktrees.contains(&meta.worktree.as_str()) && seen_worktrees.len() < KEEP_WORKTREES {
                seen_worktrees.push(&meta.worktree);
                continue;
            }
            if extra < KEEP_EXTRA {
                extra += 1;
                continue;
            }
            self.db.delete(Table::Incr, &key(crate_key, *id));
        }
        assert!(seen_worktrees.len() <= KEEP_WORKTREES);
        assert!(extra <= KEEP_EXTRA);
    }

    pub fn discard(&self, crate_key: &Hash, id: u64) {
        self.db.delete(Table::Incr, &key(crate_key, id));
        self.stats.discarded_after_crash.fetch_add(1, Ordering::Relaxed);
    }

    pub fn forget(&self, crate_key: &Hash, id: u64) {
        self.db.delete(Table::Incr, &key(crate_key, id));
    }

    pub fn mark(&self, live: &mut std::collections::HashSet<Hash>) {
        let mut metas: Vec<StateMeta> = Vec::new();
        self.db.for_each(Table::Incr, |_, v| {
            if let Ok(m) = bincode::deserialize::<StateMeta>(v) {
                metas.push(m);
            }
        });
        for meta in metas {
            live.insert(meta.manifest);
            let Ok(raw) = self.store.read(&meta.manifest) else { continue };
            let Ok(m) = bincode::deserialize::<Manifest>(&raw) else { continue };
            for (_, e) in &m.entries {
                if let MEntry::File { hash, .. } = e {
                    live.insert(*hash);
                }
            }
        }
    }

    pub fn analyze(&self) -> String {
        use std::collections::{BTreeMap, HashMap, HashSet};
        fn kind(path: &[u8]) -> &'static str {
            let name = path.rsplit(|&c| c == b'/').next().unwrap_or(path);
            match name {
                b"dep-graph.bin" => "dep-graph.bin",
                b"query-cache.bin" => "query-cache.bin",
                b"work-products.bin" => "work-products.bin",
                b"metadata.rmeta" => "metadata.rmeta",
                n if n.ends_with(b".o") => "object files",
                n if n.ends_with(b".dwo") => "split debuginfo",
                n if n.ends_with(b".bc") => "bitcode",
                _ => "other",
            }
        }
        #[derive(Default)]
        struct Row {
            files: u64,
            logical: u64,
            blobs: HashSet<Hash>,
            unique: u64,
            chunks: HashSet<Hash>,
            chunk_unique: u64,
        }
        let mut states = 0u64;
        let mut crates: HashSet<[u8; 32]> = HashSet::new();
        let mut rows: BTreeMap<&'static str, Row> = BTreeMap::new();
        let mut entries: Vec<(Vec<u8>, StateMeta)> = Vec::new();
        self.db.for_each(Table::Incr, |k, v| {
            if let Ok(m) = bincode::deserialize::<StateMeta>(v) {
                entries.push((k.to_vec(), m));
            }
        });
        let mut sizes: HashMap<Hash, u64> = HashMap::new();
        for (k, meta) in &entries {
            let Ok(raw) = self.store.read(&meta.manifest) else { continue };
            let Ok(m) = bincode::deserialize::<Manifest>(&raw) else { continue };
            states += 1;
            if let Some(c) = k.get(..32) {
                crates.insert(c.try_into().unwrap());
            }
            for (path, e) in &m.entries {
                let MEntry::File { hash, size, .. } = e else { continue };
                let row = rows.entry(kind(path)).or_default();
                row.files += 1;
                row.logical += size;
                if row.blobs.insert(*hash) {
                    row.unique += size;
                    sizes.insert(*hash, *size);
                }
            }
        }
        for row in rows.values_mut() {
            for blob in &row.blobs {
                let Ok(f) = self.store.open_blob(blob) else { continue };
                let Ok(recipe) = crate::chunk::chunk_file(&f) else { continue };
                for c in recipe {
                    if row.chunks.insert(c.hash) {
                        row.chunk_unique += c.len as u64;
                    }
                }
            }
        }
        let gib = |b: u64| b as f64 / (1u64 << 30) as f64;
        let mut out = format!(
            "{states} states of {} crates\n{:<20} {:>8} {:>12} {:>14} {:>16}\n",
            crates.len(),
            "",
            "files",
            "logical GiB",
            "as files GiB",
            "as chunks GiB"
        );
        let (mut tl, mut tu, mut tc) = (0u64, 0u64, 0u64);
        for (name, row) in &rows {
            out += &format!(
                "{:<20} {:>8} {:>12.2} {:>14.2} {:>16.2}\n",
                name,
                row.files,
                gib(row.logical),
                gib(row.unique),
                gib(row.chunk_unique)
            );
            tl += row.logical;
            tu += row.unique;
            tc += row.chunk_unique;
        }
        out += &format!("{:<20} {:>8} {:>12.2} {:>14.2} {:>16.2}\n", "total", "", gib(tl), gib(tu), gib(tc));
        out
    }

    pub fn state_count(&self, crate_key: &Hash) -> usize {
        self.list(crate_key).len()
    }
}

pub fn apply_changes(dir: &[u8], seed: Option<&Manifest>, changes: &[(Vec<u8>, MEntry)]) -> Manifest {
    let mut map: BTreeMap<Vec<u8>, MEntry> = BTreeMap::new();
    if let Some(s) = seed {
        for (p, e) in &s.entries {
            map.insert(p.clone(), e.clone());
        }
    }
    for (p, e) in changes {
        if !vpath_starts_with(p, dir) {
            continue;
        }
        match e {
            MEntry::Whiteout => {
                let doomed: Vec<Vec<u8>> =
                    map.keys().filter(|k| vpath_starts_with(k, p)).cloned().collect();
                for k in doomed {
                    map.remove(&k);
                }
            }
            other => {
                map.insert(p.clone(), other.clone());
            }
        }
    }
    Manifest { entries: map.into_iter().collect() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Arc<Store>) {
        let d = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::open(&d.path().join("db.redb")).unwrap());
        let s = Arc::new(Store::open(&d.path().join("store"), db).unwrap());
        (d, s)
    }

    fn manifest(store: &Store, files: &[(&str, &str)]) -> Manifest {
        Manifest {
            entries: files
                .iter()
                .map(|(p, c)| {
                    let hash = store.put_bytes(c.as_bytes()).unwrap();
                    (p.as_bytes().to_vec(), MEntry::File { hash, exec: false, size: c.len() as u64 })
                })
                .collect(),
        }
    }

    #[test]
    fn seed_preference_order() {
        let (_d, s) = store();
        let incr = IncrStore::new(s.clone(), 1);
        let k = Hash::of(b"crate");
        let main = [1u8; 20];
        let side = [2u8; 20];
        assert!(incr.pick(&k, "/wt/a", &|_| None).is_none());
        incr.save(&k, "/wt/main", Some(main), &manifest(&s, &[("/i/x", "main state")]), 100);
        incr.save(&k, "/wt/side", Some(side), &manifest(&s, &[("/i/x", "side state")]), 100);
        let content = |m: &Manifest| match &m.entries[0].1 {
            MEntry::File { hash, .. } => String::from_utf8(s.read(hash).unwrap()).unwrap(),
            _ => unreachable!(),
        };
        let s1 = incr.pick(&k, "/wt/new", &|c| (*c == main).then_some(1)).unwrap();
        assert_eq!((content(&s1.manifest).as_str(), s1.kind), ("main state", SeedKind::Ancestor));
        assert_eq!(s1.blobs.len(), 1);
        let s2 = incr.pick(&k, "/wt/new", &|_| None).unwrap();
        assert_eq!((content(&s2.manifest).as_str(), s2.kind), ("side state", SeedKind::Other));
        incr.save(&k, "/wt/new", Some([3u8; 20]), &manifest(&s, &[("/i/x", "own state")]), 100);
        let at = |c: &[u8; 20]| if *c == [3u8; 20] { Some(0) } else if *c == main { Some(1) } else { None };
        let s3 = incr.pick(&k, "/wt/new", &at).unwrap();
        assert_eq!((content(&s3.manifest).as_str(), s3.kind), ("own state", SeedKind::SameWorktree));
        incr.save(&k, "/wt/other", Some([9u8; 20]), &manifest(&s, &[("/i/x", "one commit back")]), 100);
        let later = |c: &[u8; 20]| if *c == [9u8; 20] { Some(1) } else if *c == [3u8; 20] { Some(10) } else { None };
        let s5 = incr.pick(&k, "/wt/new", &later).unwrap();
        assert_eq!((content(&s5.manifest).as_str(), s5.kind), ("one commit back", SeedKind::Ancestor));
        incr.forget(&k, s5.id);
        incr.discard(&k, s3.id);
        let s4 = incr.pick(&k, "/wt/new", &|c| (*c == main).then_some(1)).unwrap();
        assert_eq!(content(&s4.manifest), "main state");
    }

    #[test]
    fn evicted_state_is_skipped() {
        let (_d, s) = store();
        let incr = IncrStore::new(s.clone(), 1);
        let k = Hash::of(b"crate");
        let m = manifest(&s, &[("/i/x", "will be evicted")]);
        incr.save(&k, "/wt/a", None, &m, 100);
        if let MEntry::File { hash, .. } = &m.entries[0].1 {
            s.remove(hash);
        }
        assert!(incr.pick(&k, "/wt/a", &|_| None).is_none());
        assert_eq!(incr.state_count(&k), 0);
    }

    #[test]
    fn state_with_files_on_a_worker_is_usable_while_the_worker_holds_them() {
        let (_d, s) = store();
        let incr = IncrStore::new(s.clone(), 1);
        let k = Hash::of(b"crate");
        let away = Hash::of(b"made on a worker, not fetched yet");
        let connected = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let c2 = connected.clone();
        incr.set_held(Box::new(move |h| *h == away && c2.load(Ordering::Relaxed)));
        let mut m = manifest(&s, &[("/i/here", "at home")]);
        m.entries.push((b"/i/there".to_vec(), MEntry::File { hash: away, exec: false, size: 33 }));
        incr.save(&k, "/wt/a", None, &m, 100);
        let seed = incr.pick(&k, "/wt/a", &|_| None).unwrap();
        assert!(seed.blobs.contains(&(away, 33)));
        assert_eq!(seed.blobs.iter().filter(|(h, _)| !s.has(h)).count(), 1);
        connected.store(false, Ordering::Relaxed);
        assert!(incr.pick(&k, "/wt/a", &|_| None).is_none());
        assert_eq!(incr.state_count(&k), 0);
    }

    #[test]
    fn changes_apply_over_seed() {
        let f = |n: u8| MEntry::File { hash: Hash([n; 32]), exec: false, size: 1 };
        let seed = Manifest {
            entries: vec![
                (b"/i/c/s-old/dep-graph.bin".to_vec(), f(1)),
                (b"/i/c/s-old/a.o".to_vec(), f(2)),
            ],
        };
        let changes = vec![
            (b"/i/c/s-new".to_vec(), MEntry::Dir),
            (b"/i/c/s-new/dep-graph.bin".to_vec(), f(3)),
            (b"/i/c/s-new/a.o".to_vec(), f(2)),
            (b"/i/c/s-old".to_vec(), MEntry::Whiteout),
            (b"/elsewhere/out.rlib".to_vec(), f(9)),
        ];
        let m = apply_changes(b"/i", Some(&seed), &changes);
        let paths: Vec<&[u8]> = m.entries.iter().map(|(p, _)| p.as_slice()).collect();
        assert_eq!(paths, vec![&b"/i/c/s-new"[..], b"/i/c/s-new/a.o", b"/i/c/s-new/dep-graph.bin"]);
    }

    #[test]
    fn pruning_keeps_latest_per_worktree() {
        let (_d, s) = store();
        let incr = IncrStore::new(s.clone(), 1);
        let k = Hash::of(b"crate");
        for round in 0..6 {
            for wt in ["/wt/a", "/wt/b"] {
                incr.save(&k, wt, None, &manifest(&s, &[("/i/x", &format!("{wt} {round}"))]), 100);
            }
        }
        assert_eq!(incr.state_count(&k), 2 + KEEP_EXTRA);
    }
}
