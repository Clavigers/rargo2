use anyhow::{Context, Result, bail};
use rargo_core::build::{Config, Daemon};
use rargo_core::engine::Engine;
use rargo_core::sandbox::Zygote;
use rargo_core::sandboxfs::SandboxFs;
use rargo_core::sched::Scheduler;
use rargo_core::session::Keys;
use rargo_core::store::Store;
use rargo_core::view::Content;
use rargo_core::io;
use std::path::PathBuf;
use std::sync::Arc;

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
    action_timeout_secs: u64,
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
        action_timeout_secs: 3600,
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
            "--action-timeout-secs" => o.action_timeout_secs = val()?.parse()?,
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
    let fuse_fs = fs.clone();

    let sched = Scheduler::new(opts.jobs, opts.mem_kb)?;
    let mut engine = Engine::new(store.clone(), content, fs, mnt, zygote, sched);
    engine.cgroup_root = cgroup_root.clone();
    engine.place = if opts.worker_of.is_some() {
        rargo_core::sandbox::Place::Worker
    } else {
        rargo_core::sandbox::Place::Server
    };
    engine.action_timeout = std::time::Duration::from_secs(opts.action_timeout_secs);
    let engine = Arc::new(engine);
    if let Some(server) = opts.worker_of {
        let key = link_key(&opts, server)?;
        let jobs = opts.jobs as u32;
        let (engine, session, fuse_fs) = (engine.clone(), session.clone(), fuse_fs.clone());
        io::drive(4, move || {
            rargo_core::io::FuseDrive::spawn(&session, &fuse_fs).detach();
            let result = rargo_core::remote::worker_main(engine, server, jobs, key);
            stop(&session, result);
        });
        unreachable!("the worker leaves through stop");
    }
    let cargo_home = opts.cargo_home.clone().unwrap_or_else(|| state.join("cargo-home"));
    let (uid, gid) = (opts.uid.unwrap_or(unsafe { libc::geteuid() }), opts.gid.unwrap_or(unsafe { libc::getegid() }));
    for dir in ["registry", "git"] {
        let p = cargo_home.join(dir);
        if !p.exists() {
            std::fs::create_dir_all(&p)?;
            rargo_core::snapshot::chown(&p, uid, gid);
        }
    }
    let cargo_home = cargo_home.canonicalize()?;
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

    let worker_link = match opts.workers_listen {
        Some(addr) => Some((addr, link_key(&opts, addr)?)),
        None => None,
    };
    let drift_minutes = opts.drift_minutes;
    let listen = opts.listen;
    let public = opts.public.clone();
    let http_state = state.clone();
    let keys = Arc::new(keys);
    io::drive(4, move || {
        rargo_core::io::FuseDrive::spawn(&session, &fuse_fs).detach();
        if let Some((addr, key)) = worker_link {
            match rargo_core::remote::Pool::listen(&daemon.engine, addr, key) {
                Ok(pool) => {
                    let _ = daemon.engine.remote.set(pool.clone());
                    let _ = daemon.pool.set(pool);
                    eprintln!("rargod: accepting workers on {addr}");
                }
                Err(err) => stop(&session, Err(err)),
            }
        }
        if drift_minutes > 0 {
            let d = daemon.clone();
            let every = std::time::Duration::from_secs(drift_minutes * 60);
            io::spawn(move || {
                rargo_core::drift::background(d, every, std::time::Instant::now);
            });
        }
        let listener = match io::TcpListener::bind(listen) {
            Ok(listener) => listener,
            Err(err) => stop(&session, Err(err).context(format!("listening on {listen}"))),
        };
        let users: rargo_core::net::UserKeys = {
            let keys = keys.clone();
            Arc::new(move |user| keys.psk(user))
        };
        let local = match listener.local_addr() {
            Ok(addr) => addr,
            Err(err) => stop(&session, Err(err).context(format!("binding UDP {listen}"))),
        };
        let endpoint = match rargo_core::net::Endpoint::bind_auth(local, true, rargo_core::net::Auth::Users(users)) {
            Ok(endpoint) => endpoint,
            Err(err) => stop(&session, Err(err).context(format!("binding UDP {listen}"))),
        };
        {
            let (d, k, ep) = (daemon.clone(), keys.clone(), endpoint.clone());
            io::spawn(move || {
                while let Some(conn) = ep.accept() {
                    let (d, k, ep) = (d.clone(), k.clone(), ep.clone());
                    io::spawn(move || rargo_core::serve::udp_connection(d, conn, ep, k));
                }
            });
        }
        match listener.local_addr() {
            Ok(addr) => eprintln!(
                "rargod: serving clients on {} ({})",
                addr,
                if keys.is_open() { "no tokens: loopback only, anyone on this machine" } else { "tokens required" }
            ),
            Err(err) => stop(&session, Err(err).context(format!("listening on {listen}"))),
        }
        let http = Arc::new(rargo_core::http::Http::new(keys.clone(), public, listen.port()).with_state(http_state));
        let result = serve(&daemon, &listener, &keys, &http);
        stop(&session, result);
    });
    unreachable!("the server leaves through stop")
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

fn stop(session: &fusewire::Session, result: Result<()>) -> ! {
    session.unmount();
    let code = match result {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("Error: {err:?}");
            1
        }
    };
    std::process::exit(code)
}

fn serve(daemon: &Arc<Daemon>, listener: &io::TcpListener, keys: &Arc<Keys>, http: &Arc<rargo_core::http::Http>) -> Result<()> {
    let mut term = io::signal(io::SignalKind::terminate())?;
    let mut int = io::signal(io::SignalKind::interrupt())?;
    enum Wait<C> {
        Term,
        Int,
        Conn(C),
    }
    loop {
        match io::race_biased(vec![
            Box::new(|| {
                let _ = term.recv();
                Wait::Term
            }),
            Box::new(|| {
                let _ = int.recv();
                Wait::Int
            }),
            Box::new(|| Wait::Conn(listener.accept())),
        ]) {
            Wait::Term | Wait::Int => break,
            Wait::Conn(Ok((stream, peer))) => {
                let (d, k, h) = (daemon.clone(), keys.clone(), http.clone());
                io::spawn(move || {
                    if let Err(e) = accept_client(d, stream, k, h) {
                        eprintln!("rargod: connection from {peer}: {e:#}");
                    }
                });
            }
            Wait::Conn(Err(e)) => {
                eprintln!("rargod: accept failed: {e}");
                io::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
    eprintln!("rargod: shutting down");
    daemon.engine.store.db().flush();
    Ok(())
}

fn accept_client(d: Arc<Daemon>, stream: io::TcpStream, keys: Arc<Keys>, http: Arc<rargo_core::http::Http>) -> Result<()> {
    let _ = stream.set_nodelay(true);
    let mut first = [0u8; 4];
    let n = match io::timeout(std::time::Duration::from_secs(20), || stream.peek(&mut first)) {
        Ok(n) => n?,
        Err(_) => bail!("nothing was sent"),
    };
    if n == 4 && matches!(&first, b"GET " | b"POST" | b"HEAD") {
        return http.serve(stream);
    }
    rargo_core::serve::connection(d, stream, keys, http)
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
