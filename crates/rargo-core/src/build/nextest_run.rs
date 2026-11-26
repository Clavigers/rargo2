use rargo_machine::StateMachine;
use super::pure::{missing_components, needs_network, offline_args};
use std::collections::{BTreeMap, BTreeSet};

pub const MOST_ROUNDS: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Asked {
    Metadata(Vec<String>),
    Build(Vec<String>),
    Nothing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestAsked {
    pub key: String,
    pub listing: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    RunNextest,
    Metadata(Vec<String>),
    Fetch { args: Vec<String>, components: Vec<(String, String)>, offline: bool },
    Prepare,
    Build(Vec<String>),
    RunTests(Vec<String>),
    ShowNextest,
    Finish,
    End(i32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Ask,
    Metadata,
    Fetch,
    Build,
    Round,
    Tests,
    Done,
}

#[derive(Clone, Debug)]
pub struct NextestRun {
    phase: Phase,
    fetched: bool,
    answered_metadata: bool,
    ran: BTreeSet<String>,
    running: BTreeMap<String, bool>,
    round: usize,
    replayed: usize,
    run_now: usize,
}

impl NextestRun {
    pub fn new() -> (NextestRun, Vec<Effect>) {
        let run = NextestRun {
            phase: Phase::Ask,
            fetched: false,
            answered_metadata: false,
            ran: BTreeSet::new(),
            running: BTreeMap::new(),
            round: 0,
            replayed: 0,
            run_now: 0,
        };
        (run, vec![Effect::RunNextest])
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    fn end(&mut self, code: i32) -> Vec<Effect> {
        self.phase = Phase::Done;
        vec![Effect::End(code)]
    }

    pub fn asked(&mut self, asked: Asked, code: Option<i32>) -> Vec<Effect> {
        assert_eq!(self.phase, Phase::Ask, "nextest asks only while it is being answered");
        match asked {
            Asked::Metadata(args) => {
                self.phase = Phase::Metadata;
                vec![Effect::Metadata(args)]
            }
            Asked::Build(args) => {
                self.phase = Phase::Build;
                vec![Effect::Build(args)]
            }
            Asked::Nothing => {
                let mut fx = vec![Effect::ShowNextest];
                fx.extend(self.end(code.unwrap_or(101)));
                fx
            }
        }
    }

    pub fn metadata_answered(&mut self, args: &[String], code: i32, stderr: &[u8]) -> Vec<Effect> {
        assert_eq!(self.phase, Phase::Metadata);
        if code != 0 && !self.fetched && needs_network(stderr) {
            self.fetched = true;
            let (components, offline) = (missing_components(stderr), offline_args(args));
            if offline && components.is_empty() {
                return self.end(101);
            }
            self.phase = Phase::Fetch;
            return vec![Effect::Fetch { args: args.to_vec(), components, offline }];
        }
        self.answered_metadata = true;
        self.phase = Phase::Ask;
        vec![Effect::RunNextest]
    }

    pub fn fetched(&mut self, ok: bool) -> Vec<Effect> {
        assert_eq!(self.phase, Phase::Fetch);
        if !ok {
            return self.end(101);
        }
        self.phase = Phase::Ask;
        vec![Effect::Prepare, Effect::RunNextest]
    }

    pub fn built(&mut self) -> Vec<Effect> {
        assert_eq!(self.phase, Phase::Build);
        self.phase = Phase::Round;
        self.round = 1;
        vec![Effect::RunNextest]
    }

    pub fn round_ended(&mut self, asked: Vec<TestAsked>) -> Vec<Effect> {
        assert_eq!(self.phase, Phase::Round);
        let mut new: BTreeMap<String, bool> = BTreeMap::new();
        for q in asked {
            if !self.ran.contains(&q.key) {
                new.entry(q.key).or_insert(q.listing);
            }
        }
        if new.is_empty() || self.round == MOST_ROUNDS {
            self.phase = Phase::Done;
            return vec![Effect::ShowNextest, Effect::Finish];
        }
        self.phase = Phase::Tests;
        let keys: Vec<String> = new.keys().cloned().collect();
        self.running = new;
        vec![Effect::RunTests(keys)]
    }

    pub fn test_ran(&mut self, key: &str, cached: bool) -> Vec<Effect> {
        assert_eq!(self.phase, Phase::Tests, "tests run in their round");
        let listing = self.running.remove(key).expect("a test that ran was asked for this round");
        assert!(self.ran.insert(key.to_string()), "a test runs once");
        if !listing {
            if cached {
                self.replayed += 1;
            } else {
                self.run_now += 1;
            }
        }
        if !self.running.is_empty() {
            return Vec::new();
        }
        self.phase = Phase::Round;
        self.round += 1;
        vec![Effect::RunNextest]
    }

    pub fn summary(&self) -> Option<String> {
        let all = self.replayed + self.run_now;
        (all > 0).then(|| {
            format!(
                "{} of {all} test runs from rargo's cache ({} run on the server; `--no-cache` runs every test)",
                self.replayed, self.run_now
            )
        })
    }
}

impl StateMachine for NextestRun {
    fn invariants(&self) {
        assert!(self.round <= MOST_ROUNDS, "rounds are bounded");
        assert!(self.running.keys().all(|k| !self.ran.contains(k)), "a test runs once");
        if self.phase != Phase::Tests {
            assert!(self.running.is_empty(), "tests run only in their round");
        }
        if matches!(self.phase, Phase::Round | Phase::Tests) {
            assert!(self.round >= 1);
        }
        assert!(self.replayed + self.run_now <= self.ran.len());
    }
}

pub fn exit_code(code: Option<i32>, status: &str) -> (i32, Option<String>) {
    match code {
        Some(code) => (code, None),
        None => (101, Some(format!("nextest did not exit normally ({status})"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_needing_the_network_is_fetched_once() {
        let (mut r, fx) = NextestRun::new();
        assert_eq!(fx, [Effect::RunNextest]);
        let args = vec!["metadata".to_string()];
        assert_eq!(r.asked(Asked::Metadata(args.clone()), None), [Effect::Metadata(args.clone())]);
        let offline = b"note: offline mode (via `--offline`) can sometimes cause surprising resolution failures\n";
        assert_eq!(
            r.metadata_answered(&args, 101, offline),
            [Effect::Fetch { args: args.clone(), components: vec![], offline: false }]
        );
        assert_eq!(r.fetched(true), [Effect::Prepare, Effect::RunNextest]);
        r.asked(Asked::Metadata(args.clone()), None);
        assert_eq!(
            r.metadata_answered(&args, 101, offline),
            [Effect::RunNextest],
            "nextest then reports cargo's failure"
        );
        let (mut r, _) = NextestRun::new();
        let locked = vec!["metadata".to_string(), "--locked".to_string()];
        r.asked(Asked::Metadata(locked.clone()), None);
        assert_eq!(r.metadata_answered(&locked, 101, offline), [Effect::End(101)]);
        r.invariants();
    }

    #[test]
    fn nextest_that_asks_nothing_ends_with_its_own_code() {
        let (mut r, _) = NextestRun::new();
        assert_eq!(r.asked(Asked::Nothing, Some(4)), [Effect::ShowNextest, Effect::End(4)]);
        assert_eq!(exit_code(None, "signal 9"), (101, Some("nextest did not exit normally (signal 9)".into())));
    }

    #[test]
    fn rounds_run_each_new_question_once_and_count_what_was_replayed() {
        let (mut r, _) = NextestRun::new();
        r.asked(Asked::Build(vec!["test".into()]), None);
        assert_eq!(r.built(), [Effect::RunNextest]);
        let q = |k: &str, listing| TestAsked { key: k.into(), listing };
        assert_eq!(r.round_ended(vec![q("list", true), q("list", true)]), [Effect::RunTests(vec!["list".into()])]);
        assert_eq!(r.test_ran("list", false), [Effect::RunNextest]);
        assert_eq!(r.round_ended(vec![q("a", false), q("b", false)]), [Effect::RunTests(vec!["a".into(), "b".into()])]);
        assert_eq!(r.test_ran("a", true), []);
        assert_eq!(r.test_ran("b", false), [Effect::RunNextest]);
        assert_eq!(r.round_ended(vec![q("a", false)]), [Effect::ShowNextest, Effect::Finish]);
        assert_eq!(
            r.summary().as_deref(),
            Some("1 of 2 test runs from rargo's cache (1 run on the server; `--no-cache` runs every test)")
        );
        r.invariants();
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

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Final {
        Passed,
        Flaky,
        Failed,
        Cancelled,
    }

    #[derive(Debug, PartialEq, Eq)]
    struct Summary {
        finals: Vec<Final>,
        code: i32,
    }

    struct Scenario {
        tests: usize,
        retries: usize,
        fail_fast: bool,
        outcomes: Vec<Vec<bool>>,
    }

    fn list_key() -> String {
        "list".into()
    }

    fn test_key(t: usize, attempt: usize) -> String {
        format!("t{t}/{attempt}")
    }

    fn nextest(s: &Scenario, recorded: &dyn Fn(&str) -> Option<bool>, asked: &mut Vec<TestAsked>) -> Summary {
        let Some(_) = recorded(&list_key()) else {
            asked.push(TestAsked { key: list_key(), listing: true });
            return Summary { finals: vec![], code: 0 };
        };
        let mut finals = Vec::new();
        let mut stop = false;
        for t in 0..s.tests {
            if stop {
                finals.push(Final::Cancelled);
                continue;
            }
            let mut attempt = 1;
            let mut failed_once = false;
            let fin = loop {
                let key = test_key(t, attempt);
                let passed = match recorded(&key) {
                    Some(p) => p,
                    None => {
                        asked.push(TestAsked { key, listing: false });
                        true
                    }
                };
                if passed {
                    break if failed_once { Final::Flaky } else { Final::Passed };
                }
                failed_once = true;
                if attempt > s.retries {
                    break Final::Failed;
                }
                attempt += 1;
            };
            if fin == Final::Failed && s.fail_fast {
                stop = true;
            }
            finals.push(fin);
        }
        let code = if finals.contains(&Final::Failed) { 100 } else { 0 };
        Summary { finals, code }
    }

    fn stock(s: &Scenario) -> Summary {
        let truth = |k: &str| -> Option<bool> {
            if k == list_key() {
                return Some(true);
            }
            let (t, a) = k[1..].split_once('/').unwrap();
            Some(s.outcomes[t.parse::<usize>().unwrap()][a.parse::<usize>().unwrap() - 1])
        };
        nextest(s, &truth, &mut Vec::new())
    }

    #[test]
    fn generated_results_give_stock_nextest_s_summary_and_fail_fast_cancels_what_it_should() {
        for seed in 1..=300u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let tests = rng.below(7) as usize;
            let retries = rng.below(3) as usize;
            let s = Scenario {
                tests,
                retries,
                fail_fast: rng.below(2) == 0,
                outcomes: (0..tests)
                    .map(|_| match rng.below(4) {
                        0 => vec![false; retries + 1],
                        1 => {
                            let mut v = vec![false; retries + 1];
                            v[rng.below(retries as u64 + 1) as usize] = true;
                            v
                        }
                        _ => vec![true; retries + 1],
                    })
                    .collect(),
            };
            let (mut r, fx) = NextestRun::new();
            assert_eq!(fx, [Effect::RunNextest]);
            assert_eq!(r.asked(Asked::Build(vec!["test".into()]), None), [Effect::Build(vec!["test".into()])]);
            let mut todo: Vec<Effect> = r.built();
            let mut recorded: BTreeMap<String, bool> = BTreeMap::new();
            let mut runs = 0usize;
            let mut last = None;
            let truth = |k: &str| -> bool {
                if k == list_key() {
                    return true;
                }
                let (t, a) = k[1..].split_once('/').unwrap();
                s.outcomes[t.parse::<usize>().unwrap()][a.parse::<usize>().unwrap() - 1]
            };
            while let Some(e) = todo.pop() {
                let next = match e {
                    Effect::RunNextest => {
                        let mut asked = Vec::new();
                        let seen = |k: &str| recorded.get(k).copied();
                        last = Some(nextest(&s, &seen, &mut asked));
                        if rng.below(3) == 0 {
                            let dup = asked.clone();
                            asked.extend(dup);
                        }
                        r.round_ended(asked)
                    }
                    Effect::RunTests(keys) => {
                        let mut fx = Vec::new();
                        let mut order = keys.clone();
                        if rng.below(2) == 0 {
                            order.reverse();
                        }
                        for k in order {
                            assert!(!recorded.contains_key(&k), "seed {seed}: {k} ran twice");
                            recorded.insert(k.clone(), truth(&k));
                            runs += (k != list_key()) as usize;
                            fx.extend(r.test_ran(&k, rng.below(2) == 0));
                            r.invariants();
                        }
                        fx
                    }
                    Effect::ShowNextest => Vec::new(),
                    Effect::Finish => break,
                    other => panic!("seed {seed}: {other:?} in the test rounds"),
                };
                r.invariants();
                todo.extend(next);
            }
            assert_eq!(r.phase(), Phase::Done, "seed {seed}");
            let got = last.expect("nextest ran");
            assert_eq!(got, stock(&s), "seed {seed}: the replayed run is stock nextest's");
            if s.fail_fast {
                let first_fail = got.finals.iter().position(|f| *f == Final::Failed);
                for (t, f) in got.finals.iter().enumerate() {
                    let should = first_fail.is_some_and(|ff| t > ff);
                    assert_eq!(
                        *f == Final::Cancelled,
                        should,
                        "seed {seed}: fail-fast cancels exactly the tests after the first failure"
                    );
                }
            } else {
                assert!(!got.finals.contains(&Final::Cancelled), "seed {seed}: nothing is cancelled without fail-fast");
            }
            let counted = r.replayed + r.run_now;
            assert_eq!(counted, runs, "seed {seed}: the summary counts every test run once");
            assert_eq!(r.summary().is_some(), runs > 0);
        }
    }
}
