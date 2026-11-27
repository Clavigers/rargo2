use crate::rng::Rng;
use rargo_core::build::nextest_run::{Asked, Effect, NextestRun, Phase, TestAsked};
use rargo_machine::StateMachine;
use std::collections::{BTreeMap, VecDeque};

const LIST: &str = "list";
pub(crate) const TIMEOUT_MS: u64 = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Passed,
    Failed,
    TimedOut,
}

pub(crate) struct TestRun {
    pub machine: NextestRun,
    metadata_answered: bool,
    // The delay for a hanging process is its deadline, since it never exits on its own.
    suite: BTreeMap<String, (Outcome, u64)>,
    results: BTreeMap<String, Outcome>,
}

impl TestRun {
    pub fn generate(seed: u64, client: u64, round: u64) -> Option<Self> {
        let mut rng = Rng::new(seed ^ 0x6e65_7874_6573_7421 ^ (client << 32) ^ round);
        if !rng.chance(500) {
            return None;
        }
        let mut suite = BTreeMap::from([(LIST.into(), (Outcome::Passed, 1))]);
        for i in 0..rng.range(1, 5) {
            let (outcome, delay) = match rng.below(3) {
                0 => (Outcome::Passed, rng.range(1, TIMEOUT_MS - 1)),
                1 => (Outcome::Failed, rng.range(1, TIMEOUT_MS - 1)),
                _ => (Outcome::TimedOut, TIMEOUT_MS),
            };
            suite.insert(format!("test_{i}"), (outcome, delay));
        }

        let (machine, effects) = NextestRun::new();
        let mut run = Self {
            machine,
            metadata_answered: false,
            suite,
            results: BTreeMap::new(),
        };
        assert!(run.drive(effects).is_empty());
        assert_eq!(run.machine.phase(), Phase::Build);
        Some(run)
    }

    pub fn built(&mut self) -> Vec<(String, u64)> {
        let effects = self.machine.built();
        self.drive(effects)
    }

    pub fn completed(
        &mut self,
        key: &str,
        elapsed_ms: u64,
    ) -> (Option<Outcome>, Vec<(String, u64)>) {
        let (outcome, delay) = self.suite[key];
        assert_eq!(
            elapsed_ms, delay,
            "test {key} must finish at its exit or deadline"
        );
        assert!(
            self.results.insert(key.to_owned(), outcome).is_none(),
            "test {key} reported twice"
        );
        let effects = self.machine.test_ran(key, false);
        (
            if key == LIST { None } else { Some(outcome) },
            self.drive(effects),
        )
    }

    fn drive(&mut self, effects: Vec<Effect>) -> Vec<(String, u64)> {
        let mut todo = VecDeque::from(effects);
        let mut scheduled = Vec::new();
        self.machine.invariants();
        while let Some(effect) = todo.pop_front() {
            let next = match effect {
                Effect::RunNextest if self.machine.phase() == Phase::Ask => {
                    let asked = if self.metadata_answered {
                        Asked::Build(vec!["test".into(), "--no-run".into()])
                    } else {
                        Asked::Metadata(vec!["metadata".into()])
                    };
                    self.machine.asked(asked, None)
                }
                Effect::Metadata(args) => {
                    self.metadata_answered = true;
                    self.machine.metadata_answered(&args, 0, b"")
                }
                Effect::Build(_) => Vec::new(), // World answers through its ordinary build events.
                Effect::RunNextest => {
                    // Nextest lists the binary first. Later passes can request the same keys again.
                    let keys: Vec<_> = if self.results.contains_key(LIST) {
                        self.suite.keys().map(String::as_str).collect()
                    } else {
                        vec![LIST, LIST]
                    };
                    self.machine.round_ended(
                        keys.into_iter()
                            .map(|key| TestAsked {
                                key: key.into(),
                                listing: key == LIST,
                            })
                            .collect(),
                    )
                }
                Effect::RunTests(keys) => {
                    scheduled.extend(keys.into_iter().map(|key| {
                        assert!(
                            !self.results.contains_key(&key),
                            "test {key} scheduled again"
                        );
                        let delay = self.suite[&key].1;
                        (key, delay)
                    }));
                    Vec::new()
                }
                Effect::ShowNextest => Vec::new(),
                Effect::Finish => {
                    assert_eq!(
                        self.results.len(),
                        self.suite.len(),
                        "nextest finished with missing results"
                    );
                    Vec::new()
                }
                other => panic!("unexpected nextest effect for simulated suite: {other:?}"),
            };
            self.machine.invariants();
            todo.extend(next);
        }
        scheduled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_and_tests_complete_once_at_their_exit_or_deadline() {
        let (machine, effects) = NextestRun::new();
        let mut run = TestRun {
            machine,
            metadata_answered: false,
            suite: BTreeMap::from([
                (LIST.into(), (Outcome::Passed, 1)),
                ("passes".into(), (Outcome::Passed, 3)),
                ("fails".into(), (Outcome::Failed, 2)),
                ("hangs".into(), (Outcome::TimedOut, TIMEOUT_MS)),
            ]),
            results: BTreeMap::new(),
        };
        assert!(run.drive(effects).is_empty());
        assert_eq!(run.built(), [(LIST.into(), 1)]);
        let (outcome, scheduled) = run.completed(LIST, 1);
        assert_eq!(outcome, None);
        assert_eq!(
            scheduled,
            [
                ("fails".into(), 2),
                ("hangs".into(), TIMEOUT_MS),
                ("passes".into(), 3)
            ]
        );
        assert_eq!(run.completed("fails", 2), (Some(Outcome::Failed), vec![]));
        assert_eq!(run.completed("passes", 3), (Some(Outcome::Passed), vec![]));
        assert_eq!(run.machine.phase(), Phase::Tests);
        assert_eq!(
            run.completed("hangs", TIMEOUT_MS),
            (Some(Outcome::TimedOut), vec![])
        );
        assert_eq!(run.machine.phase(), Phase::Done);
    }
}
