use super::object::{Id, Type};
use super::sha1::{Sha1, sha1};
use std::io::Write as _;

pub struct Packed {
    pub pack: Vec<u8>,
    pub idx: Vec<u8>,
    pub checksum: Id,
}

pub fn pack(objects: &[(Id, Type, &[u8])]) -> Packed {
    let mut pack = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let unique: Vec<&(Id, Type, &[u8])> = objects.iter().filter(|(id, _, _)| seen.insert(*id)).collect();
    pack.extend_from_slice(b"PACK");
    pack.extend_from_slice(&2u32.to_be_bytes());
    pack.extend_from_slice(&(unique.len() as u32).to_be_bytes());
    let mut records: Vec<(Id, u32, u64)> = Vec::with_capacity(unique.len());
    for (id, kind, data) in unique {
        let start = pack.len();
        let mut size = data.len() as u64;
        let mut c = (kind.code() << 4) | (size & 15) as u8;
        size >>= 4;
        while size != 0 {
            pack.push(c | 0x80);
            c = (size & 0x7f) as u8;
            size >>= 7;
        }
        pack.push(c);
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(data).expect("compressing into memory");
        pack.extend_from_slice(&z.finish().expect("compressing into memory"));
        records.push((*id, crc32fast::hash(&pack[start..]), start as u64));
    }
    let checksum = sha1(&pack);
    pack.extend_from_slice(&checksum);
    records.sort();
    let mut idx = vec![0xff, b't', b'O', b'c', 0, 0, 0, 2];
    for byte in 0..256usize {
        let n = records.iter().filter(|(id, _, _)| (id[0] as usize) <= byte).count() as u32;
        idx.extend_from_slice(&n.to_be_bytes());
    }
    for (id, _, _) in &records {
        idx.extend_from_slice(id);
    }
    for (_, crc, _) in &records {
        idx.extend_from_slice(&crc.to_be_bytes());
    }
    let mut large = Vec::new();
    for (_, _, offset) in &records {
        let small = if *offset < 0x8000_0000 {
            *offset as u32
        } else {
            large.push(*offset);
            0x8000_0000 | (large.len() as u32 - 1)
        };
        idx.extend_from_slice(&small.to_be_bytes());
    }
    for offset in large {
        idx.extend_from_slice(&offset.to_be_bytes());
    }
    idx.extend_from_slice(&checksum);
    let own = sha1(&idx);
    idx.extend_from_slice(&own);
    Packed { pack, idx, checksum }
}

pub struct IndexEntry {
    pub path: Vec<u8>,
    pub mode: u32,
    pub id: Id,
}

pub fn index(mut entries: Vec<IndexEntry>) -> Vec<u8> {
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries.dedup_by(|a, b| a.path == b.path);
    let mut out = Vec::new();
    out.extend_from_slice(b"DIRC");
    out.extend_from_slice(&2u32.to_be_bytes());
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for e in &entries {
        let start = out.len();
        out.extend_from_slice(&[0u8; 24]);
        out.extend_from_slice(&e.mode.to_be_bytes());
        out.extend_from_slice(&[0u8; 12]);
        out.extend_from_slice(&e.id);
        out.extend_from_slice(&(e.path.len().min(0xfff) as u16).to_be_bytes());
        out.extend_from_slice(&e.path);
        let len = out.len() - start;
        out.resize(start + (len + 8) / 8 * 8, 0);
    }
    let mut s = Sha1::default();
    s.update(&out);
    let sum = s.finish();
    out.extend_from_slice(&sum);
    out
}
