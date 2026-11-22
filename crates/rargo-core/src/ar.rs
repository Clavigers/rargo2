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
        out.push(Member { name, header_offset: pos, data });
        pos = end + (end & 1);
    }
    Some(out)
}

/// Removes the incremental session component from a codegen-unit object
/// name: `util.129s3nt2.1sb40i6.rcgu.o` becomes `util.129s3nt2.rcgu.o`.
/// Other names are returned unchanged.
pub fn stable_member_name(name: &str) -> String {
    let Some(stem) = name.strip_suffix(".rcgu.o") else { return name.to_string() };
    let parts: Vec<&str> = stem.split('.').collect();
    // crate . cgu . session
    if parts.len() >= 3 && parts[parts.len() - 1].len() == 7 && parts[parts.len() - 1].bytes().all(|b| b.is_ascii_alphanumeric()) {
        format!("{}.rcgu.o", parts[..parts.len() - 1].join("."))
    } else {
        name.to_string()
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
        .map(|m| (stable_member_name(&m.name), crate::hash::Hash::of(m.data)))
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
}
