//! The report behind `--timings`: when each unit of a build started and
//! finished, what it cost, and which results came from the cache.
//!
//! cargo's report describes cargo's own job queue; this one describes what
//! the server did for this request. It is a single HTML file without
//! scripts, placed where cargo puts its report
//! (`target/cargo-timings/cargo-timing.html` and a dated copy).

use std::time::{Duration, Instant};

/// What happened to one unit during a build.
#[derive(Clone, Default)]
pub struct UnitTime {
    /// Everything the unit depends on was ready and its result was asked for.
    pub start: Option<Instant>,
    /// Its metadata was ready (dependents may start from here).
    pub meta: Option<Instant>,
    pub end: Option<Instant>,
    /// The result came from the cache; `wall_us` and `cpu_us` are then what
    /// producing it once cost.
    pub cached: bool,
    pub wall_us: u64,
    pub cpu_us: u64,
    pub rss_kb: u64,
}

pub struct Row {
    pub name: String,
    pub time: UnitTime,
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn secs(d: Duration) -> String {
    format!("{:.2}s", d.as_secs_f64())
}

/// `20261123T101500Z` for seconds since the epoch.
pub fn stamp(unix: u64) -> String {
    let (days, rem) = (unix / 86400, unix % 86400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Renders the report. `began` is when the build started, `total` how long
/// it took, `cores` how many the server schedules on.
pub fn render(command: &str, when: &str, began: Instant, total: Duration, cores: usize, rows: &[Row]) -> String {
    let ran: Vec<&Row> = rows.iter().filter(|r| !r.time.cached && r.time.start.is_some() && r.time.end.is_some()).collect();
    let cached = rows.iter().filter(|r| r.time.cached).count();
    let cpu: u64 = ran.iter().map(|r| r.time.cpu_us).sum();
    let saved: u64 = rows.iter().filter(|r| r.time.cached).map(|r| r.time.wall_us).sum();
    let since = |t: Instant| t.saturating_duration_since(began);

    let mut h = String::new();
    h.push_str("<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<title>rargo build timings</title>\n<style>\n");
    h.push_str(
        "body{font:14px/1.4 system-ui,sans-serif;margin:24px;color:#1b1b1b;background:#fff}\n\
         h1{font-size:20px}h2{font-size:16px;margin-top:28px}\n\
         table{border-collapse:collapse}td,th{padding:3px 10px;text-align:left;border-bottom:1px solid #ddd}\n\
         td.n,th.n{text-align:right;font-variant-numeric:tabular-nums}\n\
         .chart{overflow-x:auto;border:1px solid #ddd}\n\
         .front{fill:#3b6fb6}.back{fill:#9cc0ee}.kf{background:#3b6fb6}.kb{background:#9cc0ee}text{font:11px system-ui,sans-serif;fill:#1b1b1b}\n\
         .grid{stroke:#ddd}.key span{display:inline-block;width:12px;height:12px;margin:0 4px 0 14px;vertical-align:-1px}\n\
         @media(prefers-color-scheme:dark){body{background:#16181c;color:#e6e6e6}td,th{border-color:#3a3d44}\
         .chart{border-color:#3a3d44}text{fill:#e6e6e6}.grid{stroke:#3a3d44}.front{fill:#4f8fe0}.back{fill:#2f5486}.kf{background:#4f8fe0}.kb{background:#2f5486}}\n",
    );
    h.push_str("</style>\n</head>\n<body>\n<h1>rargo build timings</h1>\n<table>\n");
    let mut fact = |k: &str, v: String| h.push_str(&format!("<tr><th>{}</th><td>{}</td></tr>\n", esc(k), esc(&v)));
    fact("Command", command.to_string());
    fact("Finished", when.to_string());
    fact("Wall time", secs(total));
    fact("Units", format!("{} ({} run for this build, {} from the cache)", rows.len(), ran.len(), cached));
    fact("CPU time of the units run", secs(Duration::from_micros(cpu)));
    fact("Time the cached results took when they were made", secs(Duration::from_micros(saved)));
    fact("Cores the server schedules on", cores.to_string());
    h.push_str("</table>\n");

    // The chart: one bar per unit that ran, in order of start.
    let mut order: Vec<&Row> = ran.clone();
    order.sort_by_key(|r| r.time.start);
    h.push_str("<h2>Units run for this build</h2>\n");
    if order.is_empty() {
        h.push_str("<p>Every result came from the cache.</p>\n");
    } else {
        h.push_str(
            "<p class=\"key\">From asking for a unit to its result, queueing included.\
             <span class=\"kf\"></span>until its metadata was ready\
             <span class=\"kb\"></span>after (code generation; dependents may already be compiling)</p>\n",
        );
        let (row_h, left, width) = (18.0, 8.0, 1100.0);
        let span = total.as_secs_f64().max(0.001);
        let x = |d: Duration| left + d.as_secs_f64() / span * width;
        let height = order.len() as f64 * row_h + 24.0;
        h.push_str(&format!(
            "<div class=\"chart\"><svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{:.0}\" height=\"{height:.0}\" role=\"img\" aria-label=\"When each unit ran\">\n",
            left * 2.0 + width + 320.0
        ));
        // A grid line roughly every tenth of the build, on a round number.
        let step = [0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 30.0, 60.0, 120.0, 300.0, 600.0]
            .into_iter()
            .find(|s| span / s <= 12.0)
            .unwrap_or(1800.0);
        let mut t = 0.0;
        while t <= span {
            let gx = left + t / span * width;
            h.push_str(&format!(
                "<line class=\"grid\" x1=\"{gx:.1}\" y1=\"14\" x2=\"{gx:.1}\" y2=\"{height:.0}\"/><text x=\"{:.1}\" y=\"10\">{}s</text>\n",
                gx + 2.0,
                if step < 1.0 { format!("{t:.1}") } else { format!("{t:.0}") }
            ));
            t += step;
        }
        for (n, r) in order.iter().enumerate() {
            let (s, e) = (since(r.time.start.unwrap()), since(r.time.end.unwrap()));
            let m = r.time.meta.map(since).filter(|m| *m >= s && *m <= e).unwrap_or(e);
            let y = 16.0 + n as f64 * row_h;
            h.push_str(&format!(
                "<rect class=\"front\" x=\"{:.1}\" y=\"{y:.1}\" width=\"{:.1}\" height=\"13\"/>",
                x(s),
                (x(m) - x(s)).max(1.0)
            ));
            if e > m {
                h.push_str(&format!(
                    "<rect class=\"back\" x=\"{:.1}\" y=\"{y:.1}\" width=\"{:.1}\" height=\"13\"/>",
                    x(m),
                    (x(e) - x(m)).max(0.5)
                ));
            }
            h.push_str(&format!(
                "<text x=\"{:.1}\" y=\"{:.1}\">{} {}</text>\n",
                x(e) + 4.0,
                y + 10.5,
                esc(&r.name),
                secs(e.saturating_sub(s))
            ));
        }
        h.push_str("</svg></div>\n");
    }

    // The table: everything, most expensive first.
    let mut all: Vec<&Row> = rows.iter().collect();
    all.sort_by_key(|r| std::cmp::Reverse((!r.time.cached, r.time.wall_us)));
    h.push_str(
        "<h2>All units</h2>\n<table>\n<tr><th>Unit</th><th>Result</th><th class=\"n\">Asked at</th>\
         <th class=\"n\">Answered after</th><th class=\"n\">Metadata after</th><th class=\"n\">Run time</th>\
         <th class=\"n\">CPU time</th><th class=\"n\">Peak memory</th></tr>\n",
    );
    for r in all {
        let t = &r.time;
        let cell = |v: Option<String>| format!("<td class=\"n\">{}</td>", v.unwrap_or_default());
        let (s, e) = (t.start.map(since), t.end.map(since));
        h.push_str(&format!("<tr><td>{}</td><td>{}</td>", esc(&r.name), if t.cached { "from the cache" } else { "run" }));
        h.push_str(&cell(s.map(secs)));
        h.push_str(&cell(s.zip(e).map(|(s, e)| secs(e.saturating_sub(s)))));
        h.push_str(&cell(s.zip(t.meta.map(since)).filter(|_| !t.cached).map(|(s, m)| secs(m.saturating_sub(s)))));
        h.push_str(&cell(Some(secs(Duration::from_micros(t.wall_us)))));
        h.push_str(&cell(Some(secs(Duration::from_micros(t.cpu_us)))));
        h.push_str(&cell((t.rss_kb > 0).then(|| format!("{:.0} MiB", t.rss_kb as f64 / 1024.0))));
        h.push_str("</tr>\n");
    }
    h.push_str("</table>\n</body>\n</html>\n");
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps() {
        assert_eq!(stamp(0), "19700101T000000Z");
        assert_eq!(stamp(951_782_400), "20000229T000000Z");
        assert_eq!(stamp(1_795_428_930), "20261123T101530Z");
    }

    #[test]
    fn report_lists_runs_and_cache_hits() {
        let began = Instant::now();
        let at = |ms: u64| Some(began + Duration::from_millis(ms));
        let rows = vec![
            Row {
                name: "`a` (lib)".into(),
                time: UnitTime { start: at(0), meta: at(400), end: at(1000), cached: false, wall_us: 900_000, cpu_us: 1_500_000, rss_kb: 204_800 },
            },
            Row {
                name: "`b<T>` (bin)".into(),
                time: UnitTime { start: at(400), meta: None, end: at(401), cached: true, wall_us: 2_000_000, cpu_us: 2_500_000, rss_kb: 0 },
            },
        ];
        let html = render("build --timings", "2026-11-23", began, Duration::from_secs(1), 8, &rows);
        assert!(html.contains("2 (1 run for this build, 1 from the cache)"));
        // The bar of `a` has both of its phases; `b` has no bar.
        assert_eq!(html.matches("class=\"front\"").count(), 1);
        assert_eq!(html.matches("class=\"back\"").count(), 1);
        assert!(html.contains("`b&lt;T&gt;` (bin)</td><td>from the cache"));
        assert!(html.contains("`a` (lib) 1.00s"));
        assert!(html.contains("200 MiB"));
        assert!(!html.contains("<script"));
    }
}
