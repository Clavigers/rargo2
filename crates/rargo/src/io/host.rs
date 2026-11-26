//! What only the kernel can say or do about this machine and its processes.

use std::os::unix::process::CommandExt;

/// The machine's name as the kernel reports it.
pub(crate) fn hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    if unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    Some(String::from_utf8_lossy(&buf[..end]).into_owned())
}

/// Makes the command run in a grandchild that leads its own session, so the child the caller waits for exits at once.
pub(crate) fn detach(cmd: &mut std::process::Command) {
    unsafe {
        cmd.pre_exec(|| {
            match libc::fork() {
                -1 => return Err(std::io::Error::last_os_error()),
                0 => {}
                _ => libc::_exit(0),
            }
            libc::setsid();
            Ok(())
        });
    }
}
