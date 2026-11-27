use crate::{Bug, Fault, Kind, Off, run, run_off, search};

fn clean(seeds: std::ops::Range<u64>) {
    let failures = search(seeds, None);
    let text: Vec<String> = failures.iter().map(|f| f.to_string()).collect();
    assert!(failures.is_empty(), "{}", text.join("\n"));
}

fn clean_seed(seed: u64) {
    clean(seed..seed + 1);
}

// Each seed runs the full simulator and its invariants.
rargo_machine::test_batches! {
    clean_seed;
    seed_0: 0,
    seed_1: 1,
    seed_2: 2,
    seed_3: 3,
    seed_4: 4,
    seed_5: 5,
    seed_6: 6,
    seed_7: 7,
    seed_8: 8,
    seed_9: 9,
    seed_10: 10,
    seed_11: 11,
    seed_200: 200,
    seed_201: 201,
    seed_202: 202,
    seed_203: 203,
    seed_204: 204,
    seed_205: 205,
    seed_206: 206,
    seed_207: 207,
    seed_208: 208,
    seed_209: 209,
    seed_210: 210,
    seed_211: 211,
    seed_212: 212,
    seed_213: 213,
    seed_214: 214,
    seed_215: 215,
    seed_216: 216,
    seed_217: 217,
    seed_218: 218,
    seed_219: 219,
    seed_220: 220,
    seed_221: 221,
    seed_222: 222,
    seed_223: 223,
    seed_400: 400,
    seed_401: 401,
    seed_402: 402,
    seed_403: 403,
    seed_404: 404,
    seed_405: 405,
    seed_406: 406,
    seed_407: 407,
    seed_408: 408,
    seed_409: 409,
    seed_410: 410,
    seed_411: 411,
    seed_412: 412,
    seed_413: 413,
    seed_414: 414,
    seed_415: 415,
    seed_416: 416,
    seed_417: 417,
    seed_418: 418,
    seed_419: 419,
    seed_420: 420,
    seed_421: 421,
    seed_422: 422,
    seed_423: 423,
}

#[test]
fn a_seed_replays_the_same_run() {
    let first = run(3, None).expect("passes");
    assert_eq!(first, run(3, None).expect("passes"));
    assert!(first.runs > 0);
}

#[test]
fn another_seed_makes_another_run() {
    assert_ne!(run(3, None).expect("passes").trace, run(5, None).expect("passes").trace);
}

#[test]
fn seeded_test_commands_report_passes_failures_and_timeouts() {
    let report = run(49, None).expect("commands finish");
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

mod crashes_with_eviction_and_concurrent_client_rounds_finish {
    use super::clean;

    rargo_machine::test_batches! {
        clean;
        seed_635: 635..636,
        seed_8360: 8360..8361,
        seed_9391: 9391..9392,
    }
}

#[test]
fn seed_54_exercises_faults_the_cache_and_eviction() {
    let report = run(54, None).expect("passes");
    assert!(report.cuts > 0, "links are cut");
    assert!(report.server_restarts > 0, "clients finish despite server restarts");
    assert!(report.worker_crashes > 0, "workers crash");
    assert!(report.jobs_lost_to_crashes > 0, "jobs interrupted by worker crashes are recovered");
    assert!(report.cache_hits > 0, "actions are answered from the cache");
    assert!(report.evictions > 0, "content is evicted");
    assert!(report.uploads > 0, "clients upload sources");
    assert!(report.client_cuts > 0, "client links are cut");
    assert!(report.crashes > 0, "client daemons crash and restart");
    assert!(report.lost > 0 && report.retransmits > 0, "rargo-net retransmits after datagram loss");
    assert!(report.reordered > 0, "packets overtake the ones sent before them");
}

#[test]
fn seed_59_exercises_missing_content_silence_and_duplicates() {
    let report = run(59, None).expect("passes");
    assert!(report.wants > 0, "the server asks for content a client thought it held");
    assert!(report.silences > 0, "clients give up on a silent server");
    assert!(report.duplicated > 0, "packets arrive twice");
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
    caught(4, Bug::StaleKnowledge, |k| matches!(k, Kind::Liveness(_) | Kind::Deadlock(_)));
}

#[test]
fn a_client_that_never_gives_up_on_a_silent_server_is_caught() {
    caught(5, Bug::DeafToSilence, |k| matches!(k, Kind::Silence(_)));
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
    caught(11, Bug::UnshownOutputs, |k| matches!(k, Kind::Drift(d) if d.contains("target/")));
}

#[test]
fn a_build_recovers_after_a_power_cut() {
    let report = run(0, None).expect("builds recover after a power cut");
    assert_eq!((report.disk_crashes, report.torn_writes), (1, 0));
}

#[test]
fn a_build_recovers_after_a_torn_write() {
    let report = run(3, None).expect("builds recover after a torn write");
    assert_eq!((report.disk_crashes, report.torn_writes), (1, 1));
}

#[test]
fn a_full_disk_ends_the_command() {
    let report = run(19, None).expect("a full disk ends the command instead of deadlocking");
    assert_eq!(report.disk_full_errors, 1);
}

#[test]
fn a_disk_full_result_reaches_the_client_after_reconnection() {
    let report = run_off(32, None, Off::none().with(Fault::ServerCrash))
        .expect("the client gets the disk error after reconnecting");
    assert_eq!(report.disk_full_errors, 1);
}

fn disk_full_and_server_restart(seed: u64) {
    let report = run(seed, None).expect("the command finishes with both faults enabled");
    assert!(report.server_restarts > 0, "seed {seed}");
}

rargo_machine::test_batches! {
    disk_full_and_server_restart;
    disk_full_and_server_restart_708: 708,
    disk_full_and_server_restart_813: 813,
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
