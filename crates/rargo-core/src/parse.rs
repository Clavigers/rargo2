use crate::hash::Hash;
use crate::io;
use dashmap::DashMap;
use rargo_plan::{PARSE_ENV, ParseReq, ParseResp, Script};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;

struct Proc {
    pid: i32,
    stdin: OwnedFd,
    stdout: OwnedFd,
}

impl Proc {
    fn end(self) {
        unsafe { libc::kill(self.pid, libc::SIGKILL) };
        drop((self.stdin, self.stdout));
        let _ = io::wait_exit(self.pid);
    }
}

pub struct Parser {
    exe: PathBuf,
    proc: io::Mutex<Option<Proc>>,
    memo: DashMap<Hash, Arc<ParseResp>>,
}

impl Parser {
    pub fn new(exe: PathBuf) -> Arc<Parser> {
        Arc::new(Parser { exe, proc: io::Mutex::new(None), memo: DashMap::new() })
    }

    fn start(&self) -> std::io::Result<Proc> {
        let mut child = Command::new(&self.exe)
            .env_clear()
            .env(PARSE_ENV, "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = OwnedFd::from(child.stdin.take().expect("stdin is piped"));
        let stdout = OwnedFd::from(child.stdout.take().expect("stdout is piped"));
        Ok(Proc { pid: child.id() as i32, stdin, stdout })
    }

    fn write_all(fd: &OwnedFd, mut bytes: &[u8]) -> std::io::Result<()> {
        while !bytes.is_empty() {
            let n = io::write_at(fd.as_raw_fd(), 0, bytes)?;
            if n == 0 {
                return Err(std::io::ErrorKind::WriteZero.into());
            }
            bytes = &bytes[n..];
        }
        Ok(())
    }

    fn read_exact(fd: &OwnedFd, len: usize) -> std::io::Result<Vec<u8>> {
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            let got = io::read_at(fd.as_raw_fd(), 0, len - out.len())?;
            if got.is_empty() {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            out.extend_from_slice(&got);
        }
        Ok(out)
    }

    fn call(p: &Proc, req: &[u8]) -> std::io::Result<Vec<u8>> {
        let mut framed = (req.len() as u32).to_le_bytes().to_vec();
        framed.extend_from_slice(req);
        Self::write_all(&p.stdin, &framed)?;
        let len = Self::read_exact(&p.stdout, 4)?;
        Self::read_exact(&p.stdout, u32::from_le_bytes(len.try_into().expect("four bytes")) as usize)
    }

    pub fn parse(self: &Arc<Self>, script: &Script, stdout: &[u8]) -> Arc<ParseResp> {
        let req = rargo_plan::encode(&ParseReq { script: script.clone(), stdout: stdout.to_vec() });
        let key = Hash::of(&req);
        if let Some(r) = self.memo.get(&key) {
            return r.clone();
        }
        if !io::on_fiber() {
            let this = Arc::clone(self);
            return io::call(move || this.ask(key, &req));
        }
        self.ask(key, &req)
    }

    fn ask(&self, key: Hash, req: &[u8]) -> Arc<ParseResp> {
        let mut guard = self.proc.lock();
        let mut last_err = String::new();
        for _ in 0..2 {
            if guard.is_none() {
                match self.start() {
                    Ok(p) => *guard = Some(p),
                    Err(e) => {
                        last_err = format!("cannot start {}: {e}", self.exe.display());
                        continue;
                    }
                }
            }
            match Self::call(guard.as_ref().expect("the parser was started"), req) {
                Ok(bytes) => match rargo_plan::decode::<ParseResp>(&bytes) {
                    Ok(resp) => {
                        let resp = Arc::new(resp);
                        self.memo.insert(key, resp.clone());
                        return resp;
                    }
                    Err(e) => last_err = format!("bad reply from the build-script parser: {e}"),
                },
                Err(e) => last_err = format!("build-script parser failed: {e}"),
            }
            if let Some(p) = guard.take() {
                p.end();
            }
        }
        Arc::new(Err(last_err))
    }
}

impl Drop for Parser {
    fn drop(&mut self) {
        let Some(p) = self.proc.get_mut().take() else { return };
        if io::on_fiber() {
            p.end();
        } else {
            unsafe { libc::kill(p.pid, libc::SIGKILL) };
        }
    }
}
