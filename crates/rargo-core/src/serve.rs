//! Serving one client connection from handshake to exit code.

use crate::build::{self, Client, Daemon};
use crate::engine::{ClientEnv, EnvAsker};
use crate::hash::Hash;
use crate::present::Output;
use crate::proto::{ClientMsg, PROTO_VERSION, RunRequest, ServerMsg};
use crate::sched::Priority;
use crate::session::{self, Keys, Session};
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::net::TcpStream;

fn wants_color(req: &RunRequest, env: &HashMap<String, String>, tty: bool) -> bool {
    let mut choice: Option<&str> = None;
    let mut it = req.args.iter().take_while(|a| *a != "--");
    while let Some(a) = it.next() {
        if a == "--color" {
            choice = it.next().map(|s| s.as_str());
        } else if let Some(v) = a.strip_prefix("--color=") {
            choice = Some(v);
        }
    }
    let choice = choice.or(env.get("CARGO_TERM_COLOR").map(|s| s.as_str()));
    match choice {
        Some("always") => true,
        Some("never") => false,
        _ => tty && !env.contains_key("NO_COLOR") && env.get("TERM").map(|t| t != "dumb").unwrap_or(false),
    }
}

/// Handles one accepted connection.
pub async fn connection(d: Arc<Daemon>, mut stream: TcpStream, keys: Arc<Keys>) -> Result<()> {
    let _ = stream.set_nodelay(true);
    let (user, admin, first, sealer, mut opener) =
        tokio::time::timeout(std::time::Duration::from_secs(20), session::accept(&mut stream, &keys))
            .await
            .context("handshake timed out")??;
    let (mut rd, wr) = stream.into_split();
    let first = match first {
        Some(m) => m,
        None => session::read_msg(&mut rd, &mut opener).await?.context("the client sent no request")?,
    };
    let ClientMsg::Run(req) = first else { bail!("the client's first message is not a request") };
    let (sess, rx) = Session::new(d.engine.store.clone());
    let writer = tokio::spawn(session::write_loop(rx, sealer, tokio::io::BufWriter::with_capacity(256 << 10, wr), sess.clone()));
    let d2 = d.clone();
    let work = tokio::spawn(serve(d, *req, user, admin, sess.clone()));
    // Everything the client sends while the request runs; if it goes away,
    // stop: queued actions are dropped.
    let abort = work.abort_handle();
    let s2 = sess.clone();
    let reader = tokio::spawn(async move {
        loop {
            match session::read_msg(&mut rd, &mut opener).await {
                Ok(Some(m)) => {
                    if let Err(e) = s2.incoming(m) {
                        eprintln!("rargod: {e:#}");
                        s2.send(ServerMsg::Stderr(format!("error: {e:#}\n").into_bytes()));
                        break;
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }
        s2.close();
        abort.abort();
    });
    let msg = match work.await {
        Ok(m) => m,
        Err(e) if e.is_cancelled() => ServerMsg::Exit(130),
        Err(e) => {
            sess.send(ServerMsg::Stderr(format!("error: rargod internal error: {e}\n").into_bytes()));
            ServerMsg::Exit(101)
        }
    };
    sess.send(msg);
    d2.stats.bytes_in.fetch_add(sess.bytes_in.load(Ordering::Relaxed), Ordering::Relaxed);
    d2.stats.bytes_out.fetch_add(sess.bytes_out.load(Ordering::Relaxed), Ordering::Relaxed);
    let _ = writer.await;
    reader.abort();
    sess.close();
    Ok(())
}

fn fail(sess: &Session, text: String) -> ServerMsg {
    sess.send(ServerMsg::Stderr(format!("error: {text}\n").into_bytes()));
    ServerMsg::Exit(101)
}

fn say(sess: &Session, text: String) -> ServerMsg {
    sess.send(ServerMsg::Stdout(text.into_bytes()));
    ServerMsg::Exit(0)
}

async fn serve(d: Arc<Daemon>, req: RunRequest, user: String, admin: bool, sess: Arc<Session>) -> ServerMsg {
    if req.version != PROTO_VERSION {
        return fail(
            &sess,
            format!("rargo client (protocol {}) does not match rargod (protocol {PROTO_VERSION})", req.version),
        );
    }
    let command = req.args.first().map(|s| s.as_str()).unwrap_or("");
    if matches!(command, "__gc" | "__compact" | "__export" | "__import") && !admin {
        return fail(&sess, format!("user `{user}` may not administer this server (mark the user `admin` in the key file)"));
    }
    match command {
        "__status" => return say(&sess, status_text(&d)),
        "__incr" => {
            let d2 = d.clone();
            return say(&sess, tokio::task::spawn_blocking(move || d2.incr.analyze()).await.unwrap_or_default());
        }
        "__gc" => {
            // Explicit collection: everything unreferenced, regardless of
            // age, unless something is running or queued, whose recently
            // used blobs may be outputs not yet recorded anywhere.
            let (running, waiting, _, _) = d.engine.sched.snapshot();
            let age = if running + waiting == 0 { 0 } else { 600 };
            let d2 = d.clone();
            let (n, bytes) = tokio::task::spawn_blocking(move || d2.collect_garbage(age)).await.unwrap_or((0, 0));
            d.engine.store.db().flush();
            return say(
                &sess,
                format!(
                    "collected {n} unreferenced blobs, {:.1} MiB; store is now {:.1} MiB\n",
                    bytes as f64 / (1 << 20) as f64,
                    d.engine.store.total_bytes() as f64 / (1 << 20) as f64
                ),
            );
        }
        "__compact" => {
            let d2 = d.clone();
            let (n, freed) = tokio::task::spawn_blocking(move || {
                let fs = d2.engine.fs.clone();
                d2.engine.store.freeze_idle(0, &|h| fs.forget_backing(h))
            })
            .await
            .unwrap_or((0, 0));
            d.engine.store.db().flush();
            let (cn, logical, disk) = d.engine.store.cold_stats();
            let mib = |b: u64| b as f64 / (1 << 20) as f64;
            return say(
                &sess,
                format!(
                    "{n} blobs went cold, {:.1} MiB freed; cold tier holds {cn} blobs of {:.1} MiB as {:.1} MiB of chunks\n",
                    mib(freed),
                    mib(logical),
                    mib(disk)
                ),
            );
        }
        "__export" | "__import" => {
            // Cache transfer between servers; the directory is on the
            // server. Administrators only: an import adds results every
            // later build will trust.
            let Some(dir) = req.args.get(1).cloned() else { return fail(&sess, "a directory is required".into()) };
            let export = command == "__export";
            let d2 = d.clone();
            let reply = tokio::task::spawn_blocking(move || {
                let dir = PathBuf::from(dir);
                if export { d2.export_cache(&dir) } else { d2.import_cache(&dir) }
            })
            .await;
            return match reply {
                Ok(Ok((traces, blobs))) => say(
                    &sess,
                    format!("{} {traces} traces and {blobs} blobs\n", if export { "exported" } else { "imported" }),
                ),
                Ok(Err(e)) => fail(&sess, format!("{e:#}")),
                Err(e) => fail(&sess, format!("{e}")),
            };
        }
        _ => {}
    }

    // The worktree: bring whatever the store lacks over from the client.
    let snapshot = match d.trees.sync(Hash(req.tree), &*sess).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("rargod: {}: syncing {}: {e:#}", req.who, req.root);
            return fail(&sess, format!("syncing the worktree: {e:#}"));
        }
    };
    let home = match d.home(&req.home_files) {
        Ok(h) => h,
        Err(e) => return fail(&sess, format!("setting up the cargo configuration: {e:#}")),
    };
    let env: HashMap<String, String> = req.env.iter().cloned().collect();
    let prio = match env.get("RARGO_PRIORITY").map(|s| s.as_str()) {
        Some("human") => Priority::Human,
        Some("agent") => Priority::Agent,
        Some("background") => Priority::Background,
        _ if req.stderr_tty => Priority::Human,
        _ => Priority::Agent,
    };
    let out = Output::new(sess.sender(), wants_color(&req, &env, req.stderr_tty), wants_color(&req, &env, req.stdout_tty));
    let asker: EnvAsker = {
        let s = sess.clone();
        Arc::new(move |names| {
            let s = s.clone();
            Box::pin(async move { s.want_env(names).await })
        })
    };
    if command == "__drift" {
        let who = crate::drift::LastClient {
            env: Arc::new(env),
            home,
            root: req.root.clone(),
            at: std::time::Instant::now(),
            checked: None,
        };
        return match crate::drift::check(&d, &snapshot, &who).await {
            Ok(r) => {
                let text = r.render();
                *d.drift_report.lock() = Some(format!("{}: {text}", req.root));
                sess.send(ServerMsg::Stdout(text.into_bytes()));
                ServerMsg::Exit(if r.agrees() { 0 } else { 1 })
            }
            Err(e) => fail(&sess, format!("drift check could not run: {e:#}")),
        };
    }
    let client = Client {
        args: req.args,
        root: req.root,
        cwd: req.cwd,
        cargo_home: req.cargo_home,
        env: Arc::new(ClientEnv::new(env, req.env_checked, Some(asker))),
        uid: d.cfg.uid,
        gid: d.cfg.gid,
        prio,
        out,
        session: Some(sess.clone()),
        snapshot,
        worktree: req.worktree,
        commits: Arc::new(req.commits),
        home,
        outbox: Default::default(),
    };
    match build::handle(d, client).await {
        Ok(code) => ServerMsg::Exit(code),
        Err(exec) => ServerMsg::Exec(exec),
    }
}

pub fn status_text(d: &Daemon) -> String {
    let e = &d.engine;
    let (running, waiting, peak, granted) = e.sched.snapshot();
    let t = &e.traces.stats;
    let s = &e.stats;
    let f = e.fs.stats();
    let y = &d.trees.stats;
    format!(
        "requests            {}\n\
         sync                {} worktree states, {} manifests and {} files asked of clients ({} awaited from another client's upload), {:.1} MiB received, {:.1} MiB of results sent\n\
         store               {} blobs, {:.1} MiB\n\
         cold tier           {}\n\
         cache               {} hits, {} misses, {} traces stored\n\
         actions run         {} ({:.1}s cpu, {:.1}s wall)\n\
         in-flight dedup     {} joined, {} of those ran anyway\n\
         reruns              {} changed-input, {} environment\n\
         scheduler           {} running, {} waiting, peak {}, {} granted, {} tokens\n\
         fuse                {} lookups, {} opens, {} userspace reads, {} userspace writes\n\
         plans decoded       {}\n\
         incremental         {} states saved; seeds: {} same worktree, {} ancestor, {} other, {} none; {} dropped after a crash\n\
         verify              {} cached units re-run cleanly, {} differed\n\
         drift               {}\n\
         workers             {}\n",
        d.stats.requests.load(Ordering::Relaxed),
        y.snapshots.load(Ordering::Relaxed),
        y.trees_received.load(Ordering::Relaxed),
        y.blobs_requested.load(Ordering::Relaxed),
        y.shared_uploads.load(Ordering::Relaxed),
        d.stats.bytes_in.load(Ordering::Relaxed) as f64 / (1 << 20) as f64,
        d.stats.bytes_out.load(Ordering::Relaxed) as f64 / (1 << 20) as f64,
        e.store.blob_count(),
        e.store.total_bytes() as f64 / (1 << 20) as f64,
        {
            let (n, logical, disk) = e.store.cold_stats();
            format!(
                "{n} blobs of {:.1} MiB kept as {:.1} MiB of chunks",
                logical as f64 / (1 << 20) as f64,
                disk as f64 / (1 << 20) as f64
            )
        },
        t.hits.load(Ordering::Relaxed),
        t.misses.load(Ordering::Relaxed),
        t.stored.load(Ordering::Relaxed),
        s.runs.load(Ordering::Relaxed),
        s.run_cpu_us.load(Ordering::Relaxed) as f64 / 1e6,
        s.run_wall_us.load(Ordering::Relaxed) as f64 / 1e6,
        s.joined.load(Ordering::Relaxed),
        s.joined_then_ran.load(Ordering::Relaxed),
        s.tainted.load(Ordering::Relaxed),
        s.env_reruns.load(Ordering::Relaxed),
        running,
        waiting,
        peak,
        granted,
        e.sched.tokens(),
        f.lookups.load(Ordering::Relaxed),
        f.opens.load(Ordering::Relaxed),
        f.reads.load(Ordering::Relaxed),
        f.writes.load(Ordering::Relaxed),
        d.stats.plans_decoded.load(Ordering::Relaxed),
        d.incr.stats.saved.load(Ordering::Relaxed),
        d.incr.stats.seeded_same_worktree.load(Ordering::Relaxed),
        d.incr.stats.seeded_ancestor.load(Ordering::Relaxed),
        d.incr.stats.seeded_other.load(Ordering::Relaxed),
        d.incr.stats.unseeded.load(Ordering::Relaxed),
        d.incr.stats.discarded_after_crash.load(Ordering::Relaxed),
        d.stats.verify_units.load(Ordering::Relaxed),
        d.stats.verify_mismatches.load(Ordering::Relaxed),
        d.drift_report.lock().clone().unwrap_or_else(|| "not checked yet".into()).trim_end(),
        match d.pool.get() {
            None => "not accepting workers".to_string(),
            Some(p) => {
                let (pushed, skipped, fetched, reused) = p.transfer_totals();
                let ps = &p.stats;
                format!(
                    "{} connected; {} actions dispatched ({} of them also started here on an idle core: {} finished here first, {} were stopped because the worker was ahead), {} kept local (core free), {} kept local (a local core came free first), {} kept local (transfer too costly), {} kept local (incremental state here), {} worker failures; overhead per remote action {}; \
                     sent ahead {:.1} MiB, not sent because already there {:.1} MiB; outputs fetched {:.1} MiB, reused locally {:.1} MiB; \
                     {} answers sent ahead, {} lookups and {} opens asked",
                    p.worker_count(),
                    ps.dispatched.load(Ordering::Relaxed),
                    ps.hedged.load(Ordering::Relaxed),
                    ps.hedge_won.load(Ordering::Relaxed),
                    ps.hedge_stopped.load(Ordering::Relaxed),
                    ps.kept_local_no_wait.load(Ordering::Relaxed),
                    ps.kept_local_first.load(Ordering::Relaxed),
                    ps.kept_local_transfer.load(Ordering::Relaxed),
                    ps.kept_local_incremental.load(Ordering::Relaxed),
                    ps.worker_failures.load(Ordering::Relaxed),
                    p.overhead_text(),
                    pushed as f64 / 1048576.0,
                    skipped as f64 / 1048576.0,
                    fetched as f64 / 1048576.0,
                    reused as f64 / 1048576.0,
                    ps.prefilled_entries.load(Ordering::Relaxed),
                    ps.lookups_served.load(Ordering::Relaxed),
                    ps.opens_served.load(Ordering::Relaxed),
                )
            }
        },
    )
}
