use crate::build::{self, Client, Daemon};
use crate::conn_state::{Command, ConnectionState, Route, StatusFigures, WorkerFigures};
use crate::engine::{ClientEnv, EnvAsker};
use crate::hash::Hash;
use crate::net::link::Link;
use crate::present::Output;
use crate::proto::{CONTROL, ClientMsg, Frame, Reorder, RunRequest, ServerMsg};
use crate::session::{self, ClientHas, Keys, Session};
use anyhow::{Context, Result};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::net::TcpStream;

pub async fn connection(
    d: Arc<Daemon>,
    mut stream: TcpStream,
    keys: Arc<Keys>,
    http: Arc<crate::http::Http>,
) -> Result<()> {
    let _ = stream.set_nodelay(true);
    let mut first = [0u8; 4];
    let n = tokio::time::timeout(std::time::Duration::from_secs(20), stream.peek(&mut first))
        .await
        .context("nothing was sent")??;
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

pub async fn udp_connection(
    d: Arc<Daemon>,
    conn: Arc<crate::net::Conn>,
    endpoint: Arc<crate::net::Endpoint>,
    keys: Arc<Keys>,
) {
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

async fn pump(
    link: Arc<Link>,
    stream: u32,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<ServerMsg>,
    sess: Arc<Session>,
) {
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
    Some(ServerMsg::Missing {
        id: o.id,
        trees: trees.iter().map(|h| h.0).collect(),
        blobs: blobs.iter().map(|h| h.0).collect(),
    })
}

pub async fn link(d: Arc<Daemon>, link: Arc<Link>, presented: String, admin: bool, keys: Arc<Keys>) {
    let user = keys.name_of(&presented);
    let mut checked = std::time::Instant::now();
    let has = Arc::new(ClientHas::default());
    let (control, control_rx) = Session::on_link(d.engine.store.clone(), has.clone(), d.stats.clone());
    tokio::spawn(pump(link.clone(), CONTROL, control_rx, control.clone()));
    let streams: Arc<parking_lot::Mutex<HashMap<u32, Stream>>> = Arc::new(parking_lot::Mutex::new(HashMap::new()));
    let conn = Arc::new(parking_lot::Mutex::new(ConnectionState::new()));
    let mut order: Reorder<ClientMsg> = Reorder::new(false);
    let mut todo: VecDeque<Route> = VecDeque::new();
    d.stats.links.fetch_add(1, Ordering::Relaxed);
    'link: loop {
        while let Some(route) = todo.pop_front() {
            match route {
                Route::Close => break 'link,
                Route::Reply(m) => control.send(m),
                Route::Offer(o) => {
                    if let Some(reply) = tokio::task::block_in_place(|| offer(&d, *o)) {
                        control.send(reply);
                    }
                }
                Route::Control(m) => {
                    if let Err(e) = control.incoming(m) {
                        eprintln!("rargod: {user}: {e:#}");
                        todo.extend(conn.lock().control_failed());
                    }
                }
                Route::Fetch { stream, id, hashes } => {
                    let (sess, rx) = Session::on_link(d.engine.store.clone(), has.clone(), d.stats.clone());
                    tokio::spawn(pump(link.clone(), stream, rx, sess.clone()));
                    tokio::spawn(async move {
                        let _ = sess.fetch(id, hashes).await;
                    });
                }
                Route::Recipe { stream, id, hash } => {
                    let (sess, rx) = Session::on_link(d.engine.store.clone(), has.clone(), d.stats.clone());
                    tokio::spawn(pump(link.clone(), stream, rx, sess.clone()));
                    let blobs = d.client_blobs.clone();
                    tokio::task::spawn_blocking(move || {
                        let chunks: Vec<_> = blobs
                            .recipe(&Hash(hash))
                            .map(|r| r.iter().map(|c| (c.hash.0, c.len)).collect())
                            .unwrap_or_default();
                        let (outline, lazy) = match blobs.outline(&Hash(hash)) {
                            Some(o) if !chunks.is_empty() => (o.cvs.clone(), o.lazy.clone()),
                            _ => (Vec::new(), Vec::new()),
                        };
                        sess.send(ServerMsg::Recipe { hash, chunks, outline, lazy });
                        sess.send(ServerMsg::Fetched { id });
                    });
                }
                Route::Chunks { stream, id, hash, idx } => {
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
                Route::Refuse { stream } => {
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
                }
                Route::Run { stream, req } => {
                    let (sess, rx) = Session::on_link(d.engine.store.clone(), has.clone(), d.stats.clone());
                    let pumping = tokio::spawn(pump(link.clone(), stream, rx, sess.clone()));
                    let work = tokio::spawn(serve(d.clone(), *req, user.clone(), admin, keys.clone(), sess.clone()));
                    streams.lock().insert(stream, Stream { sess: sess.clone(), abort: work.abort_handle() });
                    let (streams2, conn2) = (streams.clone(), conn.clone());
                    tokio::spawn(async move {
                        let msg = match work.await {
                            Ok(m) => m,
                            Err(e) if e.is_cancelled() => ServerMsg::Exit(130),
                            Err(e) => {
                                sess.send(ServerMsg::Stderr(
                                    format!("error: rargod internal error: {e}\n").into_bytes(),
                                ));
                                ServerMsg::Exit(101)
                            }
                        };
                        sess.send(msg);
                        let _ = pumping.await;
                        sess.close();
                        let mut s = streams2.lock();
                        if s.get(&stream).is_some_and(|st| Arc::ptr_eq(&st.sess, &sess)) {
                            s.remove(&stream);
                            conn2.lock().finished(stream);
                        }
                    });
                }
                Route::Stream { stream, msg } => {
                    let sess = streams.lock().get(&stream).map(|st| st.sess.clone());
                    if let Some(sess) = sess
                        && let Err(e) = sess.incoming(msg)
                    {
                        eprintln!("rargod: {e:#}");
                        sess.send(ServerMsg::Stderr(format!("error: {e:#}\n").into_bytes()));
                        todo.extend(conn.lock().stream_failed(stream));
                    }
                }
                Route::Cancel { stream } => {
                    let gone = streams.lock().remove(&stream);
                    if let Some(st) = gone {
                        st.sess.close();
                        st.abort.abort();
                    }
                }
                Route::Report { stream, text } => eprintln!("rargod: {user}: stream {stream}: {text}"),
            }
        }
        if checked.elapsed() > std::time::Duration::from_secs(2) {
            let valid = keys.still_valid(&presented);
            if !valid {
                eprintln!("rargod: {user}: the key or token of this session is no longer valid; closing it");
            }
            todo.extend(conn.lock().key_checked(valid));
            checked = std::time::Instant::now();
            continue;
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
                let pong =
                    crate::proto::encode(&Frame { stream: crate::proto::KEEPALIVE, seq: 0, msg: ServerMsg::Pong(n) });
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
            let valid = !matches!(msg, ClientMsg::Run(_)) || keys.still_valid(&presented);
            todo.extend(conn.lock().message(stream, msg, valid));
        }
    }
    let rest = conn.lock().close();
    for route in rest {
        if let Route::Cancel { stream } = route {
            let gone = streams.lock().remove(&stream);
            if let Some(st) = gone {
                st.sess.close();
                st.abort.abort();
            }
        }
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

async fn serve(
    d: Arc<Daemon>,
    req: RunRequest,
    user: String,
    admin: bool,
    keys: Arc<Keys>,
    sess: Arc<Session>,
) -> ServerMsg {
    let command = crate::conn_state::command(req.version, &req.args, admin, &user);
    match &command {
        Command::Fail(text) => return fail(&sess, text.clone()),
        Command::Status => return say(&sess, status_text(&d)),
        Command::Settle => {
            if let Some(pool) = d.pool.get() {
                pool.settle().await;
            }
            return say(&sess, status_text(&d));
        }
        Command::Token => {
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
        Command::Incr => {
            let d2 = d.clone();
            return say(&sess, tokio::task::spawn_blocking(move || d2.incr.analyze()).await.unwrap_or_default());
        }
        Command::Gc => {
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
        Command::Compact => {
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
        Command::Export(dir) | Command::Import(dir) => {
            let export = matches!(command, Command::Export(_));
            let dir = dir.clone();
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
        Command::Drift | Command::Build => {}
    }

    let worktree = req.worktree.clone();
    let before: Option<Arc<std::collections::HashSet<crate::proto::Digest>>> = {
        let known = d.deliveries.lock();
        known
            .get(&worktree)
            .filter(|(token, _)| req.delivered != 0 && *token == req.delivered)
            .map(|(_, set)| set.clone())
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
    let prio = crate::conn_state::priority(&env, req.stderr_tty);
    let out = Output::new(
        sess.sender(),
        crate::conn_state::wants_color(&req.args, &env, req.stderr_tty),
        crate::conn_state::wants_color(&req.args, &env, req.stdout_tty),
    );
    let asker: EnvAsker = {
        let s = sess.clone();
        Arc::new(move |names| {
            let s = s.clone();
            Box::pin(async move { s.want_env(names).await })
        })
    };
    if command == Command::Drift {
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
    crate::conn_state::status_text(&status_figures(d))
}

fn status_figures(d: &Daemon) -> StatusFigures {
    let e = &d.engine;
    let (running, waiting, peak, granted) = e.sched.snapshot();
    let t = &e.traces.stats;
    let s = &e.stats;
    let f = e.fs.stats();
    let y = &d.trees.stats;
    let (cold_blobs, cold_logical, cold_disk) = e.store.cold_stats();
    let n = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed);
    StatusFigures {
        requests: n(&d.stats.requests),
        snapshots: n(&y.snapshots),
        trees_received: n(&y.trees_received),
        files_in: n(&d.stats.files_in),
        offers: n(&y.offers),
        blobs_requested: n(&y.blobs_requested),
        shared_uploads: n(&y.shared_uploads),
        bytes_in: n(&d.stats.bytes_in),
        bytes_out: n(&d.stats.bytes_out),
        links: n(&d.stats.links),
        blob_count: e.store.blob_count() as u64,
        store_bytes: e.store.total_bytes(),
        cold_blobs: cold_blobs as u64,
        cold_logical,
        cold_disk,
        hits: n(&t.hits),
        misses: n(&t.misses),
        traces_stored: n(&t.stored),
        runs: n(&s.runs),
        run_cpu_us: n(&s.run_cpu_us),
        run_wall_us: n(&s.run_wall_us),
        runs_by_kind: crate::trace::ActionKind::ALL
            .iter()
            .map(|k| (k.name(), n(&s.runs_by_kind[*k as usize])))
            .collect(),
        joined: n(&s.joined),
        joined_then_ran: n(&s.joined_then_ran),
        tainted: n(&s.tainted),
        env_reruns: n(&s.env_reruns),
        running: running as u64,
        waiting: waiting as u64,
        peak: peak as u64,
        granted,
        tokens: e.sched.tokens() as u64,
        lookups: n(&f.lookups),
        opens: n(&f.opens),
        reads: n(&f.reads),
        writes: n(&f.writes),
        plans_decoded: n(&d.stats.plans_decoded),
        incr_saved: n(&d.incr.stats.saved),
        seeded_same_worktree: n(&d.incr.stats.seeded_same_worktree),
        seeded_ancestor: n(&d.incr.stats.seeded_ancestor),
        seeded_other: n(&d.incr.stats.seeded_other),
        unseeded: n(&d.incr.stats.unseeded),
        discarded_after_crash: n(&d.incr.stats.discarded_after_crash),
        verify_units: n(&d.stats.verify_units),
        verify_mismatches: n(&d.stats.verify_mismatches),
        drift: d.drift_report.lock().clone(),
        workers: d.pool.get().map(|p| {
            let (pushed, skipped, fetched, reused) = p.transfer_totals();
            let ps = &p.stats;
            WorkerFigures {
                count: p.worker_count() as u64,
                dispatched: n(&ps.dispatched),
                hedged: n(&ps.hedged),
                hedge_won: n(&ps.hedge_won),
                hedge_stopped: n(&ps.hedge_stopped),
                kept_local_no_wait: n(&ps.kept_local_no_wait),
                kept_local_first: n(&ps.kept_local_first),
                kept_local_transfer: n(&ps.kept_local_transfer),
                kept_local_incremental: n(&ps.kept_local_incremental),
                worker_failures: n(&ps.worker_failures),
                overhead: p.overhead_text(),
                unseeded_remote: n(&ps.unseeded_remote),
                seeded_in_place: n(&ps.seeded_in_place),
                state_left_bytes: n(&ps.state_left_bytes),
                state_left_blobs: n(&ps.state_left_blobs),
                state_homed_bytes: n(&ps.state_homed_bytes),
                state_homed_blobs: n(&ps.state_homed_blobs),
                state_needed_blobs: n(&ps.state_needed_blobs),
                states_sent_ahead: n(&ps.states_sent_ahead),
                state_ahead_bytes: n(&ps.state_ahead_bytes),
                pushed,
                skipped,
                fetched,
                reused,
                prefilled: n(&ps.prefilled_entries),
                lookups_served: n(&ps.lookups_served),
                opens_served: n(&ps.opens_served),
                nix_lookups: n(&ps.nix_lookups),
            }
        }),
    }
}
