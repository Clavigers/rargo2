use rargo_machine::StateMachine;
use crate::sched::Priority;
use std::cmp::Reverse;
use std::collections::BTreeMap;

pub type Job = u64;

pub const IDLE_DWELL_MS: u64 = 40;
pub const BUSY_WAIT_MS: u64 = 1000;
pub const MAX_BYPASS: u32 = 16;

type Place = (Reverse<Priority>, u64);

#[derive(Clone, Debug)]
struct Waiting {
    job: Job,
    mem_kb: u64,
    bypassed: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Effects {
    pub grant: Vec<Job>,
    pub put_tokens: usize,
    pub reconcile: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Broker {
    Read,
    TakeForIdle,
    Wait(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reconcile {
    pub put: usize,
    pub take: usize,
}

#[derive(Clone, Debug)]
pub struct SchedState {
    tokens: usize,
    mem_budget_kb: u64,
    running: BTreeMap<Job, u64>,
    mem_kb: u64,
    waiters: BTreeMap<Place, Waiting>,
    places: BTreeMap<Job, Place>,
    seq: u64,
    spare: usize,
    peak_running: usize,
    granted: u64,
    unclaimed_since: Option<u64>,
}

impl SchedState {
    pub fn new(tokens: usize, mem_budget_kb: u64) -> SchedState {
        SchedState {
            tokens,
            mem_budget_kb,
            running: BTreeMap::new(),
            mem_kb: 0,
            waiters: BTreeMap::new(),
            places: BTreeMap::new(),
            seq: 0,
            spare: 0,
            peak_running: 0,
            granted: 0,
            unclaimed_since: None,
        }
    }

    pub fn tokens(&self) -> usize {
        self.tokens
    }

    pub fn running(&self) -> usize {
        self.running.len()
    }

    pub fn spare(&self) -> usize {
        self.spare
    }

    pub fn is_running(&self, job: Job) -> bool {
        self.running.contains_key(&job)
    }

    pub fn is_waiting(&self, job: Job) -> bool {
        self.places.contains_key(&job)
    }

    pub fn snapshot(&self) -> (usize, usize, usize, u64) {
        (self.running.len(), self.wanted(), self.peak_running, self.granted)
    }

    pub fn wanted_jobs(&self) -> Vec<Job> {
        self.waiters.iter().filter(|((Reverse(p), _), _)| *p != Priority::Idle).map(|(_, w)| w.job).collect()
    }

    pub fn waiting_jobs(&self) -> Vec<Job> {
        self.waiters.values().map(|w| w.job).collect()
    }

    fn wanted(&self) -> usize {
        self.waiters.keys().filter(|(Reverse(p), _)| *p != Priority::Idle).count()
    }

    fn idle_wanted(&self) -> bool {
        self.waiters.keys().any(|(Reverse(p), _)| *p == Priority::Idle)
    }

    fn fits(&self, mem_kb: u64) -> bool {
        self.running.is_empty() || self.mem_kb + mem_kb <= self.mem_budget_kb
    }

    pub fn request(&mut self, job: Job, prio: Priority, mem_kb: u64) -> Effects {
        assert!(!self.places.contains_key(&job) && !self.running.contains_key(&job), "job {job} is new");
        self.seq += 1;
        let place = (Reverse(prio), self.seq);
        self.waiters.insert(place, Waiting { job, mem_kb, bypassed: 0 });
        self.places.insert(job, place);
        let mut fx = Effects::default();
        self.dispatch(false, &mut fx);
        fx
    }

    pub fn cancel(&mut self, job: Job) -> Effects {
        if let Some(place) = self.places.remove(&job) {
            self.waiters.remove(&place);
        }
        let mut fx = Effects::default();
        self.dispatch(false, &mut fx);
        self.return_spares(&mut fx);
        fx
    }

    pub fn release(&mut self, job: Job) -> Effects {
        let mem_kb = self.running.remove(&job).unwrap_or_else(|| panic!("release of {job}, which is not running"));
        assert!(self.mem_kb >= mem_kb, "memory released that was never granted");
        self.mem_kb -= mem_kb;
        self.spare += 1;
        let mut fx = Effects::default();
        self.dispatch(false, &mut fx);
        self.return_spares(&mut fx);
        fx.reconcile = self.running.is_empty();
        fx
    }

    pub fn token_in(&mut self, n: usize) -> Effects {
        self.spare += n;
        let mut fx = Effects::default();
        self.dispatch(false, &mut fx);
        self.return_spares(&mut fx);
        fx
    }

    pub fn tick(&mut self, now_ms: u64, in_pipe: usize) -> Broker {
        if self.spare == 0 && self.wanted() > 0 {
            self.unclaimed_since = None;
            return Broker::Read;
        }
        if !self.idle_wanted() || in_pipe == 0 {
            self.unclaimed_since = None;
            return Broker::Wait(if self.idle_wanted() { IDLE_DWELL_MS } else { BUSY_WAIT_MS });
        }
        match self.unclaimed_since {
            Some(t) if now_ms.saturating_sub(t) >= IDLE_DWELL_MS => {
                self.unclaimed_since = None;
                Broker::TakeForIdle
            }
            Some(t) => Broker::Wait(IDLE_DWELL_MS - now_ms.saturating_sub(t)),
            None => {
                self.unclaimed_since = Some(now_ms);
                Broker::Wait(IDLE_DWELL_MS)
            }
        }
    }

    pub fn idle_token(&mut self) -> Effects {
        self.spare += 1;
        let mut fx = Effects::default();
        self.dispatch(true, &mut fx);
        self.return_spares(&mut fx);
        fx
    }

    pub fn reconcile(&self, in_pipe: usize) -> Reconcile {
        assert!(self.running.is_empty(), "the pipe is reconciled only with nothing running");
        let have = in_pipe + self.spare;
        Reconcile { put: self.tokens.saturating_sub(have), take: have.saturating_sub(self.tokens) }
    }

    fn next_grant(&self, idle_too: bool) -> Option<(Place, Vec<Place>)> {
        if self.spare == 0 {
            return None;
        }
        let mut passed = Vec::new();
        for (place, w) in &self.waiters {
            if place.0.0 == Priority::Idle && !idle_too {
                continue;
            }
            if self.fits(w.mem_kb) {
                return Some((*place, passed));
            }
            if w.bypassed >= MAX_BYPASS {
                return None;
            }
            passed.push(*place);
        }
        None
    }

    fn dispatch(&mut self, idle_too: bool, fx: &mut Effects) {
        while let Some((place, passed)) = self.next_grant(idle_too) {
            for p in passed {
                self.waiters.get_mut(&p).expect("a passed waiter waits").bypassed += 1;
            }
            let w = self.waiters.remove(&place).expect("the granted waiter waits");
            self.places.remove(&w.job);
            self.spare -= 1;
            self.mem_kb += w.mem_kb;
            self.running.insert(w.job, w.mem_kb);
            self.peak_running = self.peak_running.max(self.running.len());
            self.granted += 1;
            fx.grant.push(w.job);
        }
    }

    fn return_spares(&mut self, fx: &mut Effects) {
        let keep = self.wanted().min(1);
        while self.spare > keep {
            self.spare -= 1;
            fx.put_tokens += 1;
        }
    }
}

impl StateMachine for SchedState {
    fn invariants(&self) {
        assert_eq!(self.running.values().sum::<u64>(), self.mem_kb, "memory is the sum of what runs");
        assert!(
            self.mem_kb <= self.mem_budget_kb || self.running.len() == 1,
            "only a lone job may exceed the memory budget"
        );
        assert!(self.peak_running >= self.running.len());
        assert_eq!(self.places.len(), self.waiters.len(), "every waiter has one place");
        for (job, place) in &self.places {
            assert_eq!(self.waiters.get(place).map(|w| w.job), Some(*job), "the place of {job} holds it");
            assert!(!self.running.contains_key(job), "{job} does not wait and run at once");
            assert!(place.1 <= self.seq);
        }
        assert!(self.spare <= 1, "spare tokens go back to the pipe");
        assert!(self.next_grant(false).is_none(), "a spare token is used at once by a waiter it can serve");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct World {
        s: SchedState,
        pipe: usize,
        outside: usize,
        order: Vec<Job>,
    }

    impl World {
        fn new(tokens: usize, budget: u64) -> World {
            World { s: SchedState::new(tokens, budget), pipe: tokens, outside: 0, order: Vec::new() }
        }

        fn apply(&mut self, fx: Effects) {
            self.order.extend(&fx.grant);
            self.pipe += fx.put_tokens;
            if fx.reconcile {
                self.outside = 0;
                let r = self.s.reconcile(self.pipe);
                self.pipe += r.put;
                self.pipe -= r.take.min(self.pipe);
            }
            self.s.invariants();
        }

        fn request(&mut self, job: Job, prio: Priority, mem: u64) {
            let fx = self.s.request(job, prio, mem);
            self.apply(fx);
            self.broker();
        }

        fn release(&mut self, job: Job) {
            let fx = self.s.release(job);
            self.apply(fx);
            self.broker();
        }

        fn broker(&mut self) {
            while self.s.tick(0, self.pipe) == Broker::Read && self.pipe > 0 {
                self.pipe -= 1;
                let fx = self.s.token_in(1);
                self.apply(fx);
            }
        }
    }

    #[test]
    fn limits_concurrency_to_tokens() {
        let mut w = World::new(3, u64::MAX);
        for j in 0..4 {
            w.request(j, Priority::Agent, 0);
        }
        assert_eq!(w.order, vec![0, 1, 2]);
        assert_eq!(w.s.running(), 3);
        w.release(1);
        assert_eq!(w.order, vec![0, 1, 2, 3], "released capacity is reused at once");
        for j in [0, 2, 3] {
            w.release(j);
        }
        assert_eq!((w.s.running(), w.pipe), (0, 3));
    }

    #[test]
    fn humans_go_first() {
        let mut w = World::new(1, u64::MAX);
        w.request(0, Priority::Agent, 0);
        w.request(1, Priority::Agent, 0);
        w.request(2, Priority::Agent, 0);
        w.request(3, Priority::Human, 0);
        w.request(4, Priority::Background, 0);
        for j in [0, 3, 1, 2] {
            w.release(j);
        }
        assert_eq!(w.order, vec![0, 3, 1, 2, 4]);
    }

    #[test]
    fn memory_admission_holds_back_big_actions() {
        let mut w = World::new(8, 1000);
        w.request(0, Priority::Agent, 600);
        w.request(1, Priority::Agent, 600);
        w.request(2, Priority::Agent, 300);
        assert_eq!(w.order, vec![0, 2], "600 + 600 exceeds the 1000 budget");
        w.release(0);
        assert_eq!(w.order, vec![0, 2, 1]);
        w.release(1);
        w.release(2);
        w.request(3, Priority::Agent, 5000);
        assert_eq!(w.order, vec![0, 2, 1, 3], "a job bigger than the budget runs alone");
    }

    #[test]
    fn a_big_job_is_passed_over_a_bounded_number_of_times() {
        let mut w = World::new(4, 1000);
        w.request(0, Priority::Agent, 500);
        w.request(1, Priority::Agent, 900);
        let mut next = 2;
        let mut small_runs = Vec::new();
        for _ in 0..100 {
            if w.s.is_running(1) {
                break;
            }
            w.request(next, Priority::Agent, 100);
            if w.s.is_running(next) {
                small_runs.push(next);
            }
            next += 1;
            if small_runs.len() > 2 {
                w.release(small_runs.remove(0));
            }
            if next % 7 == 0 && w.s.is_running(0) {
                w.release(0);
            }
        }
        while !w.s.is_running(1) {
            let Some(j) = small_runs.pop() else { break };
            w.release(j);
        }
        assert!(w.s.is_running(1), "the big job ran");
        let pos = w.order.iter().position(|j| *j == 1).unwrap();
        assert!(pos <= 2 + MAX_BYPASS as usize, "granted {pos}th, after more than {MAX_BYPASS} passed it");
    }

    #[test]
    fn idle_requests_wait_for_a_token_to_lie_unclaimed() {
        let mut w = World::new(1, u64::MAX);
        w.request(0, Priority::Agent, 0);
        w.request(1, Priority::Idle, 0);
        assert_eq!(w.order, vec![0]);
        assert_eq!(w.s.snapshot().1, 0, "an idle request is not waiting work");
        w.release(0);
        assert_eq!(w.order, vec![0], "an idle request leaves the freed token in the pipe");
        assert_eq!(w.pipe, 1);
        w.pipe -= 1;
        w.outside += 1;
        assert_eq!(w.s.tick(1000, w.pipe), Broker::Wait(IDLE_DWELL_MS), "a helper holds the token");
        w.pipe += 1;
        w.outside -= 1;
        assert_eq!(w.s.tick(1001, w.pipe), Broker::Wait(IDLE_DWELL_MS));
        assert_eq!(w.s.tick(1001 + IDLE_DWELL_MS - 1, w.pipe), Broker::Wait(1));
        assert_eq!(w.s.tick(1001 + IDLE_DWELL_MS, w.pipe), Broker::TakeForIdle);
        w.pipe -= 1;
        let fx = w.s.idle_token();
        w.apply(fx);
        assert_eq!(w.order, vec![0, 1]);
        w.release(1);
        assert_eq!(w.pipe, 1);
    }

    #[test]
    fn a_cancelled_waiter_is_never_granted() {
        let mut w = World::new(1, u64::MAX);
        w.request(0, Priority::Agent, 0);
        w.request(1, Priority::Human, 0);
        w.request(2, Priority::Agent, 0);
        let fx = w.s.cancel(1);
        w.apply(fx);
        w.release(0);
        assert_eq!(w.order, vec![0, 2]);
    }

    #[test]
    fn reconcile_restores_the_token_count() {
        let s = SchedState::new(4, 0);
        assert_eq!(s.reconcile(1), Reconcile { put: 3, take: 0 });
        assert_eq!(s.reconcile(6), Reconcile { put: 0, take: 2 });
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
    fn generated_arrivals_and_releases_keep_the_scheduler_consistent() {
        let prios = [Priority::Idle, Priority::Background, Priority::Agent, Priority::Human];
        for seed in 1..=300u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let tokens = 1 + rng.below(6) as usize;
            let budget = 1000;
            let mut w = World::new(tokens, budget);
            let mut prio_of = BTreeMap::new();
            let mut mem_of = BTreeMap::new();
            let mut now = 0;
            let mut next = 0;
            for _ in 0..300 {
                now += rng.below(30);
                match rng.below(7) {
                    0..=2 => {
                        let p = prios[rng.below(4) as usize];
                        prio_of.insert(next, p);
                        let mem = match rng.below(4) {
                            0 => rng.below(1500),
                            1 => 0,
                            _ => rng.below(300),
                        };
                        mem_of.insert(next, mem);
                        let before = w.order.len();
                        w.request(next, p, mem);
                        for j in &w.order[before..] {
                            let pj = prio_of[j];
                            for k in w.s.waiting_jobs() {
                                assert!(
                                    prio_of[&k] <= pj || mem_of[&k] > 0,
                                    "seed {seed}: {k} waits while lower {j} was granted"
                                );
                            }
                        }
                        next += 1;
                    }
                    3 | 4 => {
                        let running: Vec<Job> = w.order.iter().copied().filter(|j| w.s.is_running(*j)).collect();
                        if !running.is_empty() {
                            w.release(running[rng.below(running.len() as u64) as usize]);
                        }
                    }
                    5 => {
                        let waiting = w.s.waiting_jobs();
                        if !waiting.is_empty() {
                            let fx = w.s.cancel(waiting[rng.below(waiting.len() as u64) as usize]);
                            w.apply(fx);
                        }
                    }
                    _ => {
                        if rng.below(2) == 0 && w.pipe > 0 && w.s.running() > 0 {
                            w.pipe -= 1;
                            w.outside += 1;
                        } else if w.outside > 0 {
                            w.outside -= 1;
                            w.pipe += 1;
                        }
                        if w.s.tick(now, w.pipe) == Broker::TakeForIdle && w.pipe > 0 {
                            w.pipe -= 1;
                            let fx = w.s.idle_token();
                            w.apply(fx);
                        }
                        w.broker();
                    }
                }
                assert!(
                    w.s.running() + w.pipe + w.outside + w.s.spare() <= tokens,
                    "seed {seed}: more jobs than tokens"
                );
                w.s.invariants();
            }
        }
    }
}
