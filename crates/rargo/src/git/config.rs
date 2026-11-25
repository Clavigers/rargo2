fn value(raw: &str) -> String {
    let mut out = String::new();
    let mut quoted = false;
    let mut chars = raw.trim_start().chars();
    let mut pending_space = String::new();
    while let Some(c) = chars.next() {
        match c {
            '"' => quoted = !quoted,
            '\\' => {
                out.push_str(&pending_space);
                pending_space.clear();
                match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('b') => {
                        out.pop();
                    }
                    Some(other) => out.push(other),
                    None => {}
                }
            }
            '#' | ';' if !quoted => break,
            ' ' | '\t' if !quoted => pending_space.push(c),
            _ => {
                out.push_str(&pending_space);
                pending_space.clear();
                out.push(c);
            }
        }
    }
    out
}

pub fn get(files: &[&[u8]], section: &str, key: &str) -> Option<String> {
    let mut found = None;
    for file in files {
        let text = String::from_utf8_lossy(file);
        let mut current = String::new();
        for line in text.lines() {
            let mut line = line.trim();
            if line.starts_with('[') {
                let Some(close) = line.find(']') else { continue };
                let header = &line[1..close];
                current = match header.split_once([' ', '\t']) {
                    Some((name, sub)) => format!("{}\"{}", name.to_ascii_lowercase(), sub.trim().trim_matches('"')),
                    None => header.to_ascii_lowercase(),
                };
                line = line[close + 1..].trim();
            }
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') || current != section {
                continue;
            }
            let (name, raw) = line.split_once('=').unwrap_or((line, "true"));
            if name.trim().eq_ignore_ascii_case(key) {
                found = Some(value(raw));
            }
        }
    }
    found
}

pub fn abbrev(files: &[&[u8]]) -> Option<usize> {
    match get(files, "core", "abbrev")?.to_ascii_lowercase().as_str() {
        "auto" => None,
        "no" | "false" | "off" => Some(40),
        n => n.parse().ok().filter(|n| (4..=40).contains(n)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_setting_of_a_key_wins_across_files_and_sections() {
        let global = b"[core]\n\tabbrev = 9\n\texcludesFile = ~/.ignore # mine\n[user]\n\tname = x\n";
        let repo = b"[core]\n\trepositoryformatversion = 0\n[CORE]\n\tAbbrev=11\n[remote \"core\"]\n\tabbrev = 5\n";
        assert_eq!(abbrev(&[global]), Some(9));
        assert_eq!(abbrev(&[global, repo]), Some(11));
        assert_eq!(abbrev(&[b"[core]\n\tabbrev = auto\n"]), None);
        assert_eq!(abbrev(&[b"[core]\n\tabbrev = no\n"]), Some(40));
        assert_eq!(abbrev(&[b"[core]\n\tabbrev = 2\n"]), None);
        assert_eq!(get(&[global], "core", "excludesfile").as_deref(), Some("~/.ignore"));
        assert_eq!(get(&[b"[core] excludesFile = \"a b\\\\c\" ; x\n"], "core", "excludesFile").as_deref(), Some("a b\\c"));
        assert_eq!(get(&[b"[core]\n\tbare\n"], "core", "bare").as_deref(), Some("true"));
        assert_eq!(get(&[b"[remote \"core\"]\n\tabbrev = 5\n"], "core", "abbrev"), None);
    }
}
