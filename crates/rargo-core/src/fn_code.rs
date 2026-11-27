use crate::hash::{Hash, KeyHasher};
use crate::view::{Obs, VPath};
use std::collections::BTreeMap;

pub const SLOTS: &[u8] = b".rargo_slots";
pub const SLOT_NAMES: &[u8] = b".rargo_slot_names";
pub const FN_IDS: &[u8] = b".rargo_fn_ids";
pub const MARKS_ENV: &[u8] = b"RARGO_FN_SLOTS";
pub const MARKS_DIR: &[u8] = b"/.rargo-fn-slots";
pub const MARKS_FILE: &[u8] = b"/.rargo-fn-slots/marks";
pub const FLAGS: [&str; 4] = ["-Z", "rargo-fn-slots", "-Z", "rargo-fn-ids"];

const IDENTITY: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Section {
    pub offset: u64,
    pub size: u64,
    pub nobits: bool,
    pub code: bool,
}

fn field(b: &[u8], at: usize, n: usize) -> Option<u64> {
    let bytes = b.get(at..at.checked_add(n)?)?;
    let mut v = [0u8; 8];
    v[..n].copy_from_slice(bytes);
    Some(u64::from_le_bytes(v))
}

pub type Read<'a> = &'a dyn Fn(u64, usize) -> Option<Vec<u8>>;

fn from_slice(bytes: &[u8]) -> impl Fn(u64, usize) -> Option<Vec<u8>> + '_ {
    move |at, len| {
        let at = usize::try_from(at).ok()?;
        bytes.get(at..at.checked_add(len)?).map(<[u8]>::to_vec)
    }
}

pub fn sections_by(read: Read<'_>) -> Option<BTreeMap<Vec<u8>, Section>> {
    let head = read(0, 64)?;
    if head.get(..6)? != b"\x7fELF\x02\x01" {
        return None;
    }
    let shoff = field(&head, 0x28, 8)?;
    let shentsize = field(&head, 0x3a, 2)? as usize;
    let shnum = field(&head, 0x3c, 2)? as usize;
    let shstrndx = field(&head, 0x3e, 2)? as usize;
    if shentsize < 0x40 || shstrndx >= shnum {
        return None;
    }
    let headers = read(shoff, shentsize.checked_mul(shnum)?)?;
    let header = |i: usize| headers.get(i * shentsize..(i + 1) * shentsize);
    let strtab = header(shstrndx)?;
    let names = read(field(strtab, 0x18, 8)?, usize::try_from(field(strtab, 0x20, 8)?).ok()?)?;
    let mut out = BTreeMap::new();
    for i in 0..shnum {
        let h = header(i)?;
        let name = names.get(usize::try_from(field(h, 0, 4)?).ok()?..)?;
        let name = &name[..name.iter().position(|b| *b == 0)?];
        let section = Section {
            offset: field(h, 0x18, 8)?,
            size: field(h, 0x20, 8)?,
            nobits: field(h, 4, 4)? == 8,
            code: field(h, 8, 8)? & 4 != 0,
        };
        out.insert(name.to_vec(), section);
    }
    Some(out)
}

pub fn sections(elf: &[u8]) -> Option<BTreeMap<Vec<u8>, Section>> {
    sections_by(&from_slice(elf))
}

fn contents(read: Read<'_>, s: &Section) -> Option<Vec<u8>> {
    if s.nobits {
        return None;
    }
    read(s.offset, usize::try_from(s.size).ok()?)
}

pub fn slot_names(table: &[u8]) -> Option<Vec<Vec<u8>>> {
    if table.is_empty() {
        return Some(Vec::new());
    }
    let body = table.strip_suffix(&[0])?;
    Some(body.split(|b| *b == 0).map(<[u8]>::to_vec).collect())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Identity {
    Known([u8; IDENTITY]),
    Unknown,
}

pub fn fn_ids(table: &[u8]) -> Option<BTreeMap<Vec<u8>, Identity>> {
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
        if out.insert(symbol.to_vec(), identity).is_some_and(|before| before != identity) {
            return None;
        }
        rest = &rest[end + 1 + IDENTITY + 1..];
    }
    Some(out)
}

#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CodeTable {
    pub slots: usize,
    pub names: Vec<Vec<u8>>,
    pub ids: BTreeMap<Vec<u8>, Identity>,
}

pub fn table_by(read: Read<'_>) -> Option<CodeTable> {
    let all = sections_by(read)?;
    let slots = usize::try_from(all.get(SLOTS)?.size).ok()?;
    let names = slot_names(&contents(read, all.get(SLOT_NAMES)?)?)?;
    let ids = fn_ids(&contents(read, all.get(FN_IDS)?)?)?;
    (names.len() == slots).then_some(CodeTable { slots, names, ids })
}

pub fn table(elf: &[u8]) -> Option<CodeTable> {
    table_by(&from_slice(elf))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Executed {
    Code(Vec<(Vec<u8>, [u8; IDENTITY])>),
    NoIdentity(Vec<u8>),
    Unnamed(usize),
    Unmatched,
}

pub fn executed(table: &CodeTable, marks: &[u8]) -> Executed {
    if marks.len() != table.slots {
        return Executed::Unmatched;
    }
    let mut ran = BTreeMap::new();
    for (slot, mark) in marks.iter().enumerate() {
        if *mark == 0 {
            continue;
        }
        let name = &table.names[slot];
        if name.is_empty() {
            return Executed::Unnamed(slot);
        }
        match table.ids.get(name) {
            Some(Identity::Known(id)) => {
                ran.insert(name.clone(), *id);
            }
            _ => return Executed::NoIdentity(name.clone()),
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
    NotAFile,
    NoTable,
    RestChanged,
    FunctionChanged(Vec<u8>),
    Unreadable,
}

impl Verdict {
    pub fn reason(&self) -> String {
        match self {
            Verdict::Holds => "unchanged".into(),
            Verdict::NotAFile => "the test binary is no longer an executable file".into(),
            Verdict::NoTable => "the test binary has no function identities".into(),
            Verdict::RestChanged => "rest changed".into(),
            Verdict::FunctionChanged(s) => format!("function changed: {}", String::from_utf8_lossy(s)),
            Verdict::Unreadable => "the recorded functions are no longer in the store".into(),
        }
    }
}

pub fn verdict(executed: Option<&[u8]>, rest: &Hash, now: Option<(&CodeTable, Hash)>) -> Verdict {
    let Some((table, now_rest)) = now else { return Verdict::NoTable };
    if now_rest != *rest {
        return Verdict::RestChanged;
    }
    let Some(ran) = executed.and_then(decode) else { return Verdict::Unreadable };
    for (symbol, id) in ran {
        if table.ids.get(symbol) != Some(&Identity::Known(id)) {
            return Verdict::FunctionChanged(symbol.to_vec());
        }
    }
    Verdict::Holds
}

fn codegen_opts(args: &[String]) -> impl Iterator<Item = &str> {
    args.iter().enumerate().filter_map(|(i, a)| match a.strip_prefix("-C") {
        Some("") => args.get(i + 1).map(String::as_str),
        Some(v) => Some(v),
        None => a.strip_prefix("--codegen=").or_else(|| (i > 0 && args[i - 1] == "--codegen").then_some(a.as_str())),
    })
}

pub fn debug_assertions(args: &[String]) -> bool {
    let mut stated = None;
    let mut opt = "0";
    for o in codegen_opts(args) {
        if o == "debug-assertions" {
            stated = Some(true);
        } else if let Some(v) = o.strip_prefix("debug-assertions=") {
            stated = Some(matches!(v, "on" | "yes" | "y" | "true"));
        } else if let Some(v) = o.strip_prefix("opt-level=") {
            opt = v;
        }
    }
    stated.unwrap_or(opt == "0")
}

pub fn is_link_input(path: &[u8]) -> bool {
    let name = path.rsplit(|b| *b == b'/').next().unwrap_or(path);
    [b".rlib".as_slice(), b".a", b".o", b".so"].iter().any(|ext| name.ends_with(ext))
        || name.windows(4).any(|w| w == b".so.")
}

pub fn object_tracked(object: &[u8]) -> bool {
    sections(object).is_some_and(|s| s.contains_key(SLOTS) || !s.values().any(|v| v.code && v.size > 0))
}

pub fn linked_tracked(input: &[u8]) -> bool {
    match crate::ar::members(input) {
        Some(members) => members.iter().all(|m| !m.data.starts_with(b"\x7fELF") || object_tracked(m.data)),
        None => object_tracked(input),
    }
}

pub fn link_rest(key: &Hash, obs: &[(VPath, Obs)], tracked: &dyn Fn(&Hash) -> bool) -> Hash {
    let mut contents = Vec::new();
    let mut h = KeyHasher::new("fn-code-rest");
    h.field(&key.0);
    for (path, o) in obs {
        if !is_link_input(path) {
            continue;
        }
        match o {
            Obs::File { hash, .. } if tracked(hash) => {}
            Obs::File { hash, .. } => contents.push(hash.0),
            _ => {
                h.field(path).field(&bincode::serialize(o).expect("an observation serialises"));
            }
        }
    }
    contents.sort();
    for c in contents {
        h.field(&c);
    }
    h.finish()
}

pub fn program(argv0: &[u8], cwd: &[u8]) -> Vec<u8> {
    if argv0.starts_with(b"/") {
        return argv0.to_vec();
    }
    let rel = argv0.strip_prefix(b"./").unwrap_or(argv0);
    let mut p = cwd.strip_suffix(b"/").unwrap_or(cwd).to_vec();
    p.push(b'/');
    p.extend_from_slice(rel);
    p
}

pub fn binary_read(obs: &[(VPath, Obs)], program: &[u8]) -> Option<Hash> {
    obs.iter().find_map(|(p, o)| match o {
        Obs::File { hash, exec: true } if p[..] == *program => Some(*hash),
        _ => None,
    })
}

pub fn recode(obs: &mut [(VPath, Obs)], program: &[u8], executed: Hash, rest: Hash) -> bool {
    for (p, o) in obs.iter_mut() {
        if p[..] == *program && matches!(o, Obs::File { exec: true, .. }) {
            *o = Obs::Code { executed, rest };
            return true;
        }
    }
    false
}

pub fn marks_file(side: &[(Vec<u8>, crate::trace::MEntry)]) -> Option<Hash> {
    side.iter().find_map(|(p, e)| match e {
        crate::trace::MEntry::File { hash, .. } if p == MARKS_FILE => Some(*hash),
        _ => None,
    })
}

pub fn wants_marks(spec: &crate::trace::ActionSpec) -> bool {
    spec.env.get(MARKS_ENV).is_some_and(|v| v == MARKS_FILE)
}

pub fn mark(spec: &mut crate::trace::ActionSpec) {
    spec.env.insert(MARKS_ENV.to_vec(), MARKS_FILE.to_vec());
    spec.mkdirs.push(MARKS_DIR.to_vec());
    spec.discard.push(MARKS_DIR.to_vec());
}

pub fn drift_line(label: &str, functions: &[String], failure: &str) -> String {
    let shown = functions.iter().take(8).cloned().collect::<Vec<_>>().join(", ");
    let more = if functions.len() > 8 { format!(" and {} more", functions.len() - 8) } else { String::new() };
    format!(
        "{label}: kept as a pass by the {} function(s) it executed ({shown}{more}), but a real run fails ({failure})",
        functions.len()
    )
}

pub fn sample(key: &Hash, created: u64, one_in: u64) -> bool {
    let mut h = KeyHasher::new("fn-code-drift-sample");
    h.field(&key.0).u64(created);
    one_in > 0 && u64::from_le_bytes(h.finish().0[..8].try_into().expect("eight bytes")) % one_in == 0
}

pub mod fixture {
    pub struct Function {
        pub symbol: &'static str,
        pub identity: Option<[u8; 32]>,
    }

    pub fn id(n: u8) -> Option<[u8; 32]> {
        Some([n; 32])
    }

    const MARKS_WRITER: [u8; 116] = [
        0x48, 0x8b, 0x1c, 0x24, 0x48, 0x8d, 0x5c, 0xdc, 0x10, 0x48, 0x8b, 0x3b, 0x48, 0x85, 0xff, 0x74, 0x3d, 0x48, 0x83,
        0xc3, 0x08, 0x48, 0x8d, 0x35, 0x40, 0x00, 0x00, 0x00, 0xb9, 0x0f, 0x00, 0x00, 0x00, 0xf3, 0xa6, 0x75, 0xe4, 0xb8,
        0x02, 0x00, 0x00, 0x00, 0xbe, 0x41, 0x02, 0x00, 0x00, 0xba, 0xa4, 0x01, 0x00, 0x00, 0x0f, 0x05, 0x48, 0x89, 0xc7,
        0xb8, 0x01, 0x00, 0x00, 0x00, 0x48, 0x8d, 0x35, 0x2f, 0x00, 0x00, 0x00, 0x48, 0x8b, 0x15, 0x1f, 0x00, 0x00, 0x00,
        0x0f, 0x05, 0xb8, 0x3c, 0x00, 0x00, 0x00, 0x0f, 0xb6, 0x3d, 0x19, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x52, 0x41, 0x52,
        0x47, 0x4f, 0x5f, 0x46, 0x4e, 0x5f, 0x53, 0x4c, 0x4f, 0x54, 0x53, 0x3d, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00,
    ];
    const MARKS_LEN_AT: usize = 107;
    const EXIT_AT: usize = 115;
    const BASE: u64 = 0x40_0000;
    const TEXT_AT: usize = 120;

    pub struct Run<'a> {
        pub ran: &'a [&'a str],
        pub exit: u8,
        pub pad: &'a [u8],
    }

    pub fn elf(functions: &[Function], with_sections: bool) -> Vec<u8> {
        build(functions, with_sections, None)
    }

    pub fn program(functions: &[Function], with_sections: bool, run: &Run<'_>) -> Vec<u8> {
        build(functions, with_sections, Some(run))
    }

    fn build(functions: &[Function], with_sections: bool, run: Option<&Run<'_>>) -> Vec<u8> {
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
        let text = match run {
            Some(run) => {
                let m = marks(functions, run.ran);
                let mut t = MARKS_WRITER.to_vec();
                t[MARKS_LEN_AT..MARKS_LEN_AT + 8].copy_from_slice(&(m.len() as u64).to_le_bytes());
                t[EXIT_AT] = run.exit;
                t.extend_from_slice(&m);
                t.extend_from_slice(run.pad);
                t
            }
            None => b"\x55\x48\x89\xe5\xc3".repeat(8),
        };
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
        let mut out = vec![0u8; if run.is_some() { TEXT_AT } else { 64 }];
        let mut placed = Vec::new();
        for (_, kind, body, size) in &sections {
            let at = out.len() as u64;
            out.extend_from_slice(body);
            placed.push((at, if *kind == 8 { *size } else { body.len() as u64 }));
        }
        let strtab_at = out.len() as u64;
        out.extend_from_slice(&shstrtab);
        while !out.len().is_multiple_of(8) {
            out.push(0);
        }
        let shoff = out.len() as u64;
        let header = |name: u32, kind: u32, flags: u64, offset: u64, size: u64| {
            let mut h = vec![0u8; 64];
            h[0..4].copy_from_slice(&name.to_le_bytes());
            h[4..8].copy_from_slice(&kind.to_le_bytes());
            h[8..16].copy_from_slice(&flags.to_le_bytes());
            h[0x18..0x20].copy_from_slice(&offset.to_le_bytes());
            h[0x20..0x28].copy_from_slice(&size.to_le_bytes());
            h
        };
        out.extend_from_slice(&[0u8; 64]);
        for (i, (_, kind, ..)) in sections.iter().enumerate() {
            let flags = if i == 0 && run.is_some() { 6 } else { 0 };
            out.extend_from_slice(&header(name_at[i], *kind, flags, placed[i].0, placed[i].1));
        }
        out.extend_from_slice(&header(strtab_name, 3, 0, strtab_at, shstrtab.len() as u64));
        let count = sections.len() as u16 + 2;
        let set = |out: &mut Vec<u8>, at: usize, v: u64, n: usize| out[at..at + n].copy_from_slice(&v.to_le_bytes()[..n]);
        out[0..6].copy_from_slice(b"\x7fELF\x02\x01");
        out[6] = 1;
        set(&mut out, 0x12, 62, 2);
        set(&mut out, 0x14, 1, 4);
        set(&mut out, 0x28, shoff, 8);
        set(&mut out, 0x34, 64, 2);
        set(&mut out, 0x3a, 64, 2);
        set(&mut out, 0x3c, count as u64, 2);
        set(&mut out, 0x3e, count as u64 - 1, 2);
        if run.is_some() {
            let len = out.len() as u64;
            set(&mut out, 0x10, 2, 2);
            set(&mut out, 0x18, BASE + TEXT_AT as u64, 8);
            set(&mut out, 0x20, 64, 8);
            set(&mut out, 0x36, 56, 2);
            set(&mut out, 0x38, 1, 2);
            set(&mut out, 64, 1, 4);
            set(&mut out, 68, 5, 4);
            set(&mut out, 80, BASE, 8);
            set(&mut out, 88, BASE, 8);
            set(&mut out, 96, len, 8);
            set(&mut out, 104, len, 8);
            set(&mut out, 112, 0x1000, 8);
        } else {
            set(&mut out, 0x10, 3, 2);
        }
        out
    }

    /// What a program built by `program` does when it runs: the marks it writes to the file `RARGO_FN_SLOTS` names, and its exit status.
    pub fn run_of(program: &[u8]) -> Option<(&[u8], u8)> {
        let text = program.get(TEXT_AT..)?;
        if text.get(..MARKS_LEN_AT)? != &MARKS_WRITER[..MARKS_LEN_AT] {
            return None;
        }
        let len = u64::from_le_bytes(text.get(MARKS_LEN_AT..MARKS_LEN_AT + 8)?.try_into().ok()?);
        let marks = text.get(MARKS_WRITER.len()..MARKS_WRITER.len().checked_add(usize::try_from(len).ok()?)?)?;
        Some((marks, *text.get(EXIT_AT)?))
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

    pub fn ar(members: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut longnames = Vec::new();
        let mut entries = Vec::new();
        for (name, data) in members {
            entries.push((format!("/{}", longnames.len()), data.clone()));
            longnames.extend_from_slice(format!("{name}/\n").as_bytes());
        }
        let mut out = b"!<arch>\n".to_vec();
        for (name, data) in std::iter::once(("//".to_string(), longnames)).chain(entries) {
            out.extend_from_slice(format!("{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n", name, 0, 0, 0, 644, data.len()).as_bytes());
            out.extend_from_slice(&data);
            if data.len() % 2 == 1 {
                out.push(b'\n');
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{ar, elf, id, marks, Function};
    use super::*;
    use crate::view::Obs;

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
        let t = table(&elf(&fns, true)).expect("the fixture has the three sections");
        assert_eq!(t.slots, 4096);
        assert_eq!(t.names.iter().filter(|n| !n.is_empty()).count(), 3);
        let ran = executed(&t, &marks(&fns, &["_ZN3app6render17h02E", "_ZN3app4test17h03E"]));
        let Executed::Code(pairs) = ran else { panic!("{ran:?}") };
        assert_eq!(
            pairs,
            vec![(b"_ZN3app4test17h03E".to_vec(), [3; 32]), (b"_ZN3app6render17h02E".to_vec(), [2; 32])],
            "sorted by symbol, only what ran"
        );
        assert_eq!(decode(&encode(&pairs)).unwrap(), pairs.iter().map(|(s, i)| (s.as_slice(), *i)).collect::<Vec<_>>());
    }

    #[test]
    fn a_function_without_identity_or_a_binary_without_sections_falls_back() {
        let mut fns = three();
        fns[2].identity = None;
        let t = table(&elf(&fns, true)).unwrap();
        assert_eq!(executed(&t, &marks(&fns, &["_ZN3app5parse17h01E"])), Executed::Code(vec![(b"_ZN3app5parse17h01E".to_vec(), [1; 32])]));
        assert_eq!(executed(&t, &marks(&fns, &["_ZN3app4test17h03E"])), Executed::NoIdentity(b"_ZN3app4test17h03E".to_vec()));
        let mut padded = marks(&fns, &[]);
        padded[100] = 1;
        assert_eq!(executed(&t, &padded), Executed::Unnamed(100), "a mark on a padding slot is not trusted");
        assert_eq!(executed(&t, &[1, 0, 0]), Executed::Unmatched, "marks of another binary");
        assert!(table(&elf(&fns, false)).is_none(), "no sections, no selection");
        assert!(table(b"#!/bin/sh\necho ok\n").is_none());
        assert!(sections(&[0x7f, b'E', b'L', b'F', 2, 1, 0]).is_none(), "a truncated header");
    }

    #[test]
    fn tables_that_do_not_follow_the_contract_are_refused() {
        assert_eq!(slot_names(b"a\0b\0").unwrap(), vec![b"a".to_vec(), b"b".to_vec()]);
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

    struct Run {
        executed: Vec<u8>,
        rest: Hash,
    }

    fn record(fns: &[Function], rest: Hash, ran: &[&str]) -> Option<Run> {
        match executed(&table(&elf(fns, true))?, &marks(fns, ran)) {
            Executed::Code(pairs) => Some(Run { executed: encode(&pairs), rest }),
            _ => None,
        }
    }

    fn reruns(runs: &[(&str, &Run)], fns: &[Function], rest: Hash) -> Vec<String> {
        let t = table(&elf(fns, true));
        runs.iter()
            .filter(|(_, r)| verdict(Some(&r.executed), &r.rest, t.as_ref().map(|t| (t, rest))) != Verdict::Holds)
            .map(|(n, _)| n.to_string())
            .collect()
    }

    fn suite(parse: u8, render: u8, shared: u8, unused: u8) -> Vec<Function> {
        vec![
            Function { symbol: "_RNv3app5parse", identity: id(parse) },
            Function { symbol: "_RNv3app6render", identity: id(render) },
            Function { symbol: "_RNv3app6shared", identity: id(shared) },
            Function { symbol: "_RNv3app6unused", identity: id(unused) },
            Function { symbol: "_RNv5tests1a", identity: id(50) },
            Function { symbol: "_RNv5tests1b", identity: id(51) },
            Function { symbol: "_RNv5tests1c", identity: id(52) },
        ]
    }

    #[test]
    fn three_tests_rerun_exactly_when_code_they_executed_or_the_rest_changed() {
        let rest = Hash::of(b"rest");
        let base = suite(1, 2, 3, 4);
        let a = record(&base, rest, &["_RNv5tests1a", "_RNv3app5parse", "_RNv3app6shared"]).unwrap();
        let b = record(&base, rest, &["_RNv5tests1b", "_RNv3app6render", "_RNv3app6shared"]).unwrap();
        let c = record(&base, rest, &["_RNv5tests1c", "_RNv3app6shared"]).unwrap();
        let runs = [("a", &a), ("b", &b), ("c", &c)];

        assert_eq!(reruns(&runs, &base, rest), Vec::<String>::new());
        assert_eq!(reruns(&runs, &suite(9, 2, 3, 4), rest), vec!["a"], "one test's function");
        assert_eq!(reruns(&runs, &suite(1, 2, 9, 4), rest), vec!["a", "b", "c"], "a function all three run");
        assert_eq!(reruns(&runs, &suite(1, 2, 3, 9), rest), Vec::<String>::new(), "a function none runs, relinked");
        assert_eq!(reruns(&runs, &suite(1, 2, 3, 4), rest), Vec::<String>::new(), "the change undone");
        assert_eq!(reruns(&runs, &base, Hash::of(b"another native object")), vec!["a", "b", "c"], "the rest");
        let mut renamed = suite(1, 2, 3, 4);
        renamed[1].symbol = "_RNv3app7render2";
        assert_eq!(reruns(&runs, &renamed, rest), vec!["b"], "a symbol that is gone");
        let none = verdict(Some(&a.executed), &a.rest, None);
        assert_eq!(none, Verdict::NoTable, "a binary without sections");
        assert_eq!(verdict(None, &a.rest, table(&elf(&base, true)).as_ref().map(|t| (t, rest))), Verdict::Unreadable);
        let mut flagged = suite(1, 2, 3, 4);
        flagged[0].identity = None;
        assert!(record(&flagged, rest, &["_RNv5tests1a", "_RNv3app5parse"]).is_none(), "a run of a function without identity keeps the binary read");
        assert_eq!(Verdict::FunctionChanged(b"_RNv3app5parse".to_vec()).reason(), "function changed: _RNv3app5parse");
        assert_eq!(Verdict::RestChanged.reason(), "rest changed");
    }

    #[test]
    fn debug_assertions_follow_the_flag_and_otherwise_the_opt_level() {
        let args = |s: &str| s.split(' ').filter(|a| !a.is_empty()).map(String::from).collect::<Vec<_>>();
        assert!(debug_assertions(&args("--crate-name x src/lib.rs")));
        assert!(debug_assertions(&args("-C opt-level=0")));
        assert!(!debug_assertions(&args("-C opt-level=3")));
        assert!(debug_assertions(&args("-C opt-level=3 -C debug-assertions=on")));
        assert!(debug_assertions(&args("-Copt-level=3 -Cdebug-assertions")));
        assert!(!debug_assertions(&args("-C debug-assertions=off")));
        assert!(!debug_assertions(&args("--codegen opt-level=2")));
        assert!(debug_assertions(&args("-C opt-level=3 -C opt-level=0")), "the last one counts");
    }

    #[test]
    fn the_rest_hashes_link_inputs_other_than_slotted_rlibs_and_the_command() {
        let key = Hash::of(b"rustc --test");
        let file = |p: &str, n: u8| (VPath::from(p.as_bytes()), Obs::File { hash: Hash([n; 32]), exec: false });
        let moved = |p: &str, n: u8| file(&p.replace("s-1", "s-2"), n);
        let obs = |app: u8, native: u8, src: u8, std: u8| {
            vec![
                file("/w/target/debug/deps/libapp-1.rlib", app),
                file("/w/target/debug/build/app-2/out/libnative.a", native),
                file("/w/src/lib.rs", src),
                file("/nix/store/rust/lib/rustlib/x86_64-unknown-linux-gnu/lib/libstd-3.rlib", std),
            ]
        };
        let slotted = |h: &Hash| h.0[0] < 100;
        let base = link_rest(&key, &obs(1, 101, 1, 200), &slotted);
        assert_eq!(link_rest(&key, &obs(2, 101, 2, 200), &slotted), base, "a slotted rlib and a source file");
        assert_ne!(link_rest(&key, &obs(1, 102, 1, 200), &slotted), base, "a native library");
        assert_ne!(link_rest(&key, &obs(1, 101, 1, 201), &slotted), base, "the standard library");
        assert_ne!(link_rest(&Hash::of(b"rustc --test -C lto"), &obs(1, 101, 1, 200), &slotted), base, "the command");
        let session = |n: u8, f: &dyn Fn(&str, u8) -> (VPath, Obs)| vec![f("/w/target/debug/incremental/t-1/s-1/t.o", n)];
        assert_eq!(link_rest(&key, &session(150, &file), &slotted), link_rest(&key, &session(150, &moved), &slotted), "an object in a new session directory");
        let absent = vec![(VPath::from(b"/nix/store/x/lib/libz.a".as_slice()), Obs::Absent)];
        assert_ne!(link_rest(&key, &absent, &slotted), link_rest(&key, &[], &slotted), "a library the linker looked for");
        assert!(is_link_input(b"/nix/store/glibc/lib/libc.so.6") && is_link_input(b"/x/crt1.o") && !is_link_input(b"/w/src/so.rs"));
    }

    fn elf_with_code() -> Vec<u8> {
        let mut bytes = elf(&[], false);
        let shoff = u64::from_le_bytes(bytes[0x28..0x30].try_into().unwrap()) as usize;
        bytes[shoff + 64 + 8] = 6;
        bytes
    }

    #[test]
    fn a_link_input_is_tracked_when_all_its_code_has_slots() {
        let unit = elf(&three(), true);
        let plain = elf(&[], false);
        let code = elf_with_code();
        assert!(linked_tracked(&ar(&[("lib.rmeta", b"rust".to_vec()), ("libapp.app.1-cgu.0.rcgu.o", unit.clone())])));
        assert!(linked_tracked(&ar(&[("libapp.app.1-cgu.0.rcgu.o", unit.clone()), ("libapp.app.1-cgu.1.rcgu.o", plain.clone())])), "a unit with no code");
        assert!(!linked_tracked(&ar(&[("libapp.app.1-cgu.0.rcgu.o", unit.clone()), ("shim.o", code.clone())])), "a bundled C object");
        assert!(!linked_tracked(&ar(&[("libstd-1.std.2-cgu.0.rcgu.o", code.clone())])), "built without the flag");
        assert!(linked_tracked(&unit) && linked_tracked(&plain) && !linked_tracked(&code), "loose objects, as an incremental session keeps them");
        assert!(!linked_tracked(b"not an archive"));
    }

    #[test]
    fn a_test_s_binary_read_becomes_its_code_and_its_program_is_found_from_the_command() {
        assert_eq!(program(b"/w/target/debug/deps/t-1", b"/w/app"), b"/w/target/debug/deps/t-1");
        assert_eq!(program(b"./t", b"/w/app/"), b"/w/app/t");
        let bin = Hash([7; 32]);
        let mut obs = vec![
            (VPath::from(b"/w/src/data.txt".as_slice()), Obs::File { hash: Hash([1; 32]), exec: false }),
            (VPath::from(b"/w/target/debug/deps/t-1".as_slice()), Obs::File { hash: bin, exec: true }),
        ];
        assert_eq!(binary_read(&obs, b"/w/target/debug/deps/t-1"), Some(bin));
        assert_eq!(binary_read(&obs, b"/w/src/data.txt"), None, "not an executable");
        let (e, r) = (Hash([8; 32]), Hash([9; 32]));
        assert!(recode(&mut obs, b"/w/target/debug/deps/t-1", e, r));
        assert_eq!(obs[1].1, Obs::Code { executed: e, rest: r });
        assert_eq!(obs[0].1, Obs::File { hash: Hash([1; 32]), exec: false }, "other reads stay");
        let side = vec![(MARKS_FILE.to_vec(), crate::trace::MEntry::File { hash: e, exec: false, size: 4096 })];
        assert_eq!(marks_file(&side), Some(e));
        let hits = (0..1000u64).filter(|n| sample(&Hash::of(b"k"), *n, 10)).count();
        assert!((50..150).contains(&hits), "{hits}");
    }
}
