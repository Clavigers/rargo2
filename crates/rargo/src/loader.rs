use std::path::PathBuf;

const STORE: &str = "/nix/store/";

struct Elf<'a> {
    bytes: &'a [u8],
    headers: Vec<(u32, u64, u64, u64)>,
}

impl<'a> Elf<'a> {
    fn parse(bytes: &'a [u8]) -> Option<Elf<'a>> {
        if bytes.get(..6)? != b"\x7fELF\x02\x01" {
            return None;
        }
        let elf = Elf { bytes, headers: Vec::new() };
        let (phoff, phentsize, phnum) = (elf.u64(0x20)? as usize, elf.u16(0x36)? as usize, elf.u16(0x38)? as usize);
        let headers = (0..phnum)
            .map(|i| {
                let ph = phoff.checked_add(i.checked_mul(phentsize)?)?;
                Some((elf.u32(ph)?, elf.u64(ph.checked_add(8)?)?, elf.u64(ph.checked_add(16)?)?, elf.u64(ph.checked_add(32)?)?))
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Elf { bytes, headers })
    }

    fn get(&self, at: usize, n: usize) -> Option<&'a [u8]> {
        self.bytes.get(at..at.checked_add(n)?)
    }

    fn u16(&self, at: usize) -> Option<u16> {
        Some(u16::from_le_bytes(self.get(at, 2)?.try_into().ok()?))
    }

    fn u32(&self, at: usize) -> Option<u32> {
        Some(u32::from_le_bytes(self.get(at, 4)?.try_into().ok()?))
    }

    fn u64(&self, at: usize) -> Option<u64> {
        Some(u64::from_le_bytes(self.get(at, 8)?.try_into().ok()?))
    }

    fn segment(&self, kind: u32) -> Option<&'a [u8]> {
        let (_, offset, _, size) = self.headers.iter().find(|h| h.0 == kind)?;
        self.get(usize::try_from(*offset).ok()?, usize::try_from(*size).ok()?)
    }

    fn file_offset(&self, vaddr: u64) -> Option<usize> {
        let (_, offset, start, _) = self.headers.iter().find(|h| h.0 == 1 && vaddr >= h.2 && vaddr - h.2 < h.3)?;
        usize::try_from(offset.checked_add(vaddr - start)?).ok()
    }

    fn string(&self, at: usize) -> Option<&'a [u8]> {
        let rest = self.bytes.get(at..)?;
        Some(&rest[..rest.iter().position(|b| *b == 0)?])
    }

    fn search_paths(&self) -> Vec<&'a [u8]> {
        let Some(dynamic) = self.segment(2) else { return Vec::new() };
        let entries: Vec<(u64, u64)> = dynamic
            .as_chunks::<16>()
            .0
            .iter()
            .map(|e| (u64::from_le_bytes(e[..8].try_into().unwrap()), u64::from_le_bytes(e[8..].try_into().unwrap())))
            .take_while(|(tag, _)| *tag != 0)
            .collect();
        let Some(strtab) = entries.iter().find(|(tag, _)| *tag == 5).and_then(|(_, v)| self.file_offset(*v)) else { return Vec::new() };
        entries
            .iter()
            .filter(|(tag, _)| *tag == 15 || *tag == 29)
            .filter_map(|(_, at)| self.string(strtab.checked_add(usize::try_from(*at).ok()?)?))
            .flat_map(|list| list.split(|b| *b == b':'))
            .collect()
    }
}

pub fn interpreter(elf: &[u8]) -> Option<&[u8]> {
    let interp = Elf::parse(elf)?.segment(3)?;
    Some(interp.strip_suffix(b"\0").unwrap_or(interp))
}

/// Check that an install artifact is an x86-64 ELF executable with no dynamic loader or shared dependencies.
pub fn static_executable(bytes: &[u8]) -> bool {
    let Some(elf) = Elf::parse(bytes) else { return false };
    if !matches!(elf.u16(16), Some(2 | 3)) || elf.u16(18) != Some(62) || elf.u64(24) == Some(0) || elf.u16(54) != Some(56) {
        return false;
    }
    let mut loads = false;
    for &(kind, offset, _, size) in &elf.headers {
        let Some(data) = usize::try_from(offset).ok().zip(usize::try_from(size).ok()).and_then(|(offset, size)| elf.get(offset, size)) else {
            return false;
        };
        match kind {
            1 => loads = true,
            2 if !static_dynamic_entries(data) => return false,
            3 => return false,
            _ => {}
        }
    }
    loads
}

/// A static PIE can have a dynamic table for relocations, but cannot name shared libraries.
fn static_dynamic_entries(bytes: &[u8]) -> bool {
    let (entries, remainder) = bytes.as_chunks::<16>();
    if !remainder.is_empty() {
        return false;
    }
    for entry in entries {
        match u64::from_le_bytes(entry[..8].try_into().unwrap()) {
            0 => return true,
            1 | 0x7fff_fffd | 0x7fff_ffff | 0x6fff_fefb | 0x6fff_fefc => return false,
            _ => {}
        }
    }
    false
}

pub fn store_path(path: &[u8]) -> Option<String> {
    let name = std::str::from_utf8(path).ok()?.strip_prefix(STORE)?.split('/').next()?;
    (name.len() > 33 && name.as_bytes()[32] == b'-').then(|| format!("{STORE}{name}"))
}

pub fn needed(elf: &[u8]) -> Vec<String> {
    let Some(parsed) = Elf::parse(elf) else { return Vec::new() };
    let mut out: Vec<String> = Vec::new();
    for path in interpreter(elf).into_iter().chain(parsed.search_paths()) {
        if let Some(p) = store_path(path).filter(|p| !out.contains(p)) {
            out.push(p);
        }
    }
    out
}

pub fn fetches(missing: &[String], roots: &str) -> Vec<Vec<String>> {
    missing
        .iter()
        .map(|p| {
            let name = p.strip_prefix(STORE).unwrap_or(p);
            vec!["--realise".into(), p.clone(), "--add-root".into(), format!("{roots}/{name}")]
        })
        .collect()
}

pub fn nix_store_candidates(path_var: Option<&str>, home: Option<&str>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = path_var
        .unwrap_or_default()
        .split(':')
        .filter(|d| d.starts_with('/'))
        .map(|d| PathBuf::from(d).join("nix-store"))
        .collect();
    if let Some(home) = home.filter(|h| h.starts_with('/')) {
        out.push(PathBuf::from(home).join(".nix-profile/bin/nix-store"));
    }
    out.push(PathBuf::from("/nix/var/nix/profiles/default/bin/nix-store"));
    out
}

pub fn text(program: &str, loader: &str, failed: Option<&str>) -> String {
    let mut out = format!("error: `{program}` cannot start here: it needs the program loader {loader}, which this machine does not have.\n");
    let Some(path) = store_path(loader.as_bytes()) else { return out };
    match failed {
        Some(why) => out.push_str(&format!("       Fetching Nix's C library for it failed: {why}\n       Fetch it yourself with:\n")),
        None => out.push_str(
            "       What rargo builds runs with Nix's C library, so running it on this machine needs Nix. \
             Install Nix (https://nixos.org/download) and `rargo run` fetches the library the first time; or fetch it yourself once with:\n",
        ),
    }
    for args in fetches(&[path], "~/.cache/rargo/nix") {
        out.push_str(&format!("       nix-store {}\n", args.join(" ")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const GLIBC: &str = "/nix/store/r3hvhzl5fxhrgn0rfy5g7rda17702pj4-glibc-2.44-57";
    const GCC: &str = "/nix/store/0ba5kj6r0zhfwd84l5vzccqvx7amlq2c-gcc-15.3.0-lib";

    fn elf(interp: Option<&str>, runpath: Option<&str>, rpath: Option<&str>) -> Vec<u8> {
        let mut f = vec![0u8; 0x1000];
        f[..6].copy_from_slice(b"\x7fELF\x02\x01");
        f[0x20..0x28].copy_from_slice(&0x40u64.to_le_bytes());
        f[0x36..0x38].copy_from_slice(&56u16.to_le_bytes());
        let mut headers: Vec<(u32, u64, u64, u64)> = vec![(6, 0x40, 0x40, 4 * 56), (1, 0, 0x40_0000, 0x1000)];
        if let Some(i) = interp {
            f[0x200..0x200 + i.len()].copy_from_slice(i.as_bytes());
            headers.push((3, 0x200, 0x40_0200, i.len() as u64 + 1));
        }
        let mut strtab = vec![0u8];
        let mut dynamic: Vec<(u64, u64)> = vec![(1, 0), (5, 0x40_0800)];
        for (tag, s) in [(29u64, runpath), (15, rpath)] {
            if let Some(s) = s {
                dynamic.push((tag, strtab.len() as u64));
                strtab.extend_from_slice(s.as_bytes());
                strtab.push(0);
            }
        }
        dynamic.push((0, 0));
        f[0x800..0x800 + strtab.len()].copy_from_slice(&strtab);
        for (n, (tag, val)) in dynamic.iter().enumerate() {
            f[0x600 + n * 16..0x608 + n * 16].copy_from_slice(&tag.to_le_bytes());
            f[0x608 + n * 16..0x610 + n * 16].copy_from_slice(&val.to_le_bytes());
        }
        headers.push((2, 0x600, 0x40_0600, dynamic.len() as u64 * 16));
        f[0x38..0x3a].copy_from_slice(&(headers.len() as u16).to_le_bytes());
        for (n, (kind, offset, vaddr, size)) in headers.into_iter().enumerate() {
            let at = 0x40 + n * 56;
            f[at..at + 4].copy_from_slice(&kind.to_le_bytes());
            f[at + 8..at + 16].copy_from_slice(&offset.to_le_bytes());
            f[at + 16..at + 24].copy_from_slice(&vaddr.to_le_bytes());
            f[at + 32..at + 40].copy_from_slice(&size.to_le_bytes());
        }
        f
    }

    #[test]
    fn install_accepts_static_pie_and_rejects_dynamic_or_truncated_elf() {
        let mut program = elf(None, None, None);
        program[16..18].copy_from_slice(&3u16.to_le_bytes());
        program[18..20].copy_from_slice(&62u16.to_le_bytes());
        program[24..32].copy_from_slice(&0x40_0100u64.to_le_bytes());
        assert!(!static_executable(&program), "DT_NEEDED requires a shared library");
        program[0x600..0x608].copy_from_slice(&7u64.to_le_bytes());
        assert!(static_executable(&program), "DT_RELA is valid in a static PIE");
        assert!(!static_executable(&program[..0x610]));
        program[18..20].copy_from_slice(&183u16.to_le_bytes());
        assert!(!static_executable(&program), "AArch64 is unsupported");

        let mut with_interp = elf(Some("/lib64/ld-linux-x86-64.so.2"), None, None);
        with_interp[16..32].copy_from_slice(&program[16..32]);
        with_interp[18..20].copy_from_slice(&62u16.to_le_bytes());
        with_interp[0x600..0x608].copy_from_slice(&7u64.to_le_bytes());
        assert!(!static_executable(&with_interp), "PT_INTERP requires a loader");
    }

    #[test]
    fn a_program_names_the_store_paths_it_needs_through_its_loader_and_search_paths() {
        assert_eq!(store_path(format!("{GLIBC}/lib/ld-linux-x86-64.so.2").as_bytes()).as_deref(), Some(GLIBC));
        assert_eq!(store_path(b"/lib64/ld-linux-x86-64.so.2"), None);
        let loader = format!("{GLIBC}/lib/ld-linux-x86-64.so.2");
        let runpath = format!("{GCC}/lib:$ORIGIN/../lib:{GLIBC}/lib:/usr/lib:/nix/store/short");
        let program = elf(Some(&loader), Some(&runpath), None);
        assert_eq!(interpreter(&program), Some(loader.as_bytes()));
        assert_eq!(needed(&program), [GLIBC, GCC], "each store path once, the loader's first");
        let old = elf(Some(&loader), None, Some(&format!("{GCC}/lib64")));
        assert_eq!(needed(&old), [GLIBC, GCC], "an RPATH counts like a RUNPATH");
        assert_eq!(needed(&elf(Some("/lib64/ld-linux-x86-64.so.2"), Some("/usr/lib"), None)), Vec::<String>::new());
        assert_eq!(interpreter(&elf(None, Some(&runpath), None)), None, "a static program has no loader");
        assert_eq!(needed(&elf(None, Some(&runpath), None)), [GCC, GLIBC]);
        let mut cut = elf(Some(&loader), Some(&runpath), None);
        cut.truncate(0x700);
        assert_eq!(needed(&cut), [GLIBC], "search paths past what was read are not guessed");
        let mut far = program.clone();
        far[0x20..0x28].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!((interpreter(&far), needed(&far)), (None, Vec::new()));
        let mut after_end = elf(Some(&loader), None, None);
        let gcc = format!("{GCC}/lib");
        after_end[0x900..0x900 + gcc.len()].copy_from_slice(gcc.as_bytes());
        after_end[0x630..0x638].copy_from_slice(&29u64.to_le_bytes());
        after_end[0x638..0x640].copy_from_slice(&0x100u64.to_le_bytes());
        let size = 0x40 + 3 * 56 + 32;
        after_end[size..size + 8].copy_from_slice(&64u64.to_le_bytes());
        assert_eq!(needed(&after_end), [GLIBC], "entries after DT_NULL are not read");
        assert_eq!(needed(b"#!/bin/sh\n"), Vec::<String>::new());
        assert_eq!(interpreter(&program[..0x30]), None);
    }

    #[test]
    fn missing_store_paths_are_fetched_one_root_each_or_explained() {
        let missing = vec![GLIBC.to_string(), GCC.to_string()];
        assert_eq!(
            fetches(&missing, "/home/a/.cache/rargo/nix"),
            [
                vec!["--realise".to_string(), GLIBC.into(), "--add-root".into(), "/home/a/.cache/rargo/nix/r3hvhzl5fxhrgn0rfy5g7rda17702pj4-glibc-2.44-57".into()],
                vec!["--realise".to_string(), GCC.into(), "--add-root".into(), "/home/a/.cache/rargo/nix/0ba5kj6r0zhfwd84l5vzccqvx7amlq2c-gcc-15.3.0-lib".into()],
            ]
        );
        let loader = format!("{GLIBC}/lib/ld-linux-x86-64.so.2");
        assert_eq!(
            text("target/debug/app", &loader, None),
            format!(
                "error: `target/debug/app` cannot start here: it needs the program loader {loader}, which this machine does not have.\n       \
                 What rargo builds runs with Nix's C library, so running it on this machine needs Nix. Install Nix (https://nixos.org/download) and `rargo run` fetches the library the first time; or fetch it yourself once with:\n       \
                 nix-store --realise {GLIBC} --add-root ~/.cache/rargo/nix/r3hvhzl5fxhrgn0rfy5g7rda17702pj4-glibc-2.44-57\n"
            )
        );
        let failed = text("app", &loader, Some("cannot reach cache.nixos.org"));
        assert!(failed.contains("Fetching Nix's C library for it failed: cannot reach cache.nixos.org\n       Fetch it yourself with:\n"), "{failed}");
        assert_eq!(failed.matches("nix-store --realise").count(), 1, "only the loader's own store path is named");
        assert_eq!(text("app", "/opt/ld.so", None), "error: `app` cannot start here: it needs the program loader /opt/ld.so, which this machine does not have.\n");
    }

    #[test]
    fn nix_store_is_looked_for_on_the_path_then_in_the_nix_profiles() {
        assert_eq!(
            nix_store_candidates(Some("/usr/bin:relative:/opt/nix/bin"), Some("/home/a")),
            [
                PathBuf::from("/usr/bin/nix-store"),
                PathBuf::from("/opt/nix/bin/nix-store"),
                PathBuf::from("/home/a/.nix-profile/bin/nix-store"),
                PathBuf::from("/nix/var/nix/profiles/default/bin/nix-store"),
            ]
        );
        assert_eq!(nix_store_candidates(None, None), [PathBuf::from("/nix/var/nix/profiles/default/bin/nix-store")]);
    }
}
