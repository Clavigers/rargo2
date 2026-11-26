use crate::{cache_dir, daemon_socket, find_root, load_config, project};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::Path;

pub fn run() {
    println!("Client    {}", env!("CARGO_PKG_VERSION"));
    let config = load_config();
    let daemon = config
        .as_ref()
        .ok()
        .map(|cfg| UnixStream::connect(daemon_socket(cfg, &cache_dir())).is_ok());
    println!(
        "Daemon    {}",
        match daemon {
            Some(true) => "running",
            Some(false) => "not running for this server and account",
            None => "unavailable",
        }
    );

    let target = std::env::current_dir()
        .ok()
        .as_deref()
        .and_then(find_root)
        .map(|root| root.join("target"));
    let mount = target
        .as_deref()
        .map_or("outside a Rust worktree", |target| {
            let Ok(info) = rargo_net::io::file::read("/proc/self/mountinfo") else {
                return "unavailable";
            };
            match (rargo_mount(&info, target), daemon) {
                (false, _) => "no rargo mount",
                (true, Some(false)) => "mounted without its daemon; run `rargo build` to recover",
                (true, _) => {
                    let target = target.to_path_buf();
                    let (send, recv) = std::sync::mpsc::channel();
                    std::thread::spawn(move || {
                        let _ = send.send(
                            rargo_net::io::file::read_dir(target)
                                .and_then(|mut entries| entries.next().transpose())
                                .map(|_| ()),
                        );
                    });
                    match recv.recv_timeout(std::time::Duration::from_millis(500)) {
                        Ok(Ok(())) => "mounted and responding",
                        _ => "mount is unresponsive; run `rargo build` to recover",
                    }
                }
            }
        });
    println!("Mount     {mount}");
    match rargo_net::io::unavailable() {
        None => println!("io_uring  available"),
        Some(why) => println!("io_uring  unavailable, so rargo cannot build here: {why}"),
    }

    match config {
        Ok(cfg) => {
            if cfg.host == cfg.server {
                println!("Server    {}", cfg.host);
            } else {
                println!("Server    {} ({})", cfg.host, cfg.server);
            }
            let discovery = project::server_status(&cfg.host);
            let version = discovery
                .as_ref()
                .ok()
                .and_then(|d| d.get("version")?.as_str());
            println!("Server version  {}", version.unwrap_or("unavailable"));
            let account = match (&cfg.secret, discovery) {
                (_, Err(_)) => "unavailable (server did not respond)".into(),
                (None, Ok(info)) if info.get("auth").and_then(|a| a.as_bool()) == Some(false) => {
                    "authentication disabled".into()
                }
                (None, Ok(_)) => "not signed in; run `rargo auth`".into(),
                (Some(secret), Ok(_)) => {
                    match project::holder(&cfg.host, &format!("{}.{secret}", cfg.user)) {
                        Ok(Some(account)) => account,
                        Ok(None) => "sign-in expired or revoked; run `rargo auth`".into(),
                        Err(_) => "unavailable (could not verify sign-in)".into(),
                    }
                }
            };
            println!("Account   {account}");
        }
        Err(err) => {
            println!("Server    unavailable ({err})");
            println!("Server version  unavailable");
            println!("Account   unavailable");
        }
    }
}

fn rargo_mount(info: &[u8], target: &Path) -> bool {
    let mut encoded = Vec::new();
    for &byte in target.as_os_str().as_bytes() {
        match byte {
            b' ' => encoded.extend_from_slice(b"\\040"),
            b'\t' => encoded.extend_from_slice(b"\\011"),
            b'\n' => encoded.extend_from_slice(b"\\012"),
            b'\\' => encoded.extend_from_slice(b"\\134"),
            _ => encoded.push(byte),
        }
    }
    info.split(|&b| b == b'\n')
        .filter_map(|line| {
            let mut fields = line.split(|&b| b == b' ');
            let mountpoint = fields.nth(4)?;
            (mountpoint == encoded).then_some(fields)
        })
        .filter_map(|mut fields| {
            fields.find(|field| *field == b"-")?;
            Some((fields.next()?, fields.next()?))
        })
        .last()
        .is_some_and(|(fstype, source)| fstype.starts_with(b"fuse") && source == b"rargo")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_only_the_visible_rargo_mount() {
        let info = b"10 1 0:2 / /tmp/a\\040b/target rw - fuse rargo rw\n";
        let target = Path::new("/tmp/a b/target");
        assert!(rargo_mount(info, target));
        assert!(!rargo_mount(info, Path::new("/tmp/a/target")));
        let covered = [
            info.as_slice(),
            b"11 10 0:3 / /tmp/a\\040b/target rw - tmpfs tmpfs rw\n",
        ]
        .concat();
        assert!(!rargo_mount(&covered, target));
    }
}
