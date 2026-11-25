use crate::{Bug, Kind, run, search};

fn clean(seeds: std::ops::Range<u64>) {
    let failures = search(seeds, None);
    let text: Vec<String> = failures.iter().map(|f| f.to_string()).collect();
    assert!(failures.is_empty(), "{}", text.join("\n"));
}

#[test]
fn seeds_0_to_300_keep_every_invariant() {
    clean(0..300);
}

#[test]
fn seeds_300_to_600_keep_every_invariant() {
    clean(300..600);
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
fn the_seeds_exercise_faults_the_cache_and_eviction() {
    let reports: Vec<_> = (0..100).map(|s| run(s, None).expect("passes")).collect();
    assert!(reports.iter().any(|r| r.cuts > 0), "links are cut");
    assert!(reports.iter().any(|r| r.cache_hits > 0), "actions are answered from the cache");
    assert!(reports.iter().any(|r| r.evictions > 0), "content is evicted");
    assert!(reports.iter().any(|r| r.uploads > 0), "clients upload sources");
    assert!(reports.iter().any(|r| r.wants > 0), "the server asks for content a client thought it held");
    assert!(reports.iter().any(|r| r.client_cuts > 0), "client links are cut");
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
