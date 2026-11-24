//! A thin `.git` for the server's sandbox.
//!
//! Build scripts commonly ask git about the commit they are built from
//! (`git log -1`, `git rev-parse HEAD`, `git describe --always --dirty`, or
//! simply reading `.git/HEAD`), and some about history (`git describe
//! --tags` for the last release and the distance to it, `git rev-list
//! --count HEAD`). The real `.git` is never synced: it can be gigabytes,
//! and nothing in it but the answer to those questions is an input to a
//! build. Instead the client makes a directory that is to git a clone
//! without file contents:
//!
//! - always: `HEAD`, the branch it points to, the commit object, its
//!   trees, the targets of tracked symlinks, an index read from that
//!   commit, and the tags on it. It depends only on the commit and the
//!   tags, so it is made once per commit and shared by every worktree at
//!   that commit; a few hundred kilobytes for a large repository.
//! - with history ([`Mode::History`]): every commit before HEAD (without
//!   their trees) and every tag on them, so history queries answer as on
//!   the client. The commits are packed in pieces that never change once
//!   made ([`PIECES`]), so a new commit sends a small pack, not the
//!   history again. Without history the stand-in is a clone of depth one
//!   (a `shallow` file).
//!
//! History costs: rust-analyzer's 42,000 commits are 26 MiB, 11 MiB
//! compressed, where its sources compress to 4 MiB. So it is sent where
//! the project asks for it, and by default where a build evidently asks
//! git for history ([`asks_for_history`]).
//!
//! What never works in the sandbox: file contents from git (`git diff`,
//! `git show HEAD:path`), and anything that reads the trees of commits
//! before HEAD (`git log -- path`).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// What the sandbox gets as `.git` (the policy's `[sync] git`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Mode {
    /// Nothing.
    Off,
    /// [`Mode::History`] where [`asks_for_history`] says so, else
    /// [`Mode::Head`].
    #[default]
    Detect,
    /// HEAD and the tags on it.
    Head,
    /// Also every commit before HEAD and the tags on them.
    History,
}

/// HEAD as found in the git directory.
#[derive(Clone)]
pub struct Head {
    /// The text of the `HEAD` file.
    pub text: String,
    /// The branch (`refs/heads/main`) when HEAD is symbolic.
    pub branch: Option<String>,
    /// The commit id in hex.
    pub commit: String,
    /// The repository's common git directory.
    pub common: PathBuf,
    /// Changes when a tag is made, moved or removed (see `tags_seal`).
    pub tags: u64,
}

fn is_hex40(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Reads HEAD straight from the git directory (no process is started).
pub fn head(root: &Path) -> Option<Head> {
    let dot_git = root.join(".git");
    let git_dir = if dot_git.is_file() {
        let text = std::fs::read_to_string(&dot_git).ok()?;
        let p = PathBuf::from(text.strip_prefix("gitdir:")?.trim());
        if p.is_absolute() { p } else { root.join(p) }
    } else {
        dot_git
    };
    let text = std::fs::read_to_string(git_dir.join("HEAD")).ok()?.trim().to_string();
    let common = match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(c) => {
            let c = PathBuf::from(c.trim());
            if c.is_absolute() { c } else { git_dir.join(c) }
        }
        Err(_) => git_dir.clone(),
    };
    let tags = tags_seal(&common);
    let Some(name) = text.strip_prefix("ref: ").map(|s| s.trim().to_string()) else {
        return is_hex40(&text).then(|| Head { commit: text.clone(), text, branch: None, common, tags });
    };
    let mut commit = None;
    for dir in [&git_dir, &common] {
        if let Ok(id) = std::fs::read_to_string(dir.join(&name)) {
            commit = Some(id.trim().to_string());
            break;
        }
    }
    if commit.is_none() {
        let packed = std::fs::read_to_string(common.join("packed-refs")).ok()?;
        commit = packed
            .lines()
            .find_map(|l| l.strip_suffix(name.as_str()).filter(|id| id.ends_with(' ')).map(|id| id.trim().to_string()));
    }
    let commit = commit.filter(|c| is_hex40(c))?;
    Some(Head { text, branch: Some(name), commit, common, tags })
}

/// A digest of where the tags are kept (`packed-refs` and the files under
/// `refs/tags`, by name, size and time), read without starting git.
fn tags_seal(common: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let mut h = blake3::Hasher::new();
    let mut add = |name: &[u8], m: &std::fs::Metadata| {
        h.update(name);
        h.update(&m.len().to_le_bytes());
        h.update(&m.mtime().to_le_bytes());
        h.update(&m.mtime_nsec().to_le_bytes());
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

/// The tags the stand-in carries: those on HEAD, or with history those
/// on any commit before it too. Returns the text of a `packed-refs` file
/// and the annotated tag objects it names.
fn tags_for(root: &Path, head: &Head, history: bool) -> Option<(String, Vec<String>)> {
    let mut c = git(root);
    c.args(["for-each-ref", "--format=%(objectname) %(objecttype) %(*objectname) %(refname)"]);
    if history {
        c.arg(format!("--merged={}", head.commit));
    } else {
        c.arg(format!("--points-at={}", head.commit));
    }
    c.arg("refs/tags");
    let listed = output(c)?;
    let mut lines: Vec<(String, String, Option<String>)> = Vec::new();
    for line in String::from_utf8_lossy(&listed).lines() {
        let parts: Vec<&str> = line.split(' ').collect();
        match parts.as_slice() {
            [oid, "tag", peeled, name] => lines.push((name.to_string(), oid.to_string(), Some(peeled.to_string()))),
            [oid, "commit", "", name] => lines.push((name.to_string(), oid.to_string(), None)),
            // A tag of a tree or a blob says nothing about commits.
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
    Some((text, objects))
}

fn build(root: &Path, head: &Head, history: bool, dir: &Path) -> Option<()> {
    std::fs::create_dir_all(dir.join("objects/pack")).ok()?;
    std::fs::create_dir_all(dir.join("refs/heads")).ok()?;
    std::fs::write(dir.join("HEAD"), format!("{}\n", head.text)).ok()?;
    if let Some(branch) = &head.branch {
        // A branch name is a relative path below the git directory.
        if branch.split('/').any(|c| c.is_empty() || c == "." || c == "..") || branch.starts_with('/') {
            return None;
        }
        let p = dir.join(branch);
        std::fs::create_dir_all(p.parent()?).ok()?;
        std::fs::write(p, format!("{}\n", head.commit)).ok()?;
    }
    if !history {
        // The history ends here, as in a clone of depth one.
        std::fs::write(dir.join("shallow"), format!("{}\n", head.commit)).ok()?;
    }
    let (refs, tag_objects) = tags_for(root, head, history)?;
    std::fs::write(dir.join("packed-refs"), refs).ok()?;
    // git abbreviates object names to a length that grows with the
    // repository; the stand-in is tiny, so it is told the real one's.
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
    // The commit and its trees, without file contents...
    let mut ids = output({
        let mut c = git(root);
        c.args(["rev-list", "--objects", "--no-object-names", "--filter=blob:none", "--no-walk", &head.commit]);
        c
    })?;
    // ...plus the targets of tracked symlinks, which git compares by
    // reading the object...
    let tree = output({
        let mut c = git(root);
        c.args(["ls-tree", "-r", "-z", &head.commit]);
        c
    })?;
    for rec in tree.split(|&b| b == 0) {
        if rec.starts_with(b"120000 blob ") && rec.len() >= 52 {
            ids.extend_from_slice(&rec[12..52]);
            ids.push(b'\n');
        }
    }
    // ...and the annotated tags.
    for t in tag_objects {
        ids.extend_from_slice(t.as_bytes());
        ids.push(b'\n');
    }
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
    // Where the stand-in is shown beside a `.git` file (a linked worktree),
    // it is not part of what git sees in the worktree.
    std::fs::create_dir_all(dir.join("info")).ok()?;
    std::fs::write(dir.join("info/exclude"), format!("/{}\n", rargo_proto::GIT_SIDE)).ok()?;
    // An index that says "exactly this commit is checked out".
    let read = git(root).env("GIT_INDEX_FILE", dir.join("index")).args(["read-tree", &head.commit]).status().ok()?;
    read.success().then_some(())
}

/// The thin git directory for the worktree's HEAD, made on first use and
/// kept in the client's cache. With `history` it has no `shallow` file
/// and the commits before HEAD are in [`history`]'s packs. `None`
/// when the worktree is not a git checkout, has no commit yet, or git is
/// not installed.
pub fn thin(root: &Path, head: &Head, cache: &Path, history: bool) -> Option<PathBuf> {
    let key = blake3::hash(format!("v4\n{}\n{}\n{}\n{history}", head.text, head.commit, head.tags).as_bytes()).to_hex()[..32].to_string();
    let all = cache.join("git");
    let dir = all.join(&key);
    if dir.join("HEAD").is_file() {
        return Some(dir);
    }
    // Half-made directories of clients that were killed.
    crate::tmp::sweep(&all, ".tmp-");
    let tmp = all.join(format!(".tmp-{}-{key}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let made = build(root, head, history, &tmp).and_then(|()| match std::fs::rename(&tmp, &dir) {
        Ok(()) => Some(()),
        // Another client got there first.
        Err(_) if dir.join("HEAD").is_file() => Some(()),
        Err(_) => None,
    });
    let _ = std::fs::remove_dir_all(&tmp);
    made?;
    // Commits nobody has built at for a while are not worth keeping.
    for e in std::fs::read_dir(&all).into_iter().flatten().flatten() {
        let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
        if e.path() != dir && old.is_some_and(|a| a.as_secs() > 14 * 86400) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
    Some(dir)
}

/// The sizes of the pieces history is packed in, largest first: a piece
/// of size `n` covers `n` commits of the first-parent chain, starting at a
/// multiple of `n` counted from the root, with whatever was merged into
/// them. A piece never changes once its last commit exists, so a new
/// commit adds a piece of one, and a larger piece only when the smaller
/// ones before it fill one up.
const PIECES: [usize; 4] = [4096, 256, 16, 1];

/// The commits before HEAD as packs, in a directory of their own per
/// HEAD (`pack/`), shown inside `.git/objects` beside the stand-in's pack.
/// The pieces are kept once per repository (all its worktrees) and linked
/// into each HEAD's directory; what nobody used for 14 days goes.
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
    let mut chain: Vec<&str> = std::str::from_utf8(&listed).ok()?.lines().collect();
    chain.reverse();
    let tmp = store.join("heads").join(format!(".tmp-{}-{}", std::process::id(), head.commit));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("pack")).ok()?;
    let mut start = 0;
    for size in PIECES {
        while start + size <= chain.len() {
            let (before, last) = (start.checked_sub(1).map(|i| chain[i]), chain[start + size - 1]);
            let piece = blake3::hash(format!("{}\n{last}", before.unwrap_or("")).as_bytes()).to_hex()[..32].to_string();
            let (pack, idx) = piece_files(root, &pieces_dir, &piece, before, last)?;
            for (src, ext) in [(&pack, "pack"), (&idx, "idx")] {
                std::fs::hard_link(src, tmp.join("pack").join(format!("pack-{piece}.{ext}"))).ok()?;
            }
            start += size;
        }
    }
    match std::fs::rename(&tmp, &at) {
        Ok(()) => {}
        // Another client got there first.
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

/// The pack of one piece: the commits `last` reaches that `before` does
/// not. Made once; its files are named by the piece.
fn piece_files(root: &Path, dir: &Path, piece: &str, before: Option<&str>, last: &str) -> Option<(PathBuf, PathBuf)> {
    let (pack, idx) = (dir.join(format!("{piece}.pack")), dir.join(format!("{piece}.idx")));
    if pack.is_file() && idx.is_file() {
        // In use again: not to be swept.
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
        // Written from another thread: the list can fill the pipe both ways.
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

/// Removes the HEAD directories nobody used for 14 days, then the pieces
/// none of the remaining ones links to.
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

/// The crates that ask git for history from a build script or a macro.
const HISTORY_CRATES: &[&str] = &["vergen", "vergen-gitcl", "vergen-git2", "vergen-gix", "shadow-rs", "git-version", "built", "git-testament"];

/// Whether a build of this worktree evidently asks git for history: a
/// `build.rs` in it mentions `describe` or `rev-list`, or a `Cargo.lock`
/// in it names one of [`HISTORY_CRATES`]. `files` are the worktree's
/// paths, relative to `root`.
pub fn asks_for_history<'a>(root: &Path, files: impl Iterator<Item = &'a [u8]>) -> bool {
    use std::os::unix::ffi::OsStrExt;
    for rel in files {
        let name = rel.rsplit(|&b| b == b'/').next().unwrap_or(rel);
        if name != b"build.rs" && name != b"Cargo.lock" {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(root.join(std::ffi::OsStr::from_bytes(rel))) else { continue };
        let asks = if name == b"build.rs" {
            text.contains("describe") || text.contains("rev-list")
        } else {
            text.lines().any(|l| l.strip_prefix("name = \"").and_then(|n| n.strip_suffix('"')).is_some_and(|n| HISTORY_CRATES.contains(&n)))
        };
        if asks {
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

        // What the server's sandbox shows: the files and the thin `.git`.
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
        // A repository that abbreviates to more than the default.
        run(&repo, &["config", "core.abbrev", "11"]);
        let h11 = head(&repo).unwrap();
        let other_cache = tmp.path().join("cache11");
        let thin11 = thin(&repo, &h11, &other_cache, false).unwrap();
        assert!(std::fs::read_to_string(thin11.join("config")).unwrap().contains("abbrev = 11"));
        run(&repo, &["config", "--unset", "core.abbrev"]);
        std::fs::write(copy.join("src/lib.rs"), "pub fn c() {}\n").unwrap();
        assert_eq!(run(&copy, &["status", "--porcelain"]), "M src/lib.rs");
        assert!(run(&copy, &["describe", "--always", "--dirty"]).ends_with("-dirty"));
        // No history and no file contents came along.
        assert_eq!(run(&copy, &["rev-list", "--count", "HEAD"]), "1");
        let size: u64 = walk(&thin_dir);
        assert!(size < 20_000, "{size} bytes");

        // A linked worktree and a detached HEAD resolve too.
        let wt = tmp.path().join("wt");
        run(&repo, &["worktree", "add", "-q", "--detach", wt.to_str().unwrap(), "HEAD~1"]);
        let h2 = head(&wt).unwrap();
        assert_eq!(h2.commit, run(&repo, &["rev-parse", "HEAD~1"]));
        assert!(h2.branch.is_none());
        assert_ne!(thin(&wt, &h2, &cache, false).unwrap(), thin_dir);
        assert!(head(tmp.path()).is_none());
    }

    /// What the server's sandbox shows at `copy`: `stand_in` as `.git`, with
    /// the history's loose objects in its object directory.
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
        // The side branch writes a file of its own: the merge is clean.
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
        // A branch merged back, with a lightweight tag on it.
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
        // A tag on a commit HEAD does not reach.
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

        // A new commit adds a piece of one commit; the others stay.
        let pieces = objects.parent().unwrap().parent().unwrap().join("pieces");
        let before = packs(&pieces);
        commit("seven");
        let h7 = head(&repo).unwrap();
        let objects7 = history(&repo, &h7, &cache).unwrap();
        assert_eq!(objects7.parent(), objects.parent(), "one store per repository");
        assert_eq!(packs(&pieces), before + 1);
        assert_eq!(packs(&objects7.join("pack")), before + 1, "the first-parent chain is 8 commits: 8 pieces of one");
        // A tag made without a new commit makes another stand-in.
        let seen = thin(&repo, &h7, &cache, true).unwrap();
        run(&repo, &["tag", "-a", "v0.3.0", "-m", "third release"]);
        let tagged = head(&repo).unwrap();
        assert_ne!(tagged.tags, h7.tags, "the tags' seal changed");
        let stand_in = thin(&repo, &tagged, &cache, true).unwrap();
        assert_ne!(stand_in, seen);
        std::fs::write(copy.join("f"), "seven").unwrap();
        show(&stand_in, Some(&objects7), &copy);
        assert_eq!(run(&copy, &["describe"]), "v0.3.0");

        // Without history, the tags on HEAD come along all the same.
        let bare = thin(&repo, &tagged, &cache, false).unwrap();
        show(&bare, None, &copy);
        assert_eq!(run(&copy, &["describe", "--tags"]), "v0.3.0");
        assert_eq!(run(&copy, &["rev-list", "--count", "HEAD"]), "1");
    }

    fn packs(dir: &Path) -> usize {
        std::fs::read_dir(dir).unwrap().flatten().filter(|e| e.path().extension().is_some_and(|x| x == "pack")).count()
    }

    #[test]
    fn history_is_wanted_where_a_build_script_or_a_crate_asks_for_it() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        let put = |rel: &str, text: &str| {
            std::fs::create_dir_all(r.join(rel).parent().unwrap()).unwrap();
            std::fs::write(r.join(rel), text).unwrap();
        };
        put("crates/a/build.rs", "fn main() { git(&[\"rev-parse\", \"HEAD\"]); }");
        put("Cargo.lock", "[[package]]\nname = \"serde\"\nversion = \"1.0.0\"\n");
        let files = |list: &[&'static str]| list.iter().map(|f| f.as_bytes()).collect::<Vec<_>>();
        let all = files(&["crates/a/build.rs", "Cargo.lock", "crates/a/src/lib.rs"]);
        assert!(!asks_for_history(r, all.iter().copied()), "HEAD alone");
        put("crates/b/build.rs", "fn main() { git(&[\"log\", \"-1\", \"--format=%(describe:tags)\"]); }");
        let with_b = files(&["crates/a/build.rs", "crates/b/build.rs", "Cargo.lock"]);
        assert!(asks_for_history(r, with_b.iter().copied()), "a build script that describes");
        put("Cargo.lock", "[[package]]\nname = \"vergen-gitcl\"\nversion = \"1.0.0\"\n");
        assert!(asks_for_history(r, all.iter().copied()), "a crate that asks git");
    }

    fn walk(dir: &Path) -> u64 {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| if e.path().is_dir() { walk(&e.path()) } else { e.metadata().unwrap().len() })
            .sum()
    }
}
