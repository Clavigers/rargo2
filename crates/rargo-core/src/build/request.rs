use rargo_machine::StateMachine;
use super::pure::{missing_components, needs_network};
use std::collections::BTreeSet;

pub type Component = (String, String);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    Code(i32),
    Retry,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Snapshot,
    Plan,
    Fetch,
    Units,
    Deliver,
    Done(End),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    Prepare,
    Plan { as_build: bool },
    RememberFork(bool),
    Fetch { components: Vec<Component>, offline: bool },
    ShowPlanFailure,
    RunUnits,
    Deliver,
    End(End),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    Pending,
    Running,
    Meta,
    Done,
    Failed,
    Skipped,
}

impl Unit {
    fn ended(self) -> bool {
        matches!(self, Unit::Done | Unit::Failed | Unit::Skipped)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Go {
    Ready,
    Wait,
    Skip,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OnFailure {
    Install(Vec<Component>),
    Report,
}

#[derive(Clone, Debug)]
pub struct BuildRequest {
    phase: Phase,
    check: bool,
    as_build: bool,
    offline: bool,
    fetched: bool,
    keep_going: bool,
    units: Vec<Unit>,
    installed: BTreeSet<Component>,
    retried: BTreeSet<usize>,
    installing: BTreeSet<usize>,
    inconsistent: bool,
    any_failed: bool,
}

impl BuildRequest {
    pub fn new(check: bool, as_build: bool, offline: bool) -> (BuildRequest, Vec<Effect>) {
        let req = BuildRequest {
            phase: Phase::Snapshot,
            check,
            as_build: check && as_build,
            offline,
            fetched: false,
            keep_going: false,
            units: Vec::new(),
            installed: BTreeSet::new(),
            retried: BTreeSet::new(),
            installing: BTreeSet::new(),
            inconsistent: false,
            any_failed: false,
        };
        (req, vec![Effect::Prepare])
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn as_build(&self) -> bool {
        self.as_build
    }

    fn end(&mut self, end: End) -> Vec<Effect> {
        assert!(!matches!(self.phase, Phase::Done(_)), "a request ends once");
        self.phase = Phase::Done(end);
        self.installing.clear();
        assert!(self.installing.is_empty(), "nothing is installed after the end");
        vec![Effect::End(end)]
    }

    fn in_phase(&self, p: Phase) -> bool {
        self.phase == p
    }

    pub fn prepared(&mut self) -> Vec<Effect> {
        if !self.in_phase(Phase::Snapshot) {
            return Vec::new();
        }
        assert!(self.units.is_empty(), "a request is prepared before it is planned");
        self.phase = Phase::Plan;
        assert!(self.check || !self.as_build, "only a check is planned as a build");
        vec![Effect::Plan { as_build: self.as_build }]
    }

    pub fn planned(&mut self, rustc_fork: bool, units: usize, keep_going: bool) -> Vec<Effect> {
        if !self.in_phase(Phase::Plan) {
            return Vec::new();
        }
        if self.check && rustc_fork != self.as_build {
            self.as_build = rustc_fork;
            assert_eq!(self.phase, Phase::Plan, "a fork is planned again");
            return vec![Effect::RememberFork(rustc_fork), Effect::Plan { as_build: rustc_fork }];
        }
        assert!(self.units.is_empty() && !self.any_failed && !self.inconsistent, "a request is planned once");
        self.phase = Phase::Units;
        self.keep_going = keep_going;
        self.units = vec![Unit::Pending; units];
        assert_eq!(self.units.len(), units, "every planned unit is pending");
        vec![Effect::RunUnits]
    }

    pub fn plan_failed(&mut self, stderr: &[u8], code: i32) -> Vec<Effect> {
        if !self.in_phase(Phase::Plan) {
            return Vec::new();
        }
        if self.fetched || !needs_network(stderr) {
            let mut fx = vec![Effect::ShowPlanFailure];
            fx.extend(self.end(End::Code(code)));
            return fx;
        }
        self.fetched = true;
        let components: Vec<Component> =
            missing_components(stderr).into_iter().filter(|c| !self.installed.contains(c)).collect();
        if self.offline && components.is_empty() {
            let mut fx = vec![Effect::ShowPlanFailure];
            fx.extend(self.end(End::Code(code)));
            return fx;
        }
        self.installed.extend(components.iter().cloned());
        self.phase = Phase::Fetch;
        assert!(self.fetched, "a fetch happens once");
        assert!(components.iter().all(|c| self.installed.contains(c)), "a fetched component is remembered");
        vec![Effect::Fetch { components, offline: self.offline }]
    }

    pub fn fetched(&mut self, ok: bool) -> Vec<Effect> {
        if !self.in_phase(Phase::Fetch) {
            return Vec::new();
        }
        assert!(self.fetched, "only a fetch ends in fetched");
        if !ok {
            return self.end(End::Code(101));
        }
        self.phase = Phase::Snapshot;
        assert!(self.units.is_empty(), "a fetch comes before any unit");
        vec![Effect::Prepare]
    }

    pub fn dep(&self, j: usize, meta_ok: bool) -> Go {
        if !self.in_phase(Phase::Units) {
            return Go::Skip;
        }
        match self.units[j] {
            Unit::Done => Go::Ready,
            Unit::Meta if meta_ok => Go::Ready,
            Unit::Failed | Unit::Skipped => Go::Skip,
            Unit::Pending | Unit::Running | Unit::Meta => Go::Wait,
        }
    }

    pub fn may_start(&mut self, i: usize) -> bool {
        if !self.in_phase(Phase::Units) || (self.any_failed && !self.keep_going) {
            return false;
        }
        assert_eq!(self.units[i], Unit::Pending, "a unit starts once");
        self.units[i] = Unit::Running;
        true
    }

    pub fn unit_meta(&mut self, i: usize) {
        if !self.in_phase(Phase::Units) {
            return;
        }
        assert!(i < self.units.len(), "unit {i} was planned");
        if self.units[i] == Unit::Running {
            self.units[i] = Unit::Meta;
        }
        assert_ne!(self.units[i], Unit::Running, "a unit with metadata is no longer just running");
    }

    pub fn unit_failed(&mut self, i: usize, stderr: &[u8]) -> OnFailure {
        if !self.in_phase(Phase::Units) || self.retried.contains(&i) {
            return OnFailure::Report;
        }
        assert!(!self.units[i].ended(), "only a unit not yet ended fails");
        let missing = missing_components(stderr);
        if missing.is_empty() {
            return OnFailure::Report;
        }
        self.retried.insert(i);
        self.installing.insert(i);
        let new: Vec<Component> = missing.into_iter().filter(|c| !self.installed.contains(c)).collect();
        self.installed.extend(new.iter().cloned());
        assert!(self.retried.contains(&i) && self.installing.contains(&i), "unit {i} installs on its one retry");
        OnFailure::Install(new)
    }

    pub fn components_installed(&mut self, i: usize, ok: bool) -> bool {
        let was = self.installing.remove(&i);
        assert!(!was || self.retried.contains(&i), "only a retried unit was installing");
        assert!(!self.installing.contains(&i), "unit {i} installs no more");
        was && ok && self.in_phase(Phase::Units)
    }

    pub fn unit_inconsistent(&mut self, i: usize) {
        if self.in_phase(Phase::Units) {
            assert!(!self.units[i].ended(), "only a unit still running finds its artifacts inconsistent");
            self.inconsistent = true;
        }
    }

    pub fn unit_ended(&mut self, i: usize, end: Unit) {
        assert!(end.ended(), "a unit ends done, failed or skipped");
        if !self.in_phase(Phase::Units) {
            return;
        }
        assert!(!self.units[i].ended(), "a unit ends once");
        self.units[i] = end;
        self.installing.remove(&i);
        if end == Unit::Failed {
            self.any_failed = true;
        }
    }

    pub fn units_ended(&mut self) -> (bool, Vec<Effect>) {
        if !self.in_phase(Phase::Units) {
            return (false, Vec::new());
        }
        assert!(self.units.iter().all(|u| u.ended()), "every unit has ended");
        if self.inconsistent {
            return (false, self.end(End::Retry));
        }
        let ok = !self.any_failed && self.units.iter().all(|u| *u == Unit::Done);
        if !ok {
            return (false, self.end(End::Code(101)));
        }
        self.phase = Phase::Deliver;
        (true, vec![Effect::Deliver])
    }

    pub fn delivered(&mut self, code: i32) -> Vec<Effect> {
        if !self.in_phase(Phase::Deliver) {
            return Vec::new();
        }
        assert!(self.units.iter().all(|u| *u == Unit::Done), "only a whole build is delivered");
        let fx = self.end(End::Code(code));
        assert_eq!(self.phase, Phase::Done(End::Code(code)));
        fx
    }

    pub fn cancel(&mut self) -> Vec<Effect> {
        if matches!(self.phase, Phase::Done(_)) {
            return Vec::new();
        }
        let fx = self.end(End::Cancelled);
        assert_eq!(self.phase, Phase::Done(End::Cancelled), "a cancel ends any phase");
        fx
    }
}

impl StateMachine for BuildRequest {
    fn invariants(&self) {
        assert!(self.check || !self.as_build, "only a check is planned as a build");
        assert!(self.installing.is_subset(&self.retried), "a unit installs components only on its one retry");
        match self.phase {
            Phase::Snapshot | Phase::Plan | Phase::Fetch => {
                assert!(self.units.is_empty(), "units exist only once planned")
            }
            Phase::Units => {
                assert!(self.retried.iter().all(|&i| i < self.units.len()));
                if self.any_failed {
                    assert!(self.units.contains(&Unit::Failed), "a failure is a failed unit");
                }
            }
            Phase::Deliver => {
                assert!(self.units.iter().all(|u| *u == Unit::Done), "only a whole build is delivered");
                assert!(!self.inconsistent && !self.any_failed);
            }
            Phase::Done(_) => assert!(self.installing.is_empty(), "nothing is installed after the end"),
        }
        if self.phase == Phase::Fetch {
            assert!(self.fetched, "a fetch happens once");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MISSING_MIRI: &[u8] =
        b"error: 'cargo-miri' is not installed for the toolchain 'nightly-x86_64-unknown-linux-gnu'.\n\
help: run `rustup component add --toolchain nightly-x86_64-unknown-linux-gnu miri` to install it\n";

    const MISSING_CLIPPY: &[u8] =
        b"error: 'cargo-clippy' is not installed for the toolchain 'stable-x86_64-unknown-linux-gnu'.\n\
help: run `rustup component add clippy` to install it\n";

    const OFFLINE: &[u8] = b"error: no matching package named `anyhow` found\n\
location searched: crates.io index\n\
required by package `flaky v0.1.0 (/w)`\n\
note: offline mode (via `--offline`) can sometimes cause surprising resolution failures\n";

    const COMPILE_ERROR: &[u8] = b"error[E0425]: cannot find value `x` in this scope\n";

    fn miri() -> Component {
        ("miri".into(), "nightly-x86_64-unknown-linux-gnu".into())
    }

    #[test]
    fn a_plan_missing_a_component_installs_it_with_the_fetch_and_plans_again() {
        let (mut r, fx) = BuildRequest::new(false, false, false);
        assert_eq!(fx, [Effect::Prepare]);
        assert_eq!(r.prepared(), [Effect::Plan { as_build: false }]);
        assert_eq!(r.plan_failed(MISSING_MIRI, 101), [Effect::Fetch { components: vec![miri()], offline: false }]);
        assert_eq!(r.fetched(true), [Effect::Prepare]);
        assert_eq!(r.prepared(), [Effect::Plan { as_build: false }]);
        assert_eq!(r.plan_failed(MISSING_MIRI, 101), [Effect::ShowPlanFailure, Effect::End(End::Code(101))]);
        r.invariants();
    }

    #[test]
    fn an_offline_plan_needing_the_network_shows_cargo_s_failure() {
        let (mut r, _) = BuildRequest::new(false, false, true);
        r.prepared();
        assert_eq!(r.plan_failed(OFFLINE, 101), [Effect::ShowPlanFailure, Effect::End(End::Code(101))]);
        let (mut r, _) = BuildRequest::new(false, false, true);
        r.prepared();
        assert_eq!(
            r.plan_failed(MISSING_CLIPPY, 101),
            [Effect::Fetch {
                components: vec![("clippy".into(), "stable-x86_64-unknown-linux-gnu".into())],
                offline: true
            }],
            "offline still installs components"
        );
    }

    #[test]
    fn a_check_is_planned_as_a_build_when_the_compiler_is_a_fork() {
        let (mut r, _) = BuildRequest::new(true, false, false);
        r.prepared();
        assert_eq!(r.planned(true, 2, false), [Effect::RememberFork(true), Effect::Plan { as_build: true }]);
        assert_eq!(r.planned(true, 2, false), [Effect::RunUnits]);
        assert!(r.as_build());
        let (mut r, _) = BuildRequest::new(false, true, false);
        r.prepared();
        assert_eq!(r.planned(true, 1, false), [Effect::RunUnits], "only a check changes how it is planned");
    }

    #[test]
    fn a_failed_unit_stops_its_dependents_and_the_build_reports_it() {
        let (mut r, _) = BuildRequest::new(false, false, false);
        r.prepared();
        r.planned(false, 3, false);
        assert!(r.may_start(0));
        assert_eq!(r.dep(0, false), Go::Wait);
        assert_eq!(r.unit_failed(0, COMPILE_ERROR), OnFailure::Report);
        r.unit_ended(0, Unit::Failed);
        assert_eq!(r.dep(0, true), Go::Skip);
        r.unit_ended(1, Unit::Skipped);
        assert!(!r.may_start(2), "after a failure nothing new starts unless asked to keep going");
        r.unit_ended(2, Unit::Skipped);
        assert_eq!(r.units_ended(), (false, vec![Effect::End(End::Code(101))]));
        r.invariants();
    }

    #[test]
    fn a_unit_missing_a_component_installs_it_once_and_runs_again() {
        let (mut r, _) = BuildRequest::new(false, false, false);
        r.prepared();
        r.planned(false, 2, true);
        assert!(r.may_start(0) && r.may_start(1));
        assert_eq!(r.unit_failed(0, MISSING_MIRI), OnFailure::Install(vec![miri()]));
        assert_eq!(r.unit_failed(1, MISSING_MIRI), OnFailure::Install(vec![]), "the other waits for the same install");
        assert!(r.components_installed(0, true));
        assert!(r.components_installed(1, true));
        assert_eq!(r.unit_failed(0, MISSING_MIRI), OnFailure::Report, "one retry");
        r.unit_ended(0, Unit::Failed);
        r.unit_ended(1, Unit::Done);
        assert_eq!(r.units_ended().1, [Effect::End(End::Code(101))]);
        r.invariants();
    }

    #[test]
    fn inconsistent_artifacts_retry_the_request_and_a_whole_build_is_delivered() {
        let (mut r, _) = BuildRequest::new(false, false, false);
        r.prepared();
        r.planned(false, 1, false);
        r.may_start(0);
        r.unit_inconsistent(0);
        r.unit_ended(0, Unit::Failed);
        assert_eq!(r.units_ended(), (false, vec![Effect::End(End::Retry)]));
        let (mut r, _) = BuildRequest::new(false, false, false);
        r.prepared();
        r.planned(false, 1, false);
        r.may_start(0);
        r.unit_meta(0);
        assert_eq!(r.dep(0, true), Go::Ready);
        assert_eq!(r.dep(0, false), Go::Wait);
        r.unit_ended(0, Unit::Done);
        assert_eq!(r.units_ended(), (true, vec![Effect::Deliver]));
        assert_eq!(r.delivered(0), [Effect::End(End::Code(0))]);
        assert_eq!(r.cancel(), [], "an ended request has nothing to cancel");
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
    fn generated_planner_and_unit_outcomes_end_every_request_cleanly() {
        let stderrs: [&[u8]; 4] = [MISSING_MIRI, MISSING_CLIPPY, OFFLINE, COMPILE_ERROR];
        for seed in 1..=300u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let (check, as_build, offline) = (rng.below(2) == 0, rng.below(2) == 0, rng.below(4) == 0);
            let (mut r, fx) = BuildRequest::new(check, as_build, offline);
            let mut todo: std::collections::VecDeque<Effect> = fx.into();
            let mut fetches = 0;
            let mut installed: Vec<Component> = Vec::new();
            let mut deps: Vec<Vec<usize>> = Vec::new();
            let mut cancel_at = if rng.below(4) == 0 { Some(rng.below(40)) } else { None };
            let mut reported = BTreeSet::new();
            let mut steps = 0u64;
            let mut ended = None;
            while ended.is_none() {
                steps += 1;
                assert!(steps < 10_000, "seed {seed}: the request ends");
                if cancel_at == Some(steps) {
                    cancel_at = None;
                    let was_done = matches!(r.phase(), Phase::Done(_));
                    let fx = r.cancel();
                    if was_done {
                        assert_eq!(fx, [], "seed {seed}: an ended request has nothing to cancel");
                    } else {
                        assert_eq!(fx, [Effect::End(End::Cancelled)], "seed {seed}: a cancel ends any phase");
                        todo.clear();
                        todo.extend(fx);
                    }
                    r.invariants();
                    assert_eq!(r.prepared(), [], "seed {seed}: nothing follows a cancel");
                    assert_eq!(r.fetched(true), []);
                    assert_eq!(r.plan_failed(MISSING_MIRI, 1), []);
                }
                let Some(e) = todo.pop_front() else {
                    match r.phase() {
                        Phase::Units => {}
                        p => panic!("seed {seed}: stuck in {p:?} with nothing to do"),
                    }
                    let n = deps.len();
                    let open: Vec<usize> =
                        (0..n).filter(|&i| !matches!(r.units[i], Unit::Done | Unit::Failed | Unit::Skipped)).collect();
                    if open.is_empty() {
                        let (ok, fx) = r.units_ended();
                        assert_eq!(ok, fx == [Effect::Deliver], "seed {seed}");
                        todo.extend(fx);
                        continue;
                    }
                    let i = open[rng.below(open.len() as u64) as usize];
                    if r.units[i] == Unit::Pending {
                        let gate: Vec<Go> = deps[i].iter().map(|&j| r.dep(j, rng.below(2) == 0)).collect();
                        let stopped = deps[i].iter().any(|&j| matches!(r.units[j], Unit::Failed | Unit::Skipped));
                        assert_eq!(
                            stopped,
                            gate.contains(&Go::Skip),
                            "seed {seed}: unit {i} is stopped by a failed dependency"
                        );
                        if gate.contains(&Go::Skip) || (gate.iter().all(|g| *g == Go::Ready) && !r.may_start(i)) {
                            r.unit_ended(i, Unit::Skipped);
                        }
                    } else {
                        match rng.below(6) {
                            0 if r.units[i] == Unit::Running => r.unit_meta(i),
                            1 | 2 => {
                                let stderr = stderrs[rng.below(4) as usize];
                                match r.unit_failed(i, stderr) {
                                    OnFailure::Install(new) => {
                                        for c in &new {
                                            assert!(!installed.contains(c), "seed {seed}: {c:?} installed twice");
                                        }
                                        installed.extend(new);
                                        let ok = rng.below(5) != 0;
                                        if !r.components_installed(i, ok) {
                                            assert!(reported.insert(i), "seed {seed}: unit {i} reported twice");
                                            r.unit_ended(i, Unit::Failed);
                                        }
                                    }
                                    OnFailure::Report => {
                                        assert!(reported.insert(i), "seed {seed}: unit {i} reported twice");
                                        if rng.below(10) == 0 {
                                            r.unit_inconsistent(i);
                                        }
                                        r.unit_ended(i, Unit::Failed);
                                    }
                                }
                            }
                            _ => r.unit_ended(i, Unit::Done),
                        }
                    }
                    r.invariants();
                    continue;
                };
                let fx = match e {
                    Effect::Prepare => r.prepared(),
                    Effect::Plan { as_build } => {
                        assert!(check || !as_build, "seed {seed}: only a check is planned as a build");
                        if rng.below(3) == 0 {
                            r.plan_failed(stderrs[rng.below(4) as usize], 101)
                        } else {
                            let n = rng.below(8) as usize;
                            deps = (0..n).map(|i| (0..i).filter(|_| rng.below(3) == 0).collect()).collect();
                            r.planned(rng.below(3) == 0, n, rng.below(4) == 0)
                        }
                    }
                    Effect::RememberFork(b) => {
                        assert!(check);
                        assert_eq!(b, r.as_build());
                        Vec::new()
                    }
                    Effect::Fetch { components, offline: o } => {
                        assert_eq!(o, offline);
                        fetches += 1;
                        assert_eq!(fetches, 1, "seed {seed}: a request fetches once");
                        for c in &components {
                            assert!(!installed.contains(c), "seed {seed}: {c:?} installed twice");
                        }
                        installed.extend(components);
                        r.fetched(rng.below(5) != 0)
                    }
                    Effect::ShowPlanFailure => Vec::new(),
                    Effect::RunUnits => {
                        assert_eq!(r.units.len(), deps.len());
                        Vec::new()
                    }
                    Effect::Deliver => r.delivered(rng.below(2) as i32),
                    Effect::End(end) => {
                        ended = Some(end);
                        Vec::new()
                    }
                };
                todo.extend(fx);
                r.invariants();
            }
            assert!(matches!(r.phase(), Phase::Done(_)), "seed {seed}");
            assert!(r.installing.is_empty(), "seed {seed}: an install outlived its request");
            assert!(todo.is_empty(), "seed {seed}: nothing follows the end");
            if let Some(End::Code(0)) = ended {
                assert!(r.units.iter().all(|u| *u == Unit::Done), "seed {seed}: success means every unit was built");
            }
        }
    }
}
