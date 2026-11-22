//! rargo: the client. Build commands are a request to rargod; everything
//! else is handed to the real cargo unchanged.
//!
//! When this binary is installed under the name `cargo` (the shim), agents
//! that type `cargo build` get rargo without knowing.

use rargo_proto::{ExecRequest, PROTO_VERSION, RunRequest, ServerMsg, read_frame, write_frame};
use std::ffi::{CString, OsString};
use std::io::{IsTerminal, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

/// Subcommands rargod handles.
const ROUTED: &[&str] = &["build", "b", "check", "c", "clippy", "test", "t", "run", "r", "doc", "d", "bench", "rustc", "rustdoc"];

fn socket_path() -> PathBuf {
    match std::env::var_os("RARGO_SOCKET") {
        Some(p) => PathBuf::from(p),
        None => PathBuf::from("/run/rargo/rargod.sock"),
    }
}

/// The first argument that is not an option: the subcommand.
fn subcommand(args: &[String]) -> Option<&str> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--" {
            return None;
        }
        // Global options that take a value.
        if matches!(a.as_str(), "--color" | "--config" | "-C" | "-Z" | "--explain") {
            it.next();
            continue;
        }
        if a.starts_with('-') {
            continue;
        }
        return Some(a);
    }
    None
}

/// Finds the real cargo: the first `cargo` on PATH that is not this program.
fn real_cargo() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("RARGO_REAL_CARGO") {
        return Some(PathBuf::from(p));
    }
    let me = std::env::current_exe().ok().and_then(|p| p.canonicalize().ok());
    for dir in std::env::split_paths(&std::env::var_os("PATH")?) {
        let cand = dir.join("cargo");
        if !cand.is_file() {
            continue;
        }
        if cand.canonicalize().ok() == me {
            continue;
        }
        return Some(cand);
    }
    None
}

fn passthrough(args: &[OsString]) -> ! {
    match real_cargo() {
        Some(cargo) => {
            let err = std::process::Command::new(&cargo).args(args).exec();
            eprintln!("error: cannot run {}: {err}", cargo.display());
        }
        None => eprintln!("error: no `cargo` found on PATH to hand this command to"),
    }
    std::process::exit(101)
}

fn main() {
    let raw: Vec<OsString> = std::env::args_os().skip(1).collect();
    let mut args: Vec<String> = match raw.iter().map(|a| a.clone().into_string()).collect() {
        Ok(a) => a,
        // cargo itself rejects non-UTF-8 arguments for build commands.
        Err(_) => passthrough(&raw),
    };
    let mut env: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect();
    // `rargo +nightly build` selects a toolchain like the rustup proxy does.
    if let Some(tc) = args.first().and_then(|a| a.strip_prefix('+')).map(|s| s.to_string()) {
        args.remove(0);
        env.retain(|(k, _)| k != "RUSTUP_TOOLCHAIN");
        env.push(("RUSTUP_TOOLCHAIN".into(), tc));
    }
    let status = args.first().map(|s| s.as_str()) == Some("--rargo-status");
    if status {
        args = vec!["__status".into()];
    } else {
        match subcommand(&args) {
            Some(cmd) if ROUTED.contains(&cmd) => {}
            _ => passthrough(&raw),
        }
        // Asking for help or version is cargo's business.
        if args.iter().take_while(|a| *a != "--").any(|a| a == "--help" || a == "-h" || a == "--version" || a == "-V") {
            passthrough(&raw);
        }
    }
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: cannot determine the current directory: {e}");
            std::process::exit(101);
        }
    };
    let sock = socket_path();
    let mut stream = match UnixStream::connect(&sock) {
        Ok(s) => s,
        Err(e) => {
            if std::env::var_os("RARGO_FALLBACK").is_some_and(|v| v == "cargo") {
                passthrough(&raw);
            }
            eprintln!("error: cannot reach rargod at {}: {e}", sock.display());
            eprintln!("       (start rargod, set RARGO_SOCKET, or set RARGO_FALLBACK=cargo to use plain cargo)");
            std::process::exit(101);
        }
    };
    let req = RunRequest {
        version: PROTO_VERSION,
        args,
        cwd: cwd.to_string_lossy().into_owned(),
        env,
        stdout_tty: std::io::stdout().is_terminal(),
        stderr_tty: std::io::stderr().is_terminal(),
    };
    if let Err(e) = write_frame(&mut stream, &req) {
        eprintln!("error: lost connection to rargod: {e}");
        std::process::exit(101);
    }
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    loop {
        match read_frame::<_, ServerMsg>(&mut stream) {
            Ok(Some(ServerMsg::Stdout(b))) => {
                let mut o = stdout.lock();
                if o.write_all(&b).and_then(|_| o.flush()).is_err() {
                    // Downstream closed (e.g. `| head`): stop like cargo does.
                    std::process::exit(141);
                }
            }
            Ok(Some(ServerMsg::Stderr(b))) => {
                let _ = stderr.lock().write_all(&b);
            }
            Ok(Some(ServerMsg::Exit(code))) => std::process::exit(code),
            Ok(Some(ServerMsg::Exec(e))) => {
                drop(stream);
                exec(e)
            }
            Ok(None) => {
                eprintln!("error: rargod closed the connection");
                std::process::exit(101);
            }
            Err(e) => {
                eprintln!("error: lost connection to rargod: {e}");
                std::process::exit(101);
            }
        }
    }
}

fn cstr(p: &Path) -> CString {
    CString::new(p.as_os_str().as_bytes()).expect("path without NUL")
}

fn mount(src: Option<&Path>, tgt: &Path, fstype: Option<&str>, flags: libc::c_ulong, data: Option<&str>) -> std::io::Result<()> {
    let src_c = src.map(cstr);
    let tgt_c = cstr(tgt);
    let fs_c = fstype.map(|s| CString::new(s).unwrap());
    let data_c = data.map(|s| CString::new(s).unwrap());
    // SAFETY: all pointers are valid C strings or null.
    let r = unsafe {
        libc::mount(
            src_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            tgt_c.as_ptr(),
            fs_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            flags,
            data_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr() as *const libc::c_void),
        )
    };
    if r != 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

/// Enters a mount namespace in which each `(real, expected)` pair makes the
/// real directory visible at the expected path, without touching the host:
/// a new root is assembled on a tmpfs out of binds of the old root's entries.
fn enter_namespace(binds: &[(String, String)]) -> std::io::Result<()> {
    // SAFETY: plain syscalls; this process is single-threaded here.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let os_err = std::io::Error::last_os_error;
    if unsafe { libc::unshare(libc::CLONE_NEWNS) } != 0 {
        // Not privileged: try a user namespace that maps us to ourselves.
        if unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) } != 0 {
            return Err(os_err());
        }
        std::fs::write("/proc/self/setgroups", "deny")?;
        std::fs::write("/proc/self/uid_map", format!("{uid} {uid} 1"))?;
        std::fs::write("/proc/self/gid_map", format!("{gid} {gid} 1"))?;
    }
    mount(None, Path::new("/"), None, libc::MS_REC | libc::MS_PRIVATE, None)?;
    // The new root is a tmpfs mounted over /tmp (any existing directory
    // would do); the real /tmp is reached through a descriptor.
    let entries: Vec<std::fs::DirEntry> = std::fs::read_dir("/")?.filter_map(|e| e.ok()).collect();
    let tmp_fd = std::fs::File::open("/tmp")?;
    let new_root = Path::new("/tmp");
    mount(Some(Path::new("tmpfs")), new_root, Some("tmpfs"), 0, Some("mode=755"))?;
    use std::os::fd::AsRawFd;
    for e in entries {
        let name = e.file_name();
        let src = Path::new("/").join(&name);
        let dst = new_root.join(&name);
        let Ok(ft) = e.file_type() else { continue };
        if binds.iter().any(|(_, at)| Path::new(at) == src) {
            continue;
        }
        if ft.is_symlink() {
            if let Ok(t) = std::fs::read_link(&src) {
                let _ = std::os::unix::fs::symlink(t, &dst);
            }
        } else if ft.is_dir() {
            std::fs::create_dir(&dst)?;
            let from = if name == "tmp" { PathBuf::from(format!("/proc/self/fd/{}", tmp_fd.as_raw_fd())) } else { src };
            mount(Some(&from), &dst, None, libc::MS_BIND | libc::MS_REC, None)?;
        } else if ft.is_file() {
            std::fs::File::create(&dst)?;
            mount(Some(&src), &dst, None, libc::MS_BIND, None)?;
        }
    }
    for (real, at) in binds {
        let dst = new_root.join(at.trim_start_matches('/'));
        std::fs::create_dir_all(&dst)?;
        // Paths below the old /tmp are only reachable through the descriptor.
        let real_p = match Path::new(real).strip_prefix("/tmp") {
            Ok(rest) => PathBuf::from(format!("/proc/self/fd/{}", tmp_fd.as_raw_fd())).join(rest),
            Err(_) => PathBuf::from(real),
        };
        mount(Some(&real_p), &dst, None, libc::MS_BIND | libc::MS_REC, None)?;
    }
    let root_c = cstr(new_root);
    // SAFETY: valid C strings; pivot_root(".", ".") then detach the old root.
    unsafe {
        if libc::chdir(root_c.as_ptr()) != 0
            || libc::syscall(libc::SYS_pivot_root, c".".as_ptr(), c".".as_ptr()) != 0
            || libc::umount2(c".".as_ptr(), libc::MNT_DETACH) != 0
        {
            return Err(os_err());
        }
    }
    Ok(())
}

/// Replaces `expected` path prefixes with real ones, for running without a
/// namespace.
fn to_real(s: &str, binds: &[(String, String)]) -> String {
    let mut out = String::new();
    for (i, part) in s.split(':').enumerate() {
        if i > 0 {
            out.push(':');
        }
        let mut done = false;
        for (real, at) in binds {
            if part == at {
                out.push_str(real);
                done = true;
                break;
            }
            if let Some(rest) = part.strip_prefix(at.as_str()) {
                if rest.starts_with('/') {
                    out.push_str(real);
                    out.push_str(rest);
                    done = true;
                    break;
                }
            }
        }
        if !done {
            out.push_str(part);
        }
    }
    out
}

/// Becomes the program `cargo run` would have started.
fn exec(mut e: ExecRequest) -> ! {
    // The fork must happen in a child so that this process (the one the
    // shell is waiting for) keeps its place: unshare changes only us.
    if let Err(err) = enter_namespace(&e.binds) {
        if std::env::var_os("RARGO_QUIET_NS").is_none() {
            eprintln!(
                "warning: cannot create a mount namespace ({err}); running without it. \
                 Paths compiled into the program (such as CARGO_MANIFEST_DIR) will point to `{}`.",
                e.binds.first().map(|b| b.1.as_str()).unwrap_or("/w")
            );
        }
        e.program = to_real(&e.program, &e.binds);
        for (_, v) in e.env.iter_mut() {
            *v = to_real(v, &e.binds);
        }
    }
    let mut cmd = std::process::Command::new(&e.program);
    cmd.args(&e.args).env_clear().envs(e.env.iter().map(|(k, v)| (k, v))).current_dir(&e.cwd);
    let err = cmd.exec();
    eprintln!("error: could not execute process `{}` (never executed)\n\nCaused by:\n  {err}", e.program);
    std::process::exit(101)
}
