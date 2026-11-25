use crate::build::{self, Client, Daemon};
use crate::engine::{ClientEnv, EnvAsker};
use crate::hash::Hash;
use crate::present::Output;
use crate::net::link::Link;
use crate::proto::{CONTROL, ClientMsg, Frame, PROTO_VERSION, Reorder, RunRequest, ServerMsg};
use crate::sched::Priority;
use crate::session::{self, ClientHas, Keys, Session};
use anyhow::{Context, Result};
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

pub async fn connection(d: Arc<Daemon>, mut stream: TcpStream, keys: Arc<Keys>, http: Arc<crate::http::Http>) -> Result<()> {
    let _ = stream.set_nodelay(true);
    let mut first = [0u8; 4];
    let n = tokio::time::timeout(std::time::Duration::from_secs(20), stream.peek(&mut first)).await.context("nothing was sent")??;
    if n == 4 && matches!(&first, b"GET " | b"POST" | b"HEAD") {
        let stream = stream.into_std()?;
        return tokio::task::spawn_blocking(move || http.serve(stream)).await?;
    }
    let (user, admin, sealer, opener) =
        tokio::time::timeout(std::time::Duration::from_secs(20), session::accept(&mut stream, &keys))
            .await
            .context("handshake timed out")??;
    link(d, Arc::new(Link::tcp(stream, sealer, opener)), user, admin, keys).await;
    Ok(())
}

pub async fn udp_connection(d: Arc<Daemon>, conn: Arc<crate::net::Conn>, endpoint: Arc<crate::net::Endpoint>, keys: Arc<Keys>) {
    let Some(user) = conn.user().map(String::from) else {
        endpoint.forget(conn.id);
        return;
    };
    let admin = keys.admin(&user);
    link(d, Arc::new(Link::Udp(conn, endpoint)), user, admin, keys).await;
}

struct Stream {
    sess: Arc<Session>,
    abort: tokio::task::AbortHandle,
}

async fn pump(link: Arc<Link>, stream: u32, mut rx: tokio::sync::mpsc::UnboundedReceiver<ServerMsg>, sess: Arc<Session>) {
    assert_ne!(stream, crate::proto::KEEPALIVE, "keepalives are not a session's stream");
    let mut seq = 0u64;
    while let Some(m) = rx.recv().await {
        let last = matches!(m, ServerMsg::Exit(_) | ServerMsg::Exec(_) | ServerMsg::Fetched { .. });
        let bulk = session::bulk_of(&m);
        let msg = match (bulk, crate::proto::pack(&crate::proto::encode(&m))) {
            (0, Some(z)) => ServerMsg::Packed(z),
            _ => m,
        };
        let frame = crate::proto::encode(&Frame { stream, seq, msg });
        seq += 1;
        if link.send(&frame).await.is_err() {
            break;
        }
        if bulk > 0 {
            sess.sent_bulk(bulk);
        }
        if last {
            break;
        }
    }
    sess.close_window();
}

fn offer(d: &Daemon, o: crate::proto::Offer) -> Option<ServerMsg> {
    let stats = &d.trees.stats;
    stats.offers.fetch_add(1, Ordering::Relaxed);
    for t in &o.trees {
        if d.trees.put(t).is_ok() {
            stats.trees_received.fetch_add(1, Ordering::Relaxed);
            stats.offered_bytes.fetch_add(t.len() as u64, Ordering::Relaxed);
        }
    }
    for b in o.blobs {
        let claimed = b.hash;
        let Ok(bytes) = b.unpack() else { continue };
        if Hash::of(&bytes).0 == claimed && d.engine.store.put_bytes(&bytes).is_ok() {
            stats.offered_bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed);
            d.stats.bytes_in.fetch_add(bytes.len() as u64, Ordering::Relaxed);
            d.stats.files_in.fetch_add(1, Ordering::Relaxed);
        }
    }
    if !o.ask {
        return None;
    }
    let (trees, blobs) = d.trees.lacking(Hash(o.root));
    Some(ServerMsg::Missing { id: o.id, trees: trees.iter().map(|h| h.0).collect(), blobs: blobs.iter().map(|h| h.0).collect() })
}

pub async fn link(d: Arc<Daemon>, link: Arc<Link>, presented: String, admin: bool, keys: Arc<Keys>) {
    let user = keys.name_of(&presented);
    let mut checked = std::time::Instant::now();
    let has = Arc::new(ClientHas::default());
    let (control, control_rx) = Session::on_link(d.engine.store.clone(), has.clone(), d.stats.clone());
    tokio::spawn(pump(link.clone(), CONTROL, control_rx, control.clone()));
    let streams: Arc<parking_lot::Mutex<HashMap<u32, Stream>>> = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let mut order: Reorder<ClientMsg> = Reorder::new(false);
    d.stats.links.fetch_add(1, Ordering::Relaxed);
    'link: loop {
        if checked.elapsed() > std::time::Duration::from_secs(2) {
            if !keys.still_valid(&presented) {
                eprintln!("rargod: {user}: the key or token of this session is no longer valid; closing it");
                break;
            }
            checked = std::time::Instant::now();
        }
        let raw = tokio::select! {
            m = link.recv() => m,
            _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
                if link.idle() > std::time::Duration::from_secs(180) { None } else { continue }
            }
        };
        let Some(raw) = raw else { break };
        let Ok(frame) = crate::proto::decode::<Frame<ClientMsg>>(&raw) else { break };
        if frame.stream == crate::proto::KEEPALIVE {
            if let ClientMsg::Ping(n) = frame.msg {
                let pong = crate::proto::encode(&Frame { stream: crate::proto::KEEPALIVE, seq: 0, msg: ServerMsg::Pong(n) });
                let link = link.clone();
                tokio::spawn(async move {
                    let _ = link.send(&pong).await;
                });
            }
            continue;
        }
        for (stream, msg) in order.push(frame) {
            let msg = match msg {
                ClientMsg::Packed(z) => match crate::proto::unpack(&z).and_then(|b| crate::proto::decode(&b)) {
                    Ok(m) => m,
                    Err(_) => break 'link,
                },
                m => m,
            };
            match (stream, msg) {
                (CONTROL, ClientMsg::Ping(n)) => control.send(ServerMsg::Pong(n)),
                (CONTROL, ClientMsg::Offer(o)) => {
                    if let Some(reply) = tokio::task::block_in_place(|| offer(&d, *o)) {
                        control.send(reply);
                    }
                }
                (stream, ClientMsg::Fetch { id, hashes }) if stream != CONTROL => {
                    let (sess, rx) = Session::on_link(d.engine.store.clone(), has.clone(), d.stats.clone());
                    tokio::spawn(pump(link.clone(), stream, rx, sess.clone()));
                    tokio::spawn(async move {
                        let _ = sess.fetch(id, hashes).await;
                    });
                }
                (CONTROL, m) => {
                    if let Err(e) = control.incoming(m) {
                        eprintln!("rargod: {user}: {e:#}");
                        break 'link;
                    }
                }
                (stream, ClientMsg::Recipe { id, hash }) if stream != CONTROL => {
                    let (sess, rx) = Session::on_link(d.engine.store.clone(), has.clone(), d.stats.clone());
                    tokio::spawn(pump(link.clone(), stream, rx, sess.clone()));
                    let blobs = d.client_blobs.clone();
                    tokio::task::spawn_blocking(move || {
                        let chunks: Vec<_> = blobs.recipe(&Hash(hash)).map(|r| r.iter().map(|c| (c.hash.0, c.len)).collect()).unwrap_or_default();
                        let (outline, lazy) = match blobs.outline(&Hash(hash)) {
                            Some(o) if !chunks.is_empty() => (o.cvs.clone(), o.lazy.clone()),
                            _ => (Vec::new(), Vec::new()),
                        };
                        sess.send(ServerMsg::Recipe { hash, chunks, outline, lazy });
                        sess.send(ServerMsg::Fetched { id });
                    });
                }
                (stream, ClientMsg::Chunks { id, hash, idx }) if stream != CONTROL => {
                    let (sess, rx) = Session::on_link(d.engine.store.clone(), has.clone(), d.stats.clone());
                    tokio::spawn(pump(link.clone(), stream, rx, sess.clone()));
                    let blobs = d.client_blobs.clone();
                    tokio::task::spawn_blocking(move || {
                        for group in idx.chunks(16) {
                            let Ok(data) = blobs.serve_chunks(&Hash(hash), group) else { break };
                            let chunks: Vec<(u32, crate::proto::Part)> =
                                group.iter().copied().zip(data.into_iter().map(crate::proto::Part::pack)).collect();
                            sess.count_out(chunks.iter().map(|(_, p)| p.data.len()).sum());
                            sess.send(ServerMsg::Chunks { hash, chunks });
                        }
                        sess.send(ServerMsg::Fetched { id });
                    });
                }
                (stream, ClientMsg::Run(req)) => {
                    if !keys.still_valid(&presented) {
                        eprintln!("rargod: {user}: the key or token of this session is no longer valid; closing it");
                        let (sess, rx) = Session::on_link(d.engine.store.clone(), has.clone(), d.stats.clone());
                        let pumping = tokio::spawn(pump(link.clone(), stream, rx, sess.clone()));
                        sess.send(ServerMsg::Stderr(
                            format!(
                                "error: {}\n       (this machine is not authenticated there, or its token no longer works: run `rargo auth`)\n",
                                crate::proto::REFUSED
                            )
                            .into_bytes(),
                        ));
                        sess.send(ServerMsg::Exit(101));
                        let _ = pumping.await;
                        sess.close();
                        break 'link;
                    }
                    let (sess, rx) = Session::on_link(d.engine.store.clone(), has.clone(), d.stats.clone());
                    let pumping = tokio::spawn(pump(link.clone(), stream, rx, sess.clone()));
                    let work = tokio::spawn(serve(d.clone(), *req, user.clone(), admin, keys.clone(), sess.clone()));
                    streams.lock().insert(stream, Stream { sess: sess.clone(), abort: work.abort_handle() });
                    let streams2 = streams.clone();
                    tokio::spawn(async move {
                        let msg = match work.await {
                            Ok(m) => m,
                            Err(e) if e.is_cancelled() => ServerMsg::Exit(130),
                            Err(e) => {
                                sess.send(ServerMsg::Stderr(format!("error: rargod internal error: {e}\n").into_bytes()));
                                ServerMsg::Exit(101)
                            }
                        };
                        sess.send(msg);
                        let _ = pumping.await;
                        sess.close();
                        streams2.lock().remove(&stream);
                    });
                }
                (stream, ClientMsg::Cancel) => {
                    let gone = streams.lock().remove(&stream);
                    if let Some(st) = gone {
                        st.sess.close();
                        st.abort.abort();
                    }
                }
                (stream, m) => {
                    let sess = streams.lock().get(&stream).map(|st| st.sess.clone());
                    if let Some(sess) = sess {
                        if let Err(e) = sess.incoming(m) {
                            eprintln!("rargod: {e:#}");
                            sess.send(ServerMsg::Stderr(format!("error: {e:#}\n").into_bytes()));
                            let gone = streams.lock().remove(&stream);
                            if let Some(st) = gone {
                                st.sess.close();
                                st.abort.abort();
                            }
                        }
                    }
                }
            }
        }
    }
    let all: Vec<Stream> = streams.lock().drain().map(|(_, st)| st).collect();
    for st in all {
        st.sess.close();
        st.abort.abort();
    }
    control.close();
    link.close();
}

const TOKEN_USAGE: &str = "usage: rargo auth invite [--user NAME] [--admin] [--days N] [--uses N] [--token-days N]\n       \
                           rargo auth create --user NAME [--admin] [--days N]\n       \
                           rargo auth tokens\n       \
                           rargo auth revoke ID-OR-USER\n       \
                           rargo auth allow GITHUB-LOGIN [--admin]\n       \
                           rargo auth disallow GITHUB-LOGIN";

fn fail(sess: &Session, text: String) -> ServerMsg {
    sess.send(ServerMsg::Stderr(format!("error: {text}\n").into_bytes()));
    ServerMsg::Exit(101)
}

fn say(sess: &Session, text: String) -> ServerMsg {
    sess.send(ServerMsg::Stdout(text.into_bytes()));
    ServerMsg::Exit(0)
}

async fn serve(d: Arc<Daemon>, req: RunRequest, user: String, admin: bool, keys: Arc<Keys>, sess: Arc<Session>) -> ServerMsg {
    if req.version != PROTO_VERSION {
        return fail(
            &sess,
            format!("rargo client (protocol {}) does not match rargod (protocol {PROTO_VERSION})", req.version),
        );
    }
    let command = req.args.first().map(|s| s.as_str()).unwrap_or("");
    if matches!(command, "__gc" | "__compact" | "__export" | "__import") && !admin {
        return fail(&sess, format!("user `{user}` may not administer this server (an administrator's token is made on the server with `rargod token create --admin`, `token invite --admin` or `token allow --admin`)"));
    }
    match command {
        "__status" => return say(&sess, status_text(&d)),
        "__settle" => {
            if let Some(pool) = d.pool.get() {
                pool.settle().await;
            }
            return say(&sess, status_text(&d));
        }
        "__token" => {
            let Some(store) = keys.tokens().cloned() else {
                return fail(&sess, "this server has issued no tokens: it admits anyone on its machine, and its tokens are made there (`rargod token`)".into());
            };
            assert!(!req.args.is_empty(), "a command was named");
            let args = req.args[1..].to_vec();
            let (asker_user, from) = (user.clone(), format!("rargo auth, by {user}"));
            let said = tokio::task::spawn_blocking(move || {
                let asker = crate::tokens::Asker { user: &asker_user, admin, from: &from };
                crate::tokens::command(&store, &args, &asker, TOKEN_USAGE)
            })
            .await;
            return match said {
                Ok(Ok((out, err))) => {
                    if !err.is_empty() {
                        sess.send(ServerMsg::Stderr(err.into_bytes()));
                    }
                    say(&sess, out)
                }
                Ok(Err(e)) => fail(&sess, format!("{e:#}")),
                Err(e) => fail(&sess, format!("{e}")),
            };
        }
        "__incr" => {
            let d2 = d.clone();
            return say(&sess, tokio::task::spawn_blocking(move || d2.incr.analyze()).await.unwrap_or_default());
        }
        "__gc" => {
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

    let worktree = req.worktree.clone();
    let before: Option<Arc<std::collections::HashSet<crate::proto::Digest>>> = {
        let known = d.deliveries.lock();
        known.get(&worktree).filter(|(token, _)| req.delivered != 0 && *token == req.delivered).map(|(_, set)| set.clone())
    };
    if let Some(set) = &before {
        sess.knows(set.iter().copied());
    }
    if req.lazy {
        sess.set_lazy();
    }
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
        let Some(dir) = rargo_proto::workspace::command_dir(req.root.as_ref(), req.cwd.as_ref(), &req.args) else {
            return fail(&sess, "the current directory must be inside the synced worktree".into());
        };
        let who = crate::drift::LastClient {
            env: Arc::new(env),
            home,
            root: req.root.clone(),
            dir,
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
    let outcome = build::handle(d.clone(), client).await;
    let mut delivered = sess.delivered();
    if !delivered.is_empty() && !worktree.is_empty() {
        if let Some(set) = &before {
            if set.len() < 500_000 {
                delivered.extend(set.iter().copied());
            }
        }
        let mut b = [0u8; 8];
        unsafe { libc::getrandom(b.as_mut_ptr() as *mut _, 8, 0) };
        let token = u64::from_le_bytes(b) | 1;
        assert_ne!(token, 0);
        d.deliveries.lock().insert(worktree, (token, Arc::new(delivered)));
        sess.send(ServerMsg::DeliveryToken(token));
    }
    match outcome {
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
         sync                {} worktree states, {} manifests and {} files received from clients ({} changes pushed unasked, {} files asked for, {} awaited from another client's upload), {:.1} MiB received, {:.1} MiB of results sent; {} client daemons have connected\n\
         store               {} blobs, {:.1} MiB\n\
         cold tier           {}\n\
         cache               {} hits, {} misses, {} traces stored\n\
         actions run         {} ({:.1}s cpu, {:.1}s wall)\n\
         actions by kind     {}\n\
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
        d.stats.files_in.load(Ordering::Relaxed),
        y.offers.load(Ordering::Relaxed),
        y.blobs_requested.load(Ordering::Relaxed),
        y.shared_uploads.load(Ordering::Relaxed),
        d.stats.bytes_in.load(Ordering::Relaxed) as f64 / (1 << 20) as f64,
        d.stats.bytes_out.load(Ordering::Relaxed) as f64 / (1 << 20) as f64,
        d.stats.links.load(Ordering::Relaxed),
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
        crate::trace::ActionKind::ALL
            .iter()
            .map(|k| format!("{} {}", k.name(), s.runs_by_kind[*k as usize].load(Ordering::Relaxed)))
            .collect::<Vec<_>>()
            .join(", "),
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
                    "{} connected; {} actions dispatched ({} of them also started here on an idle core: {} finished here first, {} were stopped because the worker was ahead), {} kept local (core free), {} kept local (a local core came free first), {} kept local (transfer too costly), {} kept local (no worker has the incremental state), {} worker failures; overhead per remote action {}; \
                     incremental compiles on workers: {} without state, {} with the state the worker held; state left on workers {:.1} MiB in {} files, of which {:.1} MiB in {} files came home in the background and {} files were fetched for a compile here; {} states made here sent to workers ahead of need ({:.1} MiB); \
                     sent ahead {:.1} MiB, not sent because already there {:.1} MiB; outputs fetched {:.1} MiB, reused locally {:.1} MiB; \
                     {} answers sent ahead, {} lookups and {} opens asked, {} nix store lookups",
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
                    ps.unseeded_remote.load(Ordering::Relaxed),
                    ps.seeded_in_place.load(Ordering::Relaxed),
                    ps.state_left_bytes.load(Ordering::Relaxed) as f64 / 1048576.0,
                    ps.state_left_blobs.load(Ordering::Relaxed),
                    ps.state_homed_bytes.load(Ordering::Relaxed) as f64 / 1048576.0,
                    ps.state_homed_blobs.load(Ordering::Relaxed),
                    ps.state_needed_blobs.load(Ordering::Relaxed),
                    ps.states_sent_ahead.load(Ordering::Relaxed),
                    ps.state_ahead_bytes.load(Ordering::Relaxed) as f64 / 1048576.0,
                    pushed as f64 / 1048576.0,
                    skipped as f64 / 1048576.0,
                    fetched as f64 / 1048576.0,
                    reused as f64 / 1048576.0,
                    ps.prefilled_entries.load(Ordering::Relaxed),
                    ps.lookups_served.load(Ordering::Relaxed),
                    ps.opens_served.load(Ordering::Relaxed),
                    ps.nix_lookups.load(Ordering::Relaxed),
                )
            }
        },
    )
}
