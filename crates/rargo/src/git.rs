use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Mode {
    Off,
    #[default]
    Detect,
    Head,
    History,
}

#[derive(Clone)]
pub struct Head {
    pub text: String,
    pub branch: Option<String>,
    pub commit: String,
    pub common: PathBuf,
    pub tags: u64,
}

fn is_hex40(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn relative_to(base: &Path, name: &str) -> PathBuf {
    let path = PathBuf::from(name.trim());
    if path.is_absolute() { path } else { base.join(path) }
}

fn git_link(root: &Path, contents: &[u8]) -> Option<PathBuf> {
    let name = std::str::from_utf8(contents).ok()?.strip_prefix("gitdir:")?;
    Some(relative_to(root, name))
}

enum HeadRef<'a> {
    Detached(&'a str),
    Symbolic(&'a str),
}

fn parse_head(contents: &[u8]) -> Option<(&str, HeadRef<'_>)> {
    let text = std::str::from_utf8(contents).ok()?.trim();
    let value = match text.strip_prefix("ref: ") {
        Some(name) => HeadRef::Symbolic(name.trim()),
        None if is_hex40(text) => HeadRef::Detached(text),
        None => return None,
    };
    Some((text, value))
}

fn packed_ref<'a>(contents: &'a [u8], wanted: &str) -> Option<&'a str> {
    std::str::from_utf8(contents).ok()?.lines().find_map(|line| {
        let (id, name) = line.split_once(' ')?;
        (name == wanted && is_hex40(id)).then_some(id)
    })
}

fn loose_ref(contents: &[u8]) -> Option<&str> {
    let id = std::str::from_utf8(contents).ok()?.trim();
    is_hex40(id).then_some(id)
}

pub fn head(root: &Path) -> Option<Head> {
    let dot_git = root.join(".git");
    let git_dir = if dot_git.is_file() {
        git_link(root, &std::fs::read(&dot_git).ok()?)?
    } else {
        dot_git
    };
    let contents = std::fs::read(git_dir.join("HEAD")).ok()?;
    let (text, value) = parse_head(&contents)?;
    let common = std::fs::read_to_string(git_dir.join("commondir"))
        .map_or_else(|_| git_dir.clone(), |name| relative_to(&git_dir, &name));
    let tags = tags_seal(&common);
    let (branch, commit) = match value {
        HeadRef::Detached(commit) => (None, commit.to_string()),
        HeadRef::Symbolic(name) => {
            // The worktree's own ref shadows the shared ref and packed refs.
            let loose = std::fs::read_to_string(git_dir.join(name)).or_else(|_| std::fs::read_to_string(common.join(name)));
            let commit = match loose {
                Ok(bytes) => loose_ref(bytes.as_bytes())?.to_string(),
                Err(_) => packed_ref(&std::fs::read(common.join("packed-refs")).ok()?, name)?.to_string(),
            };
            (Some(name.to_string()), commit)
        }
    };
    Some(Head { text: text.to_string(), branch, commit, common, tags })
}

fn tags_seal(common: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let mut records = Vec::new();
    let mut add = |name: &[u8], m: &std::fs::Metadata| {
        records.push((name.to_vec(), m.len(), m.mtime(), m.mtime_nsec()));
    };
    if let Ok(m) = std::fs::metadata(common.join("packed-refs")) {
        add(b"packed-refs", &m);
    }
    let mut stack = vec![common.join("refs/tags")];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let Ok(m) = e.metadata() else { continue };
            if m.is_dir() {
                stack.push(e.path());
            } else {
                use std::os::unix::ffi::OsStrExt;
                add(e.path().as_os_str().as_bytes(), &m);
            }
        }
    }
    seal(records)
}

fn seal(records: Vec<(Vec<u8>, u64, i64, i64)>) -> u64 {
    let mut h = blake3::Hasher::new();
    for (name, len, seconds, nanos) in records {
        h.update(&name);
        h.update(&len.to_le_bytes());
        h.update(&seconds.to_le_bytes());
        h.update(&nanos.to_le_bytes());
    }
    u64::from_le_bytes(h.finalize().as_bytes()[..8].try_into().unwrap_or_default())
}

fn git(root: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(root).arg("--no-optional-locks").stdin(Stdio::null()).stderr(Stdio::null());
    c
}

fn output(mut c: Command) -> Option<Vec<u8>> {
    c.output().ok().filter(|o| o.status.success()).map(|o| o.stdout)
}

fn tags_for(root: &Path, head: &Head, history: bool) -> Option<(String, Vec<String>)> {
    let mut c = git(root);
    c.args(["for-each-ref", "--format=%(objectname) %(objecttype) %(*objectname) %(refname)"]);
    if history {
        c.arg(format!("--merged={}", head.commit));
    } else {
        c.arg(format!("--points-at={}", head.commit));
    }
    c.arg("refs/tags");
    Some(stand_in_tags(&output(c)?))
}

fn stand_in_tags(listed: &[u8]) -> (String, Vec<String>) {
    let mut lines: Vec<(String, String, Option<String>)> = Vec::new();
    for line in String::from_utf8_lossy(listed).lines() {
        let parts: Vec<&str> = line.split(' ').collect();
        match parts.as_slice() {
            [oid, "tag", peeled, name] => lines.push((name.to_string(), oid.to_string(), Some(peeled.to_string()))),
            [oid, "commit", "", name] => lines.push((name.to_string(), oid.to_string(), None)),
            _ => {}
        }
    }
    lines.sort();
    let mut text = String::from("# pack-refs with: peeled fully-peeled sorted \n");
    let mut objects = Vec::new();
    for (name, oid, peeled) in lines {
        text.push_str(&format!("{oid} {name}\n"));
        if let Some(p) = peeled {
            text.push_str(&format!("^{p}\n"));
            objects.push(oid);
        }
    }
    (text, objects)
}

fn build(root: &Path, head: &Head, history: bool, dir: &Path) -> Option<()> {
    std::fs::create_dir_all(dir.join("objects/pack")).ok()?;
    std::fs::create_dir_all(dir.join("refs/heads")).ok()?;
    std::fs::write(dir.join("HEAD"), format!("{}\n", head.text)).ok()?;
    if let Some(branch) = &head.branch {
        if branch.split('/').any(|c| c.is_empty() || c == "." || c == "..") || branch.starts_with('/') {
            return None;
        }
        let p = dir.join(branch);
        std::fs::create_dir_all(p.parent()?).ok()?;
        std::fs::write(p, format!("{}\n", head.commit)).ok()?;
    }
    if !history {
        std::fs::write(dir.join("shallow"), format!("{}\n", head.commit)).ok()?;
    }
    let (refs, tag_objects) = tags_for(root, head, history)?;
    std::fs::write(dir.join("packed-refs"), refs).ok()?;
    let short = output({
        let mut c = git(root);
        c.args(["rev-parse", "--short", &head.commit]);
        c
    })?;
    let abbrev = String::from_utf8_lossy(&short).trim().len().clamp(4, 40);
    std::fs::write(
        dir.join("config"),
        format!("[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n\tabbrev = {abbrev}\n"),
    )
    .ok()?;
    let mut ids = output({
        let mut c = git(root);
        c.args(["rev-list", "--objects", "--no-object-names", "--filter=blob:none", "--no-walk", &head.commit]);
        c
    })?;
    let tree = output({
        let mut c = git(root);
        c.args(["ls-tree", "-r", "-z", &head.commit]);
        c
    })?;
    append_stand_in_objects(&mut ids, &tree, tag_objects);
    let mut pack = git(root)
        .args(["pack-objects", "--threads=1", "-q"])
        .arg(dir.join("objects/pack/pack"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .ok()?;
    pack.stdin.take()?.write_all(&ids).ok()?;
    if !pack.wait().ok()?.success() {
        return None;
    }
    std::fs::create_dir_all(dir.join("info")).ok()?;
    std::fs::write(dir.join("info/exclude"), format!("/{}\n", rargo_proto::GIT_SIDE)).ok()?;
    let read = git(root).env("GIT_INDEX_FILE", dir.join("index")).args(["read-tree", &head.commit]).status().ok()?;
    read.success().then_some(())
}

fn append_stand_in_objects(ids: &mut Vec<u8>, tree: &[u8], tag_objects: Vec<String>) {
    for rec in tree.split(|&b| b == 0) {
        if rec.starts_with(b"120000 blob ") && rec.len() >= 52 {
            ids.extend_from_slice(&rec[12..52]);
            ids.push(b'\n');
        }
    }
    for t in tag_objects {
        ids.extend_from_slice(t.as_bytes());
        ids.push(b'\n');
    }
}

pub fn ignored_paths(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes.split(|&b| b == 0).filter(|path| !path.is_empty())
}

pub fn thin(root: &Path, head: &Head, cache: &Path, history: bool) -> Option<PathBuf> {
    let key = blake3::hash(format!("v4\n{}\n{}\n{}\n{history}", head.text, head.commit, head.tags).as_bytes()).to_hex()[..32].to_string();
    let all = cache.join("git");
    let dir = all.join(&key);
    if dir.join("HEAD").is_file() {
        return Some(dir);
    }
    crate::tmp::sweep(&all, ".tmp-");
    let tmp = all.join(format!(".tmp-{}-{key}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let made = build(root, head, history, &tmp).and_then(|()| match std::fs::rename(&tmp, &dir) {
        Ok(()) => Some(()),
        Err(_) if dir.join("HEAD").is_file() => Some(()),
        Err(_) => None,
    });
    let _ = std::fs::remove_dir_all(&tmp);
    made?;
    for e in std::fs::read_dir(&all).into_iter().flatten().flatten() {
        let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
        if e.path() != dir && old.is_some_and(|a| a.as_secs() > 14 * 86400) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
    Some(dir)
}

const PIECES: [usize; 4] = [4096, 256, 16, 1];

pub fn history(root: &Path, head: &Head, cache: &Path) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let common = head.common.canonicalize().unwrap_or_else(|_| head.common.clone());
    let key = blake3::hash(common.as_os_str().as_bytes()).to_hex()[..32].to_string();
    let store = cache.join("git-history").join(key);
    let at = store.join("heads").join(&head.commit);
    if at.join("pack").is_dir() {
        return Some(at);
    }
    let pieces_dir = store.join("pieces");
    std::fs::create_dir_all(&pieces_dir).ok()?;
    let listed = output({
        let mut c = git(root);
        c.args(["rev-list", "--first-parent", &head.commit]);
        c
    })?;
    let pieces = history_pieces(&listed)?;
    let tmp = store.join("heads").join(format!(".tmp-{}-{}", std::process::id(), head.commit));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("pack")).ok()?;
    for HistoryPiece { key: piece, before, last } in pieces {
        let (pack, idx) = piece_files(root, &pieces_dir, &piece, before, last)?;
        for (src, ext) in [(&pack, "pack"), (&idx, "idx")] {
            std::fs::hard_link(src, tmp.join("pack").join(format!("pack-{piece}.{ext}"))).ok()?;
        }
    }
    match std::fs::rename(&tmp, &at) {
        Ok(()) => {}
        Err(_) if at.join("pack").is_dir() => {
            let _ = std::fs::remove_dir_all(&tmp);
        }
        Err(_) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return None;
        }
    }
    sweep_history(&store);
    Some(at)
}

struct HistoryPiece<'a> {
    key: String,
    before: Option<&'a str>,
    last: &'a str,
}

fn history_pieces(first_parent: &[u8]) -> Option<Vec<HistoryPiece<'_>>> {
    let mut chain: Vec<_> = std::str::from_utf8(first_parent).ok()?.lines().collect();
    chain.reverse();
    let mut pieces = Vec::new();
    let mut start = 0;
    for size in PIECES {
        while start + size <= chain.len() {
            let before = start.checked_sub(1).map(|i| chain[i]);
            let last = chain[start + size - 1];
            let key = blake3::hash(format!("{}\n{last}", before.unwrap_or("")).as_bytes()).to_hex()[..32].to_string();
            pieces.push(HistoryPiece { key, before, last });
            start += size;
        }
    }
    Some(pieces)
}

fn piece_files(root: &Path, dir: &Path, piece: &str, before: Option<&str>, last: &str) -> Option<(PathBuf, PathBuf)> {
    let (pack, idx) = (dir.join(format!("{piece}.pack")), dir.join(format!("{piece}.idx")));
    if pack.is_file() && idx.is_file() {
        let _ = std::fs::File::options().append(true).open(&pack).and_then(|f| f.set_modified(std::time::SystemTime::now()));
        return Some((pack, idx));
    }
    let mut revs = format!("{last}\n");
    if let Some(b) = before {
        revs.push_str(&format!("^{b}\n"));
    }
    let commits = {
        let mut walk = git(root).args(["rev-list", "--stdin"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().ok()?;
        walk.stdin.take()?.write_all(revs.as_bytes()).ok()?;
        let mut listed = Vec::new();
        walk.stdout.take()?.read_to_end(&mut listed).ok()?;
        walk.wait().ok()?.success().then_some(listed)?
    };
    let base = dir.join(format!(".tmp-{}-{piece}", std::process::id()));
    let mut packer = git(root)
        .args(["pack-objects", "--threads=1", "-q"])
        .arg(&base)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    {
        let mut input = packer.stdin.take()?;
        std::thread::spawn(move || input.write_all(&commits));
    }
    let mut name = String::new();
    packer.stdout.take()?.read_to_string(&mut name).ok()?;
    let packed = packer.wait().ok()?.success();
    let made = format!("{}-{}", base.display(), name.trim());
    let (made_pack, made_idx) = (PathBuf::from(format!("{made}.pack")), PathBuf::from(format!("{made}.idx")));
    let moved = packed && std::fs::rename(&made_idx, &idx).is_ok() && std::fs::rename(&made_pack, &pack).is_ok();
    let _ = std::fs::remove_file(&made_pack);
    let _ = std::fs::remove_file(&made_idx);
    (moved || (pack.is_file() && idx.is_file())).then_some((pack, idx))
}

fn sweep_history(store: &Path) {
    let stale = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|a| a.as_secs() > 14 * 86400);
    for e in std::fs::read_dir(store.join("heads")).into_iter().flatten().flatten() {
        if stale(&e.path()) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
    use std::os::unix::fs::MetadataExt;
    for e in std::fs::read_dir(store.join("pieces")).into_iter().flatten().flatten() {
        if e.metadata().is_ok_and(|m| m.nlink() == 1) && stale(&e.path()) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

const HISTORY_CRATES: &[&str] = &["vergen", "vergen-gitcl", "vergen-git2", "vergen-gix", "shadow-rs", "git-version", "built", "git-testament"];

fn history_input(rel: &[u8]) -> Option<&[u8]> {
    let name = rel.rsplit(|&b| b == b'/').next().unwrap_or(rel);
    matches!(name, b"build.rs" | b"Cargo.lock").then_some(name)
}

fn needs_history(name: &[u8], text: &str) -> bool {
    if name == b"build.rs" {
        text.contains("describe") || text.contains("rev-list")
    } else {
        text.lines().any(|l| l.strip_prefix("name = \"").and_then(|n| n.strip_suffix('"')).is_some_and(|n| HISTORY_CRATES.contains(&n)))
    }
}

pub fn asks_for_history<'a>(root: &Path, files: impl Iterator<Item = &'a [u8]>) -> bool {
    use std::os::unix::ffi::OsStrExt;
    for rel in files {
        let Some(name) = history_input(rel) else { continue };
        let Ok(text) = std::fs::read_to_string(root.join(std::ffi::OsStr::from_bytes(rel))) else { continue };
        if needs_history(name, &text) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "init.defaultBranch=main"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn thin_git_answers_what_build_scripts_ask() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        std::os::unix::fs::symlink("src/lib.rs", repo.join("link")).unwrap();
        run(&repo, &["init", "-q"]);
        run(&repo, &["checkout", "-q", "-b", "feature/x"]);
        run(&repo, &["add", "-A"]);
        run(&repo, &["commit", "-q", "-m", "first"]);
        std::fs::write(repo.join("src/lib.rs"), "pub fn b() {}\n").unwrap();
        run(&repo, &["commit", "-q", "-am", "second"]);
        let commit = run(&repo, &["rev-parse", "HEAD"]);

        let h = head(&repo).unwrap();
        assert_eq!(h.commit, commit);
        assert_eq!(h.branch.as_deref(), Some("refs/heads/feature/x"));
        let cache = tmp.path().join("cache");
        let thin_dir = thin(&repo, &h, &cache, false).unwrap();
        assert_eq!(thin(&repo, &h, &cache, false).unwrap(), thin_dir, "made once per commit");

        let copy = tmp.path().join("copy");
        std::fs::create_dir_all(copy.join("src")).unwrap();
        std::fs::write(copy.join("src/lib.rs"), "pub fn b() {}\n").unwrap();
        std::os::unix::fs::symlink("src/lib.rs", copy.join("link")).unwrap();
        let status = Command::new("cp").arg("-r").arg(&thin_dir).arg(copy.join(".git")).status().unwrap();
        assert!(status.success());
        assert_eq!(run(&copy, &["rev-parse", "HEAD"]), commit);
        assert_eq!(run(&copy, &["log", "-1", "--format=%H %s"]), format!("{commit} second"));
        assert_eq!(run(&copy, &["rev-parse", "--abbrev-ref", "HEAD"]), "feature/x");
        assert_eq!(run(&copy, &["status", "--porcelain"]), "");
        let short = run(&repo, &["rev-parse", "--short", "HEAD"]);
        assert_eq!(run(&copy, &["describe", "--always", "--dirty"]), short);
        assert_eq!(run(&copy, &["log", "-1", "--format=%h"]), short);
        run(&repo, &["config", "core.abbrev", "11"]);
        let h11 = head(&repo).unwrap();
        let other_cache = tmp.path().join("cache11");
        let thin11 = thin(&repo, &h11, &other_cache, false).unwrap();
        assert!(std::fs::read_to_string(thin11.join("config")).unwrap().contains("abbrev = 11"));
        run(&repo, &["config", "--unset", "core.abbrev"]);
        std::fs::write(copy.join("src/lib.rs"), "pub fn c() {}\n").unwrap();
        assert_eq!(run(&copy, &["status", "--porcelain"]), "M src/lib.rs");
        assert!(run(&copy, &["describe", "--always", "--dirty"]).ends_with("-dirty"));
        assert_eq!(run(&copy, &["rev-list", "--count", "HEAD"]), "1");
        let size: u64 = walk(&thin_dir);
        assert!(size < 20_000, "{size} bytes");

        let wt = tmp.path().join("wt");
        run(&repo, &["worktree", "add", "-q", "--detach", wt.to_str().unwrap(), "HEAD~1"]);
        let h2 = head(&wt).unwrap();
        assert_eq!(h2.commit, run(&repo, &["rev-parse", "HEAD~1"]));
        assert!(h2.branch.is_none());
        assert_ne!(thin(&wt, &h2, &cache, false).unwrap(), thin_dir);
        assert!(head(tmp.path()).is_none());
    }

    fn show(stand_in: &Path, history: Option<&Path>, copy: &Path) {
        let _ = std::fs::remove_dir_all(copy.join(".git"));
        assert!(Command::new("cp").arg("-r").arg(stand_in).arg(copy.join(".git")).status().unwrap().success());
        if let Some(h) = history {
            let status = Command::new("sh").arg("-c").arg(format!("cp -r '{}'/. '{}'", h.display(), copy.join(".git/objects").display())).status().unwrap();
            assert!(status.success());
        }
    }

    #[test]
    fn with_history_describe_and_counts_answer_as_on_the_client() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        run(&repo, &["init", "-q"]);
        let commit = |msg: &str| {
            let file = if msg.starts_with("side") || msg == "elsewhere" { "g" } else { "f" };
            std::fs::write(repo.join(file), msg).unwrap();
            run(&repo, &["add", "-A"]);
            run(&repo, &["commit", "-q", "-m", msg]);
        };
        commit("one");
        run(&repo, &["tag", "-a", "v0.1.0", "-m", "first release"]);
        commit("two");
        commit("three");
        run(&repo, &["checkout", "-q", "-b", "side", "HEAD~1"]);
        commit("side one");
        run(&repo, &["tag", "side-light"]);
        commit("side two");
        run(&repo, &["checkout", "-q", "main"]);
        run(&repo, &["merge", "-q", "--no-ff", "-m", "merge side", "side"]);
        commit("four");
        run(&repo, &["tag", "-a", "v0.2.0", "-m", "second release"]);
        commit("five");
        commit("six");
        run(&repo, &["checkout", "-q", "side"]);
        commit("elsewhere");
        run(&repo, &["tag", "elsewhere"]);
        run(&repo, &["checkout", "-q", "main"]);

        let h = head(&repo).unwrap();
        let cache = tmp.path().join("cache");
        let objects = history(&repo, &h, &cache).unwrap();
        let stand_in = thin(&repo, &h, &cache, true).unwrap();
        assert!(!stand_in.join("shallow").exists());
        let copy = tmp.path().join("copy");
        std::fs::create_dir_all(&copy).unwrap();
        std::fs::write(copy.join("f"), "six").unwrap();
        std::fs::write(copy.join("g"), "side two").unwrap();
        show(&stand_in, Some(&objects), &copy);
        for q in [
            &["describe"][..],
            &["describe", "--tags"],
            &["describe", "--tags", "--long", "--abbrev=12"],
            &["describe", "--tags", "--abbrev=0"],
            &["describe", "--always", "--dirty"],
            &["rev-list", "--count", "HEAD"],
            &["rev-list", "--count", "v0.1.0..HEAD"],
            &["log", "-1", "--date=short", "--abbrev=9", "--format=%H %h %cd %(describe:tags)"],
            &["log", "--format=%s"],
            &["tag", "--merged", "HEAD"],
        ] {
            assert_eq!(run(&copy, q), run(&repo, q), "git {q:?}");
        }
        assert_eq!(run(&copy, &["rev-list", "--count", "HEAD"]), "9");
        assert_eq!(run(&copy, &["tag"]), run(&repo, &["tag", "--merged", "HEAD"]), "only the tags HEAD reaches");

        let pieces = objects.parent().unwrap().parent().unwrap().join("pieces");
        let before = packs(&pieces);
        commit("seven");
        let h7 = head(&repo).unwrap();
        let objects7 = history(&repo, &h7, &cache).unwrap();
        assert_eq!(objects7.parent(), objects.parent(), "one store per repository");
        assert_eq!(packs(&pieces), before + 1);
        assert_eq!(packs(&objects7.join("pack")), before + 1, "the first-parent chain is 8 commits: 8 pieces of one");
        let seen = thin(&repo, &h7, &cache, true).unwrap();
        run(&repo, &["tag", "-a", "v0.3.0", "-m", "third release"]);
        let tagged = head(&repo).unwrap();
        assert_ne!(tagged.tags, h7.tags, "the tags' seal changed");
        let stand_in = thin(&repo, &tagged, &cache, true).unwrap();
        assert_ne!(stand_in, seen);
        std::fs::write(copy.join("f"), "seven").unwrap();
        show(&stand_in, Some(&objects7), &copy);
        assert_eq!(run(&copy, &["describe"]), "v0.3.0");

        let bare = thin(&repo, &tagged, &cache, false).unwrap();
        show(&bare, None, &copy);
        assert_eq!(run(&copy, &["describe", "--tags"]), "v0.3.0");
        assert_eq!(run(&copy, &["rev-list", "--count", "HEAD"]), "1");
    }

    fn packs(dir: &Path) -> usize {
        std::fs::read_dir(dir).unwrap().flatten().filter(|e| e.path().extension().is_some_and(|x| x == "pack")).count()
    }

    #[test]
    fn captured_worktree_submodule_and_packed_refs() {
        const COMMIT: &str = "aa8336085721c9907cd5cfdb1e9239873f4c94c7";
        const TAG: &str = "345dea299c18c71148e55a1573935dcd11cbf9c8";
        const LINK: &str = "b285d1235d007189c79da4d8625e7f7565e329a2";
        let packed = b"# pack-refs with: peeled fully-peeled sorted \naa8336085721c9907cd5cfdb1e9239873f4c94c7 refs/heads/feature/capture\naa8336085721c9907cd5cfdb1e9239873f4c94c7 refs/heads/main\n345dea299c18c71148e55a1573935dcd11cbf9c8 refs/tags/v1\n^aa8336085721c9907cd5cfdb1e9239873f4c94c7\n";
        let (_, HeadRef::Symbolic(name)) = parse_head(b"ref: refs/heads/feature/capture\n").unwrap() else { panic!("expected worktree branch") };
        assert_eq!(packed_ref(packed, name), Some(COMMIT));
        assert_eq!(packed_ref(packed, "heads/main"), None);
        let detached = format!("{COMMIT}\n");
        let (_, HeadRef::Detached(id)) = parse_head(detached.as_bytes()).unwrap() else { panic!("expected detached commit") };
        assert_eq!(id, COMMIT);
        let common = relative_to(Path::new("/repo/.git/worktrees/worktree"), "../..\n");
        assert_eq!(common, Path::new("/repo/.git/worktrees/worktree/../.."));
        assert_eq!(git_link(Path::new("/repo/nested/module"), b"gitdir: ../../.git/modules/nested/module\n"),
            Some(PathBuf::from("/repo/nested/module/../../.git/modules/nested/module")));

        let listed = format!("{TAG} tag {COMMIT} refs/tags/v1\n");
        let (refs, objects) = stand_in_tags(listed.as_bytes());
        assert_eq!(packed_ref(refs.as_bytes(), "refs/tags/v1"), Some(TAG));
        let tree = format!("120000 blob {LINK}\tlink\0");
        let tree = [tree.as_bytes(), b"160000 commit 304d179b4a4bb58ed59cf102d34533f9e637d25c\tnested/module\0"].concat();
        let mut ids = format!("{COMMIT}\n").into_bytes();
        append_stand_in_objects(&mut ids, &tree, objects);
        assert_eq!(ids, format!("{COMMIT}\n{LINK}\n{TAG}\n").into_bytes());
        assert_eq!(ignored_paths(b"cache.log\0").collect::<Vec<_>>(), [b"cache.log".as_slice()]);
    }

    #[test]
    fn history_piece_boundaries_and_detection_are_pure() {
        let listed = (0..17).rev().map(|i| format!("{i:040x}\n")).collect::<String>();
        let pieces = history_pieces(listed.as_bytes()).unwrap();
        let first_end = format!("{:040x}", 15);
        let final_end = format!("{:040x}", 16);
        assert_eq!(pieces.len(), 2);
        assert_eq!((pieces[0].before, pieces[0].last), (None, first_end.as_str()));
        assert_eq!((pieces[1].before, pieces[1].last), (Some(first_end.as_str()), final_end.as_str()));
        assert_ne!(pieces[0].key, pieces[1].key);
        assert!(!needs_history(b"build.rs", "git rev-parse HEAD"));
        assert!(needs_history(b"build.rs", "git describe --tags"));
        assert!(needs_history(b"build.rs", "git rev-list HEAD"));
        assert!(needs_history(b"Cargo.lock", "name = \"vergen-gitcl\"\n"));
        assert!(!needs_history(b"Cargo.lock", "name = \"serde\"\n"));
        assert_eq!(history_input(b"nested/build.rs"), Some(b"build.rs".as_slice()));
        assert_eq!(history_input(b"nested/src/lib.rs"), None);
    }

    fn walk(dir: &Path) -> u64 {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| if e.path().is_dir() { walk(&e.path()) } else { e.metadata().unwrap().len() })
            .sum()
    }
}
