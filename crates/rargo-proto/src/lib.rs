//! The protocol between the `rargo` client and `rargod`: length-prefixed
//! bincode frames over a Unix stream socket.

use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

pub const PROTO_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunRequest {
    pub version: u32,
    /// Arguments after the program name, exactly as cargo would get them.
    pub args: Vec<String>,
    pub cwd: String,
    pub env: Vec<(String, String)>,
    pub stdout_tty: bool,
    pub stderr_tty: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecRequest {
    pub program: String,
    pub args: Vec<String>,
    /// The complete environment for the program.
    pub env: Vec<(String, String)>,
    pub cwd: String,
    /// `(real path, path the program expects)`: the client makes each real
    /// path appear at the expected path before starting the program.
    pub binds: Vec<(String, String)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ServerMsg {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    /// The request is finished.
    Exit(i32),
    /// The client should replace itself with this process.
    Exec(ExecRequest),
}

pub fn write_frame<W: Write, T: Serialize>(w: &mut W, v: &T) -> io::Result<()> {
    let b = bincode::serialize(v).map_err(io::Error::other)?;
    w.write_all(&(b.len() as u32).to_le_bytes())?;
    w.write_all(&b)
}

pub fn read_frame<R: Read, T: for<'a> Deserialize<'a>>(r: &mut R) -> io::Result<Option<T>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let n = u32::from_le_bytes(len) as usize;
    if n > 256 << 20 {
        return Err(io::Error::other("frame too large"));
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    bincode::deserialize(&buf).map(Some).map_err(io::Error::other)
}
