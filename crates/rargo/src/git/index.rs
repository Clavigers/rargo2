use super::object::Id;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path: Vec<u8>,
    pub mode: u32,
    pub id: Id,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Link {
    pub shared: Id,
    deleted: Vec<usize>,
    replaced: Vec<usize>,
}

#[derive(Debug)]
pub struct Parsed {
    pub entries: Vec<Entry>,
    pub link: Option<Link>,
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at.checked_add(4)?)?.try_into().ok()?))
}

fn offset_varint(b: &[u8], at: &mut usize) -> Option<usize> {
    let mut c = *b.get(*at)?;
    *at += 1;
    let mut v = (c & 0x7f) as usize;
    while c & 0x80 != 0 {
        c = *b.get(*at)?;
        *at += 1;
        v = v.checked_add(1)?.checked_mul(128)? | (c & 0x7f) as usize;
    }
    Some(v)
}

fn ewah(b: &[u8], at: &mut usize) -> Option<Vec<usize>> {
    let bits = u32_at(b, *at)? as usize;
    let words = u32_at(b, *at + 4)? as usize;
    let body = *at + 8;
    let word = |i: usize| -> Option<u64> { Some(u64::from_be_bytes(b.get(body + i * 8..body + i * 8 + 8)?.try_into().ok()?)) };
    let mut set = Vec::new();
    let mut pos = 0usize;
    let mut i = 0;
    while i < words {
        let rlw = word(i)?;
        let run = ((rlw >> 1) & 0xffff_ffff) as usize;
        let literals = (rlw >> 33) as usize;
        if rlw & 1 == 1 {
            set.extend(pos..pos.checked_add(run.checked_mul(64)?)?);
        }
        pos = pos.checked_add(run.checked_mul(64)?)?;
        for l in 0..literals {
            let w = word(i + 1 + l)?;
            set.extend((0..64).filter(|bit| w >> bit & 1 == 1).map(|bit| pos + bit));
            pos += 64;
        }
        i += 1 + literals;
    }
    set.retain(|&p| p < bits);
    *at = body + words * 8 + 4;
    Some(set)
}

pub fn parse(b: &[u8]) -> Option<Parsed> {
    if b.len() < 32 || &b[..4] != b"DIRC" {
        return None;
    }
    let version = u32_at(b, 4)?;
    if !(2..=4).contains(&version) {
        return None;
    }
    let count = u32_at(b, 8)? as usize;
    let end = b.len() - 20;
    let mut at = 12;
    let mut entries = Vec::with_capacity(count.min(1 << 20));
    let mut previous: Vec<u8> = Vec::new();
    for _ in 0..count {
        let start = at;
        let mode = u32_at(b, at + 24)?;
        let id: Id = b.get(at + 40..at + 60)?.try_into().ok()?;
        let flags = u16::from_be_bytes(b.get(at + 60..at + 62)?.try_into().ok()?);
        at += 62;
        if version >= 3 && flags & 0x4000 != 0 {
            at += 2;
        }
        let path = if version == 4 {
            let strip = offset_varint(b, &mut at)?;
            let nul = at + b.get(at..end)?.iter().position(|&c| c == 0)?;
            let mut path = previous.get(..previous.len().checked_sub(strip)?)?.to_vec();
            path.extend_from_slice(&b[at..nul]);
            at = nul + 1;
            path
        } else {
            let nul = at + b.get(at..end)?.iter().position(|&c| c == 0)?;
            let path = b[at..nul].to_vec();
            at = start + (nul - start + 8) / 8 * 8;
            path
        };
        if at > end {
            return None;
        }
        previous = path.clone();
        entries.push(Entry { path, mode, id });
    }
    let mut link = None;
    while at + 8 <= end {
        let sig = &b[at..at + 4];
        let size = u32_at(b, at + 4)? as usize;
        let body = at + 8;
        let next = body.checked_add(size).filter(|&n| n <= end)?;
        if sig == b"link" {
            let shared: Id = b.get(body..body + 20)?.try_into().ok()?;
            let mut p = body + 20;
            let (deleted, replaced) = if p < next { (ewah(b, &mut p)?, ewah(b, &mut p)?) } else { (Vec::new(), Vec::new()) };
            link = Some(Link { shared, deleted, replaced });
        }
        at = next;
    }
    Some(Parsed { entries, link })
}

pub fn merge(split: Parsed, shared: Parsed) -> Option<Vec<Entry>> {
    let Some(link) = split.link else { return Some(split.entries) };
    let replaced: std::collections::BTreeSet<usize> = link.replaced.into_iter().collect();
    let deleted: std::collections::BTreeSet<usize> = link.deleted.into_iter().collect();
    let mut own = split.entries.into_iter();
    let mut out = Vec::new();
    for (i, mut base) in shared.entries.into_iter().enumerate() {
        if replaced.contains(&i) {
            let r = own.next()?;
            if !r.path.is_empty() {
                base.path = r.path;
            }
            base.mode = r.mode;
            base.id = r.id;
        }
        if !deleted.contains(&i) {
            out.push(base);
        }
    }
    out.extend(own);
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Some(out)
}
