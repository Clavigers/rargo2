use rargo_sim::{Bug, run};

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut seeds = 0..1;
    let mut bug = None;
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
            other => return Err(format!("usage: rargo-sim (--seed N | --seeds A..B) [--bug {}]; not {other}", Bug::ALL.map(Bug::name).join("|"))),
        }
        i += 2;
    }
    let count = seeds.end - seeds.start;
    let mut failures = Vec::new();
    let mut totals = [0u64; 5];
    for seed in seeds {
        match run(seed, bug) {
            Ok(r) => {
                for (t, n) in totals.iter_mut().zip([r.events, r.runs, r.cache_hits, r.cuts, r.evictions]) {
                    *t += n;
                }
            }
            Err(f) => {
                println!("{f}");
                failures.push(f);
            }
        }
    }
    let [events, runs, hits, cuts, evictions] = totals;
    println!("passing seeds: {events} events, {runs} runs, {hits} cache hits, {cuts} cut links, {evictions} evictions");
    println!("{} of {count} seeds failed", failures.len());
    if failures.is_empty() { Ok(()) } else { Err(format!("{} seeds failed", failures.len())) }
}
