use super::{Kind, Source};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

pub type Id = [u8; 20];

pub fn to_hex(id: &Id) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn from_hex(s: &[u8]) -> Option<Id> {
    if s.len() != 40 {
        return None;
    }
    let mut id = [0u8; 20];
    for (i, pair) in s.chunks(2).enumerate() {
        id[i] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(id)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Type {
    Commit,
    Tree,
    Blob,
    Tag,
}

impl Type {
    pub fn name(self) -> &'static str {
        match self {
            Type::Commit => "commit",
            Type::Tree => "tree",
            Type::Blob => "blob",
            Type::Tag => "tag",
        }
    }

    fn from_name(name: &[u8]) -> Option<Type> {
        [Type::Commit, Type::Tree, Type::Blob, Type::Tag].into_iter().find(|t| t.name().as_bytes() == name)
    }

    pub fn code(self) -> u8 {
        match self {
            Type::Commit => 1,
            Type::Tree => 2,
            Type::Blob => 3,
            Type::Tag => 4,
        }
    }

    fn from_code(code: u8) -> Option<Type> {
        [Type::Commit, Type::Tree, Type::Blob, Type::Tag].into_iter().find(|t| t.code() == code)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Object {
    pub kind: Type,
    pub data: Vec<u8>,
}

#[cfg(test)]
pub fn object_id(kind: Type, data: &[u8]) -> Id {
    let mut s = super::sha1::Sha1::default();
    s.update(format!("{} {}\0", kind.name(), data.len()).as_bytes());
    s.update(data);
    s.finish()
}

fn inflate(bytes: &[u8], size: Option<usize>) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(size.unwrap_or(bytes.len() * 2));
    let mut z = flate2::read::ZlibDecoder::new(bytes);
    match size {
        Some(n) => {
            (&mut z).take(n as u64 + 1).read_to_end(&mut out).ok()?;
            (out.len() == n).then_some(out)
        }
        None => z.read_to_end(&mut out).ok().map(|_| out),
    }
}

pub fn loose(bytes: &[u8]) -> Option<Object> {
    let all = inflate(bytes, None)?;
    let nul = all.iter().position(|&b| b == 0)?;
    let (kind, size) = std::str::from_utf8(&all[..nul]).ok()?.split_once(' ')?;
    let kind = Type::from_name(kind.as_bytes())?;
    let size: usize = size.parse().ok()?;
    let data = all[nul + 1..].to_vec();
    (data.len() == size).then_some(Object { kind, data })
}

pub struct Idx<D> {
    data: D,
    n: usize,
}

const IDX_HEAD: usize = 8 + 256 * 4;

impl<D: AsRef<[u8]>> Idx<D> {
    pub fn parse(data: D) -> Option<Idx<D>> {
        let b = data.as_ref();
        if b.len() < IDX_HEAD || b[..8] != [0xff, b't', b'O', b'c', 0, 0, 0, 2] {
            return None;
        }
        let n = u32::from_be_bytes(b[IDX_HEAD - 4..IDX_HEAD].try_into().ok()?) as usize;
        let fanout_sorted = (0..255).all(|i| {
            let at = |i: usize| u32::from_be_bytes(b[8 + i * 4..12 + i * 4].try_into().unwrap());
            at(i) <= at(i + 1)
        });
        let need = IDX_HEAD.checked_add(n.checked_mul(28)?)?.checked_add(40)?;
        (fanout_sorted && b.len() >= need).then_some(Idx { data, n })
    }

    pub fn len(&self) -> usize {
        self.n
    }

    fn fanout(&self, byte: usize) -> usize {
        let b = self.data.as_ref();
        u32::from_be_bytes(b[8 + byte * 4..12 + byte * 4].try_into().unwrap()) as usize
    }

    pub fn id(&self, i: usize) -> Id {
        let at = IDX_HEAD + i * 20;
        self.data.as_ref()[at..at + 20].try_into().unwrap()
    }

    pub fn lower_bound(&self, id: &Id) -> usize {
        let (mut lo, mut hi) = (if id[0] == 0 { 0 } else { self.fanout(id[0] as usize - 1) }, self.fanout(id[0] as usize));
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.id(mid) < *id {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    pub fn find(&self, id: &Id) -> Option<usize> {
        let i = self.lower_bound(id);
        (i < self.n && self.id(i) == *id).then_some(i)
    }

    pub fn offset(&self, i: usize) -> Option<u64> {
        let b = self.data.as_ref();
        let at = IDX_HEAD + self.n * 24 + i * 4;
        let small = u32::from_be_bytes(b[at..at + 4].try_into().ok()?);
        if small & 0x8000_0000 == 0 {
            return Some(small as u64);
        }
        let large = IDX_HEAD + self.n * 28 + (small & 0x7fff_ffff) as usize * 8;
        Some(u64::from_be_bytes(b.get(large..large + 8)?.try_into().ok()?))
    }
}

enum Base {
    Offset(u64),
    Id(Id),
}

fn entry(pack: &[u8], offset: u64) -> Option<(u8, usize, usize, Option<Base>)> {
    let mut at = usize::try_from(offset).ok()?;
    let mut c = *pack.get(at)?;
    at += 1;
    let code = (c >> 4) & 7;
    let mut size = (c & 15) as u64;
    let mut shift = 4;
    while c & 0x80 != 0 {
        c = *pack.get(at)?;
        at += 1;
        size |= ((c & 0x7f) as u64).checked_shl(shift)?;
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
    let base = match code {
        6 => {
            let mut c = *pack.get(at)?;
            at += 1;
            let mut back = (c & 0x7f) as u64;
            while c & 0x80 != 0 {
                c = *pack.get(at)?;
                at += 1;
                back = back.checked_add(1)?.checked_mul(128)? | (c & 0x7f) as u64;
            }
            Some(Base::Offset(offset.checked_sub(back).filter(|_| back > 0)?))
        }
        7 => {
            let id = pack.get(at..at + 20)?.try_into().ok()?;
            at += 20;
            Some(Base::Id(id))
        }
        _ => None,
    };
    Some((code, usize::try_from(size).ok()?, at, base))
}

fn varint(delta: &[u8], at: &mut usize) -> Option<usize> {
    let mut n = 0usize;
    let mut shift = 0;
    loop {
        let c = *delta.get(*at)?;
        *at += 1;
        n |= ((c & 0x7f) as usize).checked_shl(shift)?;
        if c & 0x80 == 0 {
            return Some(n);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

pub fn apply_delta(base: &[u8], delta: &[u8]) -> Option<Vec<u8>> {
    let mut at = 0;
    if varint(delta, &mut at)? != base.len() {
        return None;
    }
    let size = varint(delta, &mut at)?;
    let mut out = Vec::with_capacity(size);
    while at < delta.len() {
        let op = delta[at];
        at += 1;
        if op & 0x80 != 0 {
            let mut field = |bits: u8, bytes: usize| -> Option<usize> {
                let mut v = 0usize;
                for i in 0..bytes {
                    if op & (bits << i) != 0 {
                        v |= (*delta.get(at)? as usize) << (8 * i);
                        at += 1;
                    }
                }
                Some(v)
            };
            let offset = field(0x01, 4)?;
            let len = match field(0x10, 3)? {
                0 => 0x10000,
                n => n,
            };
            out.extend_from_slice(base.get(offset..offset.checked_add(len)?)?);
        } else if op != 0 {
            out.extend_from_slice(delta.get(at..at + op as usize)?);
            at += op as usize;
        } else {
            return None;
        }
    }
    (out.len() == size).then_some(out)
}

#[derive(Debug, PartialEq, Eq)]
pub struct Commit {
    pub tree: Id,
    pub parents: Vec<Id>,
}

fn headers(data: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
    data.split(|&b| b == b'\n').take_while(|l| !l.is_empty()).filter_map(|l| {
        let sp = l.iter().position(|&b| b == b' ')?;
        Some((&l[..sp], &l[sp + 1..]))
    })
}

pub fn commit(data: &[u8]) -> Option<Commit> {
    let mut tree = None;
    let mut parents = Vec::new();
    for (key, value) in headers(data) {
        match key {
            b"tree" => tree = Some(from_hex(value)?),
            b"parent" => parents.push(from_hex(value)?),
            _ => {}
        }
    }
    Some(Commit { tree: tree?, parents })
}

pub fn tag(data: &[u8]) -> Option<(Id, Type)> {
    let (mut object, mut kind) = (None, None);
    for (key, value) in headers(data) {
        match key {
            b"object" => object = from_hex(value),
            b"type" => kind = Type::from_name(value),
            _ => {}
        }
    }
    Some((object?, kind?))
}

#[derive(Debug, PartialEq, Eq)]
pub struct Entry<'a> {
    pub mode: u32,
    pub name: &'a [u8],
    pub id: Id,
}

pub fn tree(data: &[u8]) -> Option<Vec<Entry<'_>>> {
    let mut out = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let sp = rest.iter().position(|&b| b == b' ')?;
        let mode = u32::from_str_radix(std::str::from_utf8(&rest[..sp]).ok()?, 8).ok()?;
        let nul = sp + 1 + rest[sp + 1..].iter().position(|&b| b == 0)?;
        let id = rest.get(nul + 1..nul + 21)?.try_into().ok()?;
        out.push(Entry { mode, name: &rest[sp + 1..nul], id });
        rest = &rest[nul + 21..];
    }
    Some(out)
}

struct Pack<D> {
    idx: Idx<D>,
    data: D,
}

pub struct Store<D> {
    packs: Vec<Pack<D>>,
    dirs: Vec<PathBuf>,
    cache: BTreeMap<(usize, u64), Object>,
}

const CACHE: usize = 4096;

fn alternates<S: Source>(src: &S, objects: &Path, dirs: &mut Vec<PathBuf>, depth: usize) {
    if depth > 5 || dirs.iter().any(|d| d == objects) {
        return;
    }
    dirs.push(objects.to_path_buf());
    let Some(list) = src.read(&objects.join("info/alternates")) else { return };
    for line in list.as_ref().split(|&b| b == b'\n') {
        let line = String::from_utf8_lossy(line);
        let line = line.trim();
        if !line.is_empty() && !line.starts_with('#') {
            alternates(src, &objects.join(line), dirs, depth + 1);
        }
    }
}

impl<D: AsRef<[u8]>> Store<D> {
    pub fn open<S: Source<Data = D>>(src: &S, objects: &Path) -> Store<D> {
        let mut dirs = Vec::new();
        alternates(src, objects, &mut dirs, 0);
        let mut packs = Vec::new();
        for dir in &dirs {
            let packdir = dir.join("pack");
            let mut names: Vec<Vec<u8>> = src
                .list(&packdir)
                .unwrap_or_default()
                .into_iter()
                .filter(|(n, k)| *k == Kind::File && n.ends_with(b".idx"))
                .map(|(n, _)| n)
                .collect();
            names.sort();
            for name in names {
                let stem = &name[..name.len() - 4];
                let idx_path = packdir.join(String::from_utf8_lossy(&name).as_ref());
                let pack_path = packdir.join(format!("{}.pack", String::from_utf8_lossy(stem)));
                let (Some(idx), Some(data)) = (src.read(&idx_path).and_then(Idx::parse), src.read(&pack_path)) else { continue };
                let b = data.as_ref();
                let count = b.get(8..12).map(|c| u32::from_be_bytes(c.try_into().unwrap()) as usize);
                if b.starts_with(b"PACK") && matches!(b.get(4..8), Some([0, 0, 0, 2 | 3])) && count == Some(idx.len()) {
                    packs.push(Pack { idx, data });
                }
            }
        }
        Store { packs, dirs, cache: BTreeMap::new() }
    }

    #[cfg(test)]
    pub fn packs_len(&self) -> usize {
        self.packs.len()
    }

    #[cfg(test)]
    pub fn delta_kinds(&self) -> Vec<(usize, usize)> {
        self.packs
            .iter()
            .map(|p| {
                let codes: Vec<u8> = (0..p.idx.len()).filter_map(|i| Some(entry(p.data.as_ref(), p.idx.offset(i)?)?.0)).collect();
                (codes.iter().filter(|&&c| c == 7).count(), codes.iter().filter(|&&c| c == 6).count())
            })
            .collect()
    }

    pub fn packed_count(&self) -> usize {
        self.packs.iter().map(|p| p.idx.len()).sum()
    }

    pub fn read<S: Source<Data = D>>(&mut self, src: &S, id: &Id) -> Option<Object> {
        let found = self.packs.iter().enumerate().find_map(|(i, p)| Some((i, p.idx.offset(p.idx.find(id)?)?)));
        if let Some((pack, offset)) = found {
            return self.unpack(src, pack, offset);
        }
        let hex = to_hex(id);
        self.dirs.iter().find_map(|d| loose(src.read(&d.join(&hex[..2]).join(&hex[2..]))?.as_ref()))
    }

    fn unpack<S: Source<Data = D>>(&mut self, src: &S, pack: usize, offset: u64) -> Option<Object> {
        let mut chain: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut at = offset;
        let mut base = loop {
            if let Some(o) = self.cache.get(&(pack, at)) {
                break o.clone();
            }
            if chain.len() > 10_000 {
                return None;
            }
            let bytes = self.packs[pack].data.as_ref();
            let (code, size, start, delta_base) = entry(bytes, at)?;
            let data = inflate(bytes.get(start..)?, Some(size))?;
            match delta_base {
                None => {
                    let o = Object { kind: Type::from_code(code)?, data };
                    self.remember(pack, at, &o);
                    break o;
                }
                Some(Base::Offset(b)) => {
                    chain.push((at, data));
                    at = b;
                }
                Some(Base::Id(id)) => {
                    chain.push((at, data));
                    break self.read(src, &id)?;
                }
            }
        };
        while let Some((at, delta)) = chain.pop() {
            base = Object { kind: base.kind, data: apply_delta(&base.data, &delta)? };
            self.remember(pack, at, &base);
        }
        Some(base)
    }

    fn remember(&mut self, pack: usize, at: u64, o: &Object) {
        if self.cache.len() >= CACHE {
            self.cache.clear();
        }
        self.cache.insert((pack, at), o.clone());
    }

    pub fn unique_len<S: Source<Data = D>>(&self, src: &S, id: &Id, min: usize) -> usize {
        let common = |other: &Id| -> usize {
            let (a, b) = (to_hex(id), to_hex(other));
            a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count()
        };
        let mut longest = 0;
        for p in &self.packs {
            let i = p.idx.lower_bound(id);
            for j in [i.checked_sub(1), Some(i), Some(i + 1)].into_iter().flatten() {
                if j < p.idx.len() && p.idx.id(j) != *id {
                    longest = longest.max(common(&p.idx.id(j)));
                }
            }
        }
        let hex = to_hex(id);
        for d in &self.dirs {
            for (name, _) in src.list(&d.join(&hex[..2])).unwrap_or_default() {
                let full = [&hex.as_bytes()[..2], &name].concat();
                if let Some(other) = from_hex(&full).filter(|o| o != id) {
                    longest = longest.max(common(&other));
                }
            }
        }
        min.max(longest + 1).min(40)
    }
}
