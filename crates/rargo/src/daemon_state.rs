use crate::known_digests::KnownDigests;
use rargo_proto::Digest;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Clone)]
pub struct Head {
    pub text: String,
    pub branch: Option<String>,
    pub commit: String,
    pub common: PathBuf,
    pub tags: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Need {
    Must,
    IfUp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    Use,
    Connect,
    Skip,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanPlan {
    Reuse,
    Scan { everything: bool },
}

#[derive(Clone, Default)]
struct Wt {
    dirty: bool,
    everything: bool,
    all_watched: bool,
    scanned: Option<Option<Head>>,
    synced: Option<Digest>,
}

pub struct DaemonState {
    up: bool,
    connections: u64,
    pub held: KnownDigests,
    worktrees: BTreeMap<u64, Wt>,
}

fn same_head(a: Option<&Head>, b: Option<&Head>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.commit == b.commit && a.text == b.text && a.tags == b.tags,
        _ => false,
    }
}

impl DaemonState {
    pub fn new(held: KnownDigests) -> Self {
        Self { up: false, connections: 0, held, worktrees: BTreeMap::new() }
    }

    pub fn connections(&self) -> u64 {
        self.connections
    }

    pub fn link(&mut self, alive: bool, need: Need) -> Link {
        if alive && self.up {
            return Link::Use;
        }
        if self.up {
            self.up = false;
            for wt in self.worktrees.values_mut() {
                wt.synced = None;
            }
        }
        debug_assert!(self.worktrees.values().all(|wt| wt.synced.is_none()), "nothing is synced over a link that is down");
        match need {
            Need::Must => Link::Connect,
            Need::IfUp => Link::Skip,
        }
    }

    pub fn connected(&mut self) {
        assert!(!self.up, "a link is made only when there is none");
        self.connections += 1;
        self.up = true;
    }

    pub fn added(&mut self, worktree: u64) {
        assert!(worktree > 0, "worktree ids start at 1");
        let wt = Wt { dirty: true, everything: true, ..Wt::default() };
        assert!(self.worktrees.insert(worktree, wt).is_none(), "a worktree is added once");
    }

    pub fn expired(&mut self, worktree: u64) {
        self.worktrees.remove(&worktree);
        debug_assert!(!self.worktrees.contains_key(&worktree));
    }

    pub fn changed(&mut self, worktree: u64, everything: bool) {
        if let Some(wt) = self.worktrees.get_mut(&worktree) {
            wt.dirty = true;
            wt.everything |= everything;
        }
        debug_assert!(self.worktrees.get(&worktree).is_none_or(|wt| wt.dirty), "a changed worktree is scanned again");
    }

    pub fn watched(&mut self, worktree: u64, all: bool) {
        if let Some(wt) = self.worktrees.get_mut(&worktree) {
            wt.all_watched = all;
        }
        debug_assert!(self.worktrees.get(&worktree).is_none_or(|wt| wt.all_watched == all));
    }

    pub fn all_watched(&self) -> usize {
        self.worktrees.values().filter(|wt| wt.all_watched).count()
    }

    pub fn plan_scan(&mut self, worktree: u64, head: Option<&Head>, watching: bool) -> ScanPlan {
        let Some(wt) = self.worktrees.get_mut(&worktree) else { return ScanPlan::Scan { everything: true } };
        let unchanged = wt.scanned.as_ref().is_some_and(|last| same_head(last.as_ref(), head));
        if unchanged && watching && wt.all_watched && !wt.dirty {
            return ScanPlan::Reuse;
        }
        let everything = std::mem::take(&mut wt.everything) || !watching || wt.scanned.is_none();
        wt.dirty = false;
        wt.scanned = None;
        debug_assert!(!wt.everything, "a planned scan takes the wish for a full one");
        ScanPlan::Scan { everything }
    }

    pub fn scanned(&mut self, worktree: u64, head: Option<Head>) {
        if let Some(wt) = self.worktrees.get_mut(&worktree) {
            wt.scanned = Some(head);
        }
        debug_assert!(self.worktrees.get(&worktree).is_none_or(|wt| wt.scanned.is_some()));
    }

    pub fn offer<'a, T>(
        &self,
        worktree: Option<u64>,
        root: &Digest,
        trees: impl Iterator<Item = &'a Digest>,
        files: impl Iterator<Item = (&'a Digest, T)>,
    ) -> (Vec<Digest>, Vec<(Digest, T)>) {
        let synced = worktree.and_then(|w| self.worktrees.get(&w)).and_then(|wt| wt.synced.as_ref());
        if self.up && synced == Some(root) {
            return (Vec::new(), Vec::new());
        }
        let trees = self.held.lacking(trees.map(|d| (d, ()))).into_iter().map(|(d, ())| d).collect();
        (trees, self.held.lacking(files))
    }

    pub fn synced<'a>(&mut self, worktree: Option<u64>, root: Digest, contents: impl Iterator<Item = &'a Digest>) {
        self.held.add(contents);
        self.held.add(std::iter::once(&root));
        for wt in self.worktrees.values_mut() {
            if wt.synced.is_some_and(|d| !self.held.holds(&d)) {
                wt.synced = None;
            }
        }
        if let Some(wt) = worktree.and_then(|w| self.worktrees.get_mut(&w))
            && self.up
            && self.held.holds(&root)
        {
            wt.synced = Some(root);
        }
        debug_assert!(
            self.worktrees.values().all(|wt| wt.synced.is_none_or(|d| self.held.holds(&d))),
            "every synced root is held"
        );
    }

    pub fn missing<'a>(&mut self, contents: impl Iterator<Item = &'a Digest>) {
        self.held.forget(contents);
        for wt in self.worktrees.values_mut() {
            wt.synced = None;
        }
        debug_assert!(self.worktrees.values().all(|wt| wt.synced.is_none()), "missing content unsyncs every worktree");
    }

    pub fn invariants(&self) {
        assert!(self.connections > 0 || !self.up, "the session is up only after a link was made");
        for (id, wt) in &self.worktrees {
            assert!(*id > 0, "worktree ids start at 1");
            assert!(
                !wt.everything || wt.dirty || wt.scanned.is_none(),
                "a full rescan is wanted only of a changed worktree"
            );
            if let Some(root) = &wt.synced {
                assert!(self.up, "no worktree is synced while the session is down");
                assert!(self.held.holds(root), "a synced root is held");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
    }

    fn d(i: u64) -> Digest {
        [i as u8 + 1; 32]
    }

    fn head(i: u64) -> Option<Head> {
        (i > 0).then(|| Head {
            text: format!("ref: refs/heads/b{i}"),
            branch: Some(format!("b{i}")),
            commit: format!("{i:040}"),
            common: Default::default(),
            tags: 0,
        })
    }

    type Contents = (Vec<Digest>, Vec<(Digest, u64)>);

    #[derive(Default)]
    struct ModelWt {
        dirty: bool,
        everything: bool,
        all_watched: bool,
        scanned: Option<u64>,
    }

    struct World {
        core: DaemonState,
        server: BTreeSet<Digest>,
        alive: bool,
        wts: BTreeMap<u64, ModelWt>,
        next_wt: u64,
    }

    impl World {
        fn link(&mut self, need: Need, rng: &mut Rng, seed: u64) -> bool {
            let got = self.core.link(self.alive, need);
            self.core.invariants();
            match (need, self.alive) {
                (_, true) => assert_eq!(got, Link::Use, "seed {seed}: a live link is used"),
                (Need::IfUp, false) => {
                    assert_eq!(got, Link::Skip, "seed {seed}: no link is made for work that can wait")
                }
                (Need::Must, false) => {
                    assert_eq!(got, Link::Connect, "seed {seed}: a dead link is made again");
                    if rng.below(5) == 0 {
                        return false;
                    }
                    self.core.connected();
                    self.core.invariants();
                    self.alive = true;
                }
            }
            self.alive
        }
    }

    #[test]
    fn generated_drops_and_reconnects_never_resend_what_the_server_holds_nor_assume_what_it_lost() {
        for seed in 1..=300u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let mut w = World {
                core: DaemonState::new(KnownDigests::default()),
                server: BTreeSet::new(),
                alive: false,
                wts: BTreeMap::new(),
                next_wt: 1,
            };
            w.core.invariants();
            let tree_of: Vec<Contents> = (0..6)
                .map(|_| {
                    let trees = (0..rng.below(4)).map(|_| d(20 + rng.below(20))).collect();
                    (trees, (0..rng.below(8)).map(|_| (d(rng.below(20)), 7)).collect())
                })
                .collect();
            let mut offers = 0;
            for step in 0..150 {
                let pick = |rng: &mut Rng, wts: &BTreeMap<u64, ModelWt>| {
                    wts.keys().nth(rng.below(wts.len().max(1) as u64) as usize).copied()
                };
                match rng.below(10) {
                    0 => {
                        let id = w.next_wt;
                        w.next_wt += 1;
                        w.core.added(id);
                        w.wts.insert(id, ModelWt { dirty: true, everything: true, ..ModelWt::default() });
                    }
                    1 => {
                        if let Some(id) = pick(&mut rng, &w.wts) {
                            w.core.expired(id);
                            w.wts.remove(&id);
                        }
                    }
                    2 => w.alive = false,
                    3 => {
                        w.link(Need::IfUp, &mut rng, seed);
                    }
                    4..=6 => {
                        let Some(id) = pick(&mut rng, &w.wts) else { continue };
                        if !w.link(Need::Must, &mut rng, seed) {
                            continue;
                        }
                        let r = rng.below(6);
                        let root = d(40 + r);
                        let (trees, files) = &tree_of[r as usize];
                        let (trees, files) = (trees.clone(), files.clone());
                        let all_trees: Vec<Digest> = std::iter::once(root).chain(trees.iter().copied()).collect();
                        let (lack_trees, lack_files) =
                            w.core.offer(Some(id), &root, all_trees.iter(), files.iter().map(|(d, s)| (d, *s)));
                        let lacks: BTreeSet<Digest> =
                            lack_trees.iter().copied().chain(lack_files.iter().map(|(d, _)| *d)).collect();
                        let want: BTreeSet<Digest> = all_trees
                            .iter()
                            .copied()
                            .chain(files.iter().map(|(d, _)| *d))
                            .filter(|d| !w.server.contains(d))
                            .collect();
                        assert_eq!(lacks, want, "seed {seed} step {step}: the offer is exactly what the server lacks");
                        offers += 1;
                        if rng.below(6) == 0 {
                            w.alive = false;
                            continue;
                        }
                        w.server.extend(lacks);
                        let contents: Vec<Digest> =
                            all_trees.iter().copied().chain(files.iter().map(|(d, _)| *d)).collect();
                        w.core.synced(Some(id), root, contents.iter());
                    }
                    7 => {
                        let lost: Vec<Digest> = w.server.iter().filter(|_| rng.below(3) == 0).copied().collect();
                        for x in &lost {
                            w.server.remove(x);
                        }
                        w.core.missing(lost.iter());
                    }
                    8 => {
                        let Some(id) = pick(&mut rng, &w.wts) else { continue };
                        let m = w.wts.get_mut(&id).unwrap();
                        if rng.below(2) == 0 {
                            let everything = rng.below(3) == 0;
                            w.core.changed(id, everything);
                            m.dirty = true;
                            m.everything |= everything;
                        } else {
                            let all = rng.below(3) != 0;
                            w.core.watched(id, all);
                            m.all_watched = all;
                        }
                    }
                    _ => {
                        let Some(id) = pick(&mut rng, &w.wts) else { continue };
                        let m = w.wts.get_mut(&id).unwrap();
                        let (h, watching) = (rng.below(3), rng.below(4) != 0);
                        let plan = w.core.plan_scan(id, head(h).as_ref(), watching);
                        let want = if m.scanned == Some(h) && watching && m.all_watched && !m.dirty {
                            ScanPlan::Reuse
                        } else {
                            let everything = std::mem::take(&mut m.everything) || !watching || m.scanned.is_none();
                            m.dirty = false;
                            m.scanned = None;
                            ScanPlan::Scan { everything }
                        };
                        assert_eq!(plan, want, "seed {seed} step {step}");
                        if plan != ScanPlan::Reuse && rng.below(5) != 0 {
                            w.core.scanned(id, head(h));
                            m.scanned = Some(h);
                        }
                    }
                }
                w.core.invariants();
            }
            assert!(offers > 0, "seed {seed}: something was offered");
        }
    }

    #[test]
    fn a_dropped_session_keeps_what_the_server_holds_and_forgets_what_is_synced() {
        let mut core = DaemonState::new(KnownDigests::default());
        core.added(1);
        assert_eq!(core.link(false, Need::IfUp), Link::Skip);
        assert_eq!(core.link(false, Need::Must), Link::Connect);
        core.connected();
        core.invariants();
        let files = [(d(1), 3u64), (d(2), 4)];
        core.synced(Some(1), d(9), [d(9), d(1), d(2)].iter());
        core.invariants();
        assert_eq!(core.offer(Some(1), &d(9), [d(9)].iter(), files.iter().map(|(d, s)| (d, *s))), (vec![], vec![]));
        assert_eq!(core.link(false, Need::IfUp), Link::Skip, "the link died");
        core.invariants();
        assert_eq!(core.link(false, Need::Must), Link::Connect);
        core.connected();
        assert_eq!(core.connections(), 2);
        let (trees, blobs) = core.offer(Some(1), &d(9), [d(9)].iter(), files.iter().map(|(d, s)| (d, *s)));
        assert!(trees.is_empty() && blobs.is_empty(), "after a reconnect nothing the server holds is sent again");
        core.missing([d(2)].iter());
        core.invariants();
        let (trees, blobs) = core.offer(Some(1), &d(9), [d(9)].iter(), files.iter().map(|(d, s)| (d, *s)));
        assert_eq!((trees, blobs), (vec![], vec![(d(2), 4)]), "what the server said it lost is sent again");
    }
}
