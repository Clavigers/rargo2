use serde::{Deserialize, Serialize};

pub mod outline;
pub mod secure;
pub mod tree;
pub mod workspace;

pub const PROTO_VERSION: u32 = 8;

pub const REFUSED: &str = "the server does not know this token, or it was revoked or has expired";

pub const GIT_SIDE: &str = ".rargo-git";

pub const CONTROL: u32 = 0;
pub const KEEPALIVE: u32 = u32::MAX;
const _: () = assert!(CONTROL != KEEPALIVE);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Frame<M> {
    pub stream: u32,
    pub seq: u64,
    pub msg: M,
}

pub struct Reorder<M> {
    per_stream: bool,
    next: std::collections::HashMap<u32, u64>,
    held: std::collections::HashMap<u32, std::collections::BTreeMap<u64, (u32, M)>>,
}

impl<M> Reorder<M> {
    pub fn new(per_stream: bool) -> Reorder<M> {
        Reorder { per_stream, next: Default::default(), held: Default::default() }
    }

    pub fn push(&mut self, f: Frame<M>) -> Vec<(u32, M)> {
        let lane = if self.per_stream { f.stream } else { 0 };
        let next = self.next.entry(lane).or_insert(0);
        let mut out = Vec::new();
        if f.seq < *next {
            return out;
        }
        let held = self.held.entry(lane).or_default();
        held.insert(f.seq, (f.stream, f.msg));
        let before = *next;
        while let Some(m) = held.remove(next) {
            out.push(m);
            *next += 1;
        }
        assert_eq!(*next - before, out.len() as u64);
        assert!(!held.contains_key(next));
        if let Some((&first, _)) = held.first_key_value() {
            assert!(first > *next, "held frames lie beyond a gap");
        }
        if held.is_empty() {
            self.held.remove(&lane);
        }
        out
    }

    pub fn end(&mut self, stream: u32) {
        if self.per_stream {
            self.next.remove(&stream);
            self.held.remove(&stream);
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LocalRequest {
    pub args: Vec<String>,
    pub kind: String,
    pub cwd: String,
    pub vars: Vec<(String, String)>,
    pub stdout_tty: bool,
    pub stderr_tty: bool,
    pub mount_ns: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum LocalReply {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exit(i32),
    Exec(ExecRequest),
    Unreachable(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Offer {
    pub id: u64,
    pub root: Digest,
    pub trees: Vec<Vec<u8>>,
    pub blobs: Vec<Blob>,
    pub ask: bool,
}

pub type Digest = [u8; 32];

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunRequest {
    pub version: u32,
    pub args: Vec<String>,
    pub root: String,
    pub cwd: String,
    pub cargo_home: String,
    pub env: Vec<(String, String)>,
    pub env_checked: Vec<String>,
    pub stdout_tty: bool,
    pub stderr_tty: bool,
    pub worktree: String,
    pub commits: Vec<[u8; 20]>,
    pub tree: Digest,
    pub home_files: Vec<(String, Vec<u8>)>,
    pub who: String,
    pub delivered: u64,
    pub lazy: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecRequest {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, Option<String>)>,
    pub cwd: String,
    pub binds: Vec<(String, String)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Delivery {
    pub path: String,
    pub hash: Digest,
    pub size: u64,
    pub exec: bool,
    pub own: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Write {
    Dir { path: String },
    Remove { path: String },
    Symlink { path: String, target: Vec<u8> },
    File { path: String, hash: Digest, size: u64, exec: bool },
    Text { path: String, text: Option<String> },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ServerMsg {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exit(i32),
    Exec(ExecRequest),
    WantTrees(Vec<Digest>),
    WantBlobs(Vec<Digest>),
    WantEnv(Vec<String>),
    Deliver { id: u64, files: Vec<Delivery>, writes: Vec<Write> },
    Blobs(Vec<Blob>),
    BlobPart { hash: Digest, part: Part, last: bool },
    Delivered { id: u64 },
    DeliveryToken(u64),
    Missing { id: u64, trees: Vec<Digest>, blobs: Vec<Digest> },
    Fetched { id: u64 },
    Recipe { hash: Digest, chunks: Vec<(Digest, u32)>, outline: Vec<outline::Cv>, lazy: Vec<(u64, u64)> },
    Chunks { hash: Digest, chunks: Vec<(u32, Part)> },
    Pong(u64),
    Report(Box<FutureReport>),
    Packed(Vec<u8>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FutureReport {
    pub path: String,
    pub per_package: Vec<(String, String)>,
    pub suggestion: String,
}

pub const REPORT_ID: &str = "\u{1}report-id\u{1}";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Blob {
    pub hash: Digest,
    pub data: Vec<u8>,
    pub packed: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ClientMsg {
    Run(Box<RunRequest>),
    Trees(Vec<Vec<u8>>),
    Blobs(Vec<Blob>),
    BlobPart { hash: Digest, part: Part, last: bool },
    Changed,
    Env(Vec<(String, Option<String>)>),
    Fetch { id: u64, hashes: Vec<Digest> },
    Recipe { id: u64, hash: Digest },
    Chunks { id: u64, hash: Digest, idx: Vec<u32> },
    Offer(Box<Offer>),
    Cancel,
    Ping(u64),
    Packed(Vec<u8>),
    ReportId(u32),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Part {
    pub data: Vec<u8>,
    pub packed: bool,
}

impl Part {
    pub fn pack(bytes: Vec<u8>) -> Part {
        if let Ok(z) = zstd::bulk::compress(&bytes, 1) {
            if z.len() + bytes.len() / 10 < bytes.len() {
                assert!(z.len() < bytes.len());
                return Part { data: z, packed: true };
            }
        }
        Part { data: bytes, packed: false }
    }

    pub fn unpack(self) -> std::io::Result<Vec<u8>> {
        if self.packed { zstd::bulk::decompress(&self.data, 2 * PART_SIZE) } else { Ok(self.data) }
    }
}

pub fn pack(encoded: &[u8]) -> Option<Vec<u8>> {
    if encoded.len() < 2048 {
        return None;
    }
    let z = zstd::bulk::compress(encoded, 3).ok()?;
    let packed = (z.len() + encoded.len() / 8 < encoded.len()).then_some(z);
    if let Some(z) = &packed {
        assert!(z.len() < encoded.len());
    }
    packed
}

pub fn unpack(packed: &[u8]) -> std::io::Result<Vec<u8>> {
    zstd::stream::decode_all(packed)
}

pub const LAZY_SMALL: u64 = 64 << 10;
pub const CHUNKED_FROM: u64 = 1 << 20;

pub const PART_SIZE: usize = 1 << 20;
pub const BATCH_SIZE: usize = 4 << 20;
const _: () = assert!(LAZY_SMALL < CHUNKED_FROM);
const _: () = assert!(PART_SIZE <= BATCH_SIZE);
const _: () = assert!(CHUNKED_FROM as usize >= PART_SIZE);

impl Blob {
    pub fn pack(hash: Digest, bytes: &[u8]) -> Blob {
        if bytes.len() >= 256 {
            if let Ok(z) = zstd::bulk::compress(bytes, 1) {
                if z.len() + 16 < bytes.len() {
                    assert!(z.len() < bytes.len());
                    return Blob { hash, data: z, packed: true };
                }
            }
        }
        Blob { hash, data: bytes.to_vec(), packed: false }
    }

    pub fn unpack(self) -> std::io::Result<Vec<u8>> {
        if self.packed { zstd::stream::decode_all(&self.data[..]) } else { Ok(self.data) }
    }
}

pub fn env_allowed(name: &str) -> bool {
    const EXACT: &[&str] = &[
        "LANG", "LANGUAGE", "TZ", "RUSTUP_TOOLCHAIN", "RUSTFLAGS", "RUSTDOCFLAGS", "TERM", "NO_COLOR", "BROWSER",
        "CLICOLOR", "CLICOLOR_FORCE", "COLORTERM",
    ];
    const PREFIXES: &[&str] = &["CARGO_", "RUSTC", "RUSTDOC", "RUST_", "LC_", "__CARGO", "RARGO_", "NEXTEST_"];
    const DROP: &[&str] = &[
        "CARGO_HOME", "CARGO_MAKEFLAGS", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "RARGO_TOKEN",
        "RARGO_SERVER", "RARGO_CACHE_DIR", "RARGO_REAL_CARGO", "CARGO_REGISTRY_TOKEN",
    ];
    if DROP.contains(&name) || (name.starts_with("CARGO_REGISTRIES_") && name.ends_with("_TOKEN")) {
        return false;
    }
    EXACT.contains(&name) || PREFIXES.iter().any(|p| name.starts_with(p))
}

pub const CACHEDIR_TAG: &str = "Signature: 8a477f597d28d172789f06886806bc55\n# This file is a cache directory tag created by cargo.\n# For information about cache directory tags see https://bford.info/cachedir/\n";
pub const CACHEDIR_SIGNATURE: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55";

pub fn encode<T: Serialize>(v: &T) -> Vec<u8> {
    bincode::serialize(v).expect("protocol messages always serialise")
}

pub fn decode<T: for<'a> Deserialize<'a>>(b: &[u8]) -> std::io::Result<T> {
    bincode::deserialize(b).map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cachedir_tag_carries_its_signature() {
        assert!(CACHEDIR_TAG.as_bytes().starts_with(CACHEDIR_SIGNATURE));
    }

    #[test]
    fn allow_list() {
        assert!(env_allowed("CARGO_TARGET_DIR"));
        assert!(env_allowed("RUSTFLAGS"));
        assert!(env_allowed("RARGO_EXPLAIN"));
        assert!(!env_allowed("PATH"));
        assert!(!env_allowed("HOME"));
        assert!(!env_allowed("CARGO_HOME"));
        assert!(!env_allowed("RARGO_TOKEN"), "a token stays on the client");
        assert!(!env_allowed("CARGO_REGISTRIES_MINE_TOKEN"));
        assert!(!env_allowed("AWS_SECRET_ACCESS_KEY"));
    }

    #[test]
    fn frames_come_out_in_the_senders_order() {
        let f = |stream, seq, msg: &'static str| Frame { stream, seq, msg };
        let mut one: Reorder<&str> = Reorder::new(false);
        assert!(one.push(f(3, 1, "b")).is_empty());
        assert_eq!(one.push(f(0, 0, "a")), vec![(0, "a"), (3, "b")]);
        assert!(one.push(f(0, 0, "again")).is_empty());
        let mut per: Reorder<&str> = Reorder::new(true);
        assert_eq!(per.push(f(1, 0, "x")), vec![(1, "x")]);
        assert!(per.push(f(2, 1, "z")).is_empty());
        assert_eq!(per.push(f(1, 1, "y")), vec![(1, "y")]);
        assert_eq!(per.push(f(2, 0, "w")), vec![(2, "w"), (2, "z")]);
        per.end(2);
        assert_eq!(per.push(f(2, 0, "new stream 2")), vec![(2, "new stream 2")]);
    }

    #[test]
    fn blobs_round_trip() {
        let big = vec![7u8; 100_000];
        let b = Blob::pack([1; 32], &big);
        assert!(b.packed && b.data.len() < 1000);
        assert_eq!(b.unpack().unwrap(), big);
        let small = Blob::pack([2; 32], b"hi");
        assert!(!small.packed);
        assert_eq!(small.unpack().unwrap(), b"hi");
    }
}
