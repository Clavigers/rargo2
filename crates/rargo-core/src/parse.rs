use crate::hash::Hash;
use dashmap::DashMap;
use parking_lot::Mutex;
use rargo_plan::{PARSE_ENV, ParseReq, ParseResp, Script};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;

struct Proc {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
}

pub struct Parser {
    exe: PathBuf,
    proc: Mutex<Option<Proc>>,
    memo: DashMap<Hash, Arc<ParseResp>>,
}

impl Parser {
    pub fn new(exe: PathBuf) -> Parser {
        Parser { exe, proc: Mutex::new(None), memo: DashMap::new() }
    }

    fn start(&self) -> std::io::Result<Proc> {
        let mut child = Command::new(&self.exe)
            .env_clear()
            .env(PARSE_ENV, "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        Ok(Proc { child, stdin, stdout })
    }

    fn call(p: &mut Proc, req: &[u8]) -> std::io::Result<Vec<u8>> {
        p.stdin.write_all(&(req.len() as u32).to_le_bytes())?;
        p.stdin.write_all(req)?;
        p.stdin.flush()?;
        let mut len = [0u8; 4];
        p.stdout.read_exact(&mut len)?;
        let mut buf = vec![0u8; u32::from_le_bytes(len) as usize];
        p.stdout.read_exact(&mut buf)?;
        Ok(buf)
    }

    pub fn parse(&self, script: &Script, stdout: &[u8]) -> Arc<ParseResp> {
        let req = rargo_plan::encode(&ParseReq { script: script.clone(), stdout: stdout.to_vec() });
        let key = Hash::of(&req);
        if let Some(r) = self.memo.get(&key) {
            return r.clone();
        }
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
            match Self::call(guard.as_mut().unwrap(), &req) {
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
            if let Some(mut p) = guard.take() {
                let _ = p.child.kill();
                let _ = p.child.wait();
            }
        }
        Arc::new(Err(last_err))
    }
}

impl Drop for Parser {
    fn drop(&mut self) {
        if let Some(mut p) = self.proc.lock().take() {
            let _ = p.child.kill();
            let _ = p.child.wait();
        }
    }
}
