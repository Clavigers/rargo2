//! The protocol between the `rargo` client and the build server `rargod`.
//!
//! The client is a file and environment syncer: it describes its worktree as
//! a content-addressed tree, uploads what the server lacks, names the
//! environment, and then receives output, requested artifacts and files the
//! build wrote into the worktree. Everything else happens on the server.
//!
//! A client machine runs one daemon per server, and the daemon holds one
//! long-lived link to it (the reliable-UDP transport of `rargo-net`, or TCP
//! where UDP does not get through), encrypted and authenticated with a
//! pre-shared key per user (Noise `NNpsk0`); see [`secure`]. The link
//! carries [`Frame`]s of bincode messages ([`ClientMsg`], [`ServerMsg`]):
//! every command is a stream of its own, and stream 0 is the daemon's own
//! conversation with the server (worktree changes pushed as they happen,
//! contents fetched, keepalives). The `rargo` command itself talks to the
//! daemon over a Unix socket ([`LocalRequest`], [`LocalReply`]).

use serde::{Deserialize, Serialize};

pub mod secure;
pub mod tree;
pub mod workspace;

pub const PROTO_VERSION: u32 = 7;

/// Where the stand-in for a linked worktree's git directory is shown: the
/// worktree's `.git` is a file there, as on the client, and names this
/// directory beside it.
pub const GIT_SIDE: &str = ".rargo-git";

/// The stream of a link that belongs to no command.
pub const CONTROL: u32 = 0;
/// Frames of this stream are keepalives: outside every order, answered at
/// once, so that neither end mistakes a busy link for a dead one.
pub const KEEPALIVE: u32 = u32::MAX;

/// One message on a link, with the stream it belongs to and its place in
/// the sender's order (the UDP transport delivers messages in arrival
/// order). From client to server there is one order over all streams, so
/// that a request is never looked at before the worktree changes sent ahead
/// of it; from server to client each stream has its own, so that one
/// command's bulk does not hold up another's output.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Frame<M> {
    pub stream: u32,
    pub seq: u64,
    pub msg: M,
}

/// Puts frames back into the sender's order.
pub struct Reorder<M> {
    per_stream: bool,
    next: std::collections::HashMap<u32, u64>,
    held: std::collections::HashMap<u32, std::collections::BTreeMap<u64, (u32, M)>>,
}

impl<M> Reorder<M> {
    /// `per_stream`: each stream is ordered by itself; otherwise all
    /// frames share one order.
    pub fn new(per_stream: bool) -> Reorder<M> {
        Reorder { per_stream, next: Default::default(), held: Default::default() }
    }

    /// Takes a frame and returns whatever is now next in order.
    pub fn push(&mut self, f: Frame<M>) -> Vec<(u32, M)> {
        let lane = if self.per_stream { f.stream } else { 0 };
        let next = self.next.entry(lane).or_insert(0);
        let mut out = Vec::new();
        if f.seq < *next {
            return out;
        }
        let held = self.held.entry(lane).or_default();
        held.insert(f.seq, (f.stream, f.msg));
        while let Some(m) = held.remove(next) {
            out.push(m);
            *next += 1;
        }
        if held.is_empty() {
            self.held.remove(&lane);
        }
        out
    }

    /// Forgets a finished stream.
    pub fn end(&mut self, stream: u32) {
        if self.per_stream {
            self.next.remove(&stream);
            self.held.remove(&stream);
        }
    }
}

/// What the `rargo` command asks of the daemon on its machine.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LocalRequest {
    /// Arguments as the server should get them.
    pub args: Vec<String>,
    /// `"build"`, `"admin"` (a request about the server; no worktree) or
    /// `"drift"`.
    pub kind: String,
    pub cwd: String,
    /// The command's whole environment (UTF-8 variables). The daemon sends
    /// the server what the allow-list and the policy let through.
    pub vars: Vec<(String, String)>,
    pub stdout_tty: bool,
    pub stderr_tty: bool,
    /// The command's mount namespace (the link `/proc/self/ns/mnt`). What
    /// the daemon mounts is only seen by commands in its own.
    pub mount_ns: String,
}

/// What the daemon tells the `rargo` command.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum LocalReply {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exit(i32),
    Exec(ExecRequest),
    /// The server cannot be reached (the command may fall back to cargo).
    Unreachable(String),
}

/// Worktree content pushed to the server without being asked for: after
/// an edit, the changed files and the directory manifests on their paths.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Offer {
    pub id: u64,
    /// Digest of the root tree this brings the server up to.
    pub root: Digest,
    /// Encoded [`tree::Tree`]s.
    pub trees: Vec<Vec<u8>>,
    pub blobs: Vec<Blob>,
    /// Answer with [`ServerMsg::Missing`]: what of `root` is still not
    /// there (a worktree seen for the first time, whose contents another
    /// client may have uploaded already).
    pub ask: bool,
}

pub type Digest = [u8; 32];

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunRequest {
    pub version: u32,
    /// Arguments after the program name, exactly as cargo would get them.
    pub args: Vec<String>,
    /// The worktree root on the client. Only used to show and accept paths
    /// the way the user knows them; the server never opens it.
    pub root: String,
    /// The client's current directory (inside `root`).
    pub cwd: String,
    /// The client's cargo home, for the same purpose as `root`.
    pub cargo_home: String,
    /// Variables the client is allowed to pass (see [`env_allowed`]), plus
    /// those the repository's policy file names and those in `env_checked`.
    pub env: Vec<(String, String)>,
    /// Names outside the allow-list the client looked up because a server
    /// asked for them before. A name listed here and missing from `env` is
    /// unset on the client.
    pub env_checked: Vec<String>,
    pub stdout_tty: bool,
    pub stderr_tty: bool,
    /// Stable identity of this worktree (machine and path).
    pub worktree: String,
    /// HEAD commit and its recent first-parent ancestors, when the worktree
    /// is a git checkout. A hint for choosing incremental state; never part
    /// of correctness.
    pub commits: Vec<[u8; 20]>,
    /// Digest of the worktree's root [`tree::Tree`].
    pub tree: Digest,
    /// Cargo configuration from the client's cargo home (`config.toml`,
    /// `credentials.toml`): `(file name, content)`.
    pub home_files: Vec<(String, Vec<u8>)>,
    /// `user@host`, for the server's log.
    pub who: String,
    /// The token of the last delivery this worktree placed completely
    /// (see [`ServerMsg::DeliveryToken`]); 0 if none.
    pub delivered: u64,
    /// The client shows results in a virtual output directory: it wants to
    /// be told of every artifact as soon as it exists, and fetches contents
    /// itself when something opens them. Only small files are sent along.
    pub lazy: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecRequest {
    pub program: String,
    pub args: Vec<String>,
    /// Changes to the client's own environment: `Some` sets, `None` unsets.
    pub env: Vec<(String, Option<String>)>,
    pub cwd: String,
    /// `(real path, path the program expects)`: the client makes each real
    /// path appear at the expected path before starting the program.
    pub binds: Vec<(String, String)>,
}

/// A file the server offers to place on the client.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Delivery {
    /// Absolute path on the client.
    pub path: String,
    pub hash: Digest,
    pub size: u64,
    pub exec: bool,
    /// The file will be rewritten in place by other tools (an executable at
    /// the path rustc writes it to, generated documentation), so it must be
    /// a file of its own. Otherwise the client may share one copy between
    /// worktrees.
    pub own: bool,
}

/// A change to the client's worktree or target directory that is not a
/// content-addressed artifact.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Write {
    Dir { path: String },
    Remove { path: String },
    Symlink { path: String, target: Vec<u8> },
    /// A file whose content follows as a blob unless the client has it.
    File { path: String, hash: Digest, size: u64, exec: bool },
    /// A small generated file (`.d` files, `CACHEDIR.TAG`). `None` removes.
    Text { path: String, text: Option<String> },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ServerMsg {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    /// The request is finished.
    Exit(i32),
    /// The client should replace itself with this process.
    Exec(ExecRequest),
    /// Directory manifests the server does not have.
    WantTrees(Vec<Digest>),
    /// File contents the server does not have.
    WantBlobs(Vec<Digest>),
    /// Environment variables an action declared it reads.
    WantEnv(Vec<String>),
    /// Files to place. The contents the server believes the client lacks
    /// follow as blobs, then [`ServerMsg::Delivered`]; nothing is asked.
    /// Whatever the client then still lacks it fetches
    /// ([`ClientMsg::Fetch`]).
    Deliver { id: u64, files: Vec<Delivery>, writes: Vec<Write> },
    Blobs(Vec<Blob>),
    BlobPart { hash: Digest, part: Part, last: bool },
    /// Everything the server meant to send for delivery `id` has been sent.
    Delivered { id: u64 },
    /// Names everything this command delivered. A client that placed it
    /// all passes the token with its next request from the same worktree,
    /// and the server then knows what that worktree already holds.
    DeliveryToken(u64),
    /// Answer to an [`Offer`] with `ask`: what the server still lacks.
    Missing { id: u64, trees: Vec<Digest>, blobs: Vec<Digest> },
    /// Everything asked for in fetch `id` has been sent.
    Fetched { id: u64 },
    /// How a content is cut into chunks: `(hash, length)` in order. Empty
    /// if the server does not have the content.
    Recipe { hash: Digest, chunks: Vec<(Digest, u32)> },
    /// Chunks of a content, by their place in its recipe.
    Chunks { hash: Digest, chunks: Vec<(u32, Part)> },
    Pong(u64),
    /// A future-incompatibility report to add to the report file in the
    /// client's target directory (cargo's `.future-incompat-report.json`).
    /// The client answers with [`ClientMsg::ReportId`].
    Report(Box<FutureReport>),
    /// Another message, encoded and zstd-compressed (see [`pack`]).
    Packed(Vec<u8>),
}

/// See [`ServerMsg::Report`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FutureReport {
    /// The report file, as a path on the client.
    pub path: String,
    /// Package (`name@version`) to the rendered warnings, sorted by package.
    pub per_package: Vec<(String, String)>,
    /// cargo's suggestions for the report. [`REPORT_ID`] stands where the
    /// report's number belongs, which only the client can know.
    pub suggestion: String,
}

/// Placeholder for the report number in [`FutureReport::suggestion`].
pub const REPORT_ID: &str = "\u{1}report-id\u{1}";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Blob {
    pub hash: Digest,
    /// zstd-compressed when `packed`.
    pub data: Vec<u8>,
    pub packed: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ClientMsg {
    Run(Box<RunRequest>),
    /// Encoded [`tree::Tree`]s, in answer to `WantTrees`.
    Trees(Vec<Vec<u8>>),
    Blobs(Vec<Blob>),
    BlobPart { hash: Digest, part: Part, last: bool },
    /// A wanted blob cannot be sent (the file changed or vanished since the
    /// scan). The client will start over.
    Changed,
    Env(Vec<(String, Option<String>)>),
    /// Contents the client lacks, to be sent on the control stream and
    /// followed by [`ServerMsg::Fetched`].
    Fetch { id: u64, hashes: Vec<Digest> },
    /// Asks how a content is cut into chunks (a stream of its own,
    /// answered with [`ServerMsg::Recipe`] and [`ServerMsg::Fetched`]).
    Recipe { id: u64, hash: Digest },
    /// Asks for chunks of a content by their place in its recipe (a stream
    /// of its own, answered with [`ServerMsg::Chunks`] and
    /// [`ServerMsg::Fetched`]).
    Chunks { id: u64, hash: Digest, idx: Vec<u32> },
    Offer(Box<Offer>),
    /// The command of this stream is not wanted any more.
    Cancel,
    Ping(u64),
    /// Another message, encoded and zstd-compressed (see [`pack`]).
    Packed(Vec<u8>),
    /// The number under which a [`ServerMsg::Report`] is in the report file.
    ReportId(u32),
}

/// One piece of a large file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Part {
    /// zstd-compressed when `packed`.
    pub data: Vec<u8>,
    pub packed: bool,
}

impl Part {
    pub fn pack(bytes: Vec<u8>) -> Part {
        if let Ok(z) = zstd::bulk::compress(&bytes, 1) {
            // Worth it only when it saves a tenth or more.
            if z.len() + bytes.len() / 10 < bytes.len() {
                return Part { data: z, packed: true };
            }
        }
        Part { data: bytes, packed: false }
    }

    pub fn unpack(self) -> std::io::Result<Vec<u8>> {
        if self.packed { zstd::bulk::decompress(&self.data, 2 * PART_SIZE) } else { Ok(self.data) }
    }
}

/// Compresses an encoded message that is large enough to be worth it: a
/// delivery listing thousands of paths, a batch of directory manifests. A
/// new connection may only send a few kilobytes per round trip at first, so
/// on a slow link the size of such a message is latency. `None` means "send
/// it as it is". Messages that carry file contents are compressed already.
pub fn pack(encoded: &[u8]) -> Option<Vec<u8>> {
    if encoded.len() < 2048 {
        return None;
    }
    let z = zstd::bulk::compress(encoded, 3).ok()?;
    (z.len() + encoded.len() / 8 < encoded.len()).then_some(z)
}

/// Undoes [`pack`].
pub fn unpack(packed: &[u8]) -> std::io::Result<Vec<u8>> {
    zstd::stream::decode_all(packed)
}

/// In a lazy delivery, files up to this size are sent along with their
/// names: fetching each when it is opened would cost a round trip apiece.
pub const LAZY_SMALL: u64 = 64 << 10;
/// Contents at least this large are fetched by chunk, so that what the
/// client's cache already holds of them is not sent again.
pub const CHUNKED_FROM: u64 = 1 << 20;

/// Files larger than this travel as a sequence of parts.
pub const PART_SIZE: usize = 1 << 20;
/// Small blobs are batched up to about this many bytes per message.
pub const BATCH_SIZE: usize = 4 << 20;

impl Blob {
    pub fn pack(hash: Digest, bytes: &[u8]) -> Blob {
        if bytes.len() >= 256 {
            if let Ok(z) = zstd::bulk::compress(bytes, 1) {
                if z.len() + 16 < bytes.len() {
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

/// Whether the client passes variable `name` to the server without being
/// asked. Everything a build may legitimately be steered by that is not a
/// path on the client's machine: cargo and rustc settings, locale, colour.
/// Variables that describe the client's machine (`PATH`, `HOME`,
/// `RUSTUP_HOME`, certificate paths) are never sent; the server has its own.
pub fn env_allowed(name: &str) -> bool {
    const EXACT: &[&str] = &[
        "LANG", "LANGUAGE", "TZ", "RUSTUP_TOOLCHAIN", "RUSTFLAGS", "RUSTDOCFLAGS", "TERM", "NO_COLOR", "BROWSER",
        "CLICOLOR", "CLICOLOR_FORCE", "COLORTERM",
    ];
    const PREFIXES: &[&str] = &["CARGO_", "RUSTC", "RUSTDOC", "RUST_", "LC_", "__CARGO", "RARGO_"];
    const DROP: &[&str] = &[
        "CARGO_HOME", "CARGO_MAKEFLAGS", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "RARGO_TOKEN",
        "RARGO_SERVER", "RARGO_CACHE_DIR", "RARGO_REAL_CARGO", "CARGO_REGISTRY_TOKEN",
    ];
    if DROP.contains(&name) || (name.starts_with("CARGO_REGISTRIES_") && name.ends_with("_TOKEN")) {
        return false;
    }
    EXACT.contains(&name) || PREFIXES.iter().any(|p| name.starts_with(p))
}

/// The content cargo writes into `CACHEDIR.TAG`. A directory holding such a
/// file is build output and is never synced.
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
