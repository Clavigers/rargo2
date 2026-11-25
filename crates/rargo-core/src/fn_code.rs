use crate::hash::Hash;
use std::collections::BTreeMap;

pub const SLOTS: &[u8] = b".rargo_slots";
pub const SLOT_NAMES: &[u8] = b".rargo_slot_names";
pub const FN_IDS: &[u8] = b".rargo_fn_ids";
pub const MARKS_ENV: &str = "RARGO_FN_SLOTS";

const IDENTITY: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Section {
    pub offset: u64,
    pub size: u64,
    pub nobits: bool,
}

fn field(b: &[u8], at: usize, n: usize) -> Option<u64> {
    let bytes = b.get(at..at.checked_add(n)?)?;
    let mut v = [0u8; 8];
    v[..n].copy_from_slice(bytes);
    Some(u64::from_le_bytes(v))
}

pub fn sections(elf: &[u8]) -> Option<BTreeMap<Vec<u8>, Section>> {
    if elf.get(..6)? != b"\x7fELF\x02\x01" {
        return None;
    }
    let shoff = usize::try_from(field(elf, 0x28, 8)?).ok()?;
    let shentsize = field(elf, 0x3a, 2)? as usize;
    let shnum = field(elf, 0x3c, 2)? as usize;
    let shstrndx = field(elf, 0x3e, 2)? as usize;
    if shentsize < 0x40 {
        return None;
    }
    let header = |i: usize| elf.get(shoff.checked_add(i.checked_mul(shentsize)?)?..);
    let strtab = header(shstrndx)?;
    let names_at = usize::try_from(field(strtab, 0x18, 8)?).ok()?;
    let names_len = usize::try_from(field(strtab, 0x20, 8)?).ok()?;
    let names = elf.get(names_at..names_at.checked_add(names_len)?)?;
    let mut out = BTreeMap::new();
    for i in 0..shnum {
        let h = header(i)?;
        let name = names.get(usize::try_from(field(h, 0, 4)?).ok()?..)?;
        let name = &name[..name.iter().position(|b| *b == 0)?];
        let section = Section { offset: field(h, 0x18, 8)?, size: field(h, 0x20, 8)?, nobits: field(h, 4, 4)? == 8 };
        out.insert(name.to_vec(), section);
    }
    Some(out)
}

fn contents<'a>(elf: &'a [u8], s: &Section) -> Option<&'a [u8]> {
    if s.nobits {
        return None;
    }
    let at = usize::try_from(s.offset).ok()?;
    elf.get(at..at.checked_add(usize::try_from(s.size).ok()?)?)
}

pub fn slot_names(table: &[u8]) -> Option<Vec<&[u8]>> {
    if table.is_empty() {
        return Some(Vec::new());
    }
    let body = table.strip_suffix(&[0])?;
    Some(body.split(|b| *b == 0).collect())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Identity {
    Known([u8; IDENTITY]),
    Unknown,
}

pub fn fn_ids(table: &[u8]) -> Option<BTreeMap<&[u8], Identity>> {
    let mut out = BTreeMap::new();
    let mut rest = table;
    while !rest.is_empty() {
        let end = rest.iter().position(|b| *b == 0)?;
        let symbol = &rest[..end];
        let row = rest.get(end + 1..end + 1 + IDENTITY + 1)?;
        let identity = match row[IDENTITY] {
            0 => Identity::Known(row[..IDENTITY].try_into().ok()?),
            1 => Identity::Unknown,
            _ => return None,
        };
        if out.insert(symbol, identity).is_some_and(|before| before != identity) {
            return None;
        }
        rest = &rest[end + 1 + IDENTITY + 1..];
    }
    Some(out)
}

pub struct Binary<'a> {
    pub slots: usize,
    pub names: Vec<&'a [u8]>,
    pub ids: BTreeMap<&'a [u8], Identity>,
}

pub fn binary(elf: &[u8]) -> Option<Binary<'_>> {
    let all = sections(elf)?;
    let slots = usize::try_from(all.get(SLOTS)?.size).ok()?;
    let names = slot_names(contents(elf, all.get(SLOT_NAMES)?)?)?;
    let ids = fn_ids(contents(elf, all.get(FN_IDS)?)?)?;
    (names.len() == slots).then_some(Binary { slots, names, ids })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Executed {
    Code(Vec<(Vec<u8>, [u8; IDENTITY])>),
    NoIdentity(Vec<u8>),
    Unnamed(usize),
    Unmatched,
}

pub fn executed(bin: &Binary<'_>, marks: &[u8]) -> Executed {
    if marks.len() != bin.slots {
        return Executed::Unmatched;
    }
    let mut ran = BTreeMap::new();
    for (slot, mark) in marks.iter().enumerate() {
        if *mark == 0 {
            continue;
        }
        let name = bin.names[slot];
        if name.is_empty() {
            return Executed::Unnamed(slot);
        }
        match bin.ids.get(name) {
            Some(Identity::Known(id)) => {
                ran.insert(name.to_vec(), *id);
            }
            _ => return Executed::NoIdentity(name.to_vec()),
        }
    }
    Executed::Code(ran.into_iter().collect())
}

pub fn encode(ran: &[(Vec<u8>, [u8; IDENTITY])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (symbol, id) in ran {
        out.extend_from_slice(symbol);
        out.push(0);
        out.extend_from_slice(id);
    }
    out
}

pub fn decode(blob: &[u8]) -> Option<Vec<(&[u8], [u8; IDENTITY])>> {
    let mut out = Vec::new();
    let mut rest = blob;
    while !rest.is_empty() {
        let end = rest.iter().position(|b| *b == 0)?;
        let id = rest.get(end + 1..end + 1 + IDENTITY)?;
        out.push((&rest[..end], id.try_into().ok()?));
        rest = &rest[end + 1 + IDENTITY..];
    }
    Some(out)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Holds,
    Changed(Vec<u8>),
}

pub fn still_holds(executed: &[(&[u8], [u8; IDENTITY])], now: &BTreeMap<&[u8], Identity>) -> Verdict {
    for (symbol, id) in executed {
        if now.get(symbol) != Some(&Identity::Known(*id)) {
            return Verdict::Changed(symbol.to_vec());
        }
    }
    Verdict::Holds
}

pub fn executed_hash(ran: &[(Vec<u8>, [u8; IDENTITY])]) -> Hash {
    Hash::of(&encode(ran))
}

#[cfg(test)]
pub mod fixture {
    pub struct Function {
        pub symbol: &'static str,
        pub identity: Option<[u8; 32]>,
    }

    pub fn id(n: u8) -> Option<[u8; 32]> {
        Some([n; 32])
    }

    pub fn elf(functions: &[Function], with_sections: bool) -> Vec<u8> {
        let page = 4096;
        let slots = functions.len().next_multiple_of(page).max(page);
        let mut names = Vec::new();
        for i in 0..slots {
            if let Some(f) = functions.get(i) {
                names.extend_from_slice(f.symbol.as_bytes());
            }
            names.push(0);
        }
        let mut ids = Vec::new();
        for f in functions {
            ids.extend_from_slice(f.symbol.as_bytes());
            ids.push(0);
            ids.extend_from_slice(&f.identity.unwrap_or([0; 32]));
            ids.push(f.identity.is_none() as u8);
        }
        let text = b"\x55\x48\x89\xe5\xc3".repeat(8);
        let mut sections: Vec<(&[u8], u32, Vec<u8>, u64)> = vec![(b".text", 1, text, 0)];
        if with_sections {
            sections.push((super::SLOTS, 8, Vec::new(), slots as u64));
            sections.push((super::SLOT_NAMES, 1, names, 0));
            sections.push((super::FN_IDS, 1, ids, 0));
        }
        let mut shstrtab = vec![0u8];
        let mut name_at = Vec::new();
        for (name, ..) in &sections {
            name_at.push(shstrtab.len() as u32);
            shstrtab.extend_from_slice(name);
            shstrtab.push(0);
        }
        let strtab_name = shstrtab.len() as u32;
        shstrtab.extend_from_slice(b".shstrtab\0");
        let mut out = vec![0u8; 64];
        let mut placed = Vec::new();
        for (_, kind, body, size) in &sections {
            let at = out.len() as u64;
            out.extend_from_slice(body);
            placed.push((at, if *kind == 8 { *size } else { body.len() as u64 }));
        }
        let strtab_at = out.len() as u64;
        out.extend_from_slice(&shstrtab);
        while out.len() % 8 != 0 {
            out.push(0);
        }
        let shoff = out.len() as u64;
        let header = |name: u32, kind: u32, offset: u64, size: u64| {
            let mut h = vec![0u8; 64];
            h[0..4].copy_from_slice(&name.to_le_bytes());
            h[4..8].copy_from_slice(&kind.to_le_bytes());
            h[0x18..0x20].copy_from_slice(&offset.to_le_bytes());
            h[0x20..0x28].copy_from_slice(&size.to_le_bytes());
            h
        };
        out.extend_from_slice(&[0u8; 64]);
        for (i, (_, kind, ..)) in sections.iter().enumerate() {
            out.extend_from_slice(&header(name_at[i], *kind, placed[i].0, placed[i].1));
        }
        out.extend_from_slice(&header(strtab_name, 3, strtab_at, shstrtab.len() as u64));
        let count = sections.len() as u16 + 2;
        out[0..6].copy_from_slice(b"\x7fELF\x02\x01");
        out[6] = 1;
        out[0x10..0x12].copy_from_slice(&3u16.to_le_bytes());
        out[0x12..0x14].copy_from_slice(&62u16.to_le_bytes());
        out[0x28..0x30].copy_from_slice(&shoff.to_le_bytes());
        out[0x34..0x36].copy_from_slice(&64u16.to_le_bytes());
        out[0x3a..0x3c].copy_from_slice(&64u16.to_le_bytes());
        out[0x3c..0x3e].copy_from_slice(&count.to_le_bytes());
        out[0x3e..0x40].copy_from_slice(&(count - 1).to_le_bytes());
        out
    }

    pub fn marks(functions: &[Function], ran: &[&str]) -> Vec<u8> {
        let mut m = vec![0u8; functions.len().next_multiple_of(4096).max(4096)];
        for (i, f) in functions.iter().enumerate() {
            if ran.contains(&f.symbol) {
                m[i] = 1;
            }
        }
        m
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{elf, id, marks, Function};
    use super::*;

    fn three() -> Vec<Function> {
        vec![
            Function { symbol: "_ZN3app5parse17h01E", identity: id(1) },
            Function { symbol: "_ZN3app6render17h02E", identity: id(2) },
            Function { symbol: "_ZN3app4test17h03E", identity: id(3) },
        ]
    }

    #[test]
    fn a_binary_s_marks_join_its_slot_names_and_identities_by_symbol() {
        let fns = three();
        let bytes = elf(&fns, true);
        let bin = binary(&bytes).expect("the fixture has the three sections");
        assert_eq!(bin.slots, 4096);
        assert_eq!(bin.names.iter().filter(|n| !n.is_empty()).count(), 3);
        let ran = executed(&bin, &marks(&fns, &["_ZN3app6render17h02E", "_ZN3app4test17h03E"]));
        let Executed::Code(pairs) = ran else { panic!("{ran:?}") };
        assert_eq!(
            pairs,
            vec![(b"_ZN3app4test17h03E".to_vec(), [3; 32]), (b"_ZN3app6render17h02E".to_vec(), [2; 32])],
            "sorted by symbol, only what ran"
        );
        assert_eq!(decode(&encode(&pairs)).unwrap(), pairs.iter().map(|(s, i)| (s.as_slice(), *i)).collect::<Vec<_>>());
    }

    #[test]
    fn an_execution_holds_until_a_function_it_ran_changes_identity() {
        let fns = three();
        let bytes = elf(&fns, true);
        let Executed::Code(pairs) = executed(&binary(&bytes).unwrap(), &marks(&fns, &["_ZN3app5parse17h01E"])) else {
            panic!()
        };
        let blob = encode(&pairs);
        let recorded = decode(&blob).unwrap();

        let mut other = three();
        other[1].identity = id(9);
        let relinked = elf(&other, true);
        assert_eq!(still_holds(&recorded, &binary(&relinked).unwrap().ids), Verdict::Holds, "a function it did not run changed");

        let mut changed = three();
        changed[0].identity = id(7);
        let rebuilt = elf(&changed, true);
        assert_eq!(
            still_holds(&recorded, &binary(&rebuilt).unwrap().ids),
            Verdict::Changed(b"_ZN3app5parse17h01E".to_vec())
        );

        let mut gone = three();
        gone.remove(0);
        assert_eq!(
            still_holds(&recorded, &binary(&elf(&gone, true)).unwrap().ids),
            Verdict::Changed(b"_ZN3app5parse17h01E".to_vec()),
            "a function that is no longer there changed"
        );
    }

    #[test]
    fn a_function_without_identity_or_a_binary_without_sections_falls_back() {
        let mut fns = three();
        fns[2].identity = None;
        let bytes = elf(&fns, true);
        let bin = binary(&bytes).unwrap();
        assert_eq!(executed(&bin, &marks(&fns, &["_ZN3app5parse17h01E"])), Executed::Code(vec![(b"_ZN3app5parse17h01E".to_vec(), [1; 32])]));
        assert_eq!(executed(&bin, &marks(&fns, &["_ZN3app4test17h03E"])), Executed::NoIdentity(b"_ZN3app4test17h03E".to_vec()));
        let mut padded = marks(&fns, &[]);
        padded[100] = 1;
        assert_eq!(executed(&bin, &padded), Executed::Unnamed(100), "a mark on a padding slot is not trusted");
        assert_eq!(executed(&bin, &[1, 0, 0]), Executed::Unmatched, "marks of another binary");
        assert!(binary(&elf(&fns, false)).is_none(), "no sections, no selection");
        assert!(binary(b"#!/bin/sh\necho ok\n").is_none());
        assert!(sections(&[0x7f, b'E', b'L', b'F', 2, 1, 0]).is_none(), "a truncated header");
    }

    #[test]
    fn tables_that_do_not_follow_the_contract_are_refused() {
        assert_eq!(slot_names(b"a\0b\0").unwrap(), vec![b"a".as_slice(), b"b".as_slice()]);
        assert!(slot_names(b"a\0b").is_none(), "an unterminated name");
        let mut row = b"f\0".to_vec();
        row.extend_from_slice(&[0; 32]);
        row.push(2);
        assert!(fn_ids(&row).is_none(), "a flag that is neither 0 nor 1");
        let mut twice = b"f\0".to_vec();
        twice.extend_from_slice(&[1; 32]);
        twice.push(0);
        twice.extend_from_slice(b"f\0");
        twice.extend_from_slice(&[2; 32]);
        twice.push(0);
        assert!(fn_ids(&twice).is_none(), "one symbol with two identities");
        let mut nul_in_id = b"g\0".to_vec();
        nul_in_id.extend_from_slice(&[0; 32]);
        nul_in_id.push(0);
        assert_eq!(fn_ids(&nul_in_id).unwrap().get(b"g".as_slice()), Some(&Identity::Known([0; 32])), "an identity may hold zero bytes");
    }
}
