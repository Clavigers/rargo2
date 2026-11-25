use rargo_proto::Digest;
use std::collections::HashSet;
use std::hash::{BuildHasherDefault, DefaultHasher};

pub const MOST: usize = 4_000_000;

const HELD_MAGIC: &[u8] = b"rargo-held-1\n";

const _: () = assert!(MOST <= u32::MAX as usize);

fn short(d: &Digest) -> u64 {
    u64::from_le_bytes(d[..8].try_into().expect("eight bytes"))
}

pub struct KnownDigests {
    set: HashSet<u64, BuildHasherDefault<DefaultHasher>>,
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

impl Default for KnownDigests {
    fn default() -> Self {
        Self { set: HashSet::with_hasher(BuildHasherDefault::default()), changed: false }
    }
}
