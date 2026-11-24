//! What the client knows that outlives a daemon, kept in its cache:
//!
//! - which trees and contents a server holds ([`Held`]), so that the first
//!   command of a new daemon offers only what changed. It is a hint: the
//!   server is asked to build a root, finds what it lacks and asks for it,
//!   so a record that is stale (the server evicted something, or another
//!   server answers at that address) costs round trips, never a wrong
//!   build;
//! - which chunks the cache holds and where ([`load_chunks`]): every chunk
//!   taken from it is checked against its hash first.

use rargo_proto::Digest;
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// More is not kept: the record starts over (it is only a hint).
const MOST: usize = 4_000_000;

const HELD_MAGIC: &[u8] = b"rargo-held-1\n";
const CHUNKS_MAGIC: &[u8] = b"rargo-chunks-1\n";

/// The first eight bytes of a digest: enough to tell contents apart in a
/// hint (one that collides is offered again, or asked for by the server).
fn short(d: &Digest) -> u64 {
    u64::from_le_bytes(d[..8].try_into().expect("eight bytes"))
}

/// The trees and contents a server is believed to hold.
pub struct Held {
    path: PathBuf,
    set: Mutex<HashSet<u64>>,
    changed: AtomicBool,
}

impl Held {
    /// The record for `server` and `user`, in `cache`.
    pub fn load(cache: &Path, server: &str, user: &str) -> Held {
        let key = blake3::hash(format!("{server}\0{user}").as_bytes()).to_hex()[..32].to_string();
        let path = cache.join("held").join(key);
        let set = std::fs::read(&path)
            .ok()
            .and_then(|raw| {
                let body = raw.strip_prefix(HELD_MAGIC)?;
                let (whole, rest) = body.as_chunks::<8>();
                rest.is_empty().then(|| whole.iter().map(|c| u64::from_le_bytes(*c)).collect())
            })
            .unwrap_or_default();
        Held { path, set: Mutex::new(set), changed: AtomicBool::new(false) }
    }

    /// Of `items`, those whose digest the server is not believed to hold.
    pub fn lacking<'a, T>(&self, items: impl Iterator<Item = (&'a Digest, T)>) -> Vec<(Digest, T)> {
        let set = self.set.lock().unwrap();
        items.filter(|(d, _)| !set.contains(&short(d))).map(|(d, t)| (*d, t)).collect()
    }

    pub fn add<'a>(&self, digests: impl Iterator<Item = &'a Digest>) {
        let mut set = self.set.lock().unwrap();
        let before = set.len();
        set.extend(digests.map(short));
        if set.len() > MOST {
            set.clear();
        }
        if set.len() != before {
            self.changed.store(true, Ordering::SeqCst);
        }
    }

    pub fn len(&self) -> usize {
        self.set.lock().unwrap().len()
    }

    /// Writes the record if it changed since it was read or last written.
    pub fn save(&self) -> io::Result<()> {
        if !self.changed.swap(false, Ordering::SeqCst) {
            return Ok(());
        }
        let body: Vec<u8> = {
            let set = self.set.lock().unwrap();
            let mut b = Vec::with_capacity(HELD_MAGIC.len() + 8 * set.len());
            b.extend_from_slice(HELD_MAGIC);
            for v in set.iter() {
                b.extend_from_slice(&v.to_le_bytes());
            }
            b
        };
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        crate::write_atomic(&self.path, &body).inspect_err(|_| self.changed.store(true, Ordering::SeqCst))
    }
}

/// Where the cache holds each chunk: in which content, at which offset,
/// how long.
pub type Chunks = HashMap<Digest, (Digest, u64, u32)>;

pub fn chunks_path(cache: &Path) -> PathBuf {
    cache.join("chunks")
}

pub fn load_chunks(cache: &Path) -> Chunks {
    const ONE: usize = 32 + 32 + 8 + 4;
    std::fs::read(chunks_path(cache))
        .ok()
        .and_then(|raw| {
            let body = raw.strip_prefix(CHUNKS_MAGIC)?;
            let (whole, rest) = body.as_chunks::<ONE>();
            rest.is_empty().then(|| {
                whole
                    .iter()
                    .map(|r| {
                        let chunk: Digest = r[..32].try_into().unwrap();
                        let blob: Digest = r[32..64].try_into().unwrap();
                        let off = u64::from_le_bytes(r[64..72].try_into().unwrap());
                        let len = u32::from_le_bytes(r[72..76].try_into().unwrap());
                        (chunk, (blob, off, len))
                    })
                    .collect()
            })
        })
        .unwrap_or_default()
}

pub fn save_chunks(cache: &Path, chunks: &Chunks) -> io::Result<()> {
    let mut b = Vec::with_capacity(CHUNKS_MAGIC.len() + 76 * chunks.len());
    b.extend_from_slice(CHUNKS_MAGIC);
    for (chunk, (blob, off, len)) in chunks {
        b.extend_from_slice(chunk);
        b.extend_from_slice(blob);
        b.extend_from_slice(&off.to_le_bytes());
        b.extend_from_slice(&len.to_le_bytes());
    }
    crate::write_atomic(&chunks_path(cache), &b)
}

/// Most chunks kept in the index; past it, the index starts over.
pub const MOST_CHUNKS: usize = MOST;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_the_server_holds_and_the_chunks_outlive_the_daemon() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b, c) = ([1u8; 32], [2u8; 32], [3u8; 32]);
        let all = |h: &Held| h.lacking([(&a, 1), (&b, 2), (&c, 3)].into_iter());
        let held = Held::load(tmp.path(), "build.example:7459", "ann");
        assert_eq!(all(&held).len(), 3);
        held.add([a, b].iter());
        held.save().unwrap();
        let again = Held::load(tmp.path(), "build.example:7459", "ann");
        assert_eq!(all(&again), vec![(c, 3)]);
        assert_eq!(again.len(), 2);
        // Another server, or another user: a record of its own.
        assert_eq!(all(&Held::load(tmp.path(), "other.example:7459", "ann")).len(), 3);
        assert_eq!(all(&Held::load(tmp.path(), "build.example:7459", "bob")).len(), 3);
        // A damaged record is no record.
        let path = std::fs::read_dir(tmp.path().join("held")).unwrap().next().unwrap().unwrap().path();
        std::fs::write(&path, b"rargo-held-1\nabc").unwrap();
        assert_eq!(Held::load(tmp.path(), "build.example:7459", "ann").len(), 0);

        let mut chunks = Chunks::new();
        chunks.insert(a, (b, 4096, 512));
        chunks.insert(c, (b, 0, 4096));
        save_chunks(tmp.path(), &chunks).unwrap();
        assert_eq!(load_chunks(tmp.path()), chunks);
        std::fs::write(chunks_path(tmp.path()), b"something else").unwrap();
        assert!(load_chunks(tmp.path()).is_empty());
    }
}
