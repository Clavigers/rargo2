use crate::io::file::PathRing as _;
use crate::build::{Client, Daemon, Home, handle};
use crate::engine::ClientEnv;
use crate::present::Output;
use crate::proto::ServerMsg;
use crate::sched::Priority;
use crate::snapshot::Snapshot;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone)]
pub struct LastClient {
    pub env: Arc<HashMap<String, String>>,
    pub home: Arc<Home>,
    pub root: String,
    pub dir: std::path::PathBuf,
    pub at: Instant,
    pub checked: Option<crate::hash::Hash>,
}

#[derive(Clone, Debug)]
pub struct Report {
    pub snapshot: String,
    pub stock_ok: bool,
    pub rargo_ok: bool,
    pub verified_units: u64,
    pub mismatches: Vec<String>,
    pub unreproducible: usize,
    pub stock_secs: f64,
    pub rargo_secs: f64,
    pub detail: String,
}

impl Report {
    pub fn agrees(&self) -> bool {
        self.stock_ok == self.rargo_ok && self.mismatches.is_empty()
    }

    pub fn render(&self) -> String {
        let verdict = if self.agrees() { "no drift" } else { "DRIFT" };
        let mut s = format!(
            "{verdict} at {}: stock cargo {} ({:.1}s), rargo {} ({:.1}s), {} cached unit(s) re-run cleanly, {} differed, {} not reproducible by the tools themselves\n",
            &self.snapshot[..self.snapshot.len().min(12)],
            if self.stock_ok { "builds" } else { "fails" },
            self.stock_secs,
            if self.rargo_ok { "builds" } else { "fails" },
            self.rargo_secs,
            self.verified_units,
            self.mismatches.len(),
            self.unreproducible
        );
        for m in &self.mismatches {
            s.push_str(&format!("  {m}\n"));
        }
        if !self.agrees() && !self.detail.is_empty() {
            s.push_str(&self.detail);
        }
        s
    }
}

pub fn check(d: &Arc<Daemon>, snapshot: &Arc<Snapshot>, who: &LastClient, time: fn() -> Instant) -> Result<Report> {
    let scratch = d.scratch("drift")?;
    let result = check_in(d, snapshot, who, &scratch, time);
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

fn check_in(d: &Arc<Daemon>, snapshot: &Arc<Snapshot>, who: &LastClient, scratch: &std::path::Path, time: fn() -> Instant) -> Result<Report> {
    let (uid, gid) = (d.cfg.uid, d.cfg.gid);
    let mut env: HashMap<String, String> = d.cfg.server_env.clone();
    for (k, v) in who.env.iter() {
        if rargo_proto::env_allowed(k) && !k.starts_with("RARGO_") && k != "CARGO_TARGET_DIR" && k != "CARGO_BUILD_TARGET_DIR" {
            env.insert(k.clone(), v.clone());
        }
    }
    let policy = crate::policy::snapshot_policy(snapshot, &who.dir).map_err(|e| anyhow::anyhow!(e))?;
    let (dd, snap, dir, files) = (d.clone(), snapshot.clone(), who.dir.clone(), policy.flake.files.clone());
    let flake = crate::flake::resolve(&dd, &snap, &dir, &files, uid, gid, &|| {})
        .context("evaluating the build flake for the drift check")?;
    for k in ["RUSTUP_HOME", "RUSTUP_TOOLCHAIN"] {
        env.remove(k);
    }
    env.retain(|k, _| !k.starts_with("NIX_"));
    env.extend(flake.env.clone());
    let cargo = flake.which("cargo").context("the build flake does not provide Cargo")?;
    let client_env: HashMap<String, String> = (*who.env).clone();
    for (k, v) in policy.passed_env(&client_env) {
        env.entry(k).or_insert(v);
    }
    let (snap, scratch2, home) = (snapshot.clone(), scratch.to_path_buf(), who.home.dir.clone());
    let started = time();
    let stock_cwd = scratch.join(&who.dir);
    let stock = (|| -> Result<std::process::Output> {
        use std::os::unix::process::CommandExt;
        let stock_cwd = crate::io::cpu(|| -> Result<std::path::PathBuf> {
            snap.materialize(&scratch2, uid, gid, &|_, _| true)?;
            let stock_cwd = stock_cwd.ring_canonicalize()?;
            anyhow::ensure!(stock_cwd.starts_with(scratch2.ring_canonicalize()?), "drift check directory is outside the snapshot");
            Ok(stock_cwd)
        })?;
        let mut c = std::process::Command::new("nice");
        c.args(["-n", "19", cargo.as_str(), "build", "--offline", "-j", "4"])
            .current_dir(&stock_cwd)
            .env_clear()
            .envs(env.iter())
            .env("RARGO_BYPASS", "1")
            .env("RUSTC_WRAPPER", "")
            .env("RUSTC_WORKSPACE_WRAPPER", "")
            .env("CARGO_HOME", &home)
            .env("CARGO_TARGET_DIR", scratch2.join("target-stock"))
            .stdin(std::process::Stdio::null());
        if uid != unsafe { libc::geteuid() } {
            c.uid(uid).gid(gid);
        }
        crate::io::output(&mut c).context("running stock cargo")
    })()?;
    let stock_ran = (started, time());

    let run = |args: &[&str], flag: &str| -> (i32, String) {
        let (tx, mut rx) = crate::io::unbounded_channel();
        let mut env: HashMap<String, String> = (*who.env).clone();
        env.insert(flag.into(), "1".into());
        env.remove("CARGO_TARGET_DIR");
        env.remove("CARGO_BUILD_TARGET_DIR");
        let client = Client {
            args: args.iter().map(|a| a.to_string()).collect(),
            root: "/w".into(),
            cwd: std::path::Path::new("/w").join(&who.dir).to_string_lossy().into_owned(),
            cargo_home: "/c".into(),
            env: Arc::new(ClientEnv::new(env, Vec::new(), None)),
            uid,
            gid,
            prio: Priority::Background,
            out: Output::new(tx, false, false),
            session: None,
            snapshot: snapshot.clone(),
            worktree: String::new(),
            commits: Arc::new(Vec::new()),
            home: who.home.clone(),
            outbox: Default::default(),
            targets: Vec::new(),
        };
        let code = handle(d.clone(), client).unwrap_or(0);
        let mut text = String::new();
        while let Some(m) = rx.recv() {
            if let ServerMsg::Stderr(b) = m {
                text.push_str(&String::from_utf8_lossy(&b));
            }
        }
        (code, text)
    };
    let before = d.stats.verify_units.load(std::sync::atomic::Ordering::Relaxed);
    let started = time();
    let (code, mut text) = run(&["build"], "RARGO_VERIFY");
    let rargo_ran = (started, time());
    let verified_units = d.stats.verify_units.load(std::sync::atomic::Ordering::Relaxed) - before;
    if code == 0 {
        let (_, tests) = run(&["test", "--no-fail-fast"], crate::build::VERIFY_TESTS);
        text.extend(tests.lines().filter(|l| l.starts_with("warning: verify: ") || l.starts_with("note: verify: ")).map(|l| format!("{l}\n")));
    }
    Ok(compare(snapshot.id.hex(), (stock.status.success(), &String::from_utf8_lossy(&stock.stderr), stock_ran),
        (code == 0, &text, rargo_ran), verified_units))
}

trait Since: Copy {
    fn secs_since(self, earlier: Self) -> f64;
}

impl Since for Instant {
    fn secs_since(self, earlier: Instant) -> f64 {
        self.saturating_duration_since(earlier).as_secs_f64()
    }
}

type Ran<'a, T> = (bool, &'a str, (T, T));

fn compare<T: Since>(snapshot: String, stock: Ran<T>, rargo: Ran<T>, verified_units: u64) -> Report {
    let (stock_ok, stock_stderr, (stock_start, stock_end)) = stock;
    let (rargo_ok, text, (rargo_start, rargo_end)) = rargo;
    let (stock_secs, rargo_secs) = (stock_end.secs_since(stock_start), rargo_end.secs_since(rargo_start));
    let mismatches: Vec<String> = text
        .lines()
        .filter_map(|l| l.strip_prefix("warning: verify: ").map(|s| s.to_string()))
        .collect();
    let unreproducible = text.lines().filter(|l| l.starts_with("note: verify: not compared: ")).count();
    let mut detail = String::new();
    if stock_ok != rargo_ok {
        let tail = |s: &str| s.lines().rev().take(15).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
        detail = format!(
            "  --- stock cargo (last lines)\n{}\n  --- rargo (last lines)\n{}\n",
            tail(stock_stderr),
            tail(text)
        );
    }
    Report {
        snapshot,
        stock_ok,
        rargo_ok,
        verified_units,
        mismatches,
        unreproducible,
        stock_secs,
        rargo_secs,
        detail,
    }
}

pub fn background(d: Arc<Daemon>, interval: std::time::Duration, time: fn() -> Instant) {
    loop {
        crate::io::sleep(interval);
        for (worktree, snapshot) in d.trees.recent() {
            let (running, waiting, _, _) = d.engine.sched.snapshot();
            if running + waiting > 0 {
                continue;
            }
            let Some(who) = d.last_clients.lock().get(&worktree).cloned() else { continue };
            if who.checked == Some(snapshot.id) {
                continue;
            }
            let text = match check(&d, &snapshot, &who, time) {
                Ok(r) => format!("{}: {}", who.root, r.render()),
                Err(e) => format!("{}: drift check could not run: {e:#}\n", who.root),
            };
            eprint!("rargod: drift check: {text}");
            *d.drift_report.lock() = Some(text);
            if let Some(c) = d.last_clients.lock().get_mut(&worktree).filter(|c| c.at == who.at) {
                c.checked = Some(snapshot.id);
            }
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy)]
    struct Ms(u64);

    impl Since for Ms {
        fn secs_since(self, earlier: Ms) -> f64 {
            self.0.saturating_sub(earlier.0) as f64 / 1000.0
        }
    }

    #[test]
    fn drift_uses_build_results_and_verification_messages() {
        let same = compare("abcdef".into(), (true, "", (Ms(10_000), Ms(11_500))), (true, "note: verify: not compared: bin", (Ms(11_500), Ms(13_500))), 3);
        assert!(same.agrees());
        assert_eq!(same.unreproducible, 1);
        assert_eq!((same.stock_secs, same.rargo_secs), (1.5, 2.0));
        assert!(same.render().contains("stock cargo builds (1.5s), rargo builds (2.0s)"));
        let backwards = compare("abcdef".into(), (true, "", (Ms(5_000), Ms(4_000))), (true, "", (Ms(0), Ms(250))), 0);
        assert_eq!((backwards.stock_secs, backwards.rargo_secs), (0.0, 0.25), "a clock that steps back counts as no time");
        let content = compare("abcdef".into(), (true, "", (Ms(0), Ms(0))), (true, "warning: verify: artifact differs", (Ms(0), Ms(0))), 1);
        assert!(!content.agrees());
        assert_eq!(content.mismatches, ["artifact differs"]);
        let exit = compare("abcdef".into(), (false, "stock failure", (Ms(0), Ms(0))), (true, "rargo success", (Ms(0), Ms(0))), 0);
        assert!(!exit.agrees());
        assert!(exit.detail.contains("stock failure") && exit.detail.contains("rargo success"));
    }
}
