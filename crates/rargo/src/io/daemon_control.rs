use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

pub(super) fn runtime_dir() -> PathBuf {
    let uid = unsafe { libc::getuid() };
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir).join("rargo"),
        None => PathBuf::from(format!("/tmp/rargo-{uid}")),
    }
}

pub(super) fn unmounted_reason() -> &'static str {
    if !Path::new("/dev/fuse").exists() {
        "no rargo mount (/dev/fuse is missing; results are copied into target/)"
    } else if std::env::var_os("RARGO_OUTPUT").is_some_and(|mode| mode == "copy") {
        "no rargo mount (RARGO_OUTPUT=copy; results are copied into target/)"
    } else {
        "no rargo mount"
    }
}

/// Stop daemons from an earlier executable before starting this one.
pub(super) fn retire(current: &Path, cache: &Path) {
    let Ok(client) = fs::metadata("/proc/self/exe") else { return };
    visit_daemons(cache, |socket, process| {
        if socket != current && (client.dev(), client.ino()) != process.executable {
            report_stop(socket, process, true);
        }
    });
}

/// Stop all daemons for this cache, without needing a working server configuration.
pub(super) fn stop_all(cache: &Path) {
    let mut found = false;
    visit_daemons(cache, |socket, process| {
        found = true;
        report_stop(socket, process, false);
    });
    if !found {
        println!("No rargo daemons were running.");
    }
}

struct Process {
    stream: UnixStream,
    fd: OwnedFd,
    pid: libc::pid_t,
    executable: (u64, u64),
}

/// The short fallback socket directory can contain daemons for different caches.
fn visit_daemons(cache: &Path, mut visit: impl FnMut(&Path, Process)) {
    let run = cache.join("run");
    let runtime = runtime_dir();
    for dir in [run.as_path(), runtime.as_path()] {
        let Ok(entries) = fs::read_dir(dir) else { continue };
        for entry in entries.flatten() {
            let socket = entry.path();
            if socket.extension() != Some(OsStr::new("sock")) {
                continue;
            }
            if let Some(process) = daemon_process(&socket, (dir != run).then_some(cache)) {
                visit(&socket, process);
            }
        }
    }
}

/// Check that the socket peer is our daemon and hold its identity across signaling.
fn daemon_process(socket: &Path, cache: Option<&Path>) -> Option<Process> {
    let stream = UnixStream::connect(socket).ok()?;
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of_val(&cred) as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(stream.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&mut cred as *mut libc::ucred).cast(), &mut len)
    };
    if rc != 0 || cred.pid <= 0 || cred.uid != unsafe { libc::getuid() } {
        return None;
    }
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, cred.pid, 0) };
    if fd < 0 {
        return None;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    let proc = PathBuf::from(format!("/proc/{}", cred.pid));
    let args = fs::read(proc.join("cmdline")).ok()?;
    let mut args = args.split(|byte| *byte == 0).skip(1);
    if args.next()? != b"--rargo-daemon" || args.next()? != socket.as_os_str().as_bytes() {
        return None;
    }
    if let Some(cache) = cache {
        let env = fs::read(proc.join("environ")).ok()?;
        if daemon_cache(&env) != cache {
            return None;
        }
    }
    let exe = fs::metadata(proc.join("exe")).ok()?;
    Some(Process { stream, fd, pid: cred.pid, executable: (exe.dev(), exe.ino()) })
}

fn daemon_cache(env: &[u8]) -> PathBuf {
    let get = |key: &[u8]| {
        env.split(|byte| *byte == 0).find_map(|entry| entry.strip_prefix(key).map(OsStr::from_bytes)).map(PathBuf::from)
    };
    get(b"RARGO_CACHE_DIR=").unwrap_or_else(|| {
        get(b"XDG_CACHE_HOME=").unwrap_or_else(|| get(b"HOME=").unwrap_or_else(|| "/".into()).join(".cache")).join("rargo")
    })
}

/// Ask the daemon to unmount and exit, then report whether it did.
fn report_stop(socket: &Path, mut process: Process, retiring: bool) {
    let request = rargo_proto::encode(&rargo_proto::LocalRequest {
        args: Vec::new(),
        kind: "stop".into(),
        cwd: String::new(),
        vars: Vec::new(),
        stdout_tty: false,
        stderr_tty: false,
        mount_ns: String::new(),
    });
    let sent = process.stream.set_write_timeout(Some(std::time::Duration::from_secs(2)))
        .and_then(|()| process.stream.write_all(&(request.len() as u32).to_le_bytes()))
        .and_then(|()| process.stream.write_all(&request));
    let fd = process.fd.as_raw_fd();
    let mut poll = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    if sent.is_ok() && unsafe { libc::poll(&mut poll, 1, 2000) } > 0 {
        if retiring {
            eprintln!("rargo: stopped an older daemon {} ({}).", process.pid, socket.display());
        } else {
            println!("Stopped rargo daemon {} ({}).", process.pid, socket.display());
        }
    } else {
        eprintln!("rargo: daemon {} did not confirm it stopped ({}).", process.pid, socket.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_sockets_are_scoped_to_the_daemons_cache() {
        assert_eq!(daemon_cache(b"HOME=/home/user\0"), Path::new("/home/user/.cache/rargo"));
        assert_eq!(daemon_cache(b"HOME=/home/user\0XDG_CACHE_HOME=/cache\0"), Path::new("/cache/rargo"));
        assert_eq!(daemon_cache(b"XDG_CACHE_HOME=/cache\0RARGO_CACHE_DIR=/private\0"), Path::new("/private"));
    }
}
