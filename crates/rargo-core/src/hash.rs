use serde::{Deserialize, Serialize};
use std::fmt;
use crate::io::file::File;
use std::io;
use std::path::Path;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default)]
pub struct Hash(pub [u8; 32]);

impl Hash {
    pub const ZERO: Hash = Hash([0; 32]);

    pub fn of(bytes: &[u8]) -> Hash {
        Hash(*blake3::hash(bytes).as_bytes())
    }

    pub fn hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in self.0 {
            use fmt::Write;
            let _ = write!(s, "{b:02x}");
        }
        s
    }

    pub fn from_hex(s: &str) -> Option<Hash> {
        if s.len() != 64 {
            return None;
        }
        let mut out = [0u8; 32];
        for i in 0..32 {
            out[i] = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
        }
        Some(Hash(out))
    }

    pub fn of_file(file: &File) -> io::Result<(Hash, u64)> {
        let len = file.metadata()?.len();
        let mut h = blake3::Hasher::new();
        if len >= 4 << 20 {
            use std::os::fd::AsRawFd;
            let size = len as usize;
            unsafe {
                let p = libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ, libc::MAP_PRIVATE, file.as_raw_fd(), 0);
                if p == libc::MAP_FAILED {
                    return Err(io::Error::last_os_error());
                }
                h.update_rayon(std::slice::from_raw_parts(p as *const u8, size));
                libc::munmap(p, size);
            }
        } else {
            let mut buf = vec![0u8; len as usize];
            file.read_exact_at(&mut buf, 0)?;
            h.update(&buf);
        }
        Ok((Hash(*h.finalize().as_bytes()), len))
    }

    pub fn of_path(path: &Path) -> io::Result<(Hash, u64)> {
        Hash::of_file(&File::open(path)?)
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", &self.hex()[..16])
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.hex())
    }
}

pub struct KeyHasher(blake3::Hasher);

impl KeyHasher {
    pub fn new(domain: &str) -> KeyHasher {
        let mut h = blake3::Hasher::new();
        h.update(&(domain.len() as u64).to_le_bytes());
        h.update(domain.as_bytes());
        KeyHasher(h)
    }

    pub fn field(&mut self, b: &[u8]) -> &mut Self {
        self.0.update(&(b.len() as u64).to_le_bytes());
        self.0.update(b);
        self
    }

    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.0.update(&v.to_le_bytes());
        self
    }

    pub fn finish(&self) -> Hash {
        Hash(*self.0.finalize().as_bytes())
    }
}
