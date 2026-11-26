use crate::{Bug, Fault, Kind, Off, run, run_off, search};

fn clean(seeds: std::ops::Range<u64>) {
    let failures = search(seeds, None);
    let text: Vec<String> = failures.iter().map(|f| f.to_string()).collect();
    assert!(failures.is_empty(), "{}", text.join("\n"));
}

#[test]
fn seeds_0_to_200_keep_every_invariant() {
    clean(0..200);
}

#[test]
fn seeds_200_to_400_keep_every_invariant() {
    clean(200..400);
}

#[test]
fn seeds_400_to_600_keep_every_invariant() {
    clean(400..600);
}

#[test]
fn a_seed_replays_the_same_run() {
    for seed in [3, 77, 150] {
        let a = run(seed, None).expect("passes");
        let b = run(seed, None).expect("passes");
        assert_eq!(a, b);
        assert!(a.runs > 0);
    }
    let a = run(5, None).expect("passes");
    let b = run(6, None).expect("passes");
    assert_ne!(a.trace, b.trace);
}

#[test]
fn a_worker_restarts_even_if_its_link_was_cut_when_it_crashed() {
    let report = run(85, None).expect("the only worker reconnects and the build finishes");
    assert_eq!(report.cuts, 1);
    assert_eq!(report.worker_crashes, 1);
}

#[test]
fn crashes_with_eviction_and_concurrent_client_rounds_finish() {
    clean(635..636);
    clean(8360..8361);
    clean(9391..9392);
}

#[test]
fn the_seeds_exercise_faults_the_cache_and_eviction() {
    let reports: Vec<_> = (0..100).map(|s| run(s, None).expect("passes")).collect();
    assert!(reports.iter().any(|r| r.cuts > 0), "links are cut");
    assert!(reports.iter().any(|r| r.worker_crashes > 0), "workers crash");
    assert!(
        reports.iter().any(|r| r.jobs_lost_to_crashes > 0),
        "jobs interrupted by worker crashes are recovered"
    );
    assert!(reports.iter().any(|r| r.cache_hits > 0), "actions are answered from the cache");
    assert!(reports.iter().any(|r| r.evictions > 0), "content is evicted");
    assert!(reports.iter().any(|r| r.uploads > 0), "clients upload sources");
    assert!(reports.iter().any(|r| r.wants > 0), "the server asks for content a client thought it held");
    assert!(reports.iter().any(|r| r.client_cuts > 0), "client links are cut");
    assert!(reports.iter().any(|r| r.silences > 0), "clients give up on a silent server");
}

#[test]
fn the_network_loses_duplicates_and_reorders() {
    let reports: Vec<_> = (0..100).map(|s| run(s, None).expect("passes")).collect();
    assert!(reports.iter().any(|r| r.lost > 0), "packets are lost and sent again");
    assert!(reports.iter().any(|r| r.duplicated > 0), "packets arrive twice");
    assert!(reports.iter().any(|r| r.reordered > 0), "packets overtake the ones sent before them");
}

fn turns_off_alone(f: Fault) {
    let count = |r: &crate::Report, f: Fault| match f {
        Fault::Loss => r.lost,
        Fault::Duplicate => r.duplicated,
        Fault::Reorder => r.reordered,
    };
    let reports: Vec<_> = (0..100).map(|s| run_off(s, None, Off::none().with(f)).expect("passes")).collect();
    assert!(reports.iter().all(|r| count(r, f) == 0), "--off {} leaves no {0}", f.name());
    for other in Fault::ALL.into_iter().filter(|o| *o != f) {
        assert!(reports.iter().any(|r| count(r, other) > 0), "--off {} leaves {}", f.name(), other.name());
    }
}

#[test]
fn loss_turns_off_alone() {
    turns_off_alone(Fault::Loss);
}

#[test]
fn duplication_turns_off_alone() {
    turns_off_alone(Fault::Duplicate);
}

#[test]
fn reordering_turns_off_alone() {
    turns_off_alone(Fault::Reorder);
}

#[test]
fn a_seed_with_every_fault_off_replays_the_same_run() {
    let all = Fault::ALL.into_iter().fold(Off::none(), Off::with);
    let a = run_off(9, None, all.clone()).expect("passes");
    assert_eq!((a.lost, a.duplicated, a.reordered), (0, 0, 0));
    assert_eq!(a, run_off(9, None, all).expect("passes"), "a seed with faults off replays the same run");
}

#[test]
fn a_failure_replays_with_the_faults_it_ran_without() {
    let off = Off::none().with(Fault::Reorder).with(Fault::Loss);
    let failure = (0..60).find_map(|s| run_off(s, Some(Bug::EvictPinned), off.clone()).err()).expect("the bug is caught");
    let replay = failure.replay();
    assert!(replay.ends_with("--bug evict-pinned --off loss --off reorder"), "{replay}");
    assert_eq!(run_off(failure.seed, Some(Bug::EvictPinned), off).expect_err("fails again"), failure);
}

fn caught(bug: Bug, kind: fn(&Kind) -> bool) {
    let failures = search(0..60, Some(bug));
    let first = failures.first().unwrap_or_else(|| panic!("no seed of 0..60 catches {}", bug.name()));
    assert!(kind(&first.kind), "{} is caught as {}, not {first}", bug.name(), first.kind.name());
    let again = run(first.seed, Some(bug)).expect_err("a failing seed fails again");
    assert_eq!(&again, first, "{} replays exactly", first.replay());
}

#[test]
fn evicting_a_pinned_blob_is_caught() {
    caught(Bug::EvictPinned, |k| matches!(k, Kind::LostPinned(_)));
}

#[test]
fn delivering_a_stale_output_is_caught() {
    caught(Bug::StaleDelivery, |k| matches!(k, Kind::Drift(_)));
}

#[test]
fn never_answering_a_request_is_caught() {
    caught(Bug::NeverAnswer, |k| matches!(k, Kind::Liveness(_) | Kind::Deadlock(_)));
}

#[test]
fn evicting_content_an_upload_claimed_is_caught() {
    caught(Bug::EvictClaimed, |k| matches!(k, Kind::LostPinned(_)));
}

#[test]
fn a_client_trusting_what_the_server_no_longer_holds_is_caught() {
    caught(Bug::StaleKnowledge, |k| matches!(k, Kind::Liveness(_) | Kind::Deadlock(_)));
}

#[test]
fn a_client_that_never_gives_up_on_a_silent_server_is_caught() {
    caught(Bug::DeafToSilence, |k| matches!(k, Kind::Silence(_)));
}
