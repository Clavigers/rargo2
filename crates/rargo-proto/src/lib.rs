//! The protocol between the `rargo` client and the build server `rargod`.
//!
//! The client is a file and environment syncer: it describes its worktree as
//! a content-addressed tree, uploads what the server lacks, names the
//! environment, and then receives output, requested artifacts and files the
//! build wrote into the worktree. Everything else happens on the server.
//!
//! One TCP connection per command. The connection is encrypted and
//! authenticated with a pre-shared key per user (Noise `NNpsk0`); see
//! [`secure`]. Inside it, both directions carry bincode messages
//! ([`ClientMsg`], [`ServerMsg`]).

use serde::{Deserialize, Serialize};

pub mod secure;
pub mod tree;

pub const PROTO_VERSION: u32 = 4;

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
    /// Files to place. The client answers with [`ClientMsg::Need`] naming
    /// the contents it does not already have; those follow as blobs.
    Deliver { id: u64, files: Vec<Delivery>, writes: Vec<Write> },
    Blobs(Vec<Blob>),
    BlobPart { hash: Digest, part: Part, last: bool },
    /// Everything named in the `Need` for delivery `id` has been sent.
    Delivered { id: u64 },
}

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
    Need { id: u64, hashes: Vec<Digest> },
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
        "CARGO_HOME", "CARGO_MAKEFLAGS", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "RARGO_KEY", "RARGO_KEY_FILE",
        "RARGO_SERVER", "RARGO_USER", "RARGO_CACHE_DIR", "RARGO_REAL_CARGO", "CARGO_REGISTRY_TOKEN",
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
        assert!(!env_allowed("RARGO_KEY"));
        assert!(!env_allowed("CARGO_REGISTRIES_MINE_TOKEN"));
        assert!(!env_allowed("AWS_SECRET_ACCESS_KEY"));
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
