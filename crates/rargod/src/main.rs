//! rargod: the build server. One per machine.
//!
//!   rargod --state DIR --planner PATH [--store DIR] [--socket PATH] [--jobs N] [--mem-gb N]
//!
//! It must run as root (or with CAP_SYS_ADMIN): it mounts a FUSE filesystem,
//! registers passthrough backing files and creates namespaces.

use anyhow::{Context, Result, bail};
use rargo_core::build::{self, Client, Config, Daemon};
use rargo_core::engine::Engine;
use rargo_core::present::Output;
use rargo_core::proto::{PROTO_VERSION, RunRequest, ServerMsg};
use rargo_core::sandbox::Zygote;
use rargo_core::sandboxfs::SandboxFs;
use rargo_core::sched::{Priority, Scheduler};
use rargo_core::snapshot::Watcher;
use rargo_core::store::Store;
use rargo_core::view::Content;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

struct Opts {
    state: PathBuf,
    store: Option<PathBuf>,
    planner: PathBuf,
    socket: Option<PathBuf>,
    jobs: usize,
    mem_kb: u64,
    passthrough: bool,
    /// Size the content store is kept under.
    store_budget: u64,
    /// Whole blobs unused for this long are turned into chunks; 0 = never.
    cold_after_secs: u64,
    /// Minutes between background drift checks; 0 turns them off.
    drift_minutes: u64,
    /// Address to accept workers on.
    workers_listen: Option<std::net::SocketAddr>,
    /// Run as a worker for the server at this address.
    worker_of: Option<std::net::SocketAddr>,
    /// File holding the secret shared by the server and its workers.
    worker_key: Option<PathBuf>,
}

fn parse_args(args: &[String]) -> Result<Opts> {
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8);
    let total_kb = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines().find(|l| l.starts_with("MemTotal:")).and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
        })
        .unwrap_or(16 << 20);
    let mut o = Opts {
        state: PathBuf::new(),
        store: None,
        planner: PathBuf::new(),
        socket: None,
        jobs: cpus,
        mem_kb: total_kb / 10 * 7,
        passthrough: true,
        store_budget: 100 << 30,
        cold_after_secs: 30 * 60,
        drift_minutes: 360,
        workers_listen: None,
        worker_of: None,
        worker_key: None,
    };
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().with_context(|| format!("{a} needs a value"));
        match a.as_str() {
            "--state" => o.state = val()?.into(),
            "--store" => o.store = Some(val()?.into()),
            "--planner" => o.planner = val()?.into(),
            "--socket" => o.socket = Some(val()?.into()),
            "--jobs" => o.jobs = val()?.parse()?,
            "--mem-gb" => o.mem_kb = val()?.parse::<u64>()? << 20,
            "--no-passthrough" => o.passthrough = false,
            "--drift-interval-min" => o.drift_minutes = val()?.parse()?,
            "--workers-listen" => o.workers_listen = Some(val()?.parse()?),
            "--worker" => o.worker_of = Some(val()?.parse()?),
            "--worker-key" => o.worker_key = Some(PathBuf::from(val()?)),
            "--store-budget-gb" => o.store_budget = val()?.parse::<u64>()? << 30,
            "--store-budget-mb" => o.store_budget = val()?.parse::<u64>()? << 20,
            "--cold-after-min" => o.cold_after_secs = val()?.parse::<u64>()? * 60,
            "--cold-after-secs" => o.cold_after_secs = val()?.parse::<u64>()?,
            other => bail!("unknown option `{other}`"),
        }
    }
    if o.state.as_os_str().is_empty() || (o.planner.as_os_str().is_empty() && o.worker_of.is_none()) {
        bail!(
            "usage: rargod --state DIR --planner PATH [--store DIR] [--socket PATH] [--jobs N] [--mem-gb N] [--workers-listen ADDR --worker-key FILE]\n       rargod --state DIR --worker SERVER_ADDR --worker-key FILE [--jobs N]"
        );
    }
    Ok(o)
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(|s| s.as_str()) == Some("--internal-zygote") {
        rargo_core::sandbox::zygote_main(args[2].parse()?);
    }
    let opts = parse_args(&args)?;
    // Before anything is forked: arrange our cgroup so that each action can
    // have its own (a no-op unless the cgroup was delegated to us).
    let cgroup_root = if std::env::var_os("RARGO_NO_CGROUPS").is_some() {
        None
    } else {
        rargo_core::cgroup::setup()
    };
    // The zygote must exist before this process grows threads.
    let zygote = Zygote::start(&std::env::current_exe()?)?;

    std::fs::create_dir_all(&opts.state)?;
    let state = opts.state.canonicalize()?;
    let planner = match opts.worker_of {
        Some(_) => PathBuf::new(),
        None => opts.planner.canonicalize().with_context(|| format!("planner {}", opts.planner.display()))?,
    };
    let db = Arc::new(rargo_core::db::Db::open(&state.join("db.redb"))?);
    let default_store = state.join("store");
    let store_path = opts.store.as_deref().unwrap_or(&default_store);
    let store = Arc::new(Store::open(store_path, db).with_context(|| format!("opening store {}", store_path.display()))?);
    let content = Arc::new(Content::new(store.clone()));
    let fs = Arc::new(SandboxFs::new(content.clone()));
    let mnt = state.join("mnt");
    std::fs::create_dir_all(&mnt)?;
    // A previous instance may have died without unmounting.
    if let Ok(c) = std::ffi::CString::new(mnt.as_os_str().as_encoded_bytes()) {
        // SAFETY: valid C string; failure (nothing mounted) is expected.
        unsafe { libc::umount2(c.as_ptr(), libc::MNT_DETACH) };
    }
    let session = Arc::new(
        fusewire::Session::mount(
            &mnt,
            &fusewire::MountOptions { fsname: "rargo".into(), passthrough: opts.passthrough, ..Default::default() },
        )
        .context("mounting the sandbox filesystem (is rargod running as root?)")?,
    );
    fs.attach(session.clone());
    let fuse_threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8);
    session.spawn_workers(fs.clone(), fuse_threads)?;

    let sched = Scheduler::new(opts.jobs, opts.mem_kb)?;
    let mut engine = Engine::new(store.clone(), content, fs, mnt, zygote, sched);
    engine.cgroup_root = cgroup_root.clone();
    let engine = Arc::new(engine);
    if let Some(server) = opts.worker_of {
        // A worker: no analysis, no cache of results, no clients. It
        // executes what the server sends and keeps a content cache.
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
        let key = link_key(&opts, server)?;
        let result = rt.block_on(rargo_core::remote::worker_main(engine, server, opts.jobs as u32, key));
        session.unmount();
        return result;
    }
    let watcher = Watcher::start(&state.join("cookies"))?;
    let daemon = Arc::new(Daemon::new(Config { state_dir: state.clone(), planner }, engine, watcher));

    let socket = opts.socket.clone().unwrap_or_else(|| state.join("rargod.sock"));
    let _ = std::fs::remove_file(&socket);
    eprintln!(
        "rargod: store {} ({}), passthrough {}, per-action cgroups {}, {} jobs, {} GiB memory budget, socket {}",
        store.root().display(),
        if store.supports_reflink() { "reflinks" } else { "hard links" },
        session.caps().passthrough,
        cgroup_root.is_some(),
        opts.jobs,
        opts.mem_kb >> 20,
        socket.display()
    );

    // Eviction replaces `cargo clean`: least recently used blobs go first,
    // nothing used in the last ten minutes is touched, and a trace whose
    // outputs are gone is simply a miss.
    {
        let d = daemon.clone();
        let budget = opts.store_budget;
        let cold_after = opts.cold_after_secs;
        std::thread::Builder::new().name("evict".into()).spawn(move || {
            let mut tick = 0u64;
            loop {
                std::thread::sleep(std::time::Duration::from_secs(20));
                let store = &d.engine.store;
                // Unreferenced blobs go first, on a slower beat.
                if tick % 6 == 5 {
                    let (n, bytes) = d.collect_garbage(600);
                    if n > 0 {
                        eprintln!("rargod: collected {n} unreferenced blobs, {} MiB", bytes >> 20);
                    }
                }
                // What nobody has used for a while is kept as chunks.
                if cold_after > 0 && (tick % 6 == 2 || cold_after < 120) {
                    let fs = d.engine.fs.clone();
                    let (n, freed) = store.freeze_idle(cold_after, &|h| fs.forget_backing(h));
                    if n > 0 {
                        eprintln!("rargod: {n} blobs went cold, {} MiB freed", freed >> 20);
                    }
                }
                tick += 1;
                if store.total_bytes() > budget {
                    let fs = d.engine.fs.clone();
                    let freed = store.evict(budget / 10 * 9, 600, &|_| false, &|h| fs.forget_backing(h));
                    if freed > 0 {
                        eprintln!("rargod: evicted {} MiB (store now {} MiB)", freed >> 20, store.total_bytes() >> 20);
                    }
                }
            }
        })?;
    }

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    if let Some(addr) = opts.workers_listen {
        let key = link_key(&opts, addr)?;
        let pool = rt.block_on(rargo_core::remote::Pool::listen(&daemon.engine, addr, key))?;
        let _ = daemon.engine.remote.set(pool.clone());
        let _ = daemon.pool.set(pool);
        eprintln!("rargod: accepting workers on {addr}");
    }
    if opts.drift_minutes > 0 {
        let d = daemon.clone();
        let every = std::time::Duration::from_secs(opts.drift_minutes * 60);
        rt.spawn(rargo_core::drift::background(d, every));
    }
    let result = rt.block_on(serve(daemon, &socket));
    session.unmount();
    let _ = std::fs::remove_file(&socket);
    result
}

async fn serve(daemon: Arc<Daemon>, socket: &Path) -> Result<()> {
    let listener = UnixListener::bind(socket).with_context(|| format!("binding {}", socket.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o666))?;
    }
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    loop {
        tokio::select! {
            _ = term.recv() => break,
            _ = int.recv() => break,
            conn = listener.accept() => {
                let (stream, _) = conn?;
                let d = daemon.clone();
                tokio::spawn(async move {
                    if let Err(e) = connection(d, stream).await {
                        eprintln!("rargod: connection error: {e:#}");
                    }
                });
            }
        }
    }
    eprintln!("rargod: shutting down");
    daemon.engine.store.db().flush();
    Ok(())
}

async fn read_frame<T: for<'a> serde::Deserialize<'a>>(s: &mut (impl AsyncReadExt + Unpin)) -> Result<Option<T>> {
    let mut len = [0u8; 4];
    match s.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let n = u32::from_le_bytes(len) as usize;
    if n > 64 << 20 {
        bail!("oversized request");
    }
    let mut buf = vec![0u8; n];
    s.read_exact(&mut buf).await?;
    Ok(Some(bincode::deserialize(&buf)?))
}

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

async fn connection(daemon: Arc<Daemon>, stream: UnixStream) -> Result<()> {
    let cred = stream.peer_cred()?;
    let (mut rd, mut wr) = stream.into_split();
    let Some(req): Option<RunRequest> = read_frame(&mut rd).await? else { return Ok(()) };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ServerMsg>();
    let writer = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            let last = matches!(m, ServerMsg::Exit(_) | ServerMsg::Exec(_));
            let Ok(b) = bincode::serialize(&m) else { break };
            if wr.write_all(&(b.len() as u32).to_le_bytes()).await.is_err() || wr.write_all(&b).await.is_err() {
                break;
            }
            if last {
                let _ = wr.shutdown().await;
                break;
            }
        }
    });
    if req.version != PROTO_VERSION {
        let _ = tx.send(ServerMsg::Stderr(
            format!("error: rargo client (protocol {}) does not match rargod (protocol {PROTO_VERSION})\n", req.version)
                .into_bytes(),
        ));
        let _ = tx.send(ServerMsg::Exit(101));
        let _ = writer.await;
        return Ok(());
    }
    let env: HashMap<String, String> = req.env.iter().cloned().collect();
    if let (Some(cmd @ ("__export" | "__import")), Some(dir)) =
        (req.args.first().map(|s| s.as_str()), req.args.get(1).cloned())
    {
        // Cache transfer between machines. Only root may do it: an import
        // adds results every later build will trust.
        let export = cmd == "__export";
        let reply = if cred.uid() != 0 {
            Err(anyhow::anyhow!("only root may export or import the cache"))
        } else {
            let d = daemon.clone();
            tokio::task::spawn_blocking(move || {
                let dir = PathBuf::from(dir);
                if export { d.export_cache(&dir) } else { d.import_cache(&dir) }
            })
            .await?
        };
        let (msg, code) = match reply {
            Ok((traces, blobs)) => (
                format!("{} {traces} traces and {blobs} blobs\n", if export { "exported" } else { "imported" }),
                0,
            ),
            Err(e) => (format!("error: {e:#}\n"), 101),
        };
        let _ = tx.send(if code == 0 { ServerMsg::Stdout(msg.into_bytes()) } else { ServerMsg::Stderr(msg.into_bytes()) });
        let _ = tx.send(ServerMsg::Exit(code));
        let _ = writer.await;
        return Ok(());
    }
    if req.args.first().map(|s| s.as_str()) == Some("__drift") {
        // On-demand drift check of the worktree the client is in.
        let who = rargo_core::drift::LastClient {
            env: Arc::new(env.clone()),
            uid: cred.uid(),
            gid: cred.gid(),
            at: std::time::Instant::now(),
            checked_head: None,
        };
        let mut root = PathBuf::from(&req.cwd);
        while !root.join(".git").exists() {
            if !root.pop() {
                break;
            }
        }
        let (msg, code) = match rargo_core::drift::check(&daemon, &root, &who).await {
            Ok(r) => {
                let text = r.render();
                *daemon.drift_report.lock() = Some(format!("{}: {text}", root.display()));
                (text, if r.agrees() { 0 } else { 1 })
            }
            Err(e) => (format!("error: drift check could not run: {e:#}\n"), 101),
        };
        let _ = tx.send(ServerMsg::Stdout(msg.into_bytes()));
        let _ = tx.send(ServerMsg::Exit(code));
        let _ = writer.await;
        return Ok(());
    }
    if req.args.first().map(|s| s.as_str()) == Some("__gc") {
        // Explicit collection: everything unreferenced, regardless of age.
        // Meant for when no build is running.
        let d = daemon.clone();
        // While anything is running or queued, recently used blobs may be
        // outputs not yet recorded anywhere; keep the usual grace period.
        let (running, waiting, _, _) = daemon.engine.sched.snapshot();
        let age = if running + waiting == 0 { 0 } else { 600 };
        let (n, bytes) = tokio::task::spawn_blocking(move || d.collect_garbage(age)).await.unwrap_or((0, 0));
        daemon.engine.store.db().flush();
        let _ = tx.send(ServerMsg::Stdout(
            format!(
                "collected {n} unreferenced blobs, {:.1} MiB; store is now {:.1} MiB\n",
                bytes as f64 / (1 << 20) as f64,
                daemon.engine.store.total_bytes() as f64 / (1 << 20) as f64
            )
            .into_bytes(),
        ));
        let _ = tx.send(ServerMsg::Exit(0));
        let _ = writer.await;
        return Ok(());
    }
    if req.args.first().map(|s| s.as_str()) == Some("__compact") {
        // Everything not linked into a worktree goes cold now.
        let d = daemon.clone();
        let (n, freed) = tokio::task::spawn_blocking(move || {
            let fs = d.engine.fs.clone();
            d.engine.store.freeze_idle(0, &|h| fs.forget_backing(h))
        })
        .await
        .unwrap_or((0, 0));
        daemon.engine.store.db().flush();
        let (cn, logical, disk) = daemon.engine.store.cold_stats();
        let mib = |b: u64| b as f64 / (1 << 20) as f64;
        let _ = tx.send(ServerMsg::Stdout(
            format!(
                "{n} blobs went cold, {:.1} MiB freed; cold tier holds {cn} blobs of {:.1} MiB as {:.1} MiB of chunks\n",
                mib(freed),
                mib(logical),
                mib(disk)
            )
            .into_bytes(),
        ));
        let _ = tx.send(ServerMsg::Exit(0));
        let _ = writer.await;
        return Ok(());
    }
    if req.args.first().map(|s| s.as_str()) == Some("__incr") {
        let d = daemon.clone();
        let text = tokio::task::spawn_blocking(move || d.incr.analyze()).await.unwrap_or_default();
        let _ = tx.send(ServerMsg::Stdout(text.into_bytes()));
        let _ = tx.send(ServerMsg::Exit(0));
        let _ = writer.await;
        return Ok(());
    }
    if req.args.first().map(|s| s.as_str()) == Some("__status") {
        let _ = tx.send(ServerMsg::Stdout(status_text(&daemon).into_bytes()));
        let _ = tx.send(ServerMsg::Exit(0));
        let _ = writer.await;
        return Ok(());
    }
    let prio = match env.get("RARGO_PRIORITY").map(|s| s.as_str()) {
        Some("human") => Priority::Human,
        Some("agent") => Priority::Agent,
        Some("background") => Priority::Background,
        _ if req.stderr_tty => Priority::Human,
        _ => Priority::Agent,
    };
    let out = Output::new(tx.clone(), wants_color(&req, &env, req.stderr_tty), wants_color(&req, &env, req.stdout_tty));
    let client = Client {
        args: req.args.clone(),
        cwd: PathBuf::from(&req.cwd),
        env: Arc::new(env),
        uid: cred.uid(),
        gid: cred.gid(),
        prio,
        out,
    };
    let work = tokio::spawn(build::handle(daemon, client));
    // If the client goes away, stop: queued actions are dropped.
    let abort = work.abort_handle();
    let watch_client = tokio::spawn(async move {
        let mut buf = [0u8; 64];
        loop {
            match rd.read(&mut buf).await {
                Ok(0) | Err(_) => {
                    abort.abort();
                    break;
                }
                Ok(_) => {}
            }
        }
    });
    let msg = match work.await {
        Ok(Ok(code)) => ServerMsg::Exit(code),
        Ok(Err(exec)) => ServerMsg::Exec(exec),
        Err(e) if e.is_cancelled() => ServerMsg::Exit(130),
        Err(e) => {
            let _ = tx.send(ServerMsg::Stderr(format!("error: rargod internal error: {e}\n").into_bytes()));
            ServerMsg::Exit(101)
        }
    };
    let _ = tx.send(msg);
    drop(tx);
    let _ = writer.await;
    watch_client.abort();
    Ok(())
}

/// The key for the server-worker link. A worker runs any command the server
/// sends and the server believes the results, so a link that leaves this
/// machine must have one.
fn link_key(opts: &Opts, addr: std::net::SocketAddr) -> Result<rargo_core::net::LinkKey> {
    match &opts.worker_key {
        Some(path) => {
            let secret = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
            if secret.len() < 16 {
                anyhow::bail!("{}: a worker key needs at least 16 bytes", path.display());
            }
            Ok(Some(Arc::new(rargo_core::hash::Hash::of(&secret).0)))
        }
        None if addr.ip().is_loopback() => Ok(None),
        None => anyhow::bail!("--worker-key FILE is required unless the worker link is on the loopback interface"),
    }
}

fn status_text(d: &Daemon) -> String {
    let e = &d.engine;
    let (running, waiting, peak, granted) = e.sched.snapshot();
    let t = &e.traces.stats;
    let s = &e.stats;
    let f = e.fs.stats();
    format!(
        "requests            {}\n\
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
                    "{} connected; {} actions dispatched ({} of them then run here on an idle core), {} kept local (core free), {} kept local (transfer too costly), {} worker failures; \
                     sent ahead {:.1} MiB, not sent because already there {:.1} MiB; outputs fetched {:.1} MiB, reused locally {:.1} MiB; \
                     {} answers sent ahead, {} lookups and {} opens asked",
                    p.worker_count(),
                    ps.dispatched.load(Ordering::Relaxed),
                    ps.hedged.load(Ordering::Relaxed),
                    ps.kept_local_no_wait.load(Ordering::Relaxed),
                    ps.kept_local_transfer.load(Ordering::Relaxed),
                    ps.worker_failures.load(Ordering::Relaxed),
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
