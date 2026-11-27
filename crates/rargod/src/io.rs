//! The system calls rargod makes itself.

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Unmounts whatever is mounted at `target`.
pub fn umount(target: &Path, flags: libc::c_int) -> io::Result<()> {
    let target = CString::new(target.as_os_str().as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a nul"))?;
    if unsafe { libc::umount2(target.as_ptr(), flags) } != 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// The socket systemd named in `NOTIFY_SOCKET`, to tell it the server is ready
/// and still alive (`sd_notify`): one datagram per message, sent without
/// waiting, so a full socket loses a message rather than stalling the loop.
pub struct Notifier {
    sock: std::os::unix::net::UnixDatagram,
    to: std::os::unix::net::SocketAddr,
}

impl Notifier {
    /// The notifier for the `NOTIFY_SOCKET` value `name`: a path, or with a
    /// leading `@` an abstract socket.
    pub fn open(name: &std::ffi::OsStr) -> io::Result<Notifier> {
        use std::os::linux::net::SocketAddrExt as _;
        let bytes = name.as_bytes();
        let to = match bytes.strip_prefix(b"@") {
            Some(abstract_name) => std::os::unix::net::SocketAddr::from_abstract_name(abstract_name)?,
            None => std::os::unix::net::SocketAddr::from_pathname(Path::new(name))?,
        };
        let sock = std::os::unix::net::UnixDatagram::unbound()?;
        sock.set_nonblocking(true)?;
        Ok(Notifier { sock, to })
    }

    pub fn send(&self, message: &str) -> io::Result<()> {
        assert!(!message.is_empty());
        let n = self.sock.send_to_addr(message.as_bytes(), &self.to)?;
        assert_eq!(n, message.len(), "a datagram is sent whole");
        Ok(())
    }
}

/// The other end of a [`Notifier`], as systemd holds it: a datagram socket
/// bound to `name`, read without waiting. rargod itself only sends; the tests
/// listen here to see what it tells systemd.
#[allow(dead_code)]
pub struct Inbox(std::os::unix::net::UnixDatagram);

#[allow(dead_code)]
impl Inbox {
    pub fn bind_abstract(name: &str) -> io::Result<Inbox> {
        use std::os::linux::net::SocketAddrExt as _;
        let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())?;
        let sock = std::os::unix::net::UnixDatagram::bind_addr(&addr)?;
        sock.set_nonblocking(true)?;
        Ok(Inbox(sock))
    }

    /// The messages that arrived since the last call, oldest first.
    pub fn drain(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut buf = [0u8; 256];
        while let Ok(n) = self.0.recv(&mut buf) {
            out.push(String::from_utf8_lossy(&buf[..n]).into_owned());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{Inbox, Notifier};
    use rargo_core::io;
    use std::time::Duration;

    #[test]
    fn the_watchdog_is_pinged_by_the_loop_and_stops_while_the_loop_is_blocked() {
        if io::unavailable().is_some() {
            return;
        }
        let name = format!("rargod-test-notify-{}", io::process::id());
        let systemd = Inbox::bind_abstract(&name).unwrap();
        let notifier = Notifier::open(std::ffi::OsStr::new(&format!("@{name}"))).unwrap();
        let every = Duration::from_millis(10);
        let (before, blocked, after) = io::run(1, move || {
            crate::watchdog(notifier, Some(every));
            io::sleep(every * 10);
            let before = systemd.drain();
            io::on_this_thread(|| io::sleep(every * 15));
            let blocked = systemd.drain();
            io::sleep(every * 10);
            (before, blocked, systemd.drain())
        });
        assert_eq!(before.first().map(String::as_str), Some("READY=1"), "{before:?}");
        assert!(before[1..].iter().all(|m| m == "WATCHDOG=1"), "{before:?}");
        assert!(before.len() >= 3, "the loop pings while it runs: {before:?}");
        assert!(blocked.is_empty(), "a blocked loop sends nothing: {blocked:?}");
        assert!(!after.is_empty() && after.iter().all(|m| m == "WATCHDOG=1"), "the pings resume: {after:?}");
    }

    #[test]
    fn without_a_watchdog_timeout_only_ready_is_sent() {
        if io::unavailable().is_some() {
            return;
        }
        let name = format!("rargod-test-ready-{}", io::process::id());
        let systemd = Inbox::bind_abstract(&name).unwrap();
        let notifier = Notifier::open(std::ffi::OsStr::new(&format!("@{name}"))).unwrap();
        let seen = io::run(1, move || {
            crate::watchdog(notifier, None);
            io::sleep(Duration::from_millis(30));
            systemd.drain()
        });
        assert_eq!(seen, ["READY=1"]);
    }

    #[test]
    fn a_path_opens_and_an_empty_abstract_name_does_too() {
        assert!(Notifier::open(std::ffi::OsStr::new("/run/systemd/notify")).is_ok());
        assert!(Notifier::open(std::ffi::OsStr::new("@")).is_ok());
    }
}
