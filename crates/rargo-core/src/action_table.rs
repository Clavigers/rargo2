use crate::hash::Hash;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub type ReqId = u64;
pub type Slot = (Hash, Hash);

pub const MAX_ATTEMPTS: u32 = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum End {
    Done,
    TimedOut(Arc<str>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Start {
    Hit,
    Join,
    Run { after_wait: bool },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Woken {
    Waiting,
    Retry,
    Fail(Arc<str>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ran {
    Again,
    Record,
    Keep,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    Error,
    TimedOut(Arc<str>),
    SeedLost,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum State {
    Deciding { waited: Option<Slot> },
    Joining { slot: Slot },
    Woken { slot: Slot, end: End },
    Running { slot: Slot, attempt: u32 },
    Ran { slot: Slot },
}

#[derive(Clone, Debug)]
struct Flight {
    runner: ReqId,
    joiners: BTreeSet<ReqId>,
}

#[derive(Clone, Debug, Default)]
pub struct ActionTable {
    requests: BTreeMap<ReqId, State>,
    flights: BTreeMap<Slot, Flight>,
    next: ReqId,
}

impl ActionTable {
    pub fn new() -> ActionTable {
        ActionTable::default()
    }

    pub fn begin(&mut self) -> ReqId {
        self.next += 1;
        self.requests.insert(self.next, State::Deciding { waited: None });
        self.next
    }

    pub fn is_running(&self, slot: &Slot) -> bool {
        self.flights.contains_key(slot)
    }

    pub fn start(&mut self, id: ReqId, slot: Slot, hit: bool, no_cache: bool) -> Start {
        let waited = match self.requests.get(&id) {
            Some(State::Deciding { waited }) => *waited,
            other => panic!("request {id} decides only from the start, not from {other:?}"),
        };
        if hit && !no_cache {
            self.requests.remove(&id);
            return Start::Hit;
        }
        if let Some(f) = self.flights.get_mut(&slot)
            && !no_cache
            && waited != Some(slot)
        {
            f.joiners.insert(id);
            self.requests.insert(id, State::Joining { slot });
            return Start::Join;
        }
        let joiners = self.flights.remove(&slot).map(|old| old.joiners).unwrap_or_default();
        self.flights.insert(slot, Flight { runner: id, joiners });
        self.requests.insert(id, State::Running { slot, attempt: 1 });
        Start::Run { after_wait: waited.is_some() }
    }

    pub fn woken(&mut self, id: ReqId) -> Woken {
        match self.requests.get(&id) {
            Some(State::Joining { .. }) => Woken::Waiting,
            Some(State::Woken { slot, end }) => {
                let (slot, end) = (*slot, end.clone());
                match end {
                    End::TimedOut(why) => {
                        self.requests.remove(&id);
                        Woken::Fail(why)
                    }
                    End::Done => {
                        self.requests.insert(id, State::Deciding { waited: Some(slot) });
                        Woken::Retry
                    }
                }
            }
            other => panic!("request {id} is woken only while it joins, not from {other:?}"),
        }
    }

    pub fn ran(&mut self, id: ReqId, tainted: bool, cacheable: bool) -> Ran {
        let Some(State::Running { slot, attempt }) = self.requests.get(&id).cloned() else {
            panic!("request {id} reports a run it did not start");
        };
        if tainted && attempt < MAX_ATTEMPTS {
            self.requests.insert(id, State::Running { slot, attempt: attempt + 1 });
            return Ran::Again;
        }
        self.requests.insert(id, State::Ran { slot });
        if !tainted && cacheable { Ran::Record } else { Ran::Keep }
    }

    fn end_flight(&mut self, id: ReqId, slot: Slot, end: End) -> Vec<ReqId> {
        let Some(f) = self.flights.get(&slot) else { return Vec::new() };
        if f.runner != id {
            return Vec::new();
        }
        let f = self.flights.remove(&slot).expect("checked above");
        for j in &f.joiners {
            self.requests.insert(*j, State::Woken { slot, end: end.clone() });
        }
        f.joiners.into_iter().collect()
    }

    pub fn settled(&mut self, id: ReqId) -> Vec<ReqId> {
        let Some(State::Ran { slot }) = self.requests.get(&id).cloned() else {
            panic!("request {id} settles only after its run");
        };
        let woke = self.end_flight(id, slot, End::Done);
        self.requests.insert(id, State::Deciding { waited: None });
        woke
    }

    pub fn failed(&mut self, id: ReqId, why: Failure) -> Vec<ReqId> {
        let slot = match self.requests.remove(&id) {
            Some(State::Running { slot, .. } | State::Ran { slot }) => slot,
            other => panic!("request {id} fails only while it runs, not from {other:?}"),
        };
        let end = match why {
            Failure::TimedOut(t) => End::TimedOut(t),
            Failure::Error | Failure::SeedLost => End::Done,
        };
        self.end_flight(id, slot, end)
    }

    pub fn done(&mut self, id: ReqId) {
        match self.requests.remove(&id) {
            Some(State::Deciding { .. }) => {}
            other => panic!("request {id} is done only once it settled, not from {other:?}"),
        }
    }

    pub fn cancel(&mut self, id: ReqId) -> Vec<ReqId> {
        match self.requests.remove(&id) {
            None | Some(State::Deciding { .. } | State::Woken { .. }) => Vec::new(),
            Some(State::Joining { slot }) => {
                if let Some(f) = self.flights.get_mut(&slot) {
                    f.joiners.remove(&id);
                }
                Vec::new()
            }
            Some(State::Running { slot, .. } | State::Ran { slot }) => self.end_flight(id, slot, End::Done),
        }
    }

    pub fn invariants(&self) {
        for (slot, f) in &self.flights {
            assert!(
                matches!(self.requests.get(&f.runner), Some(State::Running { slot: s, .. } | State::Ran { slot: s }) if s == slot),
                "the runner of a flight runs it"
            );
            for j in &f.joiners {
                assert_eq!(
                    self.requests.get(j),
                    Some(&State::Joining { slot: *slot }),
                    "{j} joins the flight it waits on"
                );
            }
        }
        for (id, st) in &self.requests {
            assert!(*id <= self.next);
            match st {
                State::Joining { slot } => assert!(
                    self.flights.get(slot).is_some_and(|f| f.joiners.contains(id)),
                    "a joining request is a joiner of a running flight"
                ),
                State::Running { attempt, .. } => assert!((1..=MAX_ATTEMPTS).contains(attempt)),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(i: u8) -> Slot {
        (Hash([i; 32]), Hash::ZERO)
    }

    #[test]
    fn identical_concurrent_requests_run_once() {
        let mut t = ActionTable::new();
        let (a, b, c) = (t.begin(), t.begin(), t.begin());
        assert_eq!(t.start(a, slot(1), false, false), Start::Run { after_wait: false });
        assert_eq!(t.start(b, slot(1), false, false), Start::Join);
        assert_eq!(t.start(c, slot(1), false, false), Start::Join);
        assert_eq!(t.woken(b), Woken::Waiting);
        assert_eq!(t.ran(a, false, true), Ran::Record);
        assert_eq!(t.settled(a), vec![b, c]);
        t.done(a);
        for j in [b, c] {
            assert_eq!(t.woken(j), Woken::Retry);
            assert_eq!(t.start(j, slot(1), true, false), Start::Hit, "the joiner finds what the run recorded");
        }
        assert!(!t.is_running(&slot(1)));
        t.invariants();
    }

    #[test]
    fn a_failure_is_reported_and_never_recorded() {
        let mut t = ActionTable::new();
        let a = t.begin();
        t.start(a, slot(1), false, false);
        assert_eq!(t.ran(a, false, false), Ran::Keep, "a result that is not cacheable (a failure) is not recorded");
        t.settled(a);
        t.done(a);
        let b = t.begin();
        t.start(b, slot(2), false, false);
        let j = t.begin();
        t.start(j, slot(2), false, false);
        assert_eq!(t.failed(b, Failure::Error), vec![j]);
        assert_eq!(t.woken(j), Woken::Retry, "a joiner whose runner failed runs for itself");
        assert_eq!(t.start(j, slot(2), false, false), Start::Run { after_wait: true });
        t.invariants();
    }

    #[test]
    fn a_timed_out_run_fails_its_joiners_with_it() {
        let mut t = ActionTable::new();
        let (a, b) = (t.begin(), t.begin());
        t.start(a, slot(1), false, false);
        t.start(b, slot(1), false, false);
        assert_eq!(t.failed(a, Failure::TimedOut("`x` ran longer than 1 s".into())), vec![b]);
        assert_eq!(t.woken(b), Woken::Fail("`x` ran longer than 1 s".into()));
        t.invariants();
    }

    #[test]
    fn a_cancelled_requester_does_not_cancel_the_others() {
        let mut t = ActionTable::new();
        let (a, b, c) = (t.begin(), t.begin(), t.begin());
        t.start(a, slot(1), false, false);
        t.start(b, slot(1), false, false);
        t.start(c, slot(1), false, false);
        assert_eq!(t.cancel(c), Vec::<ReqId>::new(), "a joiner leaving wakes nobody");
        assert_eq!(t.cancel(a), vec![b], "the runner leaving wakes the rest");
        assert_eq!(t.woken(b), Woken::Retry);
        assert_eq!(t.start(b, slot(1), false, false), Start::Run { after_wait: true });
        assert_eq!(t.ran(b, false, true), Ran::Record);
        t.settled(b);
        t.done(b);
        t.invariants();
    }

    #[test]
    fn a_lost_seed_ends_the_flight_and_the_request_is_retried() {
        let mut t = ActionTable::new();
        let (a, j) = (t.begin(), t.begin());
        t.start(a, slot(1), false, false);
        t.start(j, slot(1), false, false);
        assert_eq!(t.failed(a, Failure::SeedLost), vec![j]);
        let again = t.begin();
        assert_eq!(
            t.start(again, slot(3), false, false),
            Start::Run { after_wait: false },
            "the retry with another seed runs"
        );
        assert_eq!(t.woken(j), Woken::Retry);
        t.invariants();
    }

    #[test]
    fn a_tainted_run_is_repeated_and_then_kept_unrecorded() {
        let mut t = ActionTable::new();
        let a = t.begin();
        t.start(a, slot(1), false, false);
        assert_eq!(t.ran(a, true, true), Ran::Again);
        assert_eq!(t.ran(a, true, true), Ran::Again);
        assert_eq!(
            t.ran(a, true, true),
            Ran::Keep,
            "after {MAX_ATTEMPTS} tainted runs the result is used, not recorded"
        );
        t.invariants();
    }

    #[test]
    fn no_cache_runs_beside_a_flight_and_takes_it_over() {
        let mut t = ActionTable::new();
        let (a, b, j) = (t.begin(), t.begin(), t.begin());
        t.start(a, slot(1), false, false);
        t.start(j, slot(1), false, false);
        assert_eq!(t.start(b, slot(1), true, true), Start::Run { after_wait: false }, "no cache, no hit and no join");
        assert_eq!(t.ran(a, false, true), Ran::Record);
        assert_eq!(t.settled(a), Vec::<ReqId>::new(), "the flight is b's now");
        t.ran(b, false, true);
        assert_eq!(t.settled(b), vec![j]);
        t.invariants();
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
    fn generated_requests_each_end_and_a_slot_runs_once_at_a_time() {
        for seed in 1..=300u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let mut t = ActionTable::new();
            let mut cache: BTreeSet<Slot> = BTreeSet::new();
            let mut live: Vec<(ReqId, Slot)> = Vec::new();
            let mut ended = 0;
            let mut began = 0;
            for _ in 0..400 {
                match rng.below(4) {
                    0 => {
                        let id = t.begin();
                        began += 1;
                        live.push((id, slot(rng.below(3) as u8)));
                    }
                    1 if !live.is_empty() => {
                        let (id, _) = live.remove(rng.below(live.len() as u64) as usize);
                        t.cancel(id);
                        ended += 1;
                    }
                    _ if !live.is_empty() => {
                        let i = rng.below(live.len() as u64) as usize;
                        let (id, s) = live[i];
                        let st = t.requests.get(&id).cloned().expect("a live request has a state");
                        let finished = match st {
                            State::Deciding { .. } => {
                                let no_cache = rng.below(10) == 0;
                                let before = t.flights.get(&s).map(|f| f.runner);
                                match t.start(id, s, cache.contains(&s), no_cache) {
                                    Start::Hit => true,
                                    Start::Join => {
                                        assert_eq!(t.flights[&s].runner, before.unwrap());
                                        false
                                    }
                                    Start::Run { .. } => {
                                        assert_eq!(t.flights[&s].runner, id, "seed {seed}: one runner per slot");
                                        false
                                    }
                                }
                            }
                            State::Joining { .. } | State::Woken { .. } => match t.woken(id) {
                                Woken::Fail(_) => true,
                                Woken::Waiting | Woken::Retry => false,
                            },
                            State::Running { .. } => match rng.below(6) {
                                0 => {
                                    let why = [Failure::Error, Failure::SeedLost, Failure::TimedOut("slow".into())]
                                        [rng.below(3) as usize]
                                        .clone();
                                    t.failed(id, why);
                                    true
                                }
                                n => {
                                    let cacheable = n != 1;
                                    if t.ran(id, rng.below(4) == 0, cacheable) == Ran::Record {
                                        assert!(cacheable, "seed {seed}: a failure was recorded");
                                        cache.insert(s);
                                    }
                                    false
                                }
                            },
                            State::Ran { .. } => {
                                t.settled(id);
                                t.done(id);
                                true
                            }
                        };
                        if finished {
                            live.remove(i);
                            ended += 1;
                        }
                    }
                    _ => {}
                }
                t.invariants();
            }
            for (id, _) in live.drain(..) {
                t.cancel(id);
                ended += 1;
                t.invariants();
            }
            assert_eq!(began, ended);
            assert!(t.requests.is_empty() && t.flights.is_empty(), "seed {seed}: every request and flight ended");
        }
    }
}
