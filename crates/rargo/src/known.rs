use rargo_proto::Digest;
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

const MOST: usize = 4_000_000;

const HELD_MAGIC: &[u8] = b"rargo-held-1\n";
const CHUNKS_MAGIC: &[u8] = b"rargo-chunks-1\n";

const _: () = assert!(MOST <= u32::MAX as usize);

fn short(d: &Digest) -> u64 {
    u64::from_le_bytes(d[..8].try_into().expect("eight bytes"))
}

#[derive(Default)]
pub struct KnownDigests {
    set: HashSet<u64>,
    changed: bool,
}

impl KnownDigests {
    pub fn decode(raw: &[u8]) -> Option<Self> {
        let body = raw.strip_prefix(HELD_MAGIC)?;
        let (whole, rest) = body.as_chunks::<8>();
        rest.is_empty().then(|| Self { set: whole.iter().map(|c| u64::from_le_bytes(*c)).collect(), changed: false })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut ids: Vec<_> = self.set.iter().copied().collect();
        ids.sort_unstable();
        let mut bytes = Vec::with_capacity(HELD_MAGIC.len() + 8 * ids.len());
        bytes.extend_from_slice(HELD_MAGIC);
        for id in ids {
            bytes.extend_from_slice(&id.to_le_bytes());
        }
        bytes
    }

    pub fn lacking<'a, T>(&self, items: impl Iterator<Item = (&'a Digest, T)>) -> Vec<(Digest, T)> {
        items.filter(|(d, _)| !self.set.contains(&short(d))).map(|(d, t)| (*d, t)).collect()
    }

    pub fn add<'a>(&mut self, digests: impl Iterator<Item = &'a Digest>) {
        let before = self.set.len();
        self.set.extend(digests.map(short));
        if self.set.len() > MOST {
            self.set.clear();
            self.changed = true;
        }
        self.changed |= self.set.len() != before;
    }

    pub fn forget<'a>(&mut self, digests: impl Iterator<Item = &'a Digest>) {
        for digest in digests {
            self.changed |= self.set.remove(&short(digest));
        }
    }

    pub fn holds(&self, digest: &Digest) -> bool {
        self.set.contains(&short(digest))
    }

    pub fn len(&self) -> usize {
        self.set.len()
    }

    pub fn take_changes(&mut self) -> Option<Vec<u8>> {
        std::mem::take(&mut self.changed).then(|| self.encode())
    }

    pub fn save_failed(&mut self) {
        self.changed = true;
    }
}

pub struct HeldFile(PathBuf);

impl HeldFile {
    pub fn load(cache: &Path, server: &str, user: &str) -> (Self, KnownDigests) {
        let key = blake3::hash(format!("{server}\0{user}").as_bytes()).to_hex()[..32].to_string();
        let path = cache.join("held").join(key);
        let held = std::fs::read(&path).ok().and_then(|raw| KnownDigests::decode(&raw)).unwrap_or_default();
        assert!(path.starts_with(cache));
        (Self(path), held)
    }

    pub fn save(&self, body: &[u8]) -> io::Result<()> {
        if let Some(dir) = self.0.parent() {
            std::fs::create_dir_all(dir)?;
        }
        crate::write_atomic(&self.0, body)
    }
}

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
                        const _: () = assert!(ONE == 76);
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
    assert_eq!(b.len(), CHUNKS_MAGIC.len() + 76 * chunks.len());
    crate::write_atomic(&chunks_path(cache), &b)
}

pub const MOST_CHUNKS: usize = MOST;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_the_server_holds_and_the_chunks_outlive_the_daemon() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b, c) = ([1u8; 32], [2u8; 32], [3u8; 32]);
        let all = |h: &KnownDigests| h.lacking([(&a, 1), (&b, 2), (&c, 3)].into_iter());
        let (file, mut held) = HeldFile::load(tmp.path(), "build.example:7459", "ann");
        assert_eq!(all(&held).len(), 3);
        held.add([a, b].iter());
        file.save(&held.encode()).unwrap();
        let (_, again) = HeldFile::load(tmp.path(), "build.example:7459", "ann");
        assert_eq!(all(&again), vec![(c, 3)]);
        assert_eq!(again.len(), 2);
        assert_eq!(all(&HeldFile::load(tmp.path(), "other.example:7459", "ann").1).len(), 3);
        assert_eq!(all(&HeldFile::load(tmp.path(), "build.example:7459", "bob").1).len(), 3);
        let path = std::fs::read_dir(tmp.path().join("held")).unwrap().next().unwrap().unwrap().path();
        std::fs::write(&path, b"rargo-held-1\nabc").unwrap();
        assert_eq!(HeldFile::load(tmp.path(), "build.example:7459", "ann").1.len(), 0);

        let mut chunks = Chunks::new();
        chunks.insert(a, (b, 4096, 512));
        chunks.insert(c, (b, 0, 4096));
        save_chunks(tmp.path(), &chunks).unwrap();
        assert_eq!(load_chunks(tmp.path()), chunks);
        std::fs::write(chunks_path(tmp.path()), b"something else").unwrap();
        assert!(load_chunks(tmp.path()).is_empty());
    }
}
