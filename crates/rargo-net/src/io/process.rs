use std::io;
use std::os::fd::IntoRawFd;
use std::os::unix::process::ExitStatusExt;
use std::process::Command as StdCommand;
pub use std::process::{Child, ExitStatus, Output, Stdio};

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

pub fn output(cmd: &mut StdCommand) -> io::Result<Output> {
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

/// This process's id. Every simulated process reports the same one.
pub fn id() -> u32 {
    super::sim::pid().unwrap_or_else(std::process::id)
}

/// Ends the process.
pub fn exit(code: i32) -> ! {
    assert!(!super::sim::active(), "a simulated process exits by returning, not by ending the test's process (exit code {code})");
    std::process::exit(code)
}

/// Whether a process with this id exists. In a simulation the only processes are init and the simulated one.
pub fn alive(pid: u32) -> bool {
    if let Some(simulated) = super::sim::alive(pid) {
        return simulated;
    }
    let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
    r == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

pub fn uid() -> u32 {
    super::sim::uid().unwrap_or_else(|| unsafe { libc::getuid() })
}

pub fn euid() -> u32 {
    super::sim::uid().unwrap_or_else(|| unsafe { libc::geteuid() })
}

pub fn gid() -> u32 {
    super::sim::uid().unwrap_or_else(|| unsafe { libc::getgid() })
}

pub fn egid() -> u32 {
    super::sim::uid().unwrap_or_else(|| unsafe { libc::getegid() })
}

/// A command to run: `std::process::Command`, except that a simulation
/// answers it from memory (`sim::on_command`) instead of starting a process.
pub struct Command {
    real: StdCommand,
    program: std::ffi::OsString,
    args: Vec<std::ffi::OsString>,
    cwd: Option<std::path::PathBuf>,
}

impl Command {
    pub fn new(program: impl AsRef<std::ffi::OsStr>) -> Command {
        Command { real: StdCommand::new(program.as_ref()), program: program.as_ref().to_os_string(), args: Vec::new(), cwd: None }
    }

    pub fn arg(&mut self, arg: impl AsRef<std::ffi::OsStr>) -> &mut Command {
        self.args.push(arg.as_ref().to_os_string());
        self.real.arg(arg);
        self
    }

    pub fn args<S: AsRef<std::ffi::OsStr>>(&mut self, args: impl IntoIterator<Item = S>) -> &mut Command {
        for arg in args {
            self.arg(arg);
        }
        self
    }

    pub fn env(&mut self, key: impl AsRef<std::ffi::OsStr>, value: impl AsRef<std::ffi::OsStr>) -> &mut Command {
        self.real.env(key, value);
        self
    }

    pub fn envs<K: AsRef<std::ffi::OsStr>, V: AsRef<std::ffi::OsStr>>(&mut self, vars: impl IntoIterator<Item = (K, V)>) -> &mut Command {
        self.real.envs(vars);
        self
    }

    pub fn env_remove(&mut self, key: impl AsRef<std::ffi::OsStr>) -> &mut Command {
        self.real.env_remove(key);
        self
    }

    pub fn env_clear(&mut self) -> &mut Command {
        self.real.env_clear();
        self
    }

    pub fn current_dir(&mut self, dir: impl AsRef<std::path::Path>) -> &mut Command {
        self.cwd = Some(dir.as_ref().to_path_buf());
        self.real.current_dir(dir);
        self
    }

    pub fn stdin(&mut self, to: impl Into<Stdio>) -> &mut Command {
        self.real.stdin(to);
        self
    }

    pub fn stdout(&mut self, to: impl Into<Stdio>) -> &mut Command {
        self.real.stdout(to);
        self
    }

    pub fn stderr(&mut self, to: impl Into<Stdio>) -> &mut Command {
        self.real.stderr(to);
        self
    }

    /// The command as `std` holds it, for the few settings this type does not carry (`pre_exec`, `process_group`, `arg0`).
    pub fn std_mut(&mut self) -> &mut StdCommand {
        &mut self.real
    }

    fn simulated(&self) -> Option<io::Result<Output>> {
        let spawned = super::sim::Spawned { program: &self.program, args: &self.args, cwd: self.cwd.as_deref() };
        super::sim::command(&spawned).map(|ran| {
            ran.map(|ran| Output { status: ExitStatus::from_raw((ran.status & 0xff) << 8), stdout: ran.stdout, stderr: ran.stderr })
        })
    }

    /// Runs the command to its end and collects what it printed.
    pub fn output(&mut self) -> io::Result<Output> {
        if let Some(simulated) = self.simulated() {
            return simulated;
        }
        if super::fiber::on_fiber() { output(&mut self.real) } else { self.real.output() }
    }

    /// Runs the command to its end, with its output going where it was pointed.
    pub fn status(&mut self) -> io::Result<ExitStatus> {
        if let Some(simulated) = self.simulated() {
            return simulated.map(|out| out.status);
        }
        if !super::fiber::on_fiber() {
            return self.real.status();
        }
        let child = self.real.spawn()?;
        let pid = child.id() as i32;
        drop(child);
        Ok(ExitStatus::from_raw(wait_exit(pid)?))
    }

    /// Replaces this process with the command and returns only the reason it could not. A simulation has no process to replace.
    pub fn exec(&mut self) -> io::Error {
        if super::sim::active() {
            return io::Error::new(io::ErrorKind::Unsupported, "a simulation cannot replace its process with a command");
        }
        std::os::unix::process::CommandExt::exec(&mut self.real)
    }

    /// Starts the command and returns at once. A simulation has no processes to hand back.
    pub fn spawn(&mut self) -> io::Result<Child> {
        if super::sim::active() {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "a simulation cannot hand back a running process; use output or status"));
        }
        self.real.spawn()
    }
}
