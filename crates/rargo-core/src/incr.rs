//! Shared incremental compilation state.
//!
//! rustc validates every piece of an incremental session directory against
//! the current source before reusing it, so any state is a safe starting
//! point for any source; a closer one is just faster. After a compile, the
//! crate's incremental directory is stored as an immutable manifest. Before
//! the next compile of that crate, anywhere, the best available state is
//! shown to rustc as a private copy-on-write layer.
//!
//! "Best": the same worktree's last state, else the newest state made at an
//! ancestor of the worktree's HEAD, else the newest state of all.

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
    /// Blob holding the encoded [`Manifest`] of the incremental directory.
    pub manifest: Hash,
    pub worktree: String,
    /// HEAD commit of the worktree when the state was made, if any.
    pub commit: Option<[u8; 20]>,
    pub created: u64,
    pub bytes: u64,
}

/// How many states to keep per crate besides the newest of each worktree.
const KEEP_EXTRA: usize = 2;
/// Worktrees whose latest state is kept, per crate.
const KEEP_WORKTREES: usize = 32;

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
    pub stats: IncrStats,
}

fn key(crate_key: &Hash, id: u64) -> Vec<u8> {
    let mut k = crate_key.0.to_vec();
    k.extend_from_slice(&id.to_be_bytes());
    k
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Which state was chosen, for statistics and tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeedKind {
    SameWorktree,
    Ancestor,
    Other,
}

impl IncrStore {
    pub fn new(store: Arc<Store>) -> IncrStore {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(1);
        IncrStore { db: store.db().clone(), store, next: AtomicU64::new(seed), stats: IncrStats::default() }
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
        // Newest first.
        v.sort_by(|a, b| b.0.cmp(&a.0));
        v
    }

    fn load(&self, meta: &StateMeta) -> Option<Arc<Manifest>> {
        let raw = self.store.read(&meta.manifest).ok()?;
        let m: Manifest = bincode::deserialize(&raw).ok()?;
        // Every file must still be there; states are evicted freely.
        for (_, e) in &m.entries {
            if let MEntry::File { hash, .. } = e {
                if !self.store.has(hash) {
                    return None;
                }
            }
        }
        for (_, e) in &m.entries {
            if let MEntry::File { hash, .. } = e {
                self.store.touch(hash);
            }
        }
        Some(Arc::new(m))
    }

    /// Picks a seed for `crate_key`. `is_ancestor(c)` says whether commit
    /// `c` is an ancestor of (or equal to) the requesting worktree's HEAD.
    pub fn pick(
        &self,
        crate_key: &Hash,
        worktree: &str,
        is_ancestor: &dyn Fn(&[u8; 20]) -> bool,
    ) -> Option<(u64, Arc<Manifest>, SeedKind)> {
        let all = self.list(crate_key);
        let mut order: Vec<(&(u64, StateMeta), SeedKind)> = Vec::new();
        order.extend(all.iter().filter(|(_, m)| m.worktree == worktree).map(|s| (s, SeedKind::SameWorktree)));
        order.extend(
            all.iter()
                .filter(|(_, m)| m.worktree != worktree && m.commit.as_ref().is_some_and(is_ancestor))
                .map(|s| (s, SeedKind::Ancestor)),
        );
        order.extend(all.iter().map(|s| (s, SeedKind::Other)));
        for ((id, meta), kind) in order {
            match self.load(meta) {
                Some(m) => {
                    let counter = match kind {
                        SeedKind::SameWorktree => &self.stats.seeded_same_worktree,
                        SeedKind::Ancestor => &self.stats.seeded_ancestor,
                        SeedKind::Other => &self.stats.seeded_other,
                    };
                    counter.fetch_add(1, Ordering::Relaxed);
                    return Some((*id, m, kind));
                }
                None => self.db.delete(Table::Incr, &key(crate_key, *id)),
            }
        }
        self.stats.unseeded.fetch_add(1, Ordering::Relaxed);
        None
    }

    /// Stores the state `manifest` (the full content of the incremental
    /// directory after a compile).
    pub fn save(&self, crate_key: &Hash, worktree: &str, commit: Option<[u8; 20]>, manifest: &Manifest) {
        let Ok(raw) = bincode::serialize(manifest) else { return };
        let Ok(h) = self.store.put_bytes(&raw) else { return };
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let meta = StateMeta {
            manifest: h,
            worktree: worktree.to_string(),
            commit,
            created: now(),
            bytes: manifest.total_size(),
        };
        self.db.put(Table::Incr, &key(crate_key, id), &bincode::serialize(&meta).unwrap());
        self.stats.saved.fetch_add(1, Ordering::Relaxed);
        self.prune(crate_key);
    }

    /// Keeps the newest state of each recent worktree plus a few others.
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
    }

    /// Forgets a state rustc crashed on.
    pub fn discard(&self, crate_key: &Hash, id: u64) {
        self.db.delete(Table::Incr, &key(crate_key, id));
        self.stats.discarded_after_crash.fetch_add(1, Ordering::Relaxed);
    }

    pub fn state_count(&self, crate_key: &Hash) -> usize {
        self.list(crate_key).len()
    }
}

/// The content of the incremental directory after an action: the seed with
/// the action's changes (additions, replacements, deletions) applied.
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
                // A deleted directory takes everything below it along.
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
        let incr = IncrStore::new(s.clone());
        let k = Hash::of(b"crate");
        let main = [1u8; 20];
        let side = [2u8; 20];
        assert!(incr.pick(&k, "/wt/a", &|_| false).is_none());
        incr.save(&k, "/wt/main", Some(main), &manifest(&s, &[("/i/x", "main state")]));
        incr.save(&k, "/wt/side", Some(side), &manifest(&s, &[("/i/x", "side state")]));
        let content = |m: &Manifest| match &m.entries[0].1 {
            MEntry::File { hash, .. } => String::from_utf8(s.read(hash).unwrap()).unwrap(),
            _ => unreachable!(),
        };
        // A new worktree branched from main: the ancestor's state wins over
        // the newer unrelated one.
        let (_, m, kind) = incr.pick(&k, "/wt/new", &|c| *c == main).unwrap();
        assert_eq!((content(&m).as_str(), kind), ("main state", SeedKind::Ancestor));
        // With no ancestry information, the newest of all.
        let (_, m, kind) = incr.pick(&k, "/wt/new", &|_| false).unwrap();
        assert_eq!((content(&m).as_str(), kind), ("side state", SeedKind::Other));
        // Once the worktree has its own state, that one.
        incr.save(&k, "/wt/new", Some([3u8; 20]), &manifest(&s, &[("/i/x", "own state")]));
        let (id, m, kind) = incr.pick(&k, "/wt/new", &|c| *c == main).unwrap();
        assert_eq!((content(&m).as_str(), kind), ("own state", SeedKind::SameWorktree));
        // A state rustc crashed on is not offered again.
        incr.discard(&k, id);
        let (_, m, _) = incr.pick(&k, "/wt/new", &|c| *c == main).unwrap();
        assert_eq!(content(&m), "main state");
    }

    #[test]
    fn evicted_state_is_skipped() {
        let (_d, s) = store();
        let incr = IncrStore::new(s.clone());
        let k = Hash::of(b"crate");
        let m = manifest(&s, &[("/i/x", "will be evicted")]);
        incr.save(&k, "/wt/a", None, &m);
        if let MEntry::File { hash, .. } = &m.entries[0].1 {
            s.remove(hash);
        }
        assert!(incr.pick(&k, "/wt/a", &|_| false).is_none());
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
        let incr = IncrStore::new(s.clone());
        let k = Hash::of(b"crate");
        for round in 0..6 {
            for wt in ["/wt/a", "/wt/b"] {
                incr.save(&k, wt, None, &manifest(&s, &[("/i/x", &format!("{wt} {round}"))]));
            }
        }
        // Latest of a, latest of b, and KEEP_EXTRA older ones.
        assert_eq!(incr.state_count(&k), 2 + KEEP_EXTRA);
    }
}
