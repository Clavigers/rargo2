use serde::{Deserialize, Serialize};

pub mod outline;
pub mod secure;
pub mod tree;
pub mod waits;
pub mod workspace;

pub const PROTO_VERSION: u32 = 10;

const SERVER_IN_CARGO_TOML_UNTIL: u32 = 7;

pub fn protocol_mismatch(client: u32) -> String {
    let server = env!("CARGO_PKG_VERSION");
    let install = format!("cargo install rargo --version {server}");
    let (age, verb) = if client < PROTO_VERSION { ("older", "update it") } else { ("newer", "install the matching version") };
    let rename = if client <= SERVER_IN_CARGO_TOML_UNTIL {
        "; then name the server once more with `rargo @HOST`, as you did before: the new rargo keeps it in your own \
         configuration, not in Cargo.toml"
    } else {
        ""
    };
    format!("this rargo is {age} than the build server, which runs rargo {server}; {verb} with: {install}{rename}")
}

/// An older hello can omit the version entirely.
pub fn protocol_unmarked() -> String {
    let version = env!("CARGO_PKG_VERSION");
    format!("this client did not name its protocol; the build server runs rargo {version}; install the matching client with: cargo install rargo --version {version}")
}


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
    /// The server refused this machine's token or its lack of one; the text says what to do.
    Refused(String),
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
    Deliver { id: u64, files: Vec<Delivery>, writes: Vec<Write>, targets: Vec<String> },
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
        if let Ok(z) = zstd::bulk::compress(&bytes, 1)
            && z.len() + bytes.len() / 10 < bytes.len()
        {
            assert!(z.len() < bytes.len());
            return Part { data: z, packed: true };
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
        if bytes.len() >= 256
            && let Ok(z) = zstd::bulk::compress(bytes, 1)
            && z.len() + 16 < bytes.len()
        {
            assert!(z.len() < bytes.len());
            return Blob { hash, data: z, packed: true };
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
    use expect_test::{expect, Expect};
    use std::fmt::Write as _;

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

    enum ReorderEvent {
        Receive(u32, u64, &'static str),
        End(u32),
    }

    fn check_reorder(per_stream: bool, events: impl IntoIterator<Item = ReorderEvent>, expected: Expect) {
        let mut reorder = Reorder::new(per_stream);
        let mut output = String::new();
        for event in events {
            match event {
                ReorderEvent::Receive(stream, seq, msg) => {
                    let released = reorder.push(Frame { stream, seq, msg });
                    writeln!(output, "receive {stream}:{seq} {msg:?} -> {released:?}").unwrap();
                }
                ReorderEvent::End(stream) => {
                    reorder.end(stream);
                    writeln!(output, "end {stream}").unwrap();
                }
            }
        }
        expected.assert_eq(&output);
    }

    #[test]
    fn frames_share_a_sequence_in_global_order() {
        use ReorderEvent::Receive;

        check_reorder(
            false,
            [Receive(3, 1, "b"), Receive(0, 0, "a"), Receive(0, 0, "again")],
            expect![[r#"
                receive 3:1 "b" -> []
                receive 0:0 "a" -> [(0, "a"), (3, "b")]
                receive 0:0 "again" -> []
            "#]],
        );
    }

    #[test]
    fn streams_advance_and_reset_independently() {
        use ReorderEvent::{End, Receive};

        check_reorder(
            true,
            [
                Receive(1, 0, "x"),
                Receive(2, 1, "z"),
                Receive(1, 1, "y"),
                Receive(2, 0, "w"),
                Receive(2, 3, "discard on end"),
                End(2),
                Receive(2, 0, "new stream 2"),
                Receive(2, 1, "next"),
                Receive(2, 2, "fills the gap"),
                Receive(1, 2, "stream 1 continues"),
            ],
            expect![[r#"
                receive 1:0 "x" -> [(1, "x")]
                receive 2:1 "z" -> []
                receive 1:1 "y" -> [(1, "y")]
                receive 2:0 "w" -> [(2, "w"), (2, "z")]
                receive 2:3 "discard on end" -> []
                end 2
                receive 2:0 "new stream 2" -> [(2, "new stream 2")]
                receive 2:1 "next" -> [(2, "next")]
                receive 2:2 "fills the gap" -> [(2, "fills the gap")]
                receive 1:2 "stream 1 continues" -> [(1, "stream 1 continues")]
            "#]],
        );
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
