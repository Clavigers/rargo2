//! Chunking for transfer and for the cold tier.
//!
//! A blob is described by a recipe: an ordered list of chunk hashes and
//! lengths. Two versions of an rlib that differ in one object share every
//! other chunk, so only the changed part has to cross the network or be
//! stored again.
//!
//! Boundaries are format-aware first and content-defined second: an `ar`
//! archive is cut at its members, an ELF object at its sections, and what
//! lies between structural boundaries is cut by a rolling hash (FastCDC
//! style) so that an insertion shifts at most a couple of chunks.

use crate::hash::Hash;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::sync::Arc;

pub const MIN_CHUNK: usize = 16 << 10;
pub const AVG_CHUNK: usize = 64 << 10;
pub const MAX_CHUNK: usize = 256 << 10;
const _: () = assert!(MIN_CHUNK < AVG_CHUNK);
const _: () = assert!(AVG_CHUNK < MAX_CHUNK);
const _: () = assert!(MAX_CHUNK <= u32::MAX as usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkRef {
    pub hash: Hash,
    pub len: u32,
}

pub type Recipe = Vec<ChunkRef>;

fn gear() -> &'static [u64; 256] {
    static T: std::sync::OnceLock<[u64; 256]> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        // Fixed pseudo-random table (splitmix64): both ends of a transfer
        // must cut identically.
        let mut t = [0u64; 256];
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        for v in t.iter_mut() {
            x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = x;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            *v = z ^ (z >> 31);
        }
        t
    })
}

/// Content-defined cut points inside `data`, appended to `cuts` as lengths.
fn cdc(data: &[u8], cuts: &mut Vec<usize>) {
    let g = gear();
    // Two masks, as in FastCDC: harder to cut before the average size,
    // easier after, which tightens the size distribution.
    const MASK_S: u64 = (1 << 18) - 1;
    const MASK_L: u64 = (1 << 14) - 1;
    let mut start = 0;
    while data.len() - start > MIN_CHUNK {
        let end = (start + MAX_CHUNK).min(data.len());
        let mid = (start + AVG_CHUNK).min(end);
        let mut h: u64 = 0;
        let mut cut = end;
        let mut i = start + MIN_CHUNK;
        while i < mid {
            h = (h << 1).wrapping_add(g[data[i] as usize]);
            if h & MASK_S == 0 {
                cut = i + 1;
                break;
            }
            i += 1;
        }
        if cut == end {
            while i < end {
                h = (h << 1).wrapping_add(g[data[i] as usize]);
                if h & MASK_L == 0 {
                    cut = i + 1;
                    break;
                }
                i += 1;
            }
        }
        assert!(cut - start > MIN_CHUNK);
        assert!(cut - start <= MAX_CHUNK);
        cuts.push(cut - start);
        start = cut;
    }
    assert!(start <= data.len());
    if start < data.len() {
        assert!(data.len() - start <= MIN_CHUNK);
        cuts.push(data.len() - start);
    }
}

/// Section boundaries of a 64-bit little-endian ELF file, as offsets.
fn elf_boundaries(data: &[u8]) -> Option<Vec<usize>> {
    if data.len() < 64 || &data[..4] != b"\x7fELF" || data[4] != 2 || data[5] != 1 {
        return None;
    }
    let u64_at = |o: usize| data.get(o..o + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()) as usize);
    let u16_at = |o: usize| data.get(o..o + 2).map(|b| u16::from_le_bytes(b.try_into().unwrap()) as usize);
    let shoff = u64_at(0x28)?;
    let shentsize = u16_at(0x3a)?;
    let shnum = u16_at(0x3c)?;
    if shentsize < 64 || shoff == 0 {
        return None;
    }
    let mut b = vec![shoff];
    for i in 0..shnum {
        let sh = shoff.checked_add(i * shentsize)?;
        let typ = data.get(sh + 4..sh + 8).map(|x| u32::from_le_bytes(x.try_into().unwrap()))?;
        let off = u64_at(sh + 0x18)?;
        // SHT_NOBITS occupies no file space.
        if typ != 8 && off > 0 && off < data.len() {
            b.push(off);
        }
    }
    b.sort_unstable();
    b.dedup();
    Some(b)
}

/// Structural boundaries of `data` (offsets, ascending, exclusive of 0 and
/// the end).
fn structural(data: &[u8]) -> Vec<usize> {
    if let Some(members) = crate::ar::members(data) {
        let mut b = Vec::new();
        for m in &members {
            b.push(m.header_offset);
            // The member's own structure, if it is an object file.
            let start = m.data.as_ptr() as usize - data.as_ptr() as usize;
            b.push(start);
            if let Some(inner) = elf_boundaries(m.data) {
                b.extend(inner.into_iter().map(|o| start + o));
            }
        }
        b.sort_unstable();
        b.dedup();
        return b;
    }
    elf_boundaries(data).unwrap_or_default()
}

/// Cuts `data` into chunks.
pub fn chunk(data: &[u8]) -> Recipe {
    let mut bounds = structural(data);
    bounds.retain(|&o| o > 0 && o < data.len());
    bounds.push(data.len());
    // The archive magic is its own tiny chunk, so that the first member
    // chunks the same wherever it sits.
    let hard = if data.starts_with(b"!<arch>\n") { 8 } else { 0 };
    let mut lens: Vec<usize> = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while start < data.len() {
        // Structural regions smaller than the minimum are merged with what
        // follows, so thousands of tiny sections do not become thousands of
        // tiny chunks; the cut still lands on a structural boundary.
        assert!(i < bounds.len());
        let mut end = bounds[i];
        while end - start < MIN_CHUNK && i + 1 < bounds.len() && end != hard {
            i += 1;
            end = bounds[i];
        }
        assert!(end > start, "structural bounds ascend");
        i += 1;
        cdc(&data[start..end], &mut lens);
        start = end;
    }
    let mut out = Vec::with_capacity(lens.len());
    let mut pos = 0;
    for len in lens {
        out.push(ChunkRef { hash: Hash::of(&data[pos..pos + len]), len: len as u32 });
        pos += len;
    }
    assert_eq!(pos, data.len(), "recipe covers the input");
    assert_eq!(out.is_empty(), data.is_empty());
    out
}

/// Chunks a file (memory-mapped).
pub fn chunk_file(file: &File) -> io::Result<Recipe> {
    let len = file.metadata()?.len() as usize;
    if len == 0 {
        return Ok(Vec::new());
    }
    use std::os::fd::AsRawFd;
    // SAFETY: mapping a regular file read-only; the slice is not used after
    // munmap. Blobs are immutable, so the mapping cannot change under us.
    unsafe {
        let p = libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_PRIVATE, file.as_raw_fd(), 0);
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let r = chunk(std::slice::from_raw_parts(p as *const u8, len));
        libc::munmap(p, len);
        Ok(r)
    }
}

/// Where chunks can be found locally: inside which whole blob, at which
/// offset. Blobs stay whole on disk; this index is how a new blob is put
/// together from pieces of ones already present.
#[derive(Default)]
pub struct ChunkIndex {
    at: DashMap<Hash, (Hash, u64, u32)>,
    recipes: DashMap<Hash, Arc<Recipe>>,
}

impl ChunkIndex {
    pub fn add_blob(&self, blob: Hash, recipe: Arc<Recipe>) {
        let mut off = 0u64;
        for c in recipe.iter() {
            self.at.entry(c.hash).or_insert((blob, off, c.len));
            off += c.len as u64;
        }
        self.recipes.insert(blob, recipe);
        assert!(self.recipes.contains_key(&blob));
    }

    pub fn recipe(&self, blob: &Hash) -> Option<Arc<Recipe>> {
        self.recipes.get(blob).map(|r| r.clone())
    }

    pub fn has_chunk(&self, chunk: &Hash) -> bool {
        self.at.contains_key(chunk)
    }

    pub fn locate(&self, chunk: &Hash) -> Option<(Hash, u64, u32)> {
        self.at.get(chunk).map(|e| *e)
    }

    pub fn forget_chunk(&self, chunk: &Hash) {
        self.at.remove(chunk);
    }

    pub fn chunk_count(&self) -> usize {
        self.at.len()
    }
}

/// Reads one chunk out of an open blob.
pub fn read_chunk(file: &File, offset: u64, len: u32) -> io::Result<Vec<u8>> {
    assert!(len as usize <= MAX_CHUNK);
    let mut buf = vec![0u8; len as usize];
    file.read_exact_at(&mut buf, offset)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(n: usize, seed: u64) -> Vec<u8> {
        let mut x = seed | 1;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 24) as u8
            })
            .collect()
    }

    fn total(r: &Recipe) -> usize {
        r.iter().map(|c| c.len as usize).sum()
    }

    #[test]
    fn covers_input_and_respects_size_limits() {
        let data = noise(3_000_000, 7);
        let r = chunk(&data);
        assert_eq!(total(&r), data.len());
        assert!(r.iter().all(|c| c.len as usize <= MAX_CHUNK));
        assert!(r[..r.len() - 1].iter().all(|c| c.len as usize >= MIN_CHUNK));
        let avg = data.len() / r.len();
        assert!((MIN_CHUNK..MAX_CHUNK).contains(&avg), "average chunk {avg}");
        assert_eq!(chunk(&data), r, "deterministic");
        assert!(chunk(&[]).is_empty());
        assert_eq!(chunk(b"tiny").len(), 1);
    }

    #[test]
    fn an_insertion_disturbs_only_nearby_chunks() {
        let a = noise(4_000_000, 11);
        let mut b = a.clone();
        b.splice(1_500_000..1_500_000, noise(1000, 99));
        let (ra, rb) = (chunk(&a), chunk(&b));
        let set: std::collections::HashSet<Hash> = ra.iter().map(|c| c.hash).collect();
        let new: usize = rb.iter().filter(|c| !set.contains(&c.hash)).map(|c| c.len as usize).sum();
        assert!(new < 3 * MAX_CHUNK, "{new} bytes of new chunks for a 1000-byte insertion");
    }

    fn ar(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = b"!<arch>\n".to_vec();
        for (name, data) in members {
            out.extend_from_slice(
                format!("{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n", format!("{name}/"), 0, 0, 0, 644, data.len()).as_bytes(),
            );
            out.extend_from_slice(data);
            if data.len() % 2 == 1 {
                out.push(b'\n');
            }
        }
        out
    }

    #[test]
    fn archive_members_are_chunk_boundaries() {
        // Replacing the middle member of an archive, even with one of a
        // different size, leaves the chunks of the other members intact.
        let (m1, m2, m3) = (noise(300_000, 1), noise(200_000, 2), noise(400_000, 3));
        let m2b = noise(210_001, 4);
        let a = ar(&[("a.o", &m1), ("b.o", &m2), ("c.o", &m3)]);
        let b = ar(&[("a.o", &m1), ("b.o", &m2b), ("c.o", &m3)]);
        let (ra, rb) = (chunk(&a), chunk(&b));
        assert_eq!(total(&ra), a.len());
        let set: std::collections::HashSet<Hash> = ra.iter().map(|c| c.hash).collect();
        let new: usize = rb.iter().filter(|c| !set.contains(&c.hash)).map(|c| c.len as usize).sum();
        assert!(new <= m2b.len() + 200, "only the replaced member is new ({new} bytes)");
        // The same member content at a different position is still shared.
        let c = ar(&[("c.o", &m3), ("a.o", &m1)]);
        let rc = chunk(&c);
        let new: usize = rc.iter().filter(|c| !set.contains(&c.hash)).map(|c| c.len as usize).sum();
        assert!(new < 1000, "reordered members share their chunks ({new} new bytes)");
    }

    #[test]
    fn index_locates_chunks_in_blobs() {
        let data = noise(500_000, 5);
        let recipe = Arc::new(chunk(&data));
        let idx = ChunkIndex::default();
        let blob = Hash::of(&data);
        idx.add_blob(blob, recipe.clone());
        let third = recipe[2];
        let (b, off, len) = idx.locate(&third.hash).unwrap();
        assert_eq!(b, blob);
        assert_eq!(Hash::of(&data[off as usize..off as usize + len as usize]), third.hash);
        assert!(!idx.has_chunk(&Hash::of(b"unknown")));
    }
}
