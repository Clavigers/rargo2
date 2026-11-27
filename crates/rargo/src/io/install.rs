use crate::{Real, cache_dir, cli, connect_daemon, daemon_replies, die, find_root, home_dir, loader, mount_ns, passthrough};
use rargo_proto::{LocalReply, LocalRequest};
use std::ffi::OsString;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const TARGET: &str = "x86_64-unknown-linux-musl";
const STAGE_PREFIX: &str = ".rargo-install-";

/// Build remotely where supported, verify the complete delivery, then put it on PATH.
pub(super) fn run(world: &Real, raw: &[OsString]) {
    let request = match prepare(raw) {
        Ok(Some(request)) => request,
        Ok(None) => passthrough(raw),
        Err(why) => fallback(raw, &why),
    };
    if let Some(error) = cli::unsupported_toolchain(world, None) {
        drop(request);
        die(&error);
    }
    let files = match try_remote(world, &request) {
        Ok(files) => files,
        Err(why) => {
            drop(request);
            fallback(raw, &why);
        }
    };
    if let Err(error) = install_files(&request.root.join("bin"), &files, request.force) {
        die(&format!("cannot install binaries: {error}"));
    }
    for (name, _) in files {
        eprintln!("  Installed {}", request.root.join("bin").join(name).display());
    }
    eprintln!("rargo: remote installations are not recorded by Cargo; remove the binaries from the install directory to uninstall them");
}

struct Install {
    args: Vec<String>,
    root: PathBuf,
    cwd: PathBuf,
    stage: tempfile::TempDir,
    force: bool,
    _scratch: Option<tempfile::TempDir>,
}

/// Translate the Cargo options that the remote install supports and keep the install root local.
fn prepare(raw: &[OsString]) -> Result<Option<Install>, String> {
    if raw.len() == 1 || raw.iter().any(|arg| matches!(arg.to_str(), Some("--list" | "--help" | "-h"))) {
        return Ok(None);
    }
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        return Err("remote install requires Linux x86-64".into());
    }

    let cwd = std::env::current_dir().and_then(|dir| dir.canonicalize()).map_err(|e| e.to_string())?;
    let mut root = None;
    let mut package_path = None;
    let mut packages = 0;
    let mut force = false;
    let mut args = vec!["install".into(), format!("--target={TARGET}"), "--no-track".into(), "--root=/tmp/rargo-install".into()];
    let mut raw = raw[1..].iter();
    while let Some(arg) = raw.next() {
        let arg = arg.to_str().ok_or("an install argument is not UTF-8")?;
        let (flag, value) = arg.split_once('=').map_or((arg, None), |(flag, value)| (flag, Some(value)));
        let mut value_for = || value.or_else(|| raw.next().and_then(|value| value.to_str())).ok_or_else(|| format!("{flag} needs a value"));
        match flag {
            "--root" => root = Some(cwd.join(value_for()?)),
            "--path" => {
                let path = cwd.join(value_for()?).canonicalize().map_err(|e| e.to_string())?;
                if !find_root(&cwd).is_some_and(|root| path.starts_with(root)) {
                    return Err("install path is outside the synced worktree".into());
                }
                package_path = Some(path);
            }
            "--target" if value_for()? == TARGET => {}
            "--target" => return Err(format!("remote install targets {TARGET}")),
            "--force" | "-f" if value.is_none() => force = true,
            "--no-track" if value.is_none() => {}
            "--version" | "--vers" | "--features" | "-F" | "--bin" | "--example" => args.push(format!("{flag}={}", value_for()?)),
            "--locked" | "--all-features" | "--no-default-features" | "--ignore-rust-version" | "--bins" | "--examples" if value.is_none() => {
                args.push(flag.into());
            }
            _ if !arg.starts_with('-') => {
                packages += 1;
                args.push(arg.into());
            }
            _ => return Err(format!("{arg} is unsupported for remote install")),
        }
    }
    if packages > 1 || (packages == 0 && package_path.is_none()) {
        return Err("remote install takes one crate or a path".into());
    }

    let root = root
        .or_else(|| std::env::var_os("CARGO_INSTALL_ROOT").map(|path| cwd.join(path)))
        .or_else(|| std::env::var_os("CARGO_HOME").map(|path| cwd.join(path)))
        .unwrap_or_else(|| home_dir().join(".cargo"));
    let (cwd, worktree, scratch) = if let Some(path) = package_path {
        args.push(format!("--path={}", path.to_str().ok_or("install path is not UTF-8")?));
        let worktree = find_root(&cwd).ok_or("no worktree for path install")?;
        (cwd, worktree, None)
    } else {
        let scratch = tempfile::Builder::new().prefix("rargo-install-").tempdir().map_err(|e| e.to_string())?;
        let cwd = scratch.path().canonicalize().map_err(|e| e.to_string())?;
        fs::write(cwd.join("Cargo.toml"), "[workspace]\n").map_err(|e| e.to_string())?;
        (cwd.clone(), cwd, Some(scratch))
    };

    let stage = tempfile::Builder::new().prefix(STAGE_PREFIX).rand_bytes(32).tempdir_in(worktree).map_err(|e| e.to_string())?;
    Ok(Some(Install { args, root, cwd, stage, force, _scratch: scratch }))
}

/// Deliver executables as ordinary copied artifacts so the client can inspect their bytes.
fn try_remote(world: &Real, install: &Install) -> Result<Vec<(OsString, Vec<u8>)>, String> {
    let cfg = cli::config(world)?;
    if cfg.secret.is_none() && !cli::is_loopback(&cfg.server) {
        return Err(format!("no build token for {}", cfg.host));
    }
    let cache = cache_dir();
    tell_server_trust(&cache, &cfg.host);
    world.handle_signals();
    let nonce = install.stage.path().file_name().and_then(|s| s.to_str()).and_then(|s| s.strip_prefix(STAGE_PREFIX)).ok_or("invalid stage path")?;
    let mut vars = world.vars.clone();
    vars.retain(|(name, _)| !matches!(name.as_str(), "RARGO_INSTALL_STAGE" | "RARGO_OUTPUT" | "CARGO_INSTALL_ROOT"));
    vars.extend([("RARGO_INSTALL_STAGE".into(), nonce.into()), ("RARGO_OUTPUT".into(), "copy".into())]);
    let req = LocalRequest {
        args: install.args.clone(),
        kind: "build".into(),
        cwd: install.cwd.to_str().ok_or("install directory is not UTF-8")?.into(),
        vars,
        stdout_tty: io::stdout().is_terminal(),
        stderr_tty: io::stderr().is_terminal(),
        mount_ns: mount_ns(),
    };
    let mut status = Err("the server ended the install without a result".into());
    let mut stream = connect_daemon(&cfg, &cache).map_err(|error| error.to_string())?;
    daemon_replies(&mut stream, &req, |reply| match reply {
        LocalReply::Stdout(bytes) => { let _ = io::stdout().write_all(&bytes); }
        LocalReply::Stderr(bytes) => { let _ = io::stderr().write_all(&bytes); }
        LocalReply::Exit(0) => status = Ok(()),
        LocalReply::Exit(code) => status = Err(format!("the remote build exited with status {code}")),
        LocalReply::Unreachable(reason) => status = Err(reason),
        LocalReply::Refused(text) => status = Err(text.trim_end().trim_start_matches("error: ").to_string()),
        LocalReply::Exec(_) => status = Err("the server returned an unexpected run request".into()),
    })?;
    status?;
    read_static_files(install.stage.path())
}

/// Once per server, tell the user who supplies the executable that goes onto PATH.
fn tell_server_trust(cache: &Path, host: &str) {
    let marker = cache.join(format!("install-trust-{}", blake3::hash(host.as_bytes()).to_hex()));
    let created = fs::create_dir_all(cache).and_then(|()| fs::OpenOptions::new().write(true).create_new(true).open(marker));
    if !matches!(created, Err(ref error) if error.kind() == io::ErrorKind::AlreadyExists) {
        eprintln!("rargo: remote install trusts {host} to supply programs for your PATH");
    }
}

/// Read only regular delivered files and verify all of them before replacing anything.
fn read_static_files(stage: &Path) -> Result<Vec<(OsString, Vec<u8>)>, String> {
    let mut files = Vec::new();
    for entry in fs::read_dir(stage).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        if !entry.file_type().map_err(|e| e.to_string())?.is_file() {
            return Err("the server returned a non-file install artifact".into());
        }
        let bytes = fs::read(entry.path()).map_err(|e| e.to_string())?;
        if !loader::static_executable(&bytes) {
            return Err(format!("{} is not a static x86-64 executable", entry.file_name().to_string_lossy()));
        }
        files.push((entry.file_name(), bytes));
    }
    if files.is_empty() {
        return Err("the server returned no install artifacts".into());
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

/// Write complete executable files beside their destinations and publish each atomically.
fn install_files(bin: &Path, files: &[(OsString, Vec<u8>)], force: bool) -> io::Result<()> {
    fs::create_dir_all(bin)?;
    let mut staged = Vec::new();
    for (name, bytes) in files {
        let dest = bin.join(name);
        if !force && dest.symlink_metadata().is_ok() {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("{} already exists (use --force to replace it)", dest.display())));
        }
        let mut file = tempfile::NamedTempFile::new_in(bin)?;
        file.write_all(bytes)?;
        file.as_file().set_permissions(fs::Permissions::from_mode(0o755))?;
        staged.push((dest, file));
    }
    for (dest, file) in staged {
        if force {
            file.persist(dest)?;
        } else {
            file.persist_noclobber(dest)?;
        }
    }
    Ok(())
}

/// Explain why this install uses local Cargo, then hand it the original arguments.
fn fallback(raw: &[OsString], why: &str) -> ! {
    eprintln!("rargo: {}; using local cargo install", why.lines().next().unwrap_or("remote install unavailable"));
    passthrough(raw)
}
