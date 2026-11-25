use anyhow::{Context, Result, bail};
use rargo_core::build::{Config, Daemon};
use rargo_core::engine::Engine;
use rargo_core::sandbox::Zygote;
use rargo_core::sandboxfs::SandboxFs;
use rargo_core::sched::Scheduler;
use rargo_core::session::Keys;
use rargo_core::store::Store;
use rargo_core::view::Content;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;

struct Opts {
    state: PathBuf,
    store: Option<PathBuf>,
    planner: PathBuf,
    listen: std::net::SocketAddr,
    cargo_home: Option<PathBuf>,
    uid: Option<u32>,
    gid: Option<u32>,
    jobs: usize,
    mem_kb: u64,
    passthrough: bool,
    store_budget: u64,
    cold_after_secs: u64,
    drift_minutes: u64,
    workers_listen: Option<std::net::SocketAddr>,
    worker_of: Option<std::net::SocketAddr>,
    worker_key: Option<PathBuf>,
    public: Option<String>,
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
        listen: "127.0.0.1:7459".parse().unwrap(),
        cargo_home: None,
        uid: None,
        gid: None,
        jobs: cpus,
        mem_kb: total_kb / 10 * 7,
        passthrough: true,
        store_budget: 100 << 30,
        cold_after_secs: 30 * 60,
        drift_minutes: 360,
        workers_listen: None,
        worker_of: None,
        worker_key: None,
        public: None,
    };
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().with_context(|| format!("{a} needs a value"));
        match a.as_str() {
            "--state" => o.state = val()?.into(),
            "--store" => o.store = Some(val()?.into()),
            "--planner" => o.planner = val()?.into(),
            "--listen" => o.listen = val()?.parse().context("--listen takes ADDRESS:PORT")?,
            "--cargo-home" => o.cargo_home = Some(val()?.into()),
            "--uid" => o.uid = Some(val()?.parse()?),
            "--gid" => o.gid = Some(val()?.parse()?),
            "--jobs" => o.jobs = val()?.parse()?,
            "--mem-gb" => o.mem_kb = val()?.parse::<u64>()? << 20,
            "--no-passthrough" => o.passthrough = false,
            "--drift-interval-min" => o.drift_minutes = val()?.parse()?,
            "--workers-listen" => o.workers_listen = Some(val()?.parse()?),
            "--worker" => o.worker_of = Some(val()?.parse()?),
            "--worker-key" => o.worker_key = Some(PathBuf::from(val()?)),
            "--public" => o.public = Some(val()?),
            "--store-budget-gb" => o.store_budget = val()?.parse::<u64>()? << 30,
            "--store-budget-mb" => o.store_budget = val()?.parse::<u64>()? << 20,
            "--cold-after-min" => o.cold_after_secs = val()?.parse::<u64>()? * 60,
            "--cold-after-secs" => o.cold_after_secs = val()?.parse::<u64>()?,
            other => bail!("unknown option `{other}`"),
        }
    }
    if o.state.as_os_str().is_empty() || (o.planner.as_os_str().is_empty() && o.worker_of.is_none()) {
        bail!(
"usage: rargod --state DIR --planner PATH [--listen ADDR:PORT] [--cargo-home DIR] [--uid N --gid N] [--store DIR] [--jobs N] [--mem-gb N] [--workers-listen ADDR --worker-key FILE]\n       rargod --state DIR --worker SERVER_ADDR --worker-key FILE [--jobs N]"
        );
    }
    assert!(!o.state.as_os_str().is_empty());
    assert!(o.worker_of.is_some() || !o.planner.as_os_str().is_empty());
    Ok(o)
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if matches!(args.get(1).map(|s| s.as_str()), Some("--version" | "-V")) {
        println!("rargod {} (protocol {})", env!("CARGO_PKG_VERSION"), rargo_core::proto::PROTO_VERSION);
        return Ok(());
    }
    if args.get(1).map(|s| s.as_str()) == Some("token") {
        return token_command(&args[2..]);
    }
    if args.get(1).map(|s| s.as_str()) == Some("--internal-zygote") {
        rargo_core::sandbox::zygote_main(args[2].parse()?);
    }
    let opts = parse_args(&args)?;
    let cgroup_root = if std::env::var_os("RARGO_NO_CGROUPS").is_some() {
        None
    } else {
        rargo_core::cgroup::setup()
    };
    let zygote = Zygote::start(&std::env::current_exe()?)?;

    std::fs::create_dir_all(&opts.state)?;
    let state = opts.state.canonicalize()?;
    let tokens = Arc::new(rargo_core::tokens::TokenStore::new(state.join("tokens.json")));
    let keys = if tokens.path().exists() {
        Keys::with_tokens(tokens.clone())
    } else if opts.listen.ip().is_loopback() {
        Keys::open()
    } else {
        bail!(
            "nobody could connect: make an invitation first (rargod token invite --state {}), or listen on the loopback interface",
            state.display()
        );
    };
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
    if let Ok(c) = std::ffi::CString::new(mnt.as_os_str().as_encoded_bytes()) {
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
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
        let key = link_key(&opts, server)?;
        let result = rt.block_on(rargo_core::remote::worker_main(engine, server, opts.jobs as u32, key));
        session.unmount();
        return result;
    }
    let cargo_home = opts.cargo_home.clone().unwrap_or_else(|| state.join("cargo-home"));
    std::fs::create_dir_all(&cargo_home)?;
    let cargo_home = cargo_home.canonicalize()?;
    let (uid, gid) = (opts.uid.unwrap_or(unsafe { libc::geteuid() }), opts.gid.unwrap_or(unsafe { libc::getegid() }));
    for dir in ["registry", "git"] {
        let p = cargo_home.join(dir);
        if !p.exists() {
            std::fs::create_dir_all(&p)?;
            rargo_core::snapshot::chown(&p, uid, gid);
        }
    }
    let server_env = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect();
    let daemon = Arc::new(Daemon::new(
        Config { state_dir: state.clone(), planner, cargo_home: cargo_home.clone(), uid, gid, server_env },
        engine,
    ));
    eprintln!(
        "rargod: store {} ({}), cargo home {}, passthrough {}, per-action cgroups {}, {} jobs, {} GiB memory budget, actions run as uid {uid}",
        store.root().display(),
        if store.supports_reflink() { "reflinks" } else { "hard links" },
        cargo_home.display(),
        session.caps().passthrough,
        cgroup_root.is_some(),
        opts.jobs,
        opts.mem_kb >> 20,
    );

    {
        let d = daemon.clone();
        let budget = opts.store_budget;
        let cold_after = opts.cold_after_secs;
        std::thread::Builder::new().name("evict".into()).spawn(move || {
            let mut tick = 0u64;
            loop {
                std::thread::sleep(std::time::Duration::from_secs(20));
                let store = &d.engine.store;
                if tick % 6 == 5 {
                    let (n, bytes) = d.collect_garbage(600);
                    if n > 0 {
                        eprintln!("rargod: collected {n} unreferenced blobs, {} MiB", bytes >> 20);
                    }
                }
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
    let keys = Arc::new(keys);
    let listen = opts.listen;
    let public = opts.public.clone();
    let http_state = state.clone();
    let result = rt.block_on(async move {
        let listener = TcpListener::bind(listen).await.with_context(|| format!("listening on {listen}"))?;
        let users: rargo_core::net::UserKeys = {
            let keys = keys.clone();
            Arc::new(move |user| keys.psk(user))
        };
        let endpoint = rargo_core::net::Endpoint::bind_auth(listener.local_addr()?, true, rargo_core::net::Auth::Users(users))
            .await
            .with_context(|| format!("binding UDP {listen}"))?;
        {
            let (d, k, ep) = (daemon.clone(), keys.clone(), endpoint.clone());
            tokio::spawn(async move {
                while let Some(conn) = ep.accept().await {
                    tokio::spawn(rargo_core::serve::udp_connection(d.clone(), conn, ep.clone(), k.clone()));
                }
            });
        }
        eprintln!(
            "rargod: serving clients on {} ({})",
            listener.local_addr()?,
            if keys.is_open() { "no tokens: loopback only, anyone on this machine" } else { "tokens required" }
        );
        let http = Arc::new(rargo_core::http::Http::new(keys.clone(), public, listen.port()).with_state(http_state));
        serve(daemon, listener, keys, http).await
    });
    session.unmount();
    result
}

fn token_command(args: &[String]) -> Result<()> {
    let usage = "usage: rargod token invite --state DIR [--user NAME] [--admin] [--days N] [--uses N] [--token-days N]\n       \
                 rargod token create --state DIR --user NAME [--admin] [--days N]\n       \
                 rargod token list --state DIR\n       \
                 rargod token revoke --state DIR ID-OR-USER\n       \
                 rargod token allow --state DIR GITHUB-LOGIN [--admin]      (may sign in with GitHub through `rargo auth`)\n       \
                 rargod token disallow --state DIR GITHUB-LOGIN             (and revokes the tokens it signed in for)\n       \
                 rargod token github --state DIR --client-id ID --client-secret-file FILE --url https://NAME";
    let mut state: Option<PathBuf> = None;
    let (mut client_id, mut secret_file, mut url) = (None::<String>, None::<PathBuf>, None::<String>);
    let mut rest: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let mut val = || -> Result<&String> {
            i += 1;
            args.get(i).with_context(|| format!("`{arg}` needs a value"))
        };
        match arg {
            "--state" => state = Some(val()?.into()),
            "--client-id" => client_id = Some(val()?.clone()),
            "--client-secret-file" => secret_file = Some(val()?.into()),
            "--url" => url = Some(val()?.trim_end_matches('/').to_string()),
            _ => rest.push(arg.to_string()),
        }
        i += 1;
    }
    let Some(state) = state else { bail!("{usage}") };
    if rest.first().map(String::as_str) == Some("github") {
        let (Some(client_id), Some(file), Some(url)) = (client_id, secret_file, url) else { bail!("{usage}") };
        let client_secret = std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?.trim().to_string();
        if client_id.is_empty() || client_secret.is_empty() || !(url.starts_with("https://") || url.starts_with("http://")) {
            bail!("a client id, a secret and an address starting with https:// are needed");
        }
        rargo_core::tokens::Github { client_id, client_secret, url: url.clone() }.save(&state)?;
        println!("`rargo auth` now signs people in with GitHub. The application's callback address must be {url}/auth/github/callback");
        return Ok(());
    }
    let store = rargo_core::tokens::TokenStore::new(state.join("tokens.json"));
    let asker = rargo_core::tokens::Asker { user: "", admin: true, from: "command line" };
    let (out, err) = rargo_core::tokens::command(&store, &rest, &asker, usage)?;
    print!("{out}");
    eprint!("{err}");
    Ok(())
}

async fn serve(daemon: Arc<Daemon>, listener: TcpListener, keys: Arc<Keys>, http: Arc<rargo_core::http::Http>) -> Result<()> {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    loop {
        tokio::select! {
            _ = term.recv() => break,
            _ = int.recv() => break,
            conn = listener.accept() => {
                let (stream, peer) = match conn {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("rargod: accept failed: {e}");
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let (d, k, h) = (daemon.clone(), keys.clone(), http.clone());
                tokio::spawn(async move {
                    if let Err(e) = rargo_core::serve::connection(d, stream, k, h).await {
                        eprintln!("rargod: connection from {peer}: {e:#}");
                    }
                });
            }
        }
    }
    eprintln!("rargod: shutting down");
    daemon.engine.store.db().flush();
    Ok(())
}

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
