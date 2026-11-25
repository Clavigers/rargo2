use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug)]
pub struct Calls<W> {
    next: u64,
    pending: BTreeMap<u64, W>,
}

impl<W> Default for Calls<W> {
    fn default() -> Calls<W> {
        Calls { next: 1, pending: BTreeMap::new() }
    }
}

impl<W> Calls<W> {
    pub fn start(&mut self, waiter: W) -> u64 {
        let rid = self.next;
        self.next += 1;
        assert!(self.pending.insert(rid, waiter).is_none(), "request ids are not reused");
        rid
    }

    pub fn answer(&mut self, rid: u64) -> Option<W> {
        self.pending.remove(&rid)
    }

    pub fn lost(&mut self) -> Vec<W> {
        std::mem::take(&mut self.pending).into_values().collect()
    }

    pub fn invariants(&self) {
        assert!(self.next >= 1);
        assert!(self.pending.keys().all(|rid| *rid >= 1 && *rid < self.next), "only issued ids are pending");
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    Wake,
    Lost { action: u64 },
    Cancel { worker: u64, action: u64 },
}

#[derive(Debug)]
struct Slot {
    cores: usize,
    free: usize,
}

#[derive(Debug, Default)]
pub struct WorkerPool {
    order: Vec<u64>,
    workers: BTreeMap<u64, Slot>,
    actions: BTreeMap<u64, u64>,
    leased: BTreeMap<u64, usize>,
    warmed: BTreeSet<u64>,
}

impl WorkerPool {
    pub fn new() -> WorkerPool {
        WorkerPool::default()
    }

    pub fn count(&self) -> usize {
        self.workers.len()
    }

    pub fn joined(&mut self, id: u64, cores: usize) -> Vec<Effect> {
        assert!(!self.workers.contains_key(&id), "a worker joins once");
        self.order.push(id);
        self.workers.insert(id, Slot { cores, free: cores });
        vec![Effect::Wake; cores]
    }

    pub fn left(&mut self, id: u64) -> Vec<Effect> {
        if self.workers.remove(&id).is_none() {
            return Vec::new();
        }
        self.order.retain(|w| *w != id);
        self.leased.remove(&id);
        self.warmed.remove(&id);
        let lost: Vec<u64> = self.actions.iter().filter(|(_, w)| **w == id).map(|(a, _)| *a).collect();
        lost.into_iter()
            .map(|action| {
                self.actions.remove(&action);
                Effect::Lost { action }
            })
            .collect()
    }

    pub fn lease(&mut self, among: Option<&[u64]>) -> Option<u64> {
        let mut best: Option<(u64, usize)> = None;
        for id in &self.order {
            if among.is_some_and(|ids| !ids.contains(id)) {
                continue;
            }
            let free = self.workers[id].free;
            if free > 0 && best.is_none_or(|(_, b)| free > b) {
                best = Some((*id, free));
            }
        }
        let (id, _) = best?;
        self.workers.get_mut(&id).expect("chosen").free -= 1;
        *self.leased.entry(id).or_default() += 1;
        Some(id)
    }

    pub fn give_back(&mut self, id: u64) -> Vec<Effect> {
        let Some(n) = self.leased.get_mut(&id) else { return vec![Effect::Wake] };
        *n -= 1;
        if *n == 0 {
            self.leased.remove(&id);
        }
        self.workers.get_mut(&id).expect("a leased worker is here").free += 1;
        vec![Effect::Wake]
    }

    pub fn sent(&mut self, action: u64, worker: u64) -> bool {
        if !self.workers.contains_key(&worker) {
            return false;
        }
        assert!(self.actions.insert(action, worker).is_none(), "an action is sent once");
        true
    }

    pub fn route(&self, worker: u64, action: u64) -> bool {
        self.actions.get(&action) == Some(&worker)
    }

    pub fn finished(&mut self, action: u64, answered: bool) -> Vec<Effect> {
        match self.actions.remove(&action) {
            Some(worker) if !answered => vec![Effect::Cancel { worker, action }],
            _ => Vec::new(),
        }
    }

    pub fn warm(&mut self, worker: u64, known: bool, own: bool) -> bool {
        known && !own && self.workers.contains_key(&worker) && self.warmed.insert(worker)
    }

    pub fn invariants(&self) {
        assert!(self.warmed.iter().all(|w| self.workers.contains_key(w)), "only a worker that is here is warmed");
        assert_eq!(self.order.len(), self.workers.len());
        assert_eq!(self.order.iter().collect::<BTreeSet<_>>().len(), self.order.len(), "a worker is listed once");
        for (id, s) in &self.workers {
            assert!(s.free <= s.cores, "worker {id} has no more free cores than it has");
            assert_eq!(s.cores - s.free, self.leased.get(id).copied().unwrap_or(0), "every busy core is leased");
        }
        assert!(self.leased.keys().all(|id| self.workers.contains_key(id)));
        assert!(self.actions.values().all(|w| self.workers.contains_key(w)), "an action runs on a worker that is here");
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placing {
    Local,
    LeaseNow,
    RaceLocal,
}

pub fn placing(local_free: bool, waiting: bool, away: bool) -> Placing {
    match (local_free && !waiting, away) {
        (true, false) => Placing::Local,
        (true, true) => Placing::LeaseNow,
        (false, _) => Placing::RaceLocal,
    }
}

pub fn worth_sending(known: bool, link_us: u64, action_us: u64, eager: bool) -> bool {
    eager || (known && link_us <= action_us)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Who {
    Remote,
    Local,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Idle,
    Running,
    Gone,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Settle {
    StopLocal,
    DropRemote,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Use { who: Who, stopped_other: bool },
    WaitLocal,
    RunElsewhere,
}

#[derive(Debug)]
pub struct Race {
    owner: Option<Who>,
    remote: Side,
    local: Side,
    over: bool,
}

impl Default for Race {
    fn default() -> Race {
        Race { owner: None, remote: Side::Running, local: Side::Idle, over: false }
    }
}

impl Race {
    pub fn owner(&self) -> Option<Who> {
        self.owner
    }

    pub fn remote_running(&self) -> bool {
        self.remote == Side::Running
    }

    pub fn local_running(&self) -> bool {
        self.local == Side::Running
    }

    pub fn claim(&mut self, who: Who) -> bool {
        match self.owner {
            None => {
                self.owner = Some(who);
                true
            }
            Some(o) => o == who,
        }
    }

    pub fn may_hedge(&self, away: bool, hedging: bool) -> bool {
        !self.over
            && self.local == Side::Idle
            && self.remote == Side::Running
            && !away
            && hedging
            && self.owner.is_none()
    }

    pub fn hedged(&mut self) {
        assert!(self.local == Side::Idle && !self.over, "one hedge per race");
        self.local = Side::Running;
    }

    pub fn settle(&mut self) -> Option<Settle> {
        match self.owner {
            Some(Who::Remote) if self.local == Side::Running => {
                self.local = Side::Gone;
                Some(Settle::StopLocal)
            }
            Some(Who::Local) if self.remote == Side::Running => {
                self.remote = Side::Gone;
                Some(Settle::DropRemote)
            }
            _ => None,
        }
    }

    pub fn remote_ended(&mut self, ok: bool) -> Outcome {
        assert!(self.remote == Side::Running && !self.over, "the remote run ends once");
        self.remote = Side::Gone;
        if ok {
            self.over = true;
            let stopped_other = self.local == Side::Running;
            self.local = Side::Gone;
            return Outcome::Use { who: Who::Remote, stopped_other };
        }
        if self.local == Side::Running {
            Outcome::WaitLocal
        } else {
            self.over = true;
            Outcome::RunElsewhere
        }
    }

    pub fn local_ended(&mut self) -> Outcome {
        assert!(self.local == Side::Running && !self.over, "the local run ends once");
        self.local = Side::Gone;
        self.over = true;
        let stopped_other = self.remote == Side::Running;
        self.remote = Side::Gone;
        Outcome::Use { who: Who::Local, stopped_other }
    }

    pub fn invariants(&self) {
        if self.over {
            assert!(self.remote != Side::Running && self.local != Side::Running, "nothing runs after the race is over");
        }
        if self.remote == Side::Idle {
            panic!("the remote run starts with the race");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Pool(WorkerPool);

    impl std::ops::Deref for Pool {
        type Target = WorkerPool;
        fn deref(&self) -> &WorkerPool {
            &self.0
        }
    }

    impl Pool {
        fn new() -> Pool {
            let p = WorkerPool::new();
            p.invariants();
            Pool(p)
        }

        fn checked<R>(&self, r: R) -> R {
            self.0.invariants();
            r
        }

        fn joined(&mut self, id: u64, cores: usize) -> Vec<Effect> {
            let r = self.0.joined(id, cores);
            self.checked(r)
        }

        fn left(&mut self, id: u64) -> Vec<Effect> {
            let r = self.0.left(id);
            self.checked(r)
        }

        fn lease(&mut self, among: Option<&[u64]>) -> Option<u64> {
            let r = self.0.lease(among);
            self.checked(r)
        }

        fn give_back(&mut self, id: u64) -> Vec<Effect> {
            let r = self.0.give_back(id);
            self.checked(r)
        }

        fn sent(&mut self, action: u64, worker: u64) -> bool {
            let r = self.0.sent(action, worker);
            self.checked(r)
        }

        fn finished(&mut self, action: u64, answered: bool) -> Vec<Effect> {
            let r = self.0.finished(action, answered);
            self.checked(r)
        }

        fn warm(&mut self, worker: u64, known: bool, own: bool) -> bool {
            let r = self.0.warm(worker, known, own);
            self.checked(r)
        }
    }

    struct Racing(Race);

    impl std::ops::Deref for Racing {
        type Target = Race;
        fn deref(&self) -> &Race {
            &self.0
        }
    }

    impl Racing {
        fn new() -> Racing {
            let r = Race::default();
            r.invariants();
            Racing(r)
        }

        fn checked<R>(&self, r: R) -> R {
            self.0.invariants();
            r
        }

        fn claim(&mut self, who: Who) -> bool {
            let r = self.0.claim(who);
            self.checked(r)
        }

        fn hedged(&mut self) {
            self.0.hedged();
            self.0.invariants();
        }

        fn settle(&mut self) -> Option<Settle> {
            let r = self.0.settle();
            self.checked(r)
        }

        fn remote_ended(&mut self, ok: bool) -> Outcome {
            let r = self.0.remote_ended(ok);
            self.checked(r)
        }

        fn local_ended(&mut self) -> Outcome {
            let r = self.0.local_ended();
            self.checked(r)
        }
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
    fn a_disconnected_worker_s_actions_finish_elsewhere() {
        let mut p = Pool::new();
        assert_eq!(p.joined(1, 2), vec![Effect::Wake, Effect::Wake], "each new core wakes a waiter");
        p.invariants();
        assert_eq!(p.joined(2, 1), vec![Effect::Wake]);
        p.invariants();
        assert_eq!(p.lease(None), Some(1), "the worker with the most free cores is chosen");
        p.invariants();
        assert!(p.sent(10, 1));
        assert_eq!(p.lease(None), Some(1), "first joined wins a tie");
        assert!(p.sent(11, 1));
        assert_eq!(p.lease(None), Some(2));
        assert!(p.sent(12, 2));
        p.invariants();
        let fx = p.left(1);
        assert_eq!(fx, vec![Effect::Lost { action: 10 }, Effect::Lost { action: 11 }]);
        p.invariants();
        assert!(!p.route(1, 10), "nothing from the gone worker reaches its actions");
        let mut race = Racing::new();
        assert_eq!(race.remote_ended(false), Outcome::RunElsewhere, "a lost action without a hedge runs here");
        race.invariants();
        let mut hedged = Racing::new();
        hedged.hedged();
        assert_eq!(hedged.remote_ended(false), Outcome::WaitLocal, "a lost action with a hedge finishes in it");
        assert_eq!(hedged.local_ended(), Outcome::Use { who: Who::Local, stopped_other: false });
        hedged.invariants();
        assert_eq!(p.give_back(1), vec![Effect::Wake]);
        assert_eq!(p.lease(None), None, "worker 2's core is busy and worker 1 is gone");
        assert_eq!(p.finished(12, true), vec![]);
        p.give_back(2);
        assert_eq!(p.lease(None), Some(2));
        assert!(!p.sent(13, 1), "nothing is sent to a gone worker");
        p.invariants();
    }

    #[test]
    fn a_hedged_action_s_first_result_wins_and_the_other_is_cancelled() {
        let mut r = Racing::new();
        assert!(r.may_hedge(false, true));
        assert!(!r.may_hedge(true, true), "an action whose state is away is not hedged");
        r.hedged();
        assert!(!r.may_hedge(false, true), "one hedge");
        assert_eq!(r.local_ended(), Outcome::Use { who: Who::Local, stopped_other: true });
        r.invariants();
        let mut p = Pool::new();
        p.joined(1, 1);
        p.lease(None);
        p.sent(5, 1);
        assert_eq!(
            p.finished(5, false),
            vec![Effect::Cancel { worker: 1, action: 5 }],
            "the remote loser is cancelled"
        );
        p.invariants();

        let mut r = Racing::new();
        r.hedged();
        assert_eq!(
            r.remote_ended(true),
            Outcome::Use { who: Who::Remote, stopped_other: true },
            "the local loser is stopped"
        );
        r.invariants();

        let mut r = Racing::new();
        assert!(r.claim(Who::Remote), "the first to speak owns the output");
        assert!(!r.may_hedge(false, true), "an action that already speaks is not hedged");
        assert!(!r.claim(Who::Local));
        let mut r = Racing::new();
        r.hedged();
        assert!(r.claim(Who::Local));
        assert_eq!(r.settle(), Some(Settle::DropRemote), "once the hedge speaks, the remote run is dropped");
        assert_eq!(r.local_ended(), Outcome::Use { who: Who::Local, stopped_other: false });
        r.invariants();
    }

    #[test]
    fn where_an_action_goes_and_which_worker_is_warmed_are_decided_here() {
        assert_eq!(placing(true, false, false), Placing::Local, "a free local slot and nobody waiting: run here");
        assert_eq!(placing(true, false, true), Placing::LeaseNow, "unless the action's state is on a worker");
        assert_eq!(
            placing(false, false, false),
            Placing::RaceLocal,
            "a busy server races a lease against a local slot"
        );
        assert_eq!(placing(true, true, true), Placing::RaceLocal);
        let mut p = Pool::new();
        p.joined(1, 1);
        assert!(!p.warm(1, false, false), "nothing known, nothing to warm");
        assert!(!p.warm(1, true, true), "an action's own trace is sent with it");
        assert!(p.warm(1, true, false), "a profile warms a worker");
        assert!(!p.warm(1, true, false), "once");
        assert!(!p.warm(2, true, false), "only a worker that is here");
        p.left(1);
        p.joined(1, 1);
        assert!(p.warm(1, true, false), "a worker that comes back is warmed again");
        assert!(worth_sending(true, 5, 10, false) && !worth_sending(true, 11, 10, false));
        assert!(!worth_sending(false, 0, 10, false) && worth_sending(false, 99, 1, true));
    }

    #[test]
    fn responses_never_reach_the_wrong_request() {
        for seed in 1..=300u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let mut calls: Calls<u64> = Calls::default();
            let mut open: BTreeMap<u64, u64> = BTreeMap::new();
            let mut done: BTreeSet<u64> = BTreeSet::new();
            let mut next_waiter = 0u64;
            for _ in 0..300 {
                match rng.below(5) {
                    0 | 1 => {
                        next_waiter += 1;
                        let rid = calls.start(next_waiter);
                        assert!(open.insert(rid, next_waiter).is_none(), "seed {seed}: a fresh id");
                        assert!(!done.contains(&rid), "seed {seed}: an id is never reused");
                    }
                    2 | 3 => {
                        let rid = if !open.is_empty() && rng.below(4) != 0 {
                            *open.keys().nth(rng.below(open.len() as u64) as usize).unwrap()
                        } else {
                            rng.below(next_waiter + 3)
                        };
                        let got = calls.answer(rid);
                        let want = open.remove(&rid);
                        assert_eq!(got, want, "seed {seed}: the answer to {rid} reaches its own request");
                        if want.is_some() {
                            done.insert(rid);
                        }
                    }
                    _ if rng.below(10) == 0 => {
                        let mut got = calls.lost();
                        got.sort();
                        let want: Vec<u64> = std::mem::take(&mut open).into_values().collect();
                        assert_eq!(got, want, "seed {seed}: a lost link fails every open request once");
                    }
                    _ => {}
                }
                calls.invariants();
            }
        }
    }

    #[test]
    fn generated_pools_lose_each_action_once_and_route_only_to_its_worker() {
        for seed in 1..=300u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let mut p = Pool::new();
            let mut here: BTreeMap<u64, usize> = BTreeMap::new();
            let mut on: BTreeMap<u64, u64> = BTreeMap::new();
            let mut leases: Vec<u64> = Vec::new();
            let mut lost: BTreeSet<u64> = BTreeSet::new();
            let mut next_worker = 1u64;
            let mut next_action = 1u64;
            for _ in 0..300 {
                let fx = match rng.below(7) {
                    0 => {
                        let cores = 1 + rng.below(3) as usize;
                        here.insert(next_worker, cores);
                        next_worker += 1;
                        p.joined(next_worker - 1, cores)
                    }
                    1 if !here.is_empty() && rng.below(3) == 0 => {
                        let id = *here.keys().nth(rng.below(here.len() as u64) as usize).unwrap();
                        here.remove(&id);
                        leases.retain(|w| *w != id);
                        p.left(id)
                    }
                    2 | 3 => {
                        let among: Option<Vec<u64>> =
                            (rng.below(3) == 0).then(|| here.keys().copied().filter(|_| rng.below(2) == 0).collect());
                        match p.lease(among.as_deref()) {
                            Some(w) => {
                                assert!(here.contains_key(&w), "seed {seed}: only a worker that is here is chosen");
                                assert!(among.as_ref().is_none_or(|a| a.contains(&w)), "seed {seed}: chosen among");
                                assert!(p.sent(next_action, w));
                                on.insert(next_action, w);
                                next_action += 1;
                                leases.push(w);
                            }
                            None => {
                                let busy = |w: &u64| leases.iter().filter(|l| *l == w).count();
                                assert!(
                                    here.iter()
                                        .filter(|(w, _)| among.as_ref().is_none_or(|a| a.contains(w)))
                                        .all(|(w, c)| busy(w) >= *c),
                                    "seed {seed}: no worker is chosen only when none has a free core"
                                );
                            }
                        }
                        Vec::new()
                    }
                    4 if !leases.is_empty() => {
                        let w = leases.remove(rng.below(leases.len() as u64) as usize);
                        p.give_back(w)
                    }
                    5 if !on.is_empty() => {
                        let action = *on.keys().nth(rng.below(on.len() as u64) as usize).unwrap();
                        let w = on.remove(&action).unwrap();
                        let answered = rng.below(2) == 0;
                        let fx = p.finished(action, answered);
                        if answered || lost.contains(&action) {
                            assert!(fx.is_empty());
                        } else {
                            assert_eq!(
                                fx,
                                vec![Effect::Cancel { worker: w, action }],
                                "seed {seed}: a dropped run is cancelled"
                            );
                        }
                        fx
                    }
                    _ => {
                        let w = 1 + rng.below(next_worker);
                        let action = 1 + rng.below(next_action);
                        let want = on.get(&action) == Some(&w) && !lost.contains(&action) && here.contains_key(&w);
                        assert_eq!(p.route(w, action), want, "seed {seed}: {action} hears only from its own worker");
                        Vec::new()
                    }
                };
                for e in fx {
                    if let Effect::Lost { action } = e {
                        assert!(lost.insert(action), "seed {seed}: an action is lost once");
                        assert!(!here.contains_key(&on[&action]), "seed {seed}: only a gone worker's actions are lost");
                    }
                }
                p.invariants();
            }
        }
    }

    #[test]
    fn generated_races_end_once_with_the_first_result() {
        for seed in 1..=300u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let mut r = Racing::new();
            let mut ended = None;
            for _ in 0..20 {
                if ended.is_some() {
                    break;
                }
                match rng.below(6) {
                    0 if r.may_hedge(false, true) => r.hedged(),
                    1 if r.remote_running() || r.local_running() => {
                        let who = match (r.remote_running(), r.local_running()) {
                            (true, true) => {
                                if rng.below(2) == 0 {
                                    Who::Remote
                                } else {
                                    Who::Local
                                }
                            }
                            (true, false) => Who::Remote,
                            _ => Who::Local,
                        };
                        let first = r.owner();
                        let won = r.claim(who);
                        assert_eq!(
                            won,
                            first.is_none_or(|o| o == who),
                            "seed {seed}: the first to speak keeps the voice"
                        );
                    }
                    2 if r.remote_running() => {
                        let ok = rng.below(3) != 0;
                        match r.remote_ended(ok) {
                            Outcome::Use { who: Who::Remote, .. } => ended = Some(Who::Remote),
                            Outcome::WaitLocal => assert!(r.local_running() && !ok),
                            Outcome::RunElsewhere => {
                                assert!(!ok && !r.local_running());
                                ended = Some(Who::Local);
                            }
                            other => panic!("seed {seed}: {other:?}"),
                        }
                    }
                    3 if r.local_running() => {
                        let other = r.remote_running();
                        assert_eq!(r.local_ended(), Outcome::Use { who: Who::Local, stopped_other: other });
                        ended = Some(Who::Local);
                    }
                    _ => {
                        if let Some(s) = r.settle() {
                            match s {
                                Settle::StopLocal => assert_eq!(r.owner(), Some(Who::Remote)),
                                Settle::DropRemote => assert_eq!(r.owner(), Some(Who::Local)),
                            }
                        }
                    }
                }
                r.invariants();
                if !r.remote_running() && !r.local_running() && ended.is_none() {
                    panic!("seed {seed}: a race with nothing running has ended");
                }
            }
        }
    }
}
