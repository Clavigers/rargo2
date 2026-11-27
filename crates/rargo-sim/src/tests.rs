use crate::{Bug, Fault, Kind, Off, run, run_off, search};

fn clean(seeds: std::ops::Range<u64>) {
    let failures = search(seeds, None);
    let text: Vec<String> = failures.iter().map(|f| f.to_string()).collect();
    assert!(failures.is_empty(), "{}", text.join("\n"));
}

// Keep the sampled ranges while running each batch under its own CPU budget.
macro_rules! seed_batches {
    ($($name:ident: $start:literal..$end:literal),+ $(,)?) => {
        $(
            #[test]
            fn $name() {
                clean($start..$end);
            }
        )+
    };
}

seed_batches! {
    seeds_0_to_4: 0..4,
    seeds_4_to_8: 4..8,
    seeds_8_to_12: 8..12,
    seeds_200_to_204: 200..204,
    seeds_204_to_208: 204..208,
    seeds_208_to_212: 208..212,
    seeds_212_to_216: 212..216,
    seeds_216_to_220: 216..220,
    seeds_220_to_224: 220..224,
    seeds_400_to_404: 400..404,
    seeds_404_to_408: 404..408,
    seeds_408_to_412: 408..412,
    seeds_412_to_416: 412..416,
    seeds_416_to_420: 416..420,
    seeds_420_to_424: 420..424,
}

#[test]
fn a_seed_replays_the_same_run() {
    let first = run(3, None).expect("passes");
    assert_eq!(first, run(3, None).expect("passes"));
    assert!(first.runs > 0);
    assert_ne!(first.trace, run(5, None).expect("passes").trace);
}

#[test]
fn seeded_test_commands_report_passes_failures_and_timeouts() {
    let report = run(2, None).expect("commands finish");
    assert!(report.tests_passed > 0);
    assert!(report.tests_failed > 0);
    assert!(report.tests_timed_out > 0);
}

#[test]
fn a_worker_restarts_even_if_its_link_was_cut_when_it_crashed() {
    let report = run_off(85, None, Off::none().with(Fault::ServerCrash)).expect("the only worker reconnects and the build finishes");
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
fn dozens_of_client_rounds_overlap_during_eviction() {
    let report = run_off(257, None, Off::none().with(Fault::ServerCrash)).unwrap_or_else(|failure| panic!("{failure}"));
    assert!(report.clients_at_eviction >= 24, "{report:?}");
    assert!(report.cache_hits > 0, "clients reuse action outputs");
    assert!(report.silences > 0, "clients give up on a silent server");
}

#[test]
fn the_seeds_exercise_faults_the_cache_and_eviction() {
    // Together these seeds exercise every condition checked below. The seed
    // batches separately run the simulator's invariants over a wider sample.
    let reports = [0, 8].map(|seed| run(seed, None).expect("passes"));
    assert!(reports.iter().any(|r| r.cuts > 0), "links are cut");
    assert!(reports.iter().any(|r| r.server_restarts > 0), "clients finish despite server restarts");
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
    assert!(reports.iter().any(|r| r.crashes > 0), "client daemons crash and restart");
    assert!(reports.iter().any(|r| r.lost > 0 && r.retransmits > 0), "rargo-net retransmits after datagram loss");
    assert!(reports.iter().any(|r| r.duplicated > 0), "packets arrive twice");
    assert!(reports.iter().any(|r| r.reordered > 0), "packets overtake the ones sent before them");
}

fn turns_off_alone(f: Fault) {
    let count = |r: &crate::Report, f: Fault| match f {
        Fault::Loss => r.lost,
        Fault::Duplicate => r.duplicated,
        Fault::Reorder => r.reordered,
        Fault::ServerCrash => r.server_restarts,
        Fault::Partition => r.partitioned,
    };
    // Seed 7 exercises all five faults, including when any one is switched off.
    let report = run_off(7, None, Off::none().with(f)).expect("passes");
    assert_eq!(count(&report, f), 0, "--off {} leaves no {0}", f.name());
    for other in Fault::ALL.into_iter().filter(|o| *o != f) {
        assert!(count(&report, other) > 0, "--off {} leaves {}", f.name(), other.name());
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
fn server_crashes_turn_off_alone() {
    turns_off_alone(Fault::ServerCrash);
}

#[test]
fn partitions_turn_off_alone() {
    turns_off_alone(Fault::Partition);
}

#[test]
fn a_seed_with_every_fault_off_replays_the_same_run() {
    let all = Fault::ALL.into_iter().fold(Off::none(), Off::with);
    let a = run_off(9, None, all.clone()).expect("passes");
    assert_eq!((a.lost, a.duplicated, a.reordered, a.server_restarts, a.partitioned), (0, 0, 0, 0, 0));
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

fn caught(seed: u64, bug: Bug, kind: fn(&Kind) -> bool) {
    caught_off(seed, bug, Off::none(), kind);
}

fn caught_off(seed: u64, bug: Bug, off: Off, kind: fn(&Kind) -> bool) {
    let first = run_off(seed, Some(bug), off.clone()).expect_err("the seed catches the injected bug");
    assert!(kind(&first.kind), "{} is caught as {}, not {first}", bug.name(), first.kind.name());
    let again = run_off(first.seed, Some(bug), off).expect_err("a failing seed fails again");
    assert_eq!(again, first, "{} replays exactly", first.replay());
}

#[test]
fn evicting_a_pinned_blob_is_caught() {
    caught(0, Bug::EvictPinned, |k| matches!(k, Kind::LostPinned(_)));
}

#[test]
fn delivering_a_stale_output_is_caught() {
    caught(6, Bug::StaleDelivery, |k| matches!(k, Kind::Drift(_)));
}

#[test]
fn never_answering_a_request_is_caught() {
    caught_off(1, Bug::NeverAnswer, Off::none().with(Fault::ServerCrash), |k| matches!(k, Kind::Liveness(_) | Kind::Deadlock(_)));
}

#[test]
fn evicting_content_an_upload_claimed_is_caught() {
    caught(11, Bug::EvictClaimed, |k| matches!(k, Kind::LostPinned(_)));
}

#[test]
fn a_client_trusting_what_the_server_no_longer_holds_is_caught() {
    caught(6, Bug::StaleKnowledge, |k| matches!(k, Kind::Liveness(_) | Kind::Deadlock(_)));
}

#[test]
fn a_client_that_never_gives_up_on_a_silent_server_is_caught() {
    caught(0, Bug::DeafToSilence, |k| matches!(k, Kind::Silence(_)));
}

#[test]
fn a_restarted_daemon_trusting_the_dead_one_is_caught() {
    caught(0, Bug::TrustAfterCrash, |k| matches!(k, Kind::StateMachine(_) | Kind::Liveness(_) | Kind::Deadlock(_)));
}

#[test]
fn a_rescan_that_misses_the_edits_is_caught() {
    caught(0, Bug::DeafToEdits, |k| matches!(k, Kind::Drift(_)));
}

#[test]
fn outputs_that_never_reach_target_are_caught() {
    caught(0, Bug::UnshownOutputs, |k| matches!(k, Kind::Drift(d) if d.contains("target/")));
}

#[test]
fn disk_faults_end_the_command_or_recover_with_its_pins() {
    let crash = run(0, None).expect("builds recover after a power cut");
    assert_eq!((crash.disk_crashes, crash.torn_writes), (1, 0));
    let torn = run(3, None).expect("builds recover after a torn write");
    assert_eq!((torn.disk_crashes, torn.torn_writes), (1, 1));
    let full = run(19, None).expect("a full disk ends the command instead of deadlocking");
    assert_eq!(full.disk_full_errors, 1);
}

#[test]
fn a_disk_full_result_reaches_the_client_after_reconnection() {
    let report = run_off(32, None, Off::none().with(Fault::ServerCrash))
        .expect("the client gets the disk error after reconnecting");
    assert_eq!(report.disk_full_errors, 1);

    for seed in [708, 813] {
        let report = run(seed, None).expect("the command finishes with both faults enabled");
        assert!(report.server_restarts > 0, "seed {seed}");
    }
}

#[test]
fn compiler_failures_are_repeatable_for_the_same_inputs() {
    use crate::model::compile;

    let errors: Vec<_> = (0..100)
        .filter_map(|seed| compile(seed, 1000, &[b"source"]).err().map(|error| (seed, error)))
        .collect();
    assert!(!errors.is_empty());
    for (seed, error) in errors {
        assert_eq!(compile(seed, 1000, &[b"source"]), Err(error));
        assert!((0..100).any(|version| compile(seed, 1000, &[&[version]]).is_ok()));
    }
}

#[test]
fn builds_exercise_compiler_exits_and_discard_staged_artifacts_on_cancel() {
    // Seed 3 fails a build, cancels one with staged outputs and aborts a long
    // run; the seed batches run the invariants over a wider sample.
    let r = run(3, None).expect("passes");
    assert!(r.failed_builds > 0);
    assert!(r.long_runs > 0 && r.cancelled_builds > 0 && r.aborted_runs > 0);
    assert!(r.cancelled_with_outputs > 0);
}
