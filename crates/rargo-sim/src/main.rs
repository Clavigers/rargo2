use rargo_sim::{Bug, Fault, Off, run_off};

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut seeds = 0..1;
    let mut bug = None;
    let mut off = Off::none();
    let mut i = 0;
    while i < args.len() {
        let value = args.get(i + 1).ok_or_else(|| format!("{} needs a value", args[i]))?;
        match args[i].as_str() {
            "--seed" => {
                let n: u64 = value.parse().map_err(|_| format!("not a seed: {value}"))?;
                seeds = n..n + 1;
            }
            "--seeds" => {
                let (a, b) = value.split_once("..").ok_or("--seeds takes A..B")?;
                seeds = a.parse().map_err(|_| "not a seed")?..b.parse().map_err(|_| "not a seed")?;
            }
            "--bug" => bug = Some(Bug::parse(value).ok_or_else(|| format!("no such bug: {value}"))?),
            "--off" => off = off.with(Fault::parse(value).ok_or_else(|| format!("no such fault: {value}"))?),
            other => return Err(format!("usage: rargo-sim (--seed N | --seeds A..B) [--bug {}] [--off {}]...; not {other}", names(Bug::ALL.map(Bug::name)), names(Fault::ALL.map(Fault::name)))),
        }
        i += 2;
    }
    let count = seeds.end - seeds.start;
    let mut failures = Vec::new();
    let mut totals = [0u64; 21];
    for seed in seeds {
        match run_off(seed, bug, off.clone()) {
            Ok(r) => {
                let counts = [
                    r.events, r.runs, r.cache_hits, r.cuts, r.evictions,
                    r.lost, r.duplicated, r.reordered, r.worker_crashes, r.jobs_lost_to_crashes,
                    r.tests_passed, r.tests_failed, r.tests_timed_out,
                    r.disk_full_errors, r.disk_crashes, r.torn_writes, r.server_restarts, r.partitioned,
                    r.long_runs, r.failed_builds, r.cancelled_builds,
                ];
                for (t, n) in totals.iter_mut().zip(counts) {
                    *t += n;
                }
            }
            Err(f) => {
                println!("{f}");
                failures.push(f);
            }
        }
    }
    let [events, runs, hits, cuts, evictions, lost, duplicated, reordered, crashes, interrupted, passed, failed, timed_out, disk_full, disk_crashes, torn, server_restarts, partitioned, long, compiler_failed, cancelled] = totals;
    println!(
        "passing seeds: {events} events, {runs} runs, {hits} cache hits, {cuts} cut links, {evictions} evictions, {lost} lost, {duplicated} duplicated and {reordered} reordered packets"
    );
    println!("packets dropped by partitions: {partitioned}");
    println!("server restarts: {server_restarts}");
    println!("worker crashes: {crashes}, interrupted jobs: {interrupted}");
    println!("tests: {passed} passed, {failed} failed, {timed_out} timed out");
    println!("disk faults: {disk_full} commands ended on a full disk, {disk_crashes} crashes, {torn} torn writes");
    println!("model workload: {long} long runs, {compiler_failed} compiler failures, {cancelled} cancelled builds");
    println!("{} of {count} seeds failed", failures.len());
    if failures.is_empty() { Ok(()) } else { Err(format!("{} seeds failed", failures.len())) }
}

fn names<const N: usize>(all: [&str; N]) -> String {
    all.join("|")
}
