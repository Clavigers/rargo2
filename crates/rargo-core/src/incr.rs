#[cfg(test)]
use rargo_machine::StateMachine;
use crate::db::{Table, Tables};
use crate::hash::Hash;
use crate::store::{Blobs, Store};
use crate::trace::{MEntry, Manifest};
use crate::view::vpath_starts_with;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::cell::Cell;

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
    pub saved: Cell<u64>,
    pub seeded_same_worktree: Cell<u64>,
    pub seeded_ancestor: Cell<u64>,
    pub seeded_other: Cell<u64>,
    pub unseeded: Cell<u64>,
    pub discarded_after_crash: Cell<u64>,
}

pub struct IncrStore {
    db: Arc<dyn Tables>,
    store: Arc<dyn Blobs>,
    next: Cell<u64>,
    held: std::cell::OnceCell<Box<dyn Fn(&Hash) -> bool>>,
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
    pub fn new(db: Arc<dyn Tables>, store: Arc<dyn Blobs>, seed: u64) -> IncrStore {
        IncrStore {
            db,
            store,
            next: Cell::new(seed),
            held: std::cell::OnceCell::new(),
            stats: IncrStats::default(),
        }
    }

    pub fn set_held(&self, f: Box<dyn Fn(&Hash) -> bool>) {
        let _ = self.held.set(f);
    }

    pub fn list(&self, crate_key: &Hash) -> Vec<(u64, StateMeta)> {
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
                    counter.set(counter.get() + 1);
                    return Some(Seed { id: *id, manifest, kind, blobs });
                }
                None => self.db.delete(Table::Incr, &key(crate_key, *id)),
            }
        }
        self.stats.unseeded.set(self.stats.unseeded.get() + 1);
        None
    }

    pub fn save(&self, crate_key: &Hash, worktree: &str, commit: Option<[u8; 20]>, manifest: &Manifest, created: u64) {
        let Ok(raw) = bincode::serialize(manifest) else { return };
        let Ok(h) = self.store.put_bytes(&raw) else { return };
        let id = self.next.get();
        self.next.set(id + 1);
        assert!(id < u64::MAX, "state id counter does not wrap");
        let meta = StateMeta {
            manifest: h,
            worktree: worktree.to_string(),
            commit,
            created,
            bytes: manifest.total_size(),
        };
        self.db.put(Table::Incr, &key(crate_key, id), &bincode::serialize(&meta).unwrap());
        self.stats.saved.set(self.stats.saved.get() + 1);
        self.prune(crate_key);
    }

    fn prune(&self, crate_key: &Hash) {
        for id in doomed(&self.list(crate_key)) {
            self.db.delete(Table::Incr, &key(crate_key, id));
        }
    }

    pub fn discard(&self, crate_key: &Hash, id: u64) {
        self.db.delete(Table::Incr, &key(crate_key, id));
        self.stats.discarded_after_crash.set(self.stats.discarded_after_crash.get() + 1);
    }

    pub fn forget(&self, crate_key: &Hash, id: u64) {
        self.db.delete(Table::Incr, &key(crate_key, id));
    }

    /// The tables the states are recorded in and the store their files are in.
    pub fn parts(&self) -> (Arc<dyn Tables>, Arc<dyn Blobs>) {
        (self.db.clone(), self.store.clone())
    }

    pub fn mark(&self, live: &mut std::collections::BTreeSet<Hash>) {
        mark(&*self.db, &*self.store, live)
    }

    pub fn analyze(&self, store: &Store) -> String {
        analyze(&*self.db, store)
    }

    pub fn state_count(&self, crate_key: &Hash) -> usize {
        self.list(crate_key).len()
    }
}

/// Adds the incremental states recorded in `db`, and the files they are made of, to `live`.
pub fn mark(db: &dyn Tables, store: &dyn Blobs, live: &mut std::collections::BTreeSet<Hash>) {
    let mut metas: Vec<StateMeta> = Vec::new();
    for (_, v) in db.scan_prefix(Table::Incr, &[]) {
        if let Ok(m) = bincode::deserialize::<StateMeta>(&v) {
            metas.push(m);
        }
    }
    for meta in metas {
        live.insert(meta.manifest);
        let Ok(raw) = store.read(&meta.manifest) else { continue };
        let Ok(m) = bincode::deserialize::<Manifest>(&raw) else { continue };
        for (_, e) in &m.entries {
            if let MEntry::File { hash, .. } = e {
                live.insert(*hash);
            }
        }
    }
}

/// A table of what the incremental states recorded in `db` hold in `store`.
pub fn analyze(db: &dyn Tables, store: &Store) -> String {
    use std::collections::{BTreeMap, BTreeSet};
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
        blobs: BTreeSet<Hash>,
        unique: u64,
        chunks: BTreeSet<Hash>,
        chunk_unique: u64,
    }
    let mut states = 0u64;
    let mut crates: BTreeSet<[u8; 32]> = BTreeSet::new();
    let mut rows: BTreeMap<&'static str, Row> = BTreeMap::new();
    let mut entries: Vec<(Vec<u8>, StateMeta)> = Vec::new();
    for (k, v) in db.scan_prefix(Table::Incr, &[]) {
        if let Ok(m) = bincode::deserialize::<StateMeta>(&v) {
            entries.push((k, m));
        }
    }
    for (k, meta) in &entries {
        let Ok(raw) = store.read(&meta.manifest) else { continue };
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
            }
        }
    }
    for row in rows.values_mut() {
        for blob in &row.blobs {
            let Ok(f) = store.open_blob(blob) else { continue };
            let Ok(recipe) = crate::io::cpu(|| crate::chunk::chunk_file(&f)) else { continue };
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

#[cfg(test)]
impl StateMachine for IncrStore {
    fn invariants(&self) {
        let mut by_crate: BTreeMap<[u8; 32], Vec<u64>> = BTreeMap::new();
        for (k, _) in self.db.scan_prefix(Table::Incr, &[]) {
            if let (Some(c), Some(id)) = (k.get(..32), k.get(32..40)) {
                by_crate.entry(c.try_into().unwrap()).or_default().push(u64::from_be_bytes(id.try_into().unwrap()));
            }
        }
        let next = self.next.get();
        for (c, ids) in &by_crate {
            let listed = self.list(&Hash(*c));
            assert!(ids.iter().all(|id| *id < next), "every stored id {ids:?} was handed out, below {next}");
            assert_eq!(listed.len(), ids.len(), "the list of a crate is every entry stored for it");
            assert!(doomed(&listed).is_empty(), "nothing kept for a crate is due to be evicted");
        }
    }
}

fn doomed(newest_first: &[(u64, StateMeta)]) -> Vec<u64> {
    let mut seen_worktrees: Vec<&str> = Vec::new();
    let mut extra = 0;
    let mut out = Vec::new();
    for (id, meta) in newest_first {
        if !seen_worktrees.contains(&meta.worktree.as_str()) && seen_worktrees.len() < KEEP_WORKTREES {
            seen_worktrees.push(&meta.worktree);
            continue;
        }
        if extra < KEEP_EXTRA {
            extra += 1;
            continue;
        }
        out.push(*id);
    }
    out
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
    use crate::test_support::Transcript;
    use expect_test::expect;

    use crate::db::MemTables;
    use crate::io::sim;
    use crate::store::Store;

    fn blobs() -> Arc<Store> {
        let db = Arc::new(crate::db::Db::open_in_memory().unwrap());
        Arc::new(Store::open(&sim::tempdir().join("store"), db).unwrap())
    }

    fn store() -> (Arc<MemTables>, Arc<Store>) {
        (Arc::new(MemTables::default()), blobs())
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
        sim::test(|| {
            let mut outcomes = Transcript::default();
            let (db, s) = store();
            let incr = IncrStore::new(db.clone(), s.clone(), 1);
            let k = Hash::of(b"crate");
            let main = [1u8; 20];
            let side = [2u8; 20];
            assert!(incr.pick(&k, "/wt/a", &|_| None).is_none());
            incr.save(&k, "/wt/main", Some(main), &manifest(&s, &[("/i/x", "main state")]), 100);
            incr.invariants();
            incr.save(&k, "/wt/side", Some(side), &manifest(&s, &[("/i/x", "side state")]), 100);
            incr.invariants();
            let content = |m: &Manifest| match &m.entries[0].1 {
                MEntry::File { hash, .. } => String::from_utf8(s.read(hash).unwrap()).unwrap(),
                _ => unreachable!(),
            };
            let s1 = incr.pick(&k, "/wt/new", &|c| (*c == main).then_some(1)).unwrap();
            incr.invariants();
            outcomes.record("nearest ancestor seed", (content(&s1.manifest).as_str(), s1.kind));
            outcomes.record("seed blob count", s1.blobs.len());
            let s2 = incr.pick(&k, "/wt/new", &|_| None).unwrap();
            incr.invariants();
            outcomes.record("recent fallback seed", (content(&s2.manifest).as_str(), s2.kind));
            incr.save(&k, "/wt/new", Some([3u8; 20]), &manifest(&s, &[("/i/x", "own state")]), 100);
            incr.invariants();
            let at = |c: &[u8; 20]| if *c == [3u8; 20] { Some(0) } else if *c == main { Some(1) } else { None };
            let s3 = incr.pick(&k, "/wt/new", &at).unwrap();
            incr.invariants();
            outcomes.record("same commit seed", (content(&s3.manifest).as_str(), s3.kind));
            incr.save(&k, "/wt/other", Some([9u8; 20]), &manifest(&s, &[("/i/x", "one commit back")]), 100);
            incr.invariants();
            let later = |c: &[u8; 20]| if *c == [9u8; 20] { Some(1) } else if *c == [3u8; 20] { Some(10) } else { None };
            let s5 = incr.pick(&k, "/wt/new", &later).unwrap();
            incr.invariants();
            outcomes.record("closer ancestor seed", (content(&s5.manifest).as_str(), s5.kind));
            incr.forget(&k, s5.id);
            incr.invariants();
            incr.discard(&k, s3.id);
            incr.invariants();
            let s4 = incr.pick(&k, "/wt/new", &|c| (*c == main).then_some(1)).unwrap();
            incr.invariants();
            outcomes.record("content", content(&s4.manifest));
            expect![[r#"
                nearest ancestor seed: ("main state", Ancestor)
                seed blob count: 1
                recent fallback seed: ("side state", Other)
                same commit seed: ("own state", SameWorktree)
                closer ancestor seed: ("one commit back", Ancestor)
                content: "main state"
            "#]].assert_eq(&outcomes.0);
        });
    }

    #[test]
    fn evicted_state_is_skipped() {
        sim::test(|| {
            let mut outcomes = Transcript::default();
            let (db, s) = store();
            let incr = IncrStore::new(db.clone(), s.clone(), 1);
            let k = Hash::of(b"crate");
            let m = manifest(&s, &[("/i/x", "will be evicted")]);
            incr.save(&k, "/wt/a", None, &m, 100);
            incr.invariants();
            if let MEntry::File { hash, .. } = &m.entries[0].1 {
                s.remove(hash);
            }
            assert!(incr.pick(&k, "/wt/a", &|_| None).is_none());
            outcomes.record("state count", incr.state_count(&k));
            expect![[r#"
                state count: 0
            "#]].assert_eq(&outcomes.0);
        });
    }

    #[test]
    fn state_with_files_on_a_worker_is_usable_while_the_worker_holds_them() {
        sim::test(|| {
            let mut outcomes = Transcript::default();
            let (db, s) = store();
            let incr = IncrStore::new(db.clone(), s.clone(), 1);
            let k = Hash::of(b"crate");
            let worker = blobs();
            let away = worker.put_bytes(b"made on a worker, not fetched yet").unwrap();
            let on_worker = worker.clone();
            incr.set_held(Box::new(move |h| on_worker.has(h)));
            let mut m = manifest(&s, &[("/i/here", "at home")]);
            m.entries.push((b"/i/there".to_vec(), MEntry::File { hash: away, exec: false, size: 33 }));
            incr.save(&k, "/wt/a", None, &m, 100);
            incr.invariants();
            let seed = incr.pick(&k, "/wt/a", &|_| None).unwrap();
            incr.invariants();
            assert!(seed.blobs.contains(&(away, 33)));
            outcomes.record("blobs absent locally", seed.blobs.iter().filter(|(h, _)| !s.has(h)).count());
            worker.remove(&away);
            assert!(incr.pick(&k, "/wt/a", &|_| None).is_none());
            outcomes.record("state count", incr.state_count(&k));
            expect![[r#"
                blobs absent locally: 1
                state count: 0
            "#]].assert_eq(&outcomes.0);
        });
    }

    #[test]
    fn changes_apply_over_seed() {
        let mut outcomes = Transcript::default();
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
        outcomes.record("paths", paths);
        expect![[r#"
            paths: [[47, 105, 47, 99, 47, 115, 45, 110, 101, 119], [47, 105, 47, 99, 47, 115, 45, 110, 101, 119, 47, 97, 46, 111], [47, 105, 47, 99, 47, 115, 45, 110, 101, 119, 47, 100, 101, 112, 45, 103, 114, 97, 112, 104, 46, 98, 105, 110]]
        "#]].assert_eq(&outcomes.0);
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

    fn generated_saves(seeds: std::ops::RangeInclusive<u64>) {
        sim::test(move || {
            let worktrees: Vec<String> = (0..KEEP_WORKTREES as u64 + 4).map(|i| format!("/wt/{i}")).collect();
            let (db, s) = store();
            for seed in seeds {
                let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
                db.clear();
                let start = 1000 + rng.below(1000);
                let incr = IncrStore::new(db.clone(), s.clone(), start);
                let crates = [Hash::of(b"a"), Hash::of(b"b")];
                let mut given: BTreeMap<u64, (usize, String, u64)> = BTreeMap::new();
                let mut newest: BTreeMap<(usize, String), u64> = BTreeMap::new();
                let mut clock = 1_700_000_000u64;
                for step in 0..60 {
                    let c = rng.below(2) as usize;
                    let spread = if rng.below(3) == 0 { worktrees.len() as u64 } else { 3 };
                    let wt = worktrees[rng.below(spread) as usize].clone();
                    match rng.below(6) {
                        0 => {
                            if let Some(seed) = incr.pick(&crates[c], &wt, &|_| None) {
                                incr.forget(&crates[c], seed.id);
                                newest.retain(|_, id| *id != seed.id);
                            }
                        }
                        _ => {
                            clock += rng.below(5000);
                            let id = start + given.len() as u64;
                            incr.save(&crates[c], &wt, None, &manifest(&s, &[("/i/x", &format!("{seed} {step}"))]), clock);
                            given.insert(id, (c, wt.clone(), clock));
                            newest.insert((c, wt), id);
                        }
                    }
                    rargo_machine::check(&incr, format_args!("seed {seed} step {step}"));
                    for (c, k) in crates.iter().enumerate() {
                        let listed = incr.list(k);
                        let distinct: std::collections::BTreeSet<&str> = listed.iter().map(|(_, m)| m.worktree.as_str()).collect();
                        assert!(listed.len() <= distinct.len().min(KEEP_WORKTREES) + KEEP_EXTRA, "seed {seed}: {} states", listed.len());
                        for (id, meta) in &listed {
                            let expected = given.get(id).map(|(crate_index, worktree, created)| (*crate_index, worktree.as_str(), *created));
                            assert_eq!(expected, Some((c, meta.worktree.as_str(), meta.created)), "seed {seed}: state {id} keeps the time it was saved at");
                        }
                        let mut kept_worktrees: Vec<&str> = Vec::new();
                        for (id, meta) in &listed {
                            if !kept_worktrees.contains(&meta.worktree.as_str()) {
                                kept_worktrees.push(&meta.worktree);
                                if let Some(n) = newest.get(&(c, meta.worktree.clone())).filter(|_| kept_worktrees.len() <= KEEP_WORKTREES) {
                                    assert_eq!(n, id, "seed {seed}: a worktree's newest state is the one kept first");
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    mod generated_saves_keep_each_worktrees_newest_state_and_time {
        use super::generated_saves;

        rargo_machine::test_batches! {
            generated_saves;
            seeds_1_to_5: 1..=5,
            seeds_6_to_10: 6..=10,
            seeds_11_to_15: 11..=15,
            seeds_16_to_20: 16..=20,
            seeds_21_to_25: 21..=25,
            seeds_26_to_30: 26..=30,
            seeds_31_to_35: 31..=35,
            seeds_36_to_40: 36..=40,
            seeds_41_to_45: 41..=45,
            seeds_46_to_50: 46..=50,
            seeds_51_to_55: 51..=55,
            seeds_56_to_60: 56..=60,
            seeds_61_to_65: 61..=65,
            seeds_66_to_70: 66..=70,
            seeds_71_to_75: 71..=75,
            seeds_76_to_80: 76..=80,
            seeds_81_to_85: 81..=85,
            seeds_86_to_90: 86..=90,
            seeds_91_to_95: 91..=95,
            seeds_96_to_100: 96..=100,
            seeds_101_to_105: 101..=105,
            seeds_106_to_110: 106..=110,
            seeds_111_to_115: 111..=115,
            seeds_116_to_120: 116..=120,
            seeds_121_to_125: 121..=125,
            seeds_126_to_130: 126..=130,
            seeds_131_to_135: 131..=135,
            seeds_136_to_140: 136..=140,
            seeds_141_to_145: 141..=145,
            seeds_146_to_150: 146..=150,
            seeds_151_to_155: 151..=155,
            seeds_156_to_160: 156..=160,
            seeds_161_to_165: 161..=165,
            seeds_166_to_170: 166..=170,
            seeds_171_to_175: 171..=175,
            seeds_176_to_180: 176..=180,
            seeds_181_to_185: 181..=185,
            seeds_186_to_190: 186..=190,
            seeds_191_to_195: 191..=195,
            seeds_196_to_200: 196..=200,
            seeds_201_to_205: 201..=205,
            seeds_206_to_210: 206..=210,
            seeds_211_to_215: 211..=215,
            seeds_216_to_220: 216..=220,
            seeds_221_to_225: 221..=225,
            seeds_226_to_230: 226..=230,
            seeds_231_to_235: 231..=235,
            seeds_236_to_240: 236..=240,
            seeds_241_to_245: 241..=245,
            seeds_246_to_250: 246..=250,
            seeds_251_to_255: 251..=255,
            seeds_256_to_260: 256..=260,
            seeds_261_to_265: 261..=265,
            seeds_266_to_270: 266..=270,
            seeds_271_to_275: 271..=275,
            seeds_276_to_280: 276..=280,
            seeds_281_to_285: 281..=285,
            seeds_286_to_290: 286..=290,
            seeds_291_to_295: 291..=295,
            seeds_296_to_300: 296..=300,
        }
    }

    #[test]
    fn pruning_keeps_latest_per_worktree() {
        sim::test(|| {
            let (db, s) = store();
            let incr = IncrStore::new(db.clone(), s.clone(), 1);
            let k = Hash::of(b"crate");
            for round in 0..6 {
                for wt in ["/wt/a", "/wt/b"] {
                    incr.save(&k, wt, None, &manifest(&s, &[("/i/x", &format!("{wt} {round}"))]), 100);
                    incr.invariants();
                }
            }
            assert_eq!(incr.state_count(&k), 2 + KEEP_EXTRA);
        });
    }
}
