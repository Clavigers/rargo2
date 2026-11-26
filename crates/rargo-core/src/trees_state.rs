use rargo_machine::StateMachine;
use crate::hash::Hash;
use rargo_proto::tree::{Node, Tree};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub type Upload = u64;

const COMPLETE_LIMIT: usize = 2_000_000;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Claimed {
    pub mine: Vec<Hash>,
    pub theirs: Vec<(Hash, Upload)>,
}

#[derive(Clone, Debug, Default)]
pub struct TreesState {
    complete: BTreeMap<Hash, u64>,
    claims: BTreeMap<Hash, Upload>,
    uploads: BTreeMap<Upload, BTreeSet<Hash>>,
    next_upload: Upload,
}

impl TreesState {
    pub fn new() -> TreesState {
        TreesState::default()
    }

    pub fn is_complete(&self, d: &Hash, generation: u64) -> bool {
        self.complete.get(d).is_some_and(|g| *g == generation)
    }

    pub fn completed(&mut self, walked: &[Hash], walked_at: u64, generation_now: u64) -> bool {
        if walked_at != generation_now {
            return false;
        }
        for d in walked {
            self.complete.insert(*d, walked_at);
        }
        if self.complete.len() > COMPLETE_LIMIT {
            self.complete.retain(|_, g| *g == walked_at);
        }
        true
    }

    pub fn begin_upload(&mut self) -> Upload {
        let up = self.next_upload;
        self.next_upload += 1;
        self.uploads.insert(up, BTreeSet::new());
        up
    }

    pub fn claim(&mut self, up: Upload, want: &[Hash], has: &dyn Fn(&Hash) -> bool) -> Claimed {
        assert!(self.uploads.contains_key(&up), "upload {up} has begun");
        let mut out = Claimed::default();
        for h in want {
            if has(h) {
                continue;
            }
            match self.claims.get(h) {
                Some(&other) if other == up => {}
                Some(&other) => out.theirs.push((*h, other)),
                None => {
                    self.claims.insert(*h, up);
                    self.uploads.get_mut(&up).expect("checked above").insert(*h);
                    out.mine.push(*h);
                }
            }
        }
        out
    }

    pub fn holder(&self, h: &Hash) -> Option<Upload> {
        self.claims.get(h).copied()
    }

    pub fn release(&mut self, up: Upload) -> Vec<Hash> {
        let mine = self.uploads.remove(&up).unwrap_or_default();
        for h in &mine {
            assert_eq!(self.claims.remove(h), Some(up), "a claim stays until its owner gives it up");
        }
        mine.into_iter().collect()
    }
}

impl StateMachine for TreesState {
    fn invariants(&self) {
        let mut n = 0;
        for (up, hs) in &self.uploads {
            assert!(*up < self.next_upload, "upload {up} was begun");
            for h in hs {
                assert_eq!(self.claims.get(h), Some(up), "{h} is claimed by the upload that lists it");
                n += 1;
            }
        }
        assert_eq!(n, self.claims.len(), "every claim belongs to a live upload");
        assert!(self.complete.len() <= COMPLETE_LIMIT);
    }
}

pub fn missing(
    root: Hash,
    complete: &dyn Fn(&Hash) -> bool,
    tree: &dyn Fn(&Hash) -> Option<Arc<Tree>>,
    has: &dyn Fn(&Hash) -> bool,
) -> (Vec<Hash>, Vec<Hash>, Vec<Hash>) {
    let mut trees = Vec::new();
    let mut blobs = BTreeSet::new();
    let mut walked = Vec::new();
    let mut seen = BTreeSet::from([root]);
    let mut todo = vec![root];
    while let Some(d) = todo.pop() {
        if complete(&d) {
            continue;
        }
        let Some(t) = tree(&d) else {
            trees.push(d);
            continue;
        };
        walked.push(d);
        for (_, node) in &t.entries {
            match node {
                Node::File { hash, .. } => {
                    let h = Hash(*hash);
                    if !has(&h) {
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
    (trees, blobs.into_iter().collect(), walked)
}

#[derive(Clone, Debug)]
pub struct Walk {
    level: Vec<Hash>,
    seen: BTreeSet<Hash>,
    pub walked: Vec<Hash>,
    pub missing: BTreeSet<Hash>,
    pub present: BTreeSet<Hash>,
}

impl Walk {
    pub fn new(root: Hash) -> Walk {
        Walk {
            level: vec![root],
            seen: BTreeSet::from([root]),
            walked: Vec::new(),
            missing: BTreeSet::new(),
            present: BTreeSet::new(),
        }
    }

    pub fn advance(
        &mut self,
        complete: &dyn Fn(&Hash) -> bool,
        tree: &dyn Fn(&Hash) -> Option<Arc<Tree>>,
        has: &dyn Fn(&Hash) -> bool,
    ) -> Option<Vec<Hash>> {
        while !self.level.is_empty() {
            let got: Vec<Option<Arc<Tree>>> = self.level.iter().map(tree).collect();
            let lacking: Vec<Hash> =
                self.level.iter().zip(&got).filter(|(_, t)| t.is_none()).map(|(d, _)| *d).collect();
            if !lacking.is_empty() {
                return Some(lacking);
            }
            let mut next = Vec::new();
            for (d, t) in self.level.iter().zip(got) {
                self.walked.push(*d);
                for (_, node) in &t.expect("no tree of the level is lacking").entries {
                    match node {
                        Node::File { hash, .. } => {
                            let h = Hash(*hash);
                            if has(&h) {
                                self.present.insert(h);
                            } else {
                                self.missing.insert(h);
                            }
                        }
                        Node::Dir(sub) => {
                            let sub = Hash(*sub);
                            if self.seen.insert(sub) && !complete(&sub) {
                                next.push(sub);
                            }
                        }
                        Node::Symlink(_) => {}
                    }
                }
            }
            self.level = next;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct World {
        trees: BTreeMap<Hash, Arc<Tree>>,
        stored_trees: BTreeSet<Hash>,
        blobs: BTreeSet<Hash>,
        generation: u64,
    }

    impl World {
        fn new() -> World {
            World { trees: BTreeMap::new(), stored_trees: BTreeSet::new(), blobs: BTreeSet::new(), generation: 1 }
        }

        fn file(i: u32) -> (Vec<u8>, Node) {
            let mut h = [0u8; 32];
            h[..4].copy_from_slice(&i.to_le_bytes());
            h[31] = 1;
            (format!("f{i}").into_bytes(), Node::File { hash: h, size: 1, exec: false })
        }

        fn dir(&mut self, entries: Vec<(Vec<u8>, Node)>) -> (Hash, Node) {
            let mut entries = entries;
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            entries.dedup_by(|a, b| a.0 == b.0);
            let t = Tree { entries };
            let d = Hash::of(&t.encode());
            self.trees.insert(d, Arc::new(t));
            (d, Node::Dir(d.0))
        }

        fn tree(&self, d: &Hash) -> Option<Arc<Tree>> {
            self.stored_trees.contains(d).then(|| self.trees[d].clone())
        }

        fn evict(&mut self, h: &Hash) {
            if self.blobs.remove(h) || self.stored_trees.remove(h) {
                self.generation += 1;
            }
        }

        fn files_under(&self, root: &Hash) -> BTreeSet<Hash> {
            let mut out = BTreeSet::new();
            let mut todo = vec![*root];
            while let Some(d) = todo.pop() {
                for (_, n) in &self.trees[&d].entries {
                    match n {
                        Node::File { hash, .. } => {
                            out.insert(Hash(*hash));
                        }
                        Node::Dir(s) => todo.push(Hash(*s)),
                        Node::Symlink(_) => {}
                    }
                }
            }
            out
        }

        fn sync(&mut self, st: &mut TreesState, root: Hash) -> bool {
            let generation = self.generation;
            if st.is_complete(&root, generation) {
                return true;
            }
            let mut walk = Walk::new(root);
            while let Some(lacking) =
                walk.advance(&|d| st.is_complete(d, generation), &|d| self.tree(d), &|h| self.blobs.contains(h))
            {
                self.stored_trees.extend(lacking);
            }
            let up = st.begin_upload();
            let c = st.claim(up, &walk.missing.iter().copied().collect::<Vec<_>>(), &|h| self.blobs.contains(h));
            self.blobs.extend(c.mine);
            st.release(up);
            st.completed(&walk.walked, generation, self.generation)
        }
    }

    #[test]
    fn concurrent_uploads_of_overlapping_trees_each_finish() {
        let mut st = TreesState::new();
        let (a, b, c) = (Hash([1; 32]), Hash([2; 32]), Hash([3; 32]));
        let mut store = BTreeSet::new();
        let one = st.begin_upload();
        let two = st.begin_upload();
        let first = st.claim(one, &[a, b], &|h| store.contains(h));
        let second = st.claim(two, &[b, c], &|h| store.contains(h));
        assert_eq!(first.mine, vec![a, b]);
        assert_eq!(second, Claimed { mine: vec![c], theirs: vec![(b, one)] });
        store.extend(first.mine);
        assert_eq!(st.release(one), vec![a, b]);
        store.extend(second.mine);
        assert_eq!(st.holder(&b), None);
        assert!(store.contains(&b), "the second upload's share arrived through the first");
        assert_eq!(st.release(two), vec![c]);
        st.invariants();
    }

    #[test]
    fn a_cancelled_upload_releases_exactly_its_claims() {
        let mut st = TreesState::new();
        let hs: Vec<Hash> = (0..6u8).map(|i| Hash([i; 32])).collect();
        let one = st.begin_upload();
        let two = st.begin_upload();
        st.claim(one, &hs[..4], &|_| false);
        let second = st.claim(two, &hs[2..], &|_| false);
        assert_eq!(second.mine, hs[4..].to_vec());
        assert_eq!(st.release(one), hs[..4].to_vec());
        for h in &hs[..4] {
            assert_eq!(st.holder(h), None);
        }
        for h in &hs[4..] {
            assert_eq!(st.holder(h), Some(two), "the other upload's claims stay");
        }
        let again = st.claim(two, &hs[2..4], &|_| false);
        assert_eq!(again.mine, hs[2..4].to_vec(), "what the cancelled upload held is claimed again");
        st.invariants();
    }

    #[test]
    fn evicted_content_is_asked_for_again() {
        let mut w = World::new();
        let mut st = TreesState::new();
        let (root, _) = w.dir(vec![World::file(1), World::file(2)]);
        assert!(w.sync(&mut st, root));
        assert!(st.is_complete(&root, w.generation));
        let gone = w.files_under(&root).into_iter().next().unwrap();
        w.evict(&gone);
        assert!(!st.is_complete(&root, w.generation), "eviction ends what was complete");
        let (trees, blobs, _) =
            missing(root, &|d| st.is_complete(d, w.generation), &|d| w.tree(d), &|h| w.blobs.contains(h));
        assert_eq!((trees, blobs), (vec![], vec![gone]));
        assert!(w.sync(&mut st, root));
        assert!(w.blobs.contains(&gone));
    }

    #[test]
    fn a_walk_asks_for_each_level_of_trees_once() {
        let mut w = World::new();
        let (inner, n) = w.dir(vec![World::file(3)]);
        let (mid, m) = w.dir(vec![(b"in".to_vec(), n), World::file(2)]);
        let (root, _) = w.dir(vec![(b"mid".to_vec(), m), World::file(1)]);
        let mut walk = Walk::new(root);
        let mut asked = Vec::new();
        while let Some(lacking) = walk.advance(&|_| false, &|d| w.tree(d), &|_| false) {
            asked.push(lacking.clone());
            w.stored_trees.extend(lacking);
        }
        assert_eq!(asked, vec![vec![root], vec![mid], vec![inner]]);
        assert_eq!(walk.missing.len(), 3);
        assert_eq!(walk.walked, vec![root, mid, inner]);
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

    #[test]
    fn the_completeness_cache_never_says_complete_while_a_blob_is_missing() {
        for seed in 1..=200u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let mut w = World::new();
            let mut st = TreesState::new();
            let mut roots = Vec::new();
            let mut completes = 0;
            for r in 0..4u32 {
                let files: Vec<_> =
                    (0..3 + rng.below(5) as u32).map(|i| World::file(rng.below(20) as u32 + i)).collect();
                let (sub, s) = w.dir(files);
                let (shared, sh) = w.dir(vec![World::file(100), World::file(101 + rng.below(2) as u32)]);
                let _ = (sub, shared);
                let (root, _) = w.dir(vec![(b"sub".to_vec(), s), (b"shared".to_vec(), sh), World::file(200 + r)]);
                roots.push(root);
            }
            for _ in 0..200 {
                let root = roots[rng.below(roots.len() as u64) as usize];
                match rng.below(4) {
                    0 | 1 => {
                        w.sync(&mut st, root);
                    }
                    2 => {
                        let all: Vec<Hash> = w.blobs.iter().chain(&w.stored_trees).copied().collect();
                        if !all.is_empty() {
                            let h = all[rng.below(all.len() as u64) as usize];
                            w.evict(&h);
                        }
                    }
                    _ => {
                        let generation = w.generation;
                        let (t, b, walked) =
                            missing(root, &|d| st.is_complete(d, generation), &|d| w.tree(d), &|h| w.blobs.contains(h));
                        if rng.below(2) == 0 {
                            let all: Vec<Hash> = w.blobs.iter().copied().collect();
                            if let Some(h) = all.first() {
                                w.evict(h);
                            }
                        }
                        if t.is_empty() && b.is_empty() {
                            st.completed(&walked, generation, w.generation);
                        }
                    }
                }
                for r in &roots {
                    if st.is_complete(r, w.generation) {
                        completes += 1;
                        for f in w.files_under(r) {
                            assert!(w.blobs.contains(&f), "seed {seed}: complete while {f} is missing");
                        }
                    }
                }
                st.invariants();
            }
            assert!(completes > 0, "seed {seed}: the cache was tested while it said complete");
        }
    }

    #[test]
    fn generated_overlapping_uploads_each_finish_and_cancels_release_their_claims() {
        for seed in 1..=300u64 {
            let mut rng = Rng(seed.wrapping_mul(0x2545_f491_4f6c_dd1d) | 1);
            let mut st = TreesState::new();
            let mut store: BTreeSet<Hash> = BTreeSet::new();
            let mut live: Vec<(Upload, Vec<Hash>, Claimed)> = Vec::new();
            let mut finished = 0;
            for _ in 0..300 {
                match rng.below(4) {
                    0 => {
                        let want: Vec<Hash> = (0..1 + rng.below(6)).map(|_| Hash([rng.below(16) as u8; 32])).collect();
                        let up = st.begin_upload();
                        let c = st.claim(up, &want, &|h| store.contains(h));
                        for (h, other) in &c.theirs {
                            assert!(
                                live.iter().any(|(u, _, _)| u == other),
                                "seed {seed}: {h} is held by a live upload"
                            );
                        }
                        live.push((up, want, c));
                    }
                    1 if !live.is_empty() => {
                        let (up, want, c) = live.remove(rng.below(live.len() as u64) as usize);
                        store.extend(&c.mine);
                        let released = st.release(up);
                        assert_eq!(released.len(), c.mine.len());
                        let mut rest: Vec<Hash> = want.into_iter().filter(|h| !store.contains(h)).collect();
                        rest.dedup();
                        if rest.is_empty() {
                            finished += 1;
                        } else {
                            let again = st.begin_upload();
                            let c = st.claim(again, &rest, &|h| store.contains(h));
                            live.push((again, rest, c));
                        }
                    }
                    2 if !live.is_empty() => {
                        let i = rng.below(live.len() as u64) as usize;
                        let (up, want, c) = live.remove(i);
                        let others: Vec<(Hash, Option<Upload>)> = (0..16u8)
                            .map(|b| Hash([b; 32]))
                            .filter(|h| !c.mine.contains(h))
                            .map(|h| (h, st.holder(&h)))
                            .collect();
                        let mut released = st.release(up);
                        released.sort();
                        let mut mine = c.mine.clone();
                        mine.sort();
                        assert_eq!(released, mine, "seed {seed}: a cancel releases exactly its claims");
                        for (h, before) in others {
                            assert_eq!(st.holder(&h), before, "seed {seed}: a cancel left {h}'s claim alone");
                        }
                        let again = st.begin_upload();
                        let c = st.claim(again, &want, &|h| store.contains(h));
                        live.push((again, want, c));
                    }
                    _ => {}
                }
                st.invariants();
            }
            while let Some((up, want, c)) = live.pop() {
                store.extend(&c.mine);
                st.release(up);
                let rest: Vec<Hash> = want.into_iter().filter(|h| !store.contains(h)).collect();
                if rest.is_empty() {
                    finished += 1;
                } else {
                    let again = st.begin_upload();
                    let c = st.claim(again, &rest, &|h| store.contains(h));
                    live.insert(0, (again, rest, c));
                }
                st.invariants();
            }
            assert!(finished > 0);
            assert!(st.claims.is_empty() && st.uploads.is_empty(), "seed {seed}: every claim was released");
        }
    }
}
