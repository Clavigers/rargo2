//! Reading `ar` archives (`.rlib`, `.a`).
//!
//! Used for two things: comparing rlibs modulo the per-session object names
//! rustc's incremental mode gives its members, and cutting archives at
//! member boundaries when chunking.

/// One archive member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member<'a> {
    pub name: String,
    /// Offset of the member's header in the archive.
    pub header_offset: usize,
    pub data: &'a [u8],
}

const MAGIC: &[u8] = b"!<arch>\n";
const HEADER: usize = 60;

/// Parses a GNU-format archive. Returns `None` if `bytes` is not one.
/// The symbol table (`/`) and long-name table (`//`) are returned as
/// members with those names.
pub fn members(bytes: &[u8]) -> Option<Vec<Member<'_>>> {
    if !bytes.starts_with(MAGIC) {
        return None;
    }
    let mut out = Vec::new();
    let mut pos = MAGIC.len();
    let mut longnames: &[u8] = &[];
    while pos + HEADER <= bytes.len() {
        let h = &bytes[pos..pos + HEADER];
        if &h[58..60] != b"`\n" {
            return None;
        }
        let size: usize = std::str::from_utf8(&h[48..58]).ok()?.trim().parse().ok()?;
        let start = pos + HEADER;
        let end = start.checked_add(size)?;
        if end > bytes.len() {
            return None;
        }
        let raw = std::str::from_utf8(&h[0..16]).ok()?.trim_end();
        let data = &bytes[start..end];
        let name = if raw == "/" || raw == "//" || raw == "/SYM64/" {
            raw.to_string()
        } else if let Some(off) = raw.strip_prefix('/') {
            // Offset into the long-name table; entries end with "/\n".
            let off: usize = off.parse().ok()?;
            let rest = longnames.get(off..)?;
            let len = rest.iter().position(|&c| c == b'\n').unwrap_or(rest.len());
            String::from_utf8_lossy(&rest[..len]).trim_end_matches('/').to_string()
        } else {
            raw.trim_end_matches('/').to_string()
        };
        if name == "//" {
            longnames = data;
        }
        assert!(out.last().is_none_or(|m: &Member<'_>| m.header_offset < pos), "members ascend");
        assert_eq!(data.len(), size);
        out.push(Member { name, header_offset: pos, data });
        pos = end + (end & 1);
        assert!(pos > start || size == 0);
    }
    Some(out)
}

/// Removes the incremental session component from a codegen-unit file
/// name: `util.129s3nt2.1sb40i6.rcgu.o` becomes `util.129s3nt2.rcgu.o`
/// (likewise `.rcgu.dwo`, the unit's split debug info). Other names are
/// returned unchanged.
pub fn stable_member_name(name: &str) -> String {
    for ext in [".rcgu.o", ".rcgu.dwo"] {
        let Some(stem) = name.strip_suffix(ext) else { continue };
        let parts: Vec<&str> = stem.split('.').collect();
        // crate . cgu . session
        if parts.len() >= 3 && parts[parts.len() - 1].len() == SESSION && parts[parts.len() - 1].bytes().all(|b| b.is_ascii_alphanumeric()) {
            return format!("{}{ext}", parts[..parts.len() - 1].join("."));
        }
    }
    name.to_string()
}

/// Length of the session component in a codegen-unit file name.
const SESSION: usize = 7;

/// `bytes` with the session component of every codegen-unit file name
/// mentioned in it blanked (an object file names its `.dwo` file, an
/// archive's name table names its members). The length is unchanged, so
/// everything else stays where it was.
pub fn session_neutral(bytes: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    const MARK: &[u8] = b".rcgu.";
    let mut out = std::borrow::Cow::Borrowed(bytes);
    let mut from = 0;
    while let Some(at) = bytes[from..].windows(MARK.len()).position(|w| w == MARK).map(|p| p + from) {
        from = at + 1;
        // `.<cgu>.<session>` must precede the mark.
        if at < SESSION + 2 {
            continue;
        }
        let session = &bytes[at - SESSION..at];
        let cgu_end = at - SESSION - 1;
        if bytes[cgu_end] != b'.' || !session.iter().all(|b| b.is_ascii_alphanumeric()) {
            continue;
        }
        let cgu_len = bytes[..cgu_end].iter().rev().take_while(|b| b.is_ascii_alphanumeric()).count();
        if cgu_len == 0 || cgu_end == cgu_len || bytes[cgu_end - cgu_len - 1] != b'.' {
            continue;
        }
        out.to_mut()[at - SESSION..at].fill(b'0');
    }
    assert_eq!(out.len(), bytes.len(), "length is unchanged");
    out
}

/// A hash of a compiler output that ignores the session component of
/// codegen-unit file names, in an archive's member names and inside any
/// file's content.
pub fn stable_output_hash(bytes: &[u8]) -> crate::hash::Hash {
    stable_rlib_hash(bytes).unwrap_or_else(|| crate::hash::Hash::of(&session_neutral(bytes)))
}

/// Where the linker's build id sits in an ELF file: the byte range of the
/// section `.note.gnu.build-id`. `None` for anything that is not a 64-bit
/// little-endian ELF file with such a section.
fn build_id(bytes: &[u8]) -> Option<(usize, usize)> {
    let u16_at = |o: usize| bytes.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize);
    let u32_at = |o: usize| bytes.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize);
    let u64_at = |o: usize| bytes.get(o..o + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()) as usize);
    if bytes.get(..6)? != b"\x7fELF\x02\x01" {
        return None;
    }
    let (shoff, shentsize, shnum, shstrndx) = (u64_at(0x28)?, u16_at(0x3a)?, u16_at(0x3c)?, u16_at(0x3e)?);
    let section = |i: usize| -> Option<(usize, usize, usize)> {
        let h = shoff.checked_add(i.checked_mul(shentsize)?)?;
        Some((u32_at(h)?, u64_at(h + 0x18)?, u64_at(h + 0x20)?))
    };
    let (_, names_at, names_len) = section(shstrndx)?;
    let names = bytes.get(names_at..names_at.checked_add(names_len)?)?;
    for i in 0..shnum {
        let (name, at, len) = section(i)?;
        let rest = names.get(name..)?;
        if rest.split(|&c| c == 0).next() == Some(b".note.gnu.build-id".as_slice()) {
            return (at.checked_add(len)? <= bytes.len()).then_some((at, at + len));
        }
    }
    None
}

/// What kind of difference two versions of an output have, in words.
pub fn describe_difference(a: &[u8], b: &[u8]) -> String {
    if a.len() != b.len() {
        return format!("{} and {} bytes", a.len(), b.len());
    }
    let (na, nb) = (session_neutral(a), session_neutral(b));
    match differences(&na, &nb) {
        Some(d) if d.is_empty() => "only session names inside".to_string(),
        Some(d) => {
            let longest = d.iter().map(|(s, e)| e - s).max().unwrap_or(0);
            let id = build_id(&na).map(|(s, e)| format!(", build id at {s}..{e}")).unwrap_or_default();
            format!(
                "same size, {} places differ besides session names, the longest {} bytes, the first at byte {}{}",
                d.len(),
                longest,
                d[0].0,
                id
            )
        }
        None => "different sizes".to_string(),
    }
}

/// Length of the identifier that ties an object file to its split debug
/// info (`DW_AT_GNU_dwo_id`, or the DWARF 5 unit header's `dwo_id`).
const DWO_ID: usize = 8;

/// Where two equally long byte strings differ, as `(start, end)` ranges;
/// differences fewer than [`DWO_ID`] bytes apart are one range.
fn differences(a: &[u8], b: &[u8]) -> Option<Vec<(usize, usize)>> {
    if a.len() != b.len() {
        return None;
    }
    let mut out: Vec<(usize, usize)> = Vec::new();
    for i in (0..a.len()).filter(|&i| a[i] != b[i]) {
        match out.last_mut() {
            Some((_, end)) if i < *end + DWO_ID => {
                assert!(i >= *end);
                *end = i + 1
            }
            _ => out.push((i, i + 1)),
        }
    }
    assert!(out.iter().all(|&(s, e)| s < e && e <= a.len()));
    Some(out)
}

/// The debug-info identifiers by which two compilations of one codegen unit
/// differ. With split debug info and incremental compilation, rustc names a
/// unit's `.dwo` file after the session, and the identifier stored in the
/// `.dwo` and in the object that refers to it is a hash that covers that
/// name: two compilations of the same source differ in it and in nothing
/// else. A substitution is the differing bytes of one against the other.
#[derive(Default)]
pub struct DwoIds(std::collections::HashSet<(Vec<u8>, Vec<u8>)>);

impl DwoIds {
    /// Learns the substitution from two versions of a `.dwo` file. `false`
    /// if they differ in more than one identifier.
    pub fn learn(&mut self, a: &[u8], b: &[u8]) -> bool {
        let (a, b) = (session_neutral(a), session_neutral(b));
        match differences(&a, &b).as_deref() {
            Some([]) => true,
            Some(&[(s, e)]) if e - s <= DWO_ID => {
                assert!(s < e);
                self.0.insert((a[s..e].to_vec(), b[s..e].to_vec()));
                true
            }
            _ => false,
        }
    }

    /// Whether `a` and `b` are the same apart from session names and the
    /// identifiers learnt. In a linked file (an executable, a proc macro's
    /// shared object) the linker's build id is a hash of the whole file: if
    /// an identifier differs, so does it, and that is not one more
    /// difference.
    fn same_bytes(&self, a: &[u8], b: &[u8]) -> bool {
        let (a, b) = (session_neutral(a), session_neutral(b));
        let Some(d) = differences(&a, &b) else { return false };
        let known = |&(s, e): &(usize, usize)| e - s <= DWO_ID && self.0.contains(&(a[s..e].to_vec(), b[s..e].to_vec()));
        if d.iter().all(known) {
            return true;
        }
        let (Some(ia), Some(ib)) = (build_id(&a), build_id(&b)) else { return false };
        ia == ib && d.iter().any(known) && d.iter().all(|r| known(r) || (r.0 >= ia.0 && r.1 <= ia.1))
    }

    /// The same for a whole output file: an archive is compared member by
    /// member (its tables only restate member names and offsets).
    pub fn same(&self, a: &[u8], b: &[u8]) -> bool {
        let (Some(ma), Some(mb)) = (members(a), members(b)) else { return self.same_bytes(a, b) };
        let table = |n: &str| matches!(n, "/" | "//" | "/SYM64/" | "lib.rmeta-link");
        let list = |ms: Vec<Member<'_>>| -> Vec<(String, Vec<u8>)> {
            let mut v: Vec<(String, Vec<u8>)> =
                ms.into_iter().filter(|m| !table(&m.name)).map(|m| (stable_member_name(&m.name), m.data.to_vec())).collect();
            v.sort();
            v
        };
        let (la, lb) = (list(ma), list(mb));
        if la.len() != lb.len() || la.iter().zip(&lb).any(|((na, _), (nb, _))| na != nb) {
            return false;
        }
        // Packed split debug info: the `.dwo` files are members themselves.
        let mut ids = DwoIds(self.0.clone());
        for ((name, da), (_, db)) in la.iter().zip(&lb) {
            if name.ends_with(".rcgu.dwo") && !ids.learn(da, db) {
                return false;
            }
        }
        la.iter().zip(&lb).all(|((_, da), (_, db))| ids.same_bytes(da, db))
    }
}

/// A hash of an rlib that ignores what rustc's incremental mode varies
/// between otherwise identical compilations: the session component of
/// object member names, and the tables that only restate those names.
pub fn stable_rlib_hash(bytes: &[u8]) -> Option<crate::hash::Hash> {
    let ms = members(bytes)?;
    let mut items: Vec<(String, crate::hash::Hash)> = ms
        .iter()
        .filter(|m| !matches!(m.name.as_str(), "/" | "//" | "/SYM64/" | "lib.rmeta-link"))
        .map(|m| (stable_member_name(&m.name), crate::hash::Hash::of(&session_neutral(m.data))))
        .collect();
    items.sort();
    let mut h = crate::hash::KeyHasher::new("stable-rlib");
    for (n, c) in items {
        h.field(n.as_bytes()).field(&c.0);
    }
    Some(h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut long = Vec::new();
        let mut names = Vec::new();
        for (n, _) in members {
            if n.len() > 15 {
                names.push(format!("/{}", long.len()));
                long.extend_from_slice(n.as_bytes());
                long.extend_from_slice(b"/\n");
            } else {
                names.push(format!("{n}/"));
            }
        }
        let mut out = MAGIC.to_vec();
        let mut push = |name: &str, data: &[u8]| {
            out.extend_from_slice(format!("{name:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n", 0, 0, 0, 644, data.len()).as_bytes());
            out.extend_from_slice(data);
            if data.len() % 2 == 1 {
                out.push(b'\n');
            }
        };
        if !long.is_empty() {
            push("//", &long);
        }
        for ((_, data), name) in members.iter().zip(&names) {
            push(name, data);
        }
        out
    }

    #[test]
    fn parses_short_and_long_names() {
        let a = archive(&[("lib.rmeta", b"meta"), ("util.129s3nt2zs5kd4xxpejw9mare.1sb40i6.rcgu.o", b"object!")]);
        let m = members(&a).unwrap();
        let names: Vec<&str> = m.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["//", "lib.rmeta", "util.129s3nt2zs5kd4xxpejw9mare.1sb40i6.rcgu.o"]);
        assert_eq!(m[1].data, b"meta");
        assert_eq!(m[2].data, b"object!");
        assert!(members(b"not an archive").is_none());
    }

    #[test]
    fn session_names_do_not_matter_but_content_does() {
        assert_eq!(stable_member_name("util.129s3nt2zs5kd4xxpejw9mare.1sb40i6.rcgu.o"), "util.129s3nt2zs5kd4xxpejw9mare.rcgu.o");
        assert_eq!(stable_member_name("util.util.4f2a.rcgu.o"), "util.util.4f2a.rcgu.o");
        assert_eq!(stable_member_name("lib.rmeta"), "lib.rmeta");
        let a = archive(&[("lib.rmeta", b"meta"), ("util.129s3nt2zs5kd4xxpejw9mare.1sb40i6.rcgu.o", b"object!")]);
        let b = archive(&[("lib.rmeta", b"meta"), ("util.129s3nt2zs5kd4xxpejw9mare.0ebyr4y.rcgu.o", b"object!")]);
        let c = archive(&[("lib.rmeta", b"meta"), ("util.129s3nt2zs5kd4xxpejw9mare.0ebyr4y.rcgu.o", b"OBJECT!")]);
        assert_ne!(a, b);
        assert_eq!(stable_rlib_hash(&a), stable_rlib_hash(&b));
        assert_ne!(stable_rlib_hash(&a), stable_rlib_hash(&c));
    }

    #[test]
    fn split_debug_info_names_are_neutral_too() {
        assert_eq!(stable_member_name("git_ui-1f09.0f4631ej.0183sso.rcgu.dwo"), "git_ui-1f09.0f4631ej.rcgu.dwo");
        // An object names its `.dwo`; only the session differs.
        let a = b"\x7fELF..skeleton..git_ui-1f09.0f4631ej.0183sso.rcgu.dwo\0..more".to_vec();
        let b = b"\x7fELF..skeleton..git_ui-1f09.0f4631ej.9zzzzzz.rcgu.dwo\0..more".to_vec();
        let c = b"\x7fELF..skeleton..git_ui-1f09.0f4631ej.9zzzzzz.rcgu.dwo\0..MORE".to_vec();
        assert_ne!(a, b);
        assert_eq!(session_neutral(&a).len(), a.len());
        assert_eq!(stable_output_hash(&a), stable_output_hash(&b));
        assert_ne!(stable_output_hash(&a), stable_output_hash(&c));
        // Names without a session component are left alone.
        let plain = b"util.util.4f2a.rcgu.o and .rcgu. alone".to_vec();
        assert!(matches!(session_neutral(&plain), std::borrow::Cow::Borrowed(_)));
        // Inside an archive member as well.
        let ra = archive(&[("util.129s3nt2zs5kd4xxpejw9mare.1sb40i6.rcgu.o", &a)]);
        let rb = archive(&[("util.129s3nt2zs5kd4xxpejw9mare.0ebyr4y.rcgu.o", &b)]);
        let rc = archive(&[("util.129s3nt2zs5kd4xxpejw9mare.0ebyr4y.rcgu.o", &c)]);
        assert_eq!(stable_output_hash(&ra), stable_output_hash(&rb));
        assert_ne!(stable_output_hash(&ra), stable_output_hash(&rc));
    }

    #[test]
    fn debug_info_identifiers_pair_objects_with_their_dwo() {
        // Two compilations: session `0183sso` with identifier `AAAAAAAA`,
        // session `9zzzzzz` with `BBBBBBBB`.
        let dwo = |id: &[u8]| [b"dwo-header.".as_slice(), id, b".types-and-lines"].concat();
        let obj = |sess: &str, id: &[u8], code: &str| {
            [b"ELF.".as_slice(), id, format!(".u-1f.0f46.{sess}.rcgu.dwo.").as_bytes(), id, code.as_bytes()].concat()
        };
        let mut ids = DwoIds::default();
        assert!(ids.learn(&dwo(b"AAAAAAAA"), &dwo(b"BBBBBBBB")));
        assert!(ids.same(&obj("0183sso", b"AAAAAAAA", "code"), &obj("9zzzzzz", b"BBBBBBBB", "code")));
        // Different code is different, whatever the identifiers.
        assert!(!ids.same(&obj("0183sso", b"AAAAAAAA", "code"), &obj("9zzzzzz", b"BBBBBBBB", "CODE")));
        // An identifier nobody's `.dwo` carries is a difference.
        assert!(!ids.same(&obj("0183sso", b"AAAAAAAA", "code"), &obj("9zzzzzz", b"CCCCCCCC", "code")));
        // A `.dwo` that differs in more than its identifier teaches nothing.
        assert!(!DwoIds::default().learn(&dwo(b"AAAAAAAA"), &[dwo(b"BBBBBBBB"), b"x".to_vec()].concat()));
        assert!(!DwoIds::default().learn(b"0123456789abcdefghijklmnopqrstuvwxyz", b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ"));
        // Inside an archive, with the member renamed as well.
        let ra = archive(&[("lib.rmeta", b"meta"), ("u-1f.0f46.0183sso.rcgu.o", &obj("0183sso", b"AAAAAAAA", "code"))]);
        let rb = archive(&[("lib.rmeta", b"meta"), ("u-1f.0f46.9zzzzzz.rcgu.o", &obj("9zzzzzz", b"BBBBBBBB", "code"))]);
        let rc = archive(&[("lib.rmeta", b"META"), ("u-1f.0f46.9zzzzzz.rcgu.o", &obj("9zzzzzz", b"BBBBBBBB", "code"))]);
        assert!(ids.same(&ra, &rb));
        assert!(!ids.same(&ra, &rc));
        // A linked file: its build id differs because the identifier does.
        let elf = |id: &[u8], build: &[u8], code: &[u8]| -> Vec<u8> {
            // Header, one 16-byte code block, the identifier, a 20-byte note
            // section, the section name table, two section headers.
            let mut f = vec![0u8; 0x40];
            f[..6].copy_from_slice(b"\x7fELF\x02\x01");
            f.extend_from_slice(code);
            f.extend_from_slice(id);
            f.extend_from_slice(b"----other-sections----");
            let note_at = f.len();
            f.extend_from_slice(build);
            let names_at = f.len();
            f.extend_from_slice(b"\0.note.gnu.build-id\0.shstrtab\0");
            let names_len = f.len() - names_at;
            let shoff = f.len();
            for (name, at, len) in [(1u32, note_at, build.len()), (20u32, names_at, names_len)] {
                let mut h = vec![0u8; 0x40];
                h[..4].copy_from_slice(&name.to_le_bytes());
                h[0x18..0x20].copy_from_slice(&(at as u64).to_le_bytes());
                h[0x20..0x28].copy_from_slice(&(len as u64).to_le_bytes());
                f.extend_from_slice(&h);
            }
            f[0x28..0x30].copy_from_slice(&(shoff as u64).to_le_bytes());
            f[0x3a..0x3c].copy_from_slice(&0x40u16.to_le_bytes());
            f[0x3c..0x3e].copy_from_slice(&2u16.to_le_bytes());
            f[0x3e..0x40].copy_from_slice(&1u16.to_le_bytes());
            f
        };
        let (b1, b2) = (b"build-id-number-0001".as_slice(), b"BUILD-ID-NUMBER-0002".as_slice());
        let exe = elf(b"AAAAAAAA", b1, b"0123456789abcdef");
        assert_eq!(build_id(&exe).map(|(s, e)| e - s), Some(20));
        assert!(ids.same(&exe, &elf(b"BBBBBBBB", b2, b"0123456789abcdef")));
        // A build id that differs with nothing else to explain it, or with
        // different code beside it, is a difference.
        assert!(!ids.same(&exe, &elf(b"AAAAAAAA", b2, b"0123456789abcdef")));
        assert!(!ids.same(&exe, &elf(b"BBBBBBBB", b2, b"0123456789ABCDEF")));
        assert!(describe_difference(&exe, &elf(b"AAAAAAAA", b2, b"0123456789abcdef")).contains("build id at"));
        // Identifiers that happen to share a byte still match.
        let mut ids = DwoIds::default();
        assert!(ids.learn(&dwo(b"AAAxAAAA"), &dwo(b"BBBxBBBB")));
        assert!(ids.same(&obj("0183sso", b"AAAxAAAA", "code"), &obj("9zzzzzz", b"BBBxBBBB", "code")));
    }
}
