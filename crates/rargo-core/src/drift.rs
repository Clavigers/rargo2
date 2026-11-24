//! The drift check: does plain cargo still agree with rargo?
//!
//! rargo is stricter than cargo in most ways (sandbox, clean environment)
//! and looser in none that should matter, but "should" is not evidence. The
//! check takes a worktree state a client synced and builds it twice on the
//! server: with the stock cargo of the toolchain, unsandboxed, in a scratch
//! copy; and with rargo in verify mode, where every unit that comes from
//! the cache is also re-run from nothing (no cache, no incremental seed) and
//! its outputs compared. Disagreement in either comparison is reported.
//!
//! The other half of "no drift" is on the client: what rargo leaves in a
//! worktree must not stop stock cargo from working there. `dev/nodrift.sh`
//! checks that.

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

/// Who last used a worktree: the drift check builds with their settings.
#[derive(Clone)]
pub struct LastClient {
    pub env: Arc<HashMap<String, String>>,
    pub home: Arc<Home>,
    /// The worktree root on the client, for reports.
    pub root: String,
    /// Cargo invocation directory relative to the snapshot root.
    pub dir: std::path::PathBuf,
    pub at: Instant,
    /// The snapshot the last drift check looked at.
    pub checked: Option<crate::hash::Hash>,
}

#[derive(Clone, Debug)]
pub struct Report {
    pub snapshot: String,
    pub stock_ok: bool,
    pub rargo_ok: bool,
    pub verified_units: u64,
    pub mismatches: Vec<String>,
    /// Units that could not be compared because the tools do not
    /// reproduce their output from run to run.
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

/// Runs the check for `snapshot` with the settings of `who`.
pub async fn check(d: &Arc<Daemon>, snapshot: &Arc<Snapshot>, who: &LastClient) -> Result<Report> {
    let scratch = d.scratch("drift")?;
    let result = check_in(d, snapshot, who, &scratch).await;
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

async fn check_in(d: &Arc<Daemon>, snapshot: &Arc<Snapshot>, who: &LastClient, scratch: &std::path::Path) -> Result<Report> {
    let (uid, gid) = (d.cfg.uid, d.cfg.gid);
    // 1. Stock cargo, unsandboxed, at low priority, in a real directory.
    let mut env: HashMap<String, String> = d.cfg.server_env.clone();
    for (k, v) in who.env.iter() {
        if rargo_proto::env_allowed(k) && !k.starts_with("RARGO_") && k != "CARGO_TARGET_DIR" && k != "CARGO_BUILD_TARGET_DIR" {
            env.insert(k.clone(), v.clone());
        }
    }
    // A repository whose environment is a flake is built by stock cargo in
    // that environment too (the devShell's tools and variables), as its
    // users would: `nix develop -c cargo build`. Unsandboxed, so the store
    // paths are simply the host's.
    let policy = crate::policy::snapshot_policy(snapshot, &who.dir).map_err(|e| anyhow::anyhow!(e))?;
    let has_flake = matches!(snapshot.node(b"flake.nix"), Some(rargo_proto::tree::Node::File { .. }));
    let mut cargo = "cargo".to_string();
    if has_flake && policy.flake.enabled != Some(false) {
        let (dd, snap, files) = (d.clone(), snapshot.clone(), policy.flake.files.clone());
        let flake = tokio::task::spawn_blocking(move || crate::flake::resolve(&dd, &snap, &files, uid, gid))
            .await?
            .context("evaluating the repository's flake for the drift check")?;
        for k in ["RUSTUP_HOME", "RUSTUP_TOOLCHAIN"] {
            env.remove(k);
        }
        env.retain(|k, _| !k.starts_with("NIX_"));
        for (k, v) in &flake.env {
            env.insert(k.clone(), v.clone());
        }
        if let Some(found) = flake.which("cargo") {
            cargo = found;
        }
    }
    let client_env: HashMap<String, String> = (*who.env).clone();
    for (k, v) in policy.passed_env(&client_env) {
        env.entry(k).or_insert(v);
    }
    let (snap, scratch2, home) = (snapshot.clone(), scratch.to_path_buf(), who.home.dir.clone());
    let started = Instant::now();
    let stock_cwd = scratch.join(&who.dir);
    let stock = tokio::task::spawn_blocking(move || -> Result<std::process::Output> {
        use std::os::unix::process::CommandExt;
        snap.materialize(&scratch2, uid, gid, &|_, _| true)?;
        let stock_cwd = stock_cwd.canonicalize()?;
        anyhow::ensure!(stock_cwd.starts_with(scratch2.canonicalize()?), "drift check directory is outside the snapshot");
        let mut c = std::process::Command::new("nice");
        c.args(["-n", "19", cargo.as_str(), "build", "--offline", "-j", "4"])
            .current_dir(&stock_cwd)
            .env_clear()
            .envs(env.iter())
            // If `cargo` on PATH is the rargo shim, it must step aside.
            .env("RARGO_BYPASS", "1")
            .env("RUSTC_WRAPPER", "")
            .env("RUSTC_WORKSPACE_WRAPPER", "")
            .env("CARGO_HOME", &home)
            .env("CARGO_TARGET_DIR", scratch2.join("target-stock"))
            .stdin(std::process::Stdio::null());
        if uid != unsafe { libc::geteuid() } {
            c.uid(uid).gid(gid);
        }
        c.output().context("running stock cargo")
    })
    .await??;
    let stock_secs = started.elapsed().as_secs_f64();

    // 2. rargo on the same snapshot, verifying every cached unit.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut env: HashMap<String, String> = (*who.env).clone();
    env.insert("RARGO_VERIFY".into(), "1".into());
    env.remove("CARGO_TARGET_DIR");
    env.remove("CARGO_BUILD_TARGET_DIR");
    let client = Client {
        args: vec!["build".into()],
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
    };
    let before = d.stats.verify_units.load(std::sync::atomic::Ordering::Relaxed);
    let started = Instant::now();
    let code = handle(d.clone(), client).await.unwrap_or(0);
    let rargo_secs = started.elapsed().as_secs_f64();
    let mut text = String::new();
    while let Ok(m) = rx.try_recv() {
        if let ServerMsg::Stderr(b) = m {
            text.push_str(&String::from_utf8_lossy(&b));
        }
    }
    let mismatches: Vec<String> = text
        .lines()
        .filter_map(|l| l.strip_prefix("warning: verify: ").map(|s| s.to_string()))
        .collect();
    let unreproducible = text.lines().filter(|l| l.starts_with("note: verify: not compared: ")).count();
    let verified_units = d.stats.verify_units.load(std::sync::atomic::Ordering::Relaxed) - before;
    let stock_ok = stock.status.success();
    let rargo_ok = code == 0;
    let mut detail = String::new();
    if stock_ok != rargo_ok {
        let tail = |s: &str| s.lines().rev().take(15).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
        detail = format!(
            "  --- stock cargo (last lines)\n{}\n  --- rargo (last lines)\n{}\n",
            tail(&String::from_utf8_lossy(&stock.stderr)),
            tail(&text)
        );
    }
    Ok(Report {
        snapshot: snapshot.id.hex(),
        stock_ok,
        rargo_ok,
        verified_units,
        mismatches,
        unreproducible,
        stock_secs,
        rargo_secs,
        detail,
    })
}

/// Background loop: every `interval`, check each worktree whose newest
/// synced state has not been checked yet.
pub async fn background(d: Arc<Daemon>, interval: std::time::Duration) {
    loop {
        tokio::time::sleep(interval).await;
        for (worktree, snapshot) in d.trees.recent() {
            // Only when the machine is otherwise idle.
            let (running, waiting, _, _) = d.engine.sched.snapshot();
            if running + waiting > 0 {
                continue;
            }
            let Some(who) = d.last_clients.lock().get(&worktree).cloned() else { continue };
            if who.checked == Some(snapshot.id) {
                continue;
            }
            let text = match check(&d, &snapshot, &who).await {
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
