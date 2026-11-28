use rargo_machine::StateMachine;
use crate::chunk::ChunkRef;
use crate::hash::Hash;
use std::collections::{BTreeMap, BTreeSet};

pub const COLD_MIN_SIZE: u64 = 64 << 10;
const TOUCH_PERSIST_SECS: u64 = 600;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tier {
    Hot { mtime_ns: i64 },
    Freezing { mtime_ns: i64, seen_used: u64 },
    Cold { added: u64, recipe: Vec<ChunkRef> },
    Thawing { added: u64, recipe: Vec<ChunkRef>, thawers: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blob {
    pub size: u64,
    pub last_used: u64,
    pub tier: Tier,
}

impl Blob {
    fn is_whole(&self) -> bool {
        matches!(self.tier, Tier::Hot { .. } | Tier::Freezing { .. })
    }

    fn is_moving(&self) -> bool {
        matches!(self.tier, Tier::Freezing { .. } | Tier::Thawing { .. })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HotMeta {
    pub size: u64,
    pub mtime_ns: i64,
    pub last_used: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColdRecord {
    pub size: u64,
    pub last_used: u64,
    pub added: u64,
    pub recipe: Vec<ChunkRef>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Put {
    pub new: bool,
    pub was_cold: bool,
    pub meta: HotMeta,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Touch {
    Absent,
    Quiet,
    Persist(HotMeta),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Forgot {
    pub hot: bool,
    pub cold: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FreezeStart {
    pub size: u64,
    pub mtime_ns: i64,
    pub last_used: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Thaw {
    NotCold,
    Build,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Evicted {
    pub hash: Hash,
    pub cold: bool,
    pub freed: u64,
}

#[derive(Default, Debug, Clone)]
pub struct StoreIndex {
    blobs: BTreeMap<Hash, Blob>,
    pins: BTreeMap<Hash, u32>,
    generation: u64,
    whole_bytes: u64,
    cold_disk: u64,
    visited: std::cell::Cell<u64>,
}

impl StoreIndex {
    pub fn new() -> StoreIndex {
        StoreIndex { generation: 1, ..StoreIndex::default() }
    }

    pub fn load_hot(&mut self, h: Hash, m: HotMeta) {
        let old = self
            .blobs
            .insert(h, Blob { size: m.size, last_used: m.last_used, tier: Tier::Hot { mtime_ns: m.mtime_ns } });
        if let Some(old) = old {
            self.unaccount(&old);
        }
        self.whole_bytes += m.size;
        assert!(self.hot(&h) == Some(m), "{h} is loaded hot as recorded");
    }

    pub fn load_cold(&mut self, h: Hash, c: ColdRecord) {
        if self.blobs.get(&h).is_some_and(Blob::is_whole) {
            return;
        }
        let whole = self.whole_bytes;
        let size = c.size;
        self.blobs.insert(
            h,
            Blob { size: c.size, last_used: c.last_used, tier: Tier::Cold { added: c.added, recipe: c.recipe } },
        );
        assert!(self.is_cold(&h) && self.size_of(&h) == Some(size), "{h} is loaded cold as recorded");
        assert_eq!(self.whole_bytes, whole, "a cold record replaces no whole blob");
    }

    pub fn set_cold_disk(&mut self, bytes: u64) {
        self.cold_disk = bytes;
        assert_eq!(self.cold_disk, bytes);
    }

    pub fn chunks_written(&mut self, bytes: u64) {
        assert!(self.cold_disk.checked_add(bytes).is_some(), "cold disk bytes do not overflow");
        self.cold_disk += bytes;
    }

    fn unaccount(&mut self, b: &Blob) {
        if b.is_whole() {
            assert!(self.whole_bytes >= b.size, "whole bytes do not go negative");
            self.whole_bytes -= b.size;
        }
    }

    /// Every blob, counted into `visited`: each walk of the index goes through here.
    fn walk(&self) -> std::collections::btree_map::Iter<'_, Hash, Blob> {
        self.visited.set(self.visited.get() + self.blobs.len() as u64);
        self.blobs.iter()
    }

    /// How many blobs the walks of the index have gone through, so a test can bound what a pass costs.
    pub fn visited(&self) -> u64 {
        self.visited.get()
    }

    pub fn cold_disk(&self) -> u64 {
        self.cold_disk
    }

    pub fn has(&self, h: &Hash) -> bool {
        self.blobs.contains_key(h)
    }

    pub fn size_of(&self, h: &Hash) -> Option<u64> {
        self.blobs.get(h).map(|b| b.size)
    }

    pub fn is_whole(&self, h: &Hash) -> bool {
        self.blobs.get(h).is_some_and(Blob::is_whole)
    }

    pub fn is_cold(&self, h: &Hash) -> bool {
        self.blobs.get(h).is_some_and(|b| matches!(b.tier, Tier::Cold { .. } | Tier::Thawing { .. }))
    }

    pub fn hot(&self, h: &Hash) -> Option<HotMeta> {
        let b = self.blobs.get(h)?;
        match b.tier {
            Tier::Hot { mtime_ns } | Tier::Freezing { mtime_ns, .. } => {
                Some(HotMeta { size: b.size, mtime_ns, last_used: b.last_used })
            }
            _ => None,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn total_bytes(&self) -> u64 {
        self.whole_bytes + self.cold_disk
    }

    pub fn cold_stats(&self) -> (usize, u64, u64) {
        let cold = self.walk().map(|(_, b)| b).filter(|b| !b.is_whole());
        let (n, logical) = cold.fold((0, 0), |(n, l), b| (n + 1, l + b.size));
        (n, logical, self.cold_disk)
    }

    pub fn all_hashes(&self) -> Vec<Hash> {
        self.walk().map(|(h, _)| *h).collect()
    }

    pub fn whole_count(&self) -> usize {
        self.walk().filter(|(_, b)| b.is_whole()).count()
    }

    pub fn live_chunks(&self) -> BTreeSet<Hash> {
        let mut live = BTreeSet::new();
        for (_, b) in self.walk() {
            if let Tier::Cold { recipe, .. } | Tier::Thawing { recipe, .. } = &b.tier {
                live.extend(recipe.iter().map(|c| c.hash));
            }
        }
        live
    }

    pub fn chunks_collected(&mut self, kept: u64) {
        self.cold_disk = kept;
        assert_eq!(self.cold_disk, kept);
    }

    pub fn put(&mut self, h: Hash, size: u64, mtime_ns: i64, now: u64) -> Put {
        let meta = HotMeta { size, mtime_ns, last_used: now };
        let old = self.blobs.insert(h, Blob { size, last_used: now, tier: Tier::Hot { mtime_ns } });
        let (new, was_cold) = match &old {
            None => (true, false),
            Some(o) => {
                assert_eq!(o.size, size, "one hash, one size");
                (!o.is_whole(), !o.is_whole())
            }
        };
        if new {
            self.whole_bytes += size;
        }
        Put { new, was_cold, meta }
    }

    pub fn touch(&mut self, h: &Hash, now: u64) -> Touch {
        let Some(b) = self.blobs.get_mut(h) else {
            return Touch::Absent;
        };
        let stale = now.saturating_sub(b.last_used) > TOUCH_PERSIST_SECS;
        b.last_used = now;
        let out = match b.tier {
            Tier::Hot { mtime_ns } if stale => Touch::Persist(HotMeta { size: b.size, mtime_ns, last_used: now }),
            _ => Touch::Quiet,
        };
        assert_eq!(self.blobs[h].last_used, now, "a touched blob was used now");
        out
    }

    pub fn forget(&mut self, h: &Hash) -> Forgot {
        let Some(b) = self.blobs.remove(h) else {
            return Forgot { hot: false, cold: false };
        };
        let generation = self.generation;
        self.unaccount(&b);
        self.generation += 1;
        assert!(!self.has(h) && self.generation > generation, "{h} is forgotten in a new generation");
        Forgot { hot: b.is_whole(), cold: !b.is_whole() }
    }

    pub fn freeze_candidates(&self, now: u64, min_idle_secs: u64) -> Vec<Hash> {
        let cutoff = now.saturating_sub(min_idle_secs);
        let mut idle: Vec<(u64, Hash)> = self
            .walk()
            .filter(|(_, b)| matches!(b.tier, Tier::Hot { .. }) && b.last_used <= cutoff && b.size >= COLD_MIN_SIZE)
            .map(|(h, b)| (b.last_used, *h))
            .collect();
        idle.sort_unstable();
        idle.into_iter().map(|(_, h)| h).collect()
    }

    pub fn begin_freeze(&mut self, h: &Hash) -> Option<FreezeStart> {
        let b = self.blobs.get_mut(h)?;
        let Tier::Hot { mtime_ns } = b.tier else {
            return None;
        };
        if b.size < COLD_MIN_SIZE {
            return None;
        }
        b.tier = Tier::Freezing { mtime_ns, seen_used: b.last_used };
        assert!(b.is_whole() && b.is_moving(), "a freezing blob is still whole");
        Some(FreezeStart { size: b.size, mtime_ns, last_used: b.last_used })
    }

    pub fn abort_freeze(&mut self, h: &Hash) {
        if let Some(b) = self.blobs.get_mut(h)
            && let Tier::Freezing { mtime_ns, .. } = b.tier
        {
            b.tier = Tier::Hot { mtime_ns };
        }
        assert!(!self.blobs.get(h).is_some_and(|b| matches!(b.tier, Tier::Freezing { .. })), "{h} is not freezing");
    }

    pub fn finish_freeze(&mut self, h: &Hash, recipe: Vec<ChunkRef>, added: u64) -> Option<ColdRecord> {
        let b = self.blobs.get_mut(h)?;
        let Tier::Freezing { mtime_ns, seen_used } = b.tier else {
            return None;
        };
        if b.last_used != seen_used {
            b.tier = Tier::Hot { mtime_ns };
            return None;
        }
        assert_eq!(recipe.iter().map(|c| c.len as u64).sum::<u64>(), b.size, "the recipe covers the blob");
        let size = b.size;
        b.tier = Tier::Cold { added, recipe: recipe.clone() };
        assert!(self.whole_bytes >= size, "whole bytes do not go negative");
        self.whole_bytes -= size;
        Some(ColdRecord { size, last_used: seen_used, added, recipe })
    }

    pub fn begin_thaw(&mut self, h: &Hash) -> (Thaw, Option<(u64, Vec<ChunkRef>)>) {
        let Some(b) = self.blobs.get_mut(h) else {
            return (Thaw::NotCold, None);
        };
        let tier = std::mem::replace(&mut b.tier, Tier::Hot { mtime_ns: 0 });
        let (next, out) = match tier {
            Tier::Cold { added, recipe } => {
                let out = (b.size, recipe.clone());
                (Tier::Thawing { added, recipe, thawers: 1 }, Some(out))
            }
            Tier::Thawing { added, recipe, thawers } => {
                let out = (b.size, recipe.clone());
                (Tier::Thawing { added, recipe, thawers: thawers + 1 }, Some(out))
            }
            whole => (whole, None),
        };
        b.tier = next;
        assert_eq!(out.is_some(), matches!(b.tier, Tier::Thawing { .. }), "only a cold blob thaws");
        match out {
            Some(o) => (Thaw::Build, Some(o)),
            None => (Thaw::NotCold, None),
        }
    }

    pub fn abort_thaw(&mut self, h: &Hash) {
        let Some(b) = self.blobs.get_mut(h) else { return };
        if let Tier::Thawing { added, recipe, thawers } = &mut b.tier {
            assert!(*thawers > 0, "a thawing blob has a thawer to abort");
            if *thawers > 1 {
                *thawers -= 1;
            } else {
                b.tier = Tier::Cold { added: *added, recipe: std::mem::take(recipe) };
            }
        }
        assert!(!matches!(b.tier, Tier::Thawing { thawers: 0, .. }), "{h} has a thawer while it thaws");
    }

    pub fn fail_thaw(&mut self, h: &Hash) -> bool {
        let thawing = self.blobs.get(h).is_some_and(|b| matches!(b.tier, Tier::Thawing { .. } | Tier::Cold { .. }));
        if thawing {
            self.forget(h);
        }
        assert!(!(thawing && self.has(h)), "a failed thaw forgets {h}");
        thawing
    }

    pub fn pin_all(&mut self, hashes: &[Hash]) -> bool {
        if !hashes.iter().all(|h| self.has(h)) {
            return false;
        }
        for h in hashes {
            *self.pins.entry(*h).or_insert(0) += 1;
        }
        assert!(hashes.iter().all(|h| self.is_pinned(h)), "every hash is pinned");
        true
    }

    pub fn unpin_all(&mut self, hashes: &[Hash]) {
        for h in hashes {
            let n = self.pins.get_mut(h).unwrap_or_else(|| panic!("unpinning {h}, which is not pinned"));
            assert!(*n > 0, "a pin count of zero is removed");
            *n -= 1;
            if *n == 0 {
                self.pins.remove(h);
            }
        }
    }

    pub fn is_pinned(&self, h: &Hash) -> bool {
        self.pins.contains_key(h)
    }

    pub fn evict(&mut self, budget: u64, min_age_secs: u64, now: u64, pinned: &dyn Fn(&Hash) -> bool) -> Vec<Evicted> {
        let total = self.total_bytes();
        if total <= budget {
            return Vec::new();
        }
        let cutoff = now.saturating_sub(min_age_secs);
        let mut all: Vec<(u64, Hash)> =
            self.walk().filter(|(_, b)| !b.is_moving()).map(|(h, b)| (b.last_used, *h)).collect();
        all.sort_unstable();
        let mut freed = 0u64;
        let mut out = Vec::new();
        for (used, h) in all {
            if total.saturating_sub(freed) <= budget {
                break;
            }
            if self.is_pinned(&h) || pinned(&h) || (min_age_secs > 0 && used > cutoff) {
                continue;
            }
            let b = &self.blobs[&h];
            let (cold, bytes) = match b.tier {
                Tier::Cold { added, .. } => (true, added),
                _ => (false, b.size),
            };
            self.forget(&h);
            freed += bytes;
            out.push(Evicted { hash: h, cold, freed: bytes });
        }
        assert!(out.iter().all(|e| !self.has(&e.hash) && !self.is_pinned(&e.hash)), "evicted blobs are unpinned and gone");
        out
    }

    pub fn sweep(&mut self, live: &dyn Fn(&Hash) -> bool, min_age_secs: u64, now: u64) -> Vec<Evicted> {
        let cutoff = now.saturating_sub(min_age_secs);
        let dead: Vec<Hash> = self
            .walk()
            .filter(|(h, b)| {
                !b.is_moving() && !live(h) && (min_age_secs == 0 || b.last_used <= cutoff) && !self.is_pinned(h)
            })
            .map(|(h, _)| *h)
            .collect();
        let mut out = Vec::new();
        for h in dead {
            let b = &self.blobs[&h];
            let (cold, freed) = match b.tier {
                Tier::Cold { .. } => (true, 0),
                _ => (false, b.size),
            };
            self.forget(&h);
            out.push(Evicted { hash: h, cold, freed });
        }
        out.sort_by_key(|e| e.cold);
        assert!(out.iter().all(|e| !self.has(&e.hash) && !self.is_pinned(&e.hash)), "swept blobs are unpinned and gone");
        out
    }
}

impl StateMachine for StoreIndex {
    fn invariants(&self) {
        let whole: u64 = self.blobs.values().filter(|b| b.is_whole()).map(|b| b.size).sum();
        assert_eq!(whole, self.whole_bytes, "whole bytes are the sum of the whole blobs");
        assert!(self.generation >= 1);
        for (h, n) in &self.pins {
            assert!(*n > 0, "pin of {h} has a count");
        }
        for (h, b) in &self.blobs {
            match &b.tier {
                Tier::Cold { recipe, .. } | Tier::Thawing { recipe, .. } => {
                    assert_eq!(recipe.iter().map(|c| c.len as u64).sum::<u64>(), b.size, "the recipe of {h} covers it");
                }
                Tier::Freezing { .. } | Tier::Hot { .. } => {}
            }
            if let Tier::Thawing { thawers, .. } = b.tier {
                assert!(thawers > 0, "a thawing blob has a thawer");
            }
            if let Tier::Freezing { .. } = b.tier {
                assert!(b.size >= COLD_MIN_SIZE, "freezing {h} is big enough to go cold");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Transcript;
    use expect_test::expect;

    fn h(i: u8) -> Hash {
        Hash([i; 32])
    }

    fn recipe(size: u64) -> Vec<ChunkRef> {
        let mut out = Vec::new();
        let mut left = size;
        let mut i = 0u8;
        while left > 0 {
            let len = left.min(40_000);
            out.push(ChunkRef { hash: Hash([i.wrapping_add(200); 32]), len: len as u32 });
            left -= len;
            i = i.wrapping_add(1);
        }
        out
    }

    #[test]
    fn eviction_is_lru_and_respects_pins() {
        let mut outcomes = Transcript::default();
        let mut ix = StoreIndex::new();
        for (i, used) in [(1, 10), (2, 20), (3, 30)] {
            ix.put(h(i), 1000, 0, used);
        }
        let gone = ix.evict(1500, 0, 100, &|x| *x == h(1));
        outcomes.record("evictions", &gone);
        outcomes.record("bytes freed", gone.iter().map(|e| e.freed).sum::<u64>());
        assert!(ix.has(&h(1)) && !ix.has(&h(2)) && !ix.has(&h(3)));
        outcomes.record("generation", ix.generation());
        ix.invariants();
        expect![[r#"
            evictions: [Evicted { hash: 0202020202020202, cold: false, freed: 1000 }, Evicted { hash: 0303030303030303, cold: false, freed: 1000 }]
            bytes freed: 2000
            generation: 3
        "#]].assert_eq(&outcomes.0);
    }

    #[test]
    fn going_cold_and_back_keeps_a_blob_present_and_the_generation() {
        let mut ix = StoreIndex::new();
        let size = 1 << 20;
        ix.put(h(1), size, 7, 5);
        for _ in 0..3 {
            let g = ix.generation();
            let start = ix.begin_freeze(&h(1)).expect("a hot blob can go cold");
            assert_eq!(start.size, size);
            assert!(ix.has(&h(1)) && ix.size_of(&h(1)) == Some(size));
            let rec = ix.finish_freeze(&h(1), recipe(size), 1000).expect("nothing used it meanwhile");
            assert_eq!(rec.added, 1000);
            assert!(ix.is_cold(&h(1)) && ix.has(&h(1)));
            assert_eq!(ix.total_bytes(), 0, "no chunk bytes were written");
            let (thaw, r) = ix.begin_thaw(&h(1));
            assert_eq!(thaw, Thaw::Build);
            assert_eq!(r.unwrap().0, size);
            assert!(ix.has(&h(1)));
            let put = ix.put(h(1), size, 8, 6);
            assert!(put.was_cold);
            assert!(ix.is_whole(&h(1)));
            assert_eq!(ix.generation(), g);
            ix.invariants();
        }
    }

    #[test]
    fn a_blob_used_while_going_cold_stays_whole() {
        let mut outcomes = Transcript::default();
        let mut ix = StoreIndex::new();
        ix.put(h(1), 1 << 20, 0, 5);
        ix.begin_freeze(&h(1)).unwrap();
        outcomes.record("touch", ix.touch(&h(1), 9));
        outcomes.record("finish freeze", ix.finish_freeze(&h(1), recipe(1 << 20), 10));
        assert!(ix.is_whole(&h(1)));
        assert!(ix.begin_freeze(&h(2)).is_none());
        ix.put(h(3), 100, 0, 1);
        assert!(ix.begin_freeze(&h(3)).is_none(), "a small blob stays whole");
        ix.invariants();
        expect![[r#"
            touch: Quiet
            finish freeze: None
        "#]].assert_eq(&outcomes.0);
    }

    #[test]
    fn a_failed_thaw_drops_the_blob() {
        let mut outcomes = Transcript::default();
        let mut ix = StoreIndex::new();
        ix.put(h(1), 1 << 20, 0, 5);
        ix.begin_freeze(&h(1)).unwrap();
        ix.finish_freeze(&h(1), recipe(1 << 20), 10).unwrap();
        ix.begin_thaw(&h(1));
        outcomes.record("generation before failed thaw", ix.generation());
        assert!(ix.fail_thaw(&h(1)));
        assert!(!ix.has(&h(1)));
        outcomes.record("generation", ix.generation());
        ix.invariants();
        expect![[r#"
            generation before failed thaw: 1
            generation: 2
        "#]].assert_eq(&outcomes.0);
    }

    #[test]
    fn touch_persists_only_after_ten_minutes() {
        let mut outcomes = Transcript::default();
        let mut ix = StoreIndex::new();
        ix.put(h(1), 10, 3, 100);
        outcomes.record("touch", ix.touch(&h(1), 200));
        outcomes.record("touch", ix.touch(&h(1), 801));
        outcomes.record("touch", ix.touch(&h(2), 801));
        expect![[r#"
            touch: Quiet
            touch: Persist(HotMeta { size: 10, mtime_ns: 3, last_used: 801 })
            touch: Absent
        "#]].assert_eq(&outcomes.0);
    }

    #[test]
    fn sweep_drops_what_is_not_live_and_not_pinned() {
        let mut outcomes = Transcript::default();
        let mut ix = StoreIndex::new();
        ix.put(h(1), 10, 0, 1);
        ix.put(h(2), 20, 0, 1);
        ix.put(h(3), 30, 0, 1000);
        assert!(ix.pin_all(&[h(2)]));
        let gone = ix.sweep(&|x| *x == h(4), 100, 1050);
        outcomes.record("gone", gone);
        ix.unpin_all(&[h(2)]);
        let gone = ix.sweep(&|_| false, 0, 1050);
        outcomes.record("sweep after unpin", gone);
        assert!(!ix.pin_all(&[h(1)]));
        ix.invariants();
        expect![[r#"
            gone: [Evicted { hash: 0101010101010101, cold: false, freed: 10 }]
            sweep after unpin: [Evicted { hash: 0202020202020202, cold: false, freed: 20 }, Evicted { hash: 0303030303030303, cold: false, freed: 30 }]
        "#]].assert_eq(&outcomes.0);
    }

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    #[derive(Default)]
    struct Model {
        pins: BTreeMap<Hash, u32>,
        freezing: BTreeSet<Hash>,
        thawing: BTreeMap<Hash, u32>,
        sizes: BTreeMap<Hash, u64>,
    }

    impl Model {
        fn moving(&self) -> BTreeSet<Hash> {
            self.freezing.iter().chain(self.thawing.keys()).copied().collect()
        }
    }

    #[test]
    fn generated_sequences_keep_pins_moving_blobs_and_the_budget() {
        for seed in 1..=300u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let mut ix = StoreIndex::new();
            let mut m = Model::default();
            let mut now = 1000u64;
            for _ in 0..200 {
                now += rng.below(50);
                let x = h(rng.below(12) as u8);
                match rng.below(11) {
                    0 | 1 => {
                        let size = *m.sizes.entry(x).or_insert(if rng.below(2) == 0 {
                            100 + rng.below(1000)
                        } else {
                            COLD_MIN_SIZE + rng.below(1 << 20)
                        });
                        ix.put(x, size, 1, now);
                        m.freezing.remove(&x);
                        m.thawing.remove(&x);
                    }
                    2 => {
                        ix.touch(&x, now);
                    }
                    3 => {
                        let set: Vec<Hash> = (0..1 + rng.below(3)).map(|_| h(rng.below(12) as u8)).collect();
                        if ix.pin_all(&set) {
                            for p in &set {
                                *m.pins.entry(*p).or_insert(0) += 1;
                            }
                        } else {
                            assert!(set.iter().any(|p| !ix.has(p)));
                        }
                    }
                    4 => {
                        if let Some((&p, _)) = m.pins.iter().nth(rng.below(m.pins.len().max(1) as u64) as usize) {
                            ix.unpin_all(&[p]);
                            let n = m.pins.get_mut(&p).unwrap();
                            *n -= 1;
                            if *n == 0 {
                                m.pins.remove(&p);
                            }
                        }
                    }
                    5 => {
                        if ix.begin_freeze(&x).is_some() {
                            m.freezing.insert(x);
                        }
                    }
                    6 => {
                        if m.freezing.contains(&x) && ix.hot(&x).is_some() {
                            let size = ix.size_of(&x).unwrap();
                            if rng.below(4) == 0 {
                                ix.abort_freeze(&x);
                            } else {
                                let added = rng.below(size);
                                ix.chunks_written(added);
                                ix.finish_freeze(&x, recipe(size), added);
                            }
                            m.freezing.remove(&x);
                        }
                    }
                    7 => {
                        let cold: Vec<Hash> = ix.blobs.iter().filter(|(_, b)| !b.is_whole()).map(|(h, _)| *h).collect();
                        let t = if cold.is_empty() { x } else { cold[rng.below(cold.len() as u64) as usize] };
                        if let (Thaw::Build, Some(_)) = ix.begin_thaw(&t) {
                            *m.thawing.entry(t).or_insert(0) += 1;
                        }
                    }
                    9 => {
                        let open = m.thawing.len() as u64;
                        if let Some((&t, _)) = m.thawing.iter().nth(rng.below(open.max(1)) as usize) {
                            match rng.below(5) {
                                0 => {
                                    assert!(ix.fail_thaw(&t), "seed {seed}: {t}, being thawed, could not fail");
                                    m.thawing.remove(&t);
                                }
                                1 | 2 => {
                                    ix.abort_thaw(&t);
                                    let n = m.thawing.get_mut(&t).unwrap();
                                    *n -= 1;
                                    if *n == 0 {
                                        m.thawing.remove(&t);
                                    }
                                }
                                _ => {
                                    ix.put(t, m.sizes[&t], 2, now);
                                    m.thawing.remove(&t);
                                }
                            }
                        }
                    }
                    8 => {
                        let before: Vec<Hash> = m.moving().into_iter().filter(|p| ix.has(p)).collect();
                        let budget = rng.below(3 << 20);
                        let min_age = rng.below(2) * 100;
                        let total = ix.total_bytes();
                        let gone = ix.evict(budget, min_age, now, &|_| false);
                        for e in &gone {
                            assert!(!m.pins.contains_key(&e.hash), "seed {seed}: pinned {} was evicted", e.hash);
                            assert!(!before.contains(&e.hash), "seed {seed}: moving {} was evicted", e.hash);
                        }
                        let freed: u64 = gone.iter().map(|e| e.freed).sum();
                        let hot_freed: u64 = gone.iter().filter(|e| !e.cold).map(|e| e.freed).sum();
                        assert_eq!(ix.total_bytes(), total - hot_freed, "seed {seed}: a whole blob's bytes go at once");
                        if total.saturating_sub(freed) > budget {
                            for (p, b) in &ix.blobs {
                                let kept = b.is_moving()
                                    || m.pins.contains_key(p)
                                    || (min_age > 0 && b.last_used > now.saturating_sub(min_age));
                                assert!(kept, "seed {seed}: over the budget while {p} could have gone");
                            }
                        }
                    }
                    _ => {
                        let live = |p: &Hash| p.0[0].is_multiple_of(2);
                        let gone = ix.sweep(&live, 0, now);
                        for e in &gone {
                            assert!(!m.pins.contains_key(&e.hash) && !live(&e.hash) && !m.moving().contains(&e.hash));
                        }
                    }
                }
                for p in &m.moving() {
                    assert!(ix.has(p), "seed {seed}: {p}, being moved, was reported missing");
                }
                for (t, n) in &m.thawing {
                    let thawers = match &ix.blobs[t].tier {
                        Tier::Thawing { thawers, .. } => *thawers,
                        _ => 0,
                    };
                    assert_eq!(thawers, *n, "seed {seed}: {t} has its open thaws");
                }
                ix.invariants();
            }
        }
    }
}
