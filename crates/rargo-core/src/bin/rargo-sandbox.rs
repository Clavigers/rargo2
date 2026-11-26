use rargo_core::buildview::{read_host_list, read_host_lookup};
use rargo_core::sandbox::{HostLayout, NetMode, Place, SetupSpec, Zygote};
use std::collections::BTreeMap;
use rargo_core::sandboxfs::{ActionFsConfig, SandboxFs, UpperKind};
use rargo_core::view::*;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::Arc;

struct HostView {
    root: HostRegion,
}

impl View for HostView {
    fn lookup(&self, dir: &[u8], name: &[u8]) -> Looked {
        read_host_lookup(&self.root.real_path(dir, name))
    }
    fn list(&self, dir: &[u8]) -> std::io::Result<Listing> {
        read_host_list(&self.root.real_path(dir, b""))
    }
}

struct Options {
    state: PathBuf,
    passthrough: bool,
    quiet: bool,
    cmd: Vec<Vec<u8>>,
}

fn options(args: &[String]) -> anyhow::Result<Options> {
    let mut state = PathBuf::from("/root/rargo-work/sandbox-tool");
    let mut passthrough = true;
    let mut quiet = false;
    let mut i = 1;
    while i < args.len() && args[i] != "--" {
        match args[i].as_str() {
            "--state" => {
                i += 1;
                state = PathBuf::from(args.get(i).ok_or_else(|| anyhow::anyhow!("--state requires a directory"))?);
            }
            "--no-passthrough" => passthrough = false,
            "--quiet" => quiet = true,
            other => anyhow::bail!("unknown option {other}"),
        }
        i += 1;
    }
    let cmd: Vec<Vec<u8>> = args[(i + 1).min(args.len())..].iter().map(|s| s.clone().into_bytes()).collect();
    anyhow::ensure!(!cmd.is_empty(), "usage: rargo-sandbox [options] -- CMD ARGS...");

    Ok(Options { state, passthrough, quiet, cmd })
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(|s| s.as_str()) == Some("--internal-zygote") {
        let fd = args.get(2).ok_or_else(|| anyhow::anyhow!("--internal-zygote requires a descriptor"))?.parse()?;
        rargo_core::sandbox::zygote_main(fd);
    }
    rargo_net::io::run(1, move || sandbox(args))
}

fn sandbox(args: Vec<String>) -> anyhow::Result<()> {
    let Options { state, passthrough, quiet, cmd } = options(&args)?;
    let zygote = Zygote::start(&std::env::current_exe()?)?;
    rargo_core::io::file::create_dir_all(&state)?;
    let db = Arc::new(rargo_core::db::Db::open(&state.join("db.redb"))?);
    let store = Arc::new(rargo_core::store::Store::open(&state.join("store"), db)?);
    let content = Arc::new(Content::new(store.clone()));
    let fs = Arc::new(SandboxFs::new(content));
    let mnt = state.join("mnt");
    rargo_core::io::file::create_dir_all(&mnt)?;
    let session = Arc::new(fusewire::Session::mount_uring(
        &mnt,
        &fusewire::MountOptions { fsname: "rargo".into(), passthrough, ..Default::default() },
    )?);
    fs.attach(session.clone());
    let _fuse = rargo_core::io::FuseDrive::spawn(&session, &fs)?;
    eprintln!("[rargo-sandbox] passthrough: {}", session.caps().passthrough);

    let scratch = store.tmp_dir("tool-action")?;
    let action = fs.create_action(ActionFsConfig {
        view: Arc::new(HostView { root: HostRegion::new("/") }),
        uid: rargo_core::io::process::uid(),
        gid: rargo_core::io::process::gid(),
        scratch,
        mountpoints: vec![b"proc".to_vec(), b"dev".to_vec(), b"sys".to_vec(), b"tmp".to_vec()],
        size_blind: false,
    });
    let env: BTreeMap<Vec<u8>, Vec<u8>> = rargo_core::io::env::vars_os()
        .into_iter()
        .map(|(k, v)| (k.into_encoded_bytes(), v.into_encoded_bytes()))
        .collect();
    let cwd = std::env::current_dir()?.into_os_string().into_encoded_bytes();
    let uid = rargo_core::io::process::uid();
    let gid = rargo_core::io::process::gid();
    let spec = SetupSpec { argv: cmd, env, cwd, net: NetMode::Loopback };
    let host = HostLayout {
        place: Place::Server,
        root: mnt.join(SandboxFs::dir_name(action.seq)),
        seq: action.seq,
        cgroup_root: None,
        uid,
        gid,
        caller: (uid, gid),
        flake: false,
        binds: vec![],
    };
    let plan = rargo_core::sandbox::plan(&spec, &host)?;
    let out: OwnedFd = std::io::stdout().as_fd().try_clone_to_owned()?;
    let err: OwnedFd = std::io::stderr().as_fd().try_clone_to_owned()?;
    use std::os::fd::AsFd;
    let start = rargo_core::io::now();
    let running = zygote.spawn(plan, None, out, err, &[])?;
    let info = rargo_net::io::call(|| running.done.expect("the exit is waited once").recv())
        .ok_or_else(|| anyhow::anyhow!("the sandbox helper process died"))?;
    let wall = rargo_core::io::now().saturating_duration_since(start);
    let fin = fs.finish_action(&action);
    eprintln!(
        "[rargo-sandbox] status={:?} signal={:?} wall={:.3}s user={:.3}s sys={:.3}s maxrss={}MB",
        info.code(),
        info.signal(),
        wall.as_secs_f64(),
        info.user_us as f64 / 1e6,
        info.sys_us as f64 / 1e6,
        info.max_rss_kb / 1024
    );
    let st = fs.stats();
    use std::sync::atomic::Ordering::Relaxed;
    eprintln!(
        "[rargo-sandbox] fuse: lookups={} opens={} reads={} writes={}; observations={} upper={} tainted={}",
        st.lookups.load(Relaxed),
        st.opens.load(Relaxed),
        st.reads.load(Relaxed),
        st.writes.load(Relaxed),
        fin.obs.len(),
        fin.upper.len(),
        fin.tainted
    );
    if !quiet {
        for (p, o) in &fin.obs {
            println!("obs {} {:?}", show(p), o);
        }
        for u in &fin.upper {
            let k = match &u.kind {
                UpperKind::Dir => "dir".to_string(),
                UpperKind::File { path, exec } => {
                    format!("file {} bytes exec={exec}", rargo_core::io::file::metadata(path).map(|m| m.len()).unwrap_or(0))
                }
                UpperKind::LowerFile { .. } => "lower-file".into(),
                UpperKind::Symlink(t) => format!("symlink -> {}", show(t)),
                UpperKind::Whiteout => "whiteout".into(),
            };
            println!("out {} {k}", show(&u.path));
        }
    }
    session.unmount();
    rargo_core::io::process::exit(info.code().unwrap_or(1));
}
