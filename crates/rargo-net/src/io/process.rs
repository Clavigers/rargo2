use std::io;
use std::os::fd::IntoRawFd;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, ExitStatus, Output, Stdio};

use super::fs::read_at;
use super::ops::{Event, Op};
use super::task::spawn;
use super::fiber::op_event;

pub fn wait_exit(pid: i32) -> io::Result<i32> {
    match op_event(Op::WaitPid { pid, reap: true })? {
        Event::Exited { status } => Ok(status),
        _ => Err(io::Error::other("waitid")),
    }
}

pub fn wait_exited(pid: i32) -> io::Result<()> {
    match op_event(Op::WaitPid { pid, reap: false })? {
        Event::Exited { .. } => Ok(()),
        _ => Err(io::Error::other("waitid")),
    }
}

struct Fd(i32);

impl Drop for Fd {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

fn read_to_end(fd: Fd) -> io::Result<Vec<u8>> {
    let mut all = Vec::new();
    loop {
        let got = read_at(fd.0, 0, 64 << 10)?;
        if got.is_empty() {
            return Ok(all);
        }
        all.extend_from_slice(&got);
    }
}

pub fn output(cmd: &mut Command) -> io::Result<Output> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let pid = child.id() as i32;
    let out = Fd(child.stdout.take().expect("stdout is piped").into_raw_fd());
    let err = Fd(child.stderr.take().expect("stderr is piped").into_raw_fd());
    drop(child);
    let mut errs = spawn(move || read_to_end(err));
    let stdout = read_to_end(out);
    let stderr = errs.join().map_err(|e| io::Error::other(e.to_string()))?;
    let status = wait_exit(pid)?;
    Ok(Output { status: ExitStatus::from_raw(status), stdout: stdout?, stderr: stderr? })
}
