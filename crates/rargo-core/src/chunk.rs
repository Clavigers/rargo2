use crate::hash::Hash;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use crate::io::file::File;
use std::io;
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

const GEAR: [u64; 256] = gear();

const fn gear() -> [u64; 256] {
    let mut table = [0; 256];
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let mut i = 0;
    while i < table.len() {
        x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        table[i] = z ^ (z >> 31);
        i += 1;
    }
    table
}

fn cdc(data: &[u8], cuts: &mut Vec<usize>) {
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
            h = (h << 1).wrapping_add(GEAR[data[i] as usize]);
            if h & MASK_S == 0 {
                cut = i + 1;
                break;
            }
            i += 1;
        }
        if cut == end {
            while i < end {
                h = (h << 1).wrapping_add(GEAR[data[i] as usize]);
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
        if typ != 8 && off > 0 && off < data.len() {
            b.push(off);
        }
    }
    b.sort_unstable();
    b.dedup();
    Some(b)
}

pub fn debug_ranges(data: &[u8]) -> Vec<(u64, u64)> {
    debug_sections(data).unwrap_or_default()
}

fn debug_sections(data: &[u8]) -> Option<Vec<(u64, u64)>> {
    if data.len() < 64 || &data[..4] != b"\x7fELF" || data[4] != 2 || data[5] != 1 {
        return None;
    }
    let u64_at = |o: usize| data.get(o..o + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()));
    let u32_at = |o: usize| data.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
    let u16_at = |o: usize| data.get(o..o + 2).map(|b| u16::from_le_bytes(b.try_into().unwrap()) as usize);
    let shoff = u64_at(0x28)? as usize;
    let shentsize = u16_at(0x3a)?;
    let shnum = u16_at(0x3c)?;
    let shstrndx = u16_at(0x3e)?;
    if shentsize < 64 || shoff == 0 || shstrndx >= shnum {
        return None;
    }
    let sh = |i: usize| shoff.checked_add(i.checked_mul(shentsize)?);
    let names = u64_at(sh(shstrndx)? + 0x18)? as usize;
    let mut out: Vec<(u64, u64)> = Vec::new();
    for i in 0..shnum {
        let h = sh(i)?;
        let name = names.checked_add(u32_at(h)? as usize)?;
        let name = data.get(name..)?;
        let debug = name.starts_with(b".debug_") || name.starts_with(b".zdebug_");
        if !debug || u32_at(h + 4)? == 8 {
            continue;
        }
        let (off, size) = (u64_at(h + 0x18)?, u64_at(h + 0x20)?);
        let end = off.checked_add(size)?;
        if size > 0 && end <= data.len() as u64 {
            out.push((off, end));
        }
    }
    out.sort_unstable();
    let mut joined: Vec<(u64, u64)> = Vec::with_capacity(out.len());
    for (s, e) in out {
        match joined.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => joined.push((s, e)),
        }
    }
    Some(joined)
}

fn structural(data: &[u8]) -> Vec<usize> {
    if let Some(members) = crate::ar::members(data) {
        let mut b = Vec::new();
        for m in &members {
            b.push(m.header_offset);
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

pub fn chunk(data: &[u8]) -> Recipe {
    let mut bounds = structural(data);
    bounds.retain(|&o| o > 0 && o < data.len());
    bounds.push(data.len());
    let hard = if data.starts_with(b"!<arch>\n") { 8 } else { 0 };
    let mut lens: Vec<usize> = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while start < data.len() {
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

pub fn chunk_file(file: &File) -> io::Result<Recipe> {
    let len = file.metadata()?.len() as usize;
    if len == 0 {
        return Ok(Vec::new());
    }
    Ok(chunk(&file.map(len)?))
}

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

    fn elf(sections: &[(&str, u32, usize)]) -> (Vec<u8>, Vec<(u64, u64)>) {
        let mut data = vec![0u8; 64];
        data[..6].copy_from_slice(b"\x7fELF\x02\x01");
        let mut placed = Vec::new();
        for (i, (_, typ, len)) in sections.iter().enumerate() {
            let off = data.len() as u64;
            if *typ != 8 {
                data.extend(noise(*len, i as u64 + 1));
            }
            placed.push((off, *len as u64));
        }
        let names_at = data.len() as u64;
        let mut names = vec![0u8];
        let mut name_offs = Vec::new();
        for (n, _, _) in sections.iter().copied().chain([(".shstrtab", 3, 0)]) {
            name_offs.push(names.len() as u32);
            names.extend(n.as_bytes());
            names.push(0);
        }
        data.extend(&names);
        let shoff = data.len() as u64;
        let mut headers = vec![0u8; 64];
        let all: Vec<(u32, u32, u64, u64)> = sections
            .iter()
            .enumerate()
            .map(|(i, (_, typ, _))| (name_offs[i], *typ, placed[i].0, placed[i].1))
            .chain([(name_offs[sections.len()], 3, names_at, names.len() as u64)])
            .collect();
        for (name, typ, off, size) in &all {
            let mut h = vec![0u8; 64];
            h[0..4].copy_from_slice(&name.to_le_bytes());
            h[4..8].copy_from_slice(&typ.to_le_bytes());
            h[0x18..0x20].copy_from_slice(&off.to_le_bytes());
            h[0x20..0x28].copy_from_slice(&size.to_le_bytes());
            headers.extend(h);
        }
        data.extend(headers);
        data[0x28..0x30].copy_from_slice(&shoff.to_le_bytes());
        data[0x3a..0x3c].copy_from_slice(&64u16.to_le_bytes());
        data[0x3c..0x3e].copy_from_slice(&(all.len() as u16 + 1).to_le_bytes());
        data[0x3e..0x40].copy_from_slice(&(all.len() as u16).to_le_bytes());
        let placed = placed.into_iter().map(|(o, l)| (o, o + l)).collect();
        (data, placed)
    }

    #[test]
    fn debug_sections_are_found_and_joined() {
        let (data, at) = elf(&[
            (".text", 1, 5000),
            (".debug_info", 1, 7000),
            (".debug_line", 1, 3000),
            (".data", 1, 100),
            (".zdebug_str", 1, 900),
            (".bss", 8, 4096),
            (".debug_empty", 1, 0),
            (".debugger_not", 1, 50),
        ]);
        assert_eq!(debug_ranges(&data), vec![(at[1].0, at[2].1), at[4]]);
        assert!(debug_ranges(b"not an ELF file at all, just some text").is_empty());
        let mut broken = data.clone();
        broken.truncate(data.len() - 64);
        assert!(debug_ranges(&broken).is_empty(), "a section header past the end");
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
        let (m1, m2, m3) = (noise(300_000, 1), noise(200_000, 2), noise(400_000, 3));
        let m2b = noise(210_001, 4);
        let a = ar(&[("a.o", &m1), ("b.o", &m2), ("c.o", &m3)]);
        let b = ar(&[("a.o", &m1), ("b.o", &m2b), ("c.o", &m3)]);
        let (ra, rb) = (chunk(&a), chunk(&b));
        assert_eq!(total(&ra), a.len());
        let set: std::collections::HashSet<Hash> = ra.iter().map(|c| c.hash).collect();
        let new: usize = rb.iter().filter(|c| !set.contains(&c.hash)).map(|c| c.len as usize).sum();
        assert!(new <= m2b.len() + 200, "only the replaced member is new ({new} bytes)");
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
