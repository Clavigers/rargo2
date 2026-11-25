use super::index::Entry;
use super::{Kind, Source};
use std::collections::BTreeSet;
use std::path::Path;

#[derive(Debug, PartialEq, Eq)]
enum Wild {
    Match,
    NoMatch,
    AbortAll,
    AbortToStarStar,
}

fn special(c: u8) -> bool {
    matches!(c, b'*' | b'?' | b'[' | b'\\')
}

fn class(name: &[u8], c: u8) -> Option<bool> {
    Some(match name {
        b"alnum" => c.is_ascii_alphanumeric(),
        b"alpha" => c.is_ascii_alphabetic(),
        b"blank" => c == b' ' || c == b'\t',
        b"cntrl" => c.is_ascii_control(),
        b"digit" => c.is_ascii_digit(),
        b"graph" => c.is_ascii_graphic(),
        b"lower" => c.is_ascii_lowercase(),
        b"print" => c.is_ascii_graphic() || c == b' ',
        b"punct" => c.is_ascii_punctuation(),
        b"space" => c.is_ascii_whitespace() || c == 0x0b,
        b"upper" => c.is_ascii_uppercase(),
        b"xdigit" => c.is_ascii_hexdigit(),
        _ => return None,
    })
}

fn dowild(p: &[u8], text: &[u8], pathname: bool) -> Wild {
    let at = |s: &[u8], i: usize| s.get(i).copied().unwrap_or(0);
    let (mut pi, mut ti) = (0usize, 0usize);
    while pi < p.len() {
        let mut p_ch = p[pi];
        let t_ch = at(text, ti);
        if t_ch == 0 && p_ch != b'*' {
            return Wild::AbortAll;
        }
        match p_ch {
            b'?' => {
                if pathname && t_ch == b'/' {
                    return Wild::NoMatch;
                }
            }
            b'*' => {
                let match_slash;
                pi += 1;
                if at(p, pi) == b'*' {
                    let prev = pi.checked_sub(2);
                    while at(p, pi) == b'*' {
                        pi += 1;
                    }
                    let next = at(p, pi);
                    if (prev.is_none() || p[prev.unwrap()] == b'/') && (next == 0 || next == b'/' || (next == b'\\' && at(p, pi + 1) == b'/')) {
                        if next == b'/' && dowild(&p[pi + 1..], &text[ti..], pathname) == Wild::Match {
                            return Wild::Match;
                        }
                        match_slash = true;
                    } else {
                        match_slash = false;
                    }
                } else {
                    match_slash = !pathname;
                }
                if pi >= p.len() {
                    if !match_slash && text[ti..].contains(&b'/') {
                        return Wild::NoMatch;
                    }
                    return Wild::Match;
                } else if !match_slash && p[pi] == b'/' {
                    let Some(slash) = text[ti..].iter().position(|&c| c == b'/') else { return Wild::NoMatch };
                    ti += slash;
                    pi += 1;
                    ti += 1;
                    continue;
                }
                let mut t = ti;
                loop {
                    let mut t_ch = at(text, t);
                    if t_ch == 0 {
                        break;
                    }
                    if !special(p[pi]) {
                        let lit = p[pi];
                        while t < text.len() && (match_slash || text[t] != b'/') {
                            if text[t] == lit {
                                break;
                            }
                            t += 1;
                        }
                        t_ch = at(text, t);
                        if t_ch != lit {
                            if match_slash {
                                return Wild::AbortAll;
                            }
                            break;
                        }
                    }
                    let matched = dowild(&p[pi..], &text[t..], pathname);
                    if matched != Wild::NoMatch {
                        if !match_slash || matched != Wild::AbortToStarStar {
                            return matched;
                        }
                    } else if !match_slash && t_ch == b'/' {
                        return Wild::AbortToStarStar;
                    }
                    t += 1;
                }
                return Wild::AbortAll;
            }
            b'[' => {
                pi += 1;
                p_ch = at(p, pi);
                if p_ch == b'^' {
                    p_ch = b'!';
                }
                let negated = p_ch == b'!';
                if negated {
                    pi += 1;
                    p_ch = at(p, pi);
                }
                let mut prev_ch = 0u8;
                let mut matched = false;
                loop {
                    if p_ch == 0 {
                        return Wild::AbortAll;
                    }
                    if p_ch == b'\\' {
                        pi += 1;
                        p_ch = at(p, pi);
                        if p_ch == 0 {
                            return Wild::AbortAll;
                        }
                        if t_ch == p_ch {
                            matched = true;
                        }
                    } else if p_ch == b'-' && prev_ch != 0 && at(p, pi + 1) != 0 && at(p, pi + 1) != b']' {
                        pi += 1;
                        p_ch = at(p, pi);
                        if p_ch == b'\\' {
                            pi += 1;
                            p_ch = at(p, pi);
                            if p_ch == 0 {
                                return Wild::AbortAll;
                            }
                        }
                        if t_ch <= p_ch && t_ch >= prev_ch {
                            matched = true;
                        }
                        p_ch = 0;
                    } else if p_ch == b'[' && at(p, pi + 1) == b':' {
                        let s = pi + 2;
                        let mut q = s;
                        while at(p, q) != 0 && at(p, q) != b']' {
                            q += 1;
                        }
                        if at(p, q) == 0 {
                            return Wild::AbortAll;
                        }
                        if q < s + 1 || p[q - 1] != b':' {
                            pi = s - 2;
                            p_ch = b'[';
                            if t_ch == p_ch {
                                matched = true;
                            }
                            prev_ch = p_ch;
                            pi += 1;
                            p_ch = at(p, pi);
                            if p_ch == b']' {
                                break;
                            }
                            continue;
                        }
                        pi = q;
                        match class(&p[s..q - 1], t_ch) {
                            Some(true) => matched = true,
                            Some(false) => {}
                            None => return Wild::AbortAll,
                        }
                        p_ch = 0;
                    } else if t_ch == p_ch {
                        matched = true;
                    }
                    prev_ch = p_ch;
                    pi += 1;
                    p_ch = at(p, pi);
                    if p_ch == b']' {
                        break;
                    }
                }
                if matched == negated || (pathname && t_ch == b'/') {
                    return Wild::NoMatch;
                }
            }
            _ => {
                if p_ch == b'\\' {
                    pi += 1;
                    p_ch = at(p, pi);
                }
                if t_ch != p_ch {
                    return Wild::NoMatch;
                }
            }
        }
        pi += 1;
        ti += 1;
    }
    if ti < text.len() { Wild::NoMatch } else { Wild::Match }
}

pub fn wildmatch(pattern: &[u8], text: &[u8], pathname: bool) -> bool {
    dowild(pattern, text, pathname) == Wild::Match
}

#[derive(Clone, Debug)]
pub struct Pattern {
    pattern: Vec<u8>,
    base: Vec<u8>,
    negative: bool,
    must_be_dir: bool,
    no_dir: bool,
}

fn trim_trailing_spaces(line: &[u8]) -> &[u8] {
    let mut last_space = None;
    let mut i = 0;
    while i < line.len() {
        match line[i] {
            b' ' => {
                if last_space.is_none() {
                    last_space = Some(i);
                }
            }
            b'\\' => {
                i += 1;
                if i >= line.len() {
                    return line;
                }
                last_space = None;
            }
            _ => last_space = None,
        }
        i += 1;
    }
    &line[..last_space.unwrap_or(line.len())]
}

pub fn patterns(bytes: &[u8], base: &[u8]) -> Vec<Pattern> {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    let mut out = Vec::new();
    for line in bytes.split(|&b| b == b'\n') {
        if line.is_empty() || line[0] == b'#' {
            continue;
        }
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let mut p = trim_trailing_spaces(line);
        let negative = p.first() == Some(&b'!');
        if negative {
            p = &p[1..];
        }
        let must_be_dir = p.last() == Some(&b'/');
        if must_be_dir {
            p = &p[..p.len() - 1];
        }
        let no_dir = !p.contains(&b'/');
        out.push(Pattern { pattern: p.to_vec(), base: base.to_vec(), negative, must_be_dir, no_dir });
    }
    out
}

impl Pattern {
    fn matches(&self, path: &[u8], is_dir: bool) -> bool {
        if self.must_be_dir && !is_dir {
            return false;
        }
        if self.no_dir {
            let name = path.rsplit(|&b| b == b'/').next().unwrap_or(path);
            return wildmatch(&self.pattern, name, false);
        }
        let pattern = self.pattern.strip_prefix(b"/").unwrap_or(&self.pattern);
        let name = if self.base.is_empty() {
            path
        } else {
            match path.strip_prefix(self.base.as_slice()) {
                Some(rest) => rest,
                None => return false,
            }
        };
        wildmatch(pattern, name, true)
    }
}

pub fn excluded(lists: &[Vec<Pattern>], path: &[u8], is_dir: bool) -> bool {
    lists.iter().rev().flat_map(|l| l.iter().rev()).find(|p| p.matches(path, is_dir)).is_some_and(|p| !p.negative)
}

struct Walk<'a, S> {
    src: &'a S,
    root: &'a Path,
    tracked: BTreeSet<&'a [u8]>,
    tracked_dirs: BTreeSet<&'a [u8]>,
    out: Vec<Vec<u8>>,
}

fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    if dir.is_empty() { name.to_vec() } else { [dir, b"/", name].concat() }
}

impl<S: Source> Walk<'_, S> {
    fn at(&self, rel: &[u8]) -> std::path::PathBuf {
        use std::os::unix::ffi::OsStrExt;
        if rel.is_empty() { self.root.to_path_buf() } else { self.root.join(std::ffi::OsStr::from_bytes(rel)) }
    }

    fn nested_repo(&self, rel: &[u8]) -> bool {
        let dot_git = self.at(&join(rel, b".git"));
        self.src.read(&dot_git.join("HEAD")).is_some()
            || self.src.read(&dot_git).is_some_and(|b| b.as_ref().starts_with(b"gitdir:"))
    }

    fn visit(&mut self, dir: &[u8], lists: &mut Vec<Vec<Pattern>>, parent_excluded: bool) -> bool {
        let mut entries = self.src.list(&self.at(dir)).unwrap_or_default();
        entries.retain(|(n, _)| n != b".git");
        entries.sort();
        let loaded = !parent_excluded && entries.iter().any(|(n, k)| n == b".gitignore" && *k == Kind::File);
        if loaded {
            let base = if dir.is_empty() { Vec::new() } else { [dir, b"/"].concat() };
            let text = self.src.read(&self.at(&join(dir, b".gitignore"))).map(|b| b.as_ref().to_vec()).unwrap_or_default();
            lists.push(patterns(&text, &base));
        }
        let mut all = !entries.is_empty();
        for (name, kind) in entries {
            let path = join(dir, &name);
            if kind == Kind::Dir {
                if self.tracked.contains(path.as_slice()) {
                    all = false;
                    continue;
                }
                if self.tracked_dirs.contains(path.as_slice()) {
                    let excluded_here = parent_excluded || excluded(lists, &path, true);
                    self.visit(&path, lists, excluded_here);
                    all = false;
                    continue;
                }
                if parent_excluded || excluded(lists, &path, true) {
                    self.out.push([path.as_slice(), b"/"].concat());
                    continue;
                }
                if self.nested_repo(&path) {
                    all = false;
                    continue;
                }
                if self.visit(&path, lists, false) {
                    self.out.push([path.as_slice(), b"/"].concat());
                } else {
                    all = false;
                }
            } else if self.tracked.contains(path.as_slice()) {
                all = false;
            } else if parent_excluded || excluded(lists, &path, false) {
                self.out.push(path);
            } else {
                all = false;
            }
        }
        if loaded {
            lists.pop();
        }
        all
    }
}

pub fn ignored<S: Source>(src: &S, root: &Path, index: &[Entry], base_lists: Vec<Vec<Pattern>>) -> Vec<Vec<u8>> {
    let mut tracked = BTreeSet::new();
    let mut tracked_dirs = BTreeSet::new();
    for e in index {
        let path = e.path.strip_suffix(b"/").unwrap_or(&e.path);
        tracked.insert(path);
        let mut at = path;
        while let Some(i) = at.iter().rposition(|&b| b == b'/') {
            at = &at[..i];
            if !tracked_dirs.insert(at) {
                break;
            }
        }
    }
    let mut walk = Walk { src, root, tracked, tracked_dirs, out: Vec::new() };
    let mut lists = base_lists;
    walk.visit(b"", &mut lists, false);
    walk.out.sort();
    walk.out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildmatch_agrees_with_gits_own_cases() {
        for (expect, text, pattern, pathname) in [
            (true, "foo", "foo", true),
            (false, "bar", "foo", true),
            (true, "foo", "???", true),
            (true, "foo", "*f*", true),
            (true, "foo/bar", "foo/*", true),
            (false, "foo/bar/baz", "foo/*", true),
            (true, "foo/bar/baz", "foo/*", false),
            (true, "foo/bar/baz", "foo/**", true),
            (true, "foo/bar", "foo/**/bar", true),
            (true, "foo/a/b/bar", "foo/**/bar", true),
            (true, "a/b/deep2/c", "**/deep2/**", true),
            (false, "a/b/deep2", "**/deep2/**", true),
            (true, "deep/x", "**/x", true),
            (true, "x", "**/x", true),
            (false, "foo/bar", "foo?bar", true),
            (true, "foo/bar", "foo?bar", false),
            (true, "a]", "[]a]]", true),
            (true, "-", "[a-]", true),
            (true, "b1.txt", "[a-c]?.txt", true),
            (false, "d2.txt", "[a-c]?.txt", true),
            (true, "x", "[!a-c]", true),
            (true, "x", "[^a-c]", true),
            (false, "b", "[!a-c]", true),
            (true, "a1", "[[:alpha:]][[:digit:]]", true),
            (false, "1a", "[[:alpha:]][[:digit:]]", true),
            (true, "[", "[[]", true),
            (true, "#hash", "\\#hash", true),
            (true, "*", "\\*", true),
            (false, "a", "\\*", true),
            (true, "", "*", true),
            (true, "foo.log", "*.log", true),
            (false, "foo.logx", "*.log", true),
            (true, "XXX/adobe/courier/bold/o/normal//12/120/75/75/m/70/iso8859/1", "XXX/*/*/*/*/*/*/12/*/*/*/m/*/*/*", true),
            (false, "abc/def", "a*b*c", true),
            (true, "ab/cd", "**/cd", true),
        ] {
            assert_eq!(wildmatch(pattern.as_bytes(), text.as_bytes(), pathname), expect, "{pattern} {text} {pathname}");
        }
    }

    #[test]
    fn pattern_lines_are_parsed_as_git_parses_them() {
        let p = patterns(b"\xef\xbb\xbf#c\n\\#hash\nfoo \\ \nbar  \r\n!keep/\n/top\n\n", b"sub/");
        let shown: Vec<(String, bool, bool, bool)> =
            p.iter().map(|p| (String::from_utf8_lossy(&p.pattern).into_owned(), p.negative, p.must_be_dir, p.no_dir)).collect();
        assert_eq!(
            shown,
            [
                ("\\#hash".into(), false, false, true),
                ("foo \\ ".into(), false, false, true),
                ("bar".into(), false, false, true),
                ("keep".into(), true, true, true),
                ("/top".into(), false, false, false),
            ]
        );
        assert!(p[4].matches(b"sub/top", false) && !p[4].matches(b"top", false) && !p[4].matches(b"sub/x/top", false));
        assert!(p[2].matches(b"a/b/bar", false));
        assert!(p[3].matches(b"x/keep", true) && !p[3].matches(b"x/keep", false));
    }
}
