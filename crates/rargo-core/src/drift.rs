//! The drift check: does plain cargo still agree with rargo?
//!
//! rargo is stricter than cargo in most ways (sandbox, clean environment)
//! and looser in none that should matter, but "should" is not evidence. The
//! check clones a commit into a scratch directory and builds it twice: with
//! the stock cargo of the user's toolchain, unsandboxed, and with rargo in
//! verify mode, where every unit that comes from the cache is also re-run
//! from nothing (no cache, no incremental seed) and its outputs compared.
//! Disagreement in either comparison is reported.

use crate::build::{Client, Daemon, handle};
use crate::present::Output;
use crate::proto::ServerMsg;
use crate::sched::Priority;
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

/// Who last used a worktree: the drift check runs as them.
#[derive(Clone)]
pub struct LastClient {
    pub env: Arc<HashMap<String, String>>,
    pub uid: u32,
    pub gid: u32,
    pub at: Instant,
    /// HEAD at the last drift check, to avoid repeating one.
    pub checked_head: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Report {
    pub commit: String,
    pub stock_ok: bool,
    pub rargo_ok: bool,
    pub verified_units: u64,
    pub mismatches: Vec<String>,
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
            "{verdict} at {}: stock cargo {} ({:.1}s), rargo {} ({:.1}s), {} cached unit(s) re-run cleanly, {} differed\n",
            &self.commit[..self.commit.len().min(12)],
            if self.stock_ok { "builds" } else { "fails" },
            self.stock_secs,
            if self.rargo_ok { "builds" } else { "fails" },
            self.rargo_secs,
            self.verified_units,
            self.mismatches.len()
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

fn run_as(cmd: &mut std::process::Command, uid: u32, gid: u32) {
    use std::os::unix::process::CommandExt;
    if uid != unsafe { libc::geteuid() } {
        cmd.uid(uid).gid(gid);
    }
}

/// Runs the check for the worktree at `root` as `who`.
pub async fn check(d: &Arc<Daemon>, root: &Path, who: &LastClient) -> Result<Report> {
    let scratch_parent = d.cfg.state_dir.join("drift");
    std::fs::create_dir_all(&scratch_parent)?;
    let scratch = scratch_parent.join(format!("check-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    let result = check_in(d, root, who, &scratch).await;
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

async fn check_in(d: &Arc<Daemon>, root: &Path, who: &LastClient, scratch: &Path) -> Result<Report> {
    let (root2, scratch2, env, uid, gid) = (root.to_path_buf(), scratch.to_path_buf(), who.env.clone(), who.uid, who.gid);
    // A shared clone: no objects are copied and the user's repository gets
    // no new worktree entry.
    let commit = tokio::task::spawn_blocking(move || -> Result<String> {
        let git = |args: &[&str], cwd: &Path| -> Result<String> {
            let mut c = std::process::Command::new("git");
            c.args(args).current_dir(cwd).env_clear().envs(env.iter()).stdin(std::process::Stdio::null());
            c.env("GIT_CONFIG_COUNT", "1").env("GIT_CONFIG_KEY_0", "safe.directory").env("GIT_CONFIG_VALUE_0", "*");
            run_as(&mut c, uid, gid);
            let out = c.output().context("running git")?;
            if !out.status.success() {
                bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
            }
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        };
        let commit = git(&["rev-parse", "HEAD"], &root2)?;
        if uid != unsafe { libc::geteuid() } {
            use std::os::unix::ffi::OsStrExt;
            if let Some(parent) = scratch2.parent() {
                if let Ok(c) = std::ffi::CString::new(parent.as_os_str().as_bytes()) {
                    // SAFETY: valid C string.
                    unsafe { libc::chown(c.as_ptr(), uid, gid) };
                }
            }
        }
        git(
            &["clone", "-q", "--shared", "--no-checkout", root2.to_str().unwrap_or("."), scratch2.to_str().unwrap_or(".")],
            &root2,
        )?;
        git(&["checkout", "-q", "--detach", &commit], &scratch2)?;
        Ok(commit)
    })
    .await??;

    // 1. Stock cargo, unsandboxed, at low priority.
    let (scratch2, env, uid, gid) = (scratch.to_path_buf(), who.env.clone(), who.uid, who.gid);
    let started = Instant::now();
    let stock = tokio::task::spawn_blocking(move || {
        let mut c = std::process::Command::new("nice");
        c.args(["-n", "19", "cargo", "build", "--offline", "-j", "4"])
            .current_dir(&scratch2)
            .env_clear()
            .envs(env.iter())
            // If `cargo` on PATH is the rargo shim, it must step aside.
            .env("RARGO_BYPASS", "1")
            .env("RUSTC_WRAPPER", "")
            .env("RUSTC_WORKSPACE_WRAPPER", "")
            .env("CARGO_TARGET_DIR", scratch2.join("target-stock"))
            .stdin(std::process::Stdio::null());
        run_as(&mut c, uid, gid);
        c.output()
    })
    .await?
    .context("running stock cargo")?;
    let stock_secs = started.elapsed().as_secs_f64();

    // 2. rargo on the same checkout, verifying every cached unit.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut env: HashMap<String, String> = (*who.env).clone();
    env.insert("RARGO_VERIFY".into(), "1".into());
    env.insert("RARGO_DRIFT_CHECK".into(), "1".into());
    let client = Client {
        args: vec!["build".into()],
        cwd: scratch.to_path_buf(),
        env: Arc::new(env),
        uid: who.uid,
        gid: who.gid,
        prio: Priority::Background,
        out: Output::new(tx, false, false),
    };
    let before = (
        d.stats.verify_units.load(std::sync::atomic::Ordering::Relaxed),
        d.stats.verify_mismatches.load(std::sync::atomic::Ordering::Relaxed),
    );
    let started = Instant::now();
    let code = match handle(d.clone(), client).await {
        Ok(code) => code,
        Err(_) => 0,
    };
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
    let verified_units = d.stats.verify_units.load(std::sync::atomic::Ordering::Relaxed) - before.0;
    let _ = before.1;
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
    Ok(Report { commit, stock_ok, rargo_ok, verified_units, mismatches, stock_secs, rargo_secs, detail })
}

/// Background loop: every `interval`, check each worktree that was built in
/// since its last check and whose HEAD has moved.
pub async fn background(d: Arc<Daemon>, interval: std::time::Duration) {
    loop {
        tokio::time::sleep(interval).await;
        let candidates: Vec<(PathBuf, LastClient)> =
            d.last_clients.lock().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        for (root, who) in candidates {
            // Only when the machine is otherwise idle.
            let (running, waiting, _, _) = d.engine.sched.snapshot();
            if running + waiting > 0 {
                continue;
            }
            let head = std::process::Command::new("git")
                .args(["-C", root.to_str().unwrap_or("."), "rev-parse", "HEAD"])
                .env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "safe.directory")
                .env("GIT_CONFIG_VALUE_0", "*")
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
            if head.is_none() || head == who.checked_head {
                continue;
            }
            let text = match check(&d, &root, &who).await {
                Ok(r) => format!("{}: {}", root.display(), r.render()),
                Err(e) => format!("{}: drift check could not run: {e:#}\n", root.display()),
            };
            eprint!("rargod: drift check: {text}");
            *d.drift_report.lock() = Some(text);
            if let Some(c) = d.last_clients.lock().get_mut(&root) {
                c.checked_head = head;
            }
        }
    }
}
