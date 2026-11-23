//! A thin `.git` for the server's sandbox.
//!
//! Build scripts commonly ask git about the commit they are built from
//! (`git log -1`, `git rev-parse HEAD`, `git describe --always --dirty`, or
//! simply reading `.git/HEAD`). The real `.git` is never synced: it can be
//! gigabytes, and nothing in it but the answer to those questions is an
//! input to a build. Instead the client makes a directory that is to git a
//! clone of depth one without file contents: `HEAD`, the branch it points
//! to, the commit object, its trees, the targets of tracked symlinks, and an
//! index read from that commit. It depends only on the commit, so it is
//! made once per commit, shared by every worktree at that commit, and a few
//! hundred kilobytes for a large repository.
//!
//! What works in the sandbox: anything about HEAD itself, the branch name,
//! and whether the worktree is dirty (`git status`, which hashes the
//! worktree files against the index). What does not: history (`git
//! describe --tags` fails as in any shallow clone) and file contents from
//! git (`git diff`, `git show HEAD:path`).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// HEAD as found in the git directory.
pub struct Head {
    /// The text of the `HEAD` file.
    pub text: String,
    /// The branch (`refs/heads/main`) when HEAD is symbolic.
    pub branch: Option<String>,
    /// The commit id in hex.
    pub commit: String,
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
    let Some(name) = text.strip_prefix("ref: ").map(|s| s.trim().to_string()) else {
        return is_hex40(&text).then(|| Head { commit: text.clone(), text, branch: None });
    };
    let common = match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(c) => {
            let c = PathBuf::from(c.trim());
            if c.is_absolute() { c } else { git_dir.join(c) }
        }
        Err(_) => git_dir.clone(),
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
    Some(Head { text, branch: Some(name), commit })
}

fn git(root: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(root).arg("--no-optional-locks").stdin(Stdio::null()).stderr(Stdio::null());
    c
}

fn build(root: &Path, head: &Head, dir: &Path) -> Option<()> {
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
    // The history ends here, as in a clone of depth one.
    std::fs::write(dir.join("shallow"), format!("{}\n", head.commit)).ok()?;
    std::fs::write(dir.join("config"), "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n").ok()?;
    // The commit and its trees, without file contents...
    let listed = git(root)
        .args(["rev-list", "--objects", "--no-object-names", "--filter=blob:none", "--no-walk", &head.commit])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let mut ids = listed.stdout;
    // ...plus the targets of tracked symlinks, which git compares by
    // reading the object.
    let tree = git(root).args(["ls-tree", "-r", "-z", &head.commit]).output().ok().filter(|o| o.status.success())?;
    for rec in tree.stdout.split(|&b| b == 0) {
        if rec.starts_with(b"120000 blob ") && rec.len() >= 52 {
            ids.extend_from_slice(&rec[12..52]);
            ids.push(b'\n');
        }
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
    // An index that says "exactly this commit is checked out".
    let read = git(root).env("GIT_INDEX_FILE", dir.join("index")).args(["read-tree", &head.commit]).status().ok()?;
    read.success().then_some(())
}

/// The thin git directory for the worktree's HEAD, made on first use and
/// kept in the client's cache. `None` when the worktree is not a git
/// checkout, has no commit yet, or git is not installed.
pub fn thin(root: &Path, head: &Head, cache: &Path) -> Option<PathBuf> {
    let key = blake3::hash(format!("v1\n{}\n{}", head.text, head.commit).as_bytes()).to_hex()[..32].to_string();
    let all = cache.join("git");
    let dir = all.join(&key);
    if dir.join("HEAD").is_file() {
        return Some(dir);
    }
    let tmp = all.join(format!(".tmp-{}-{key}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let made = build(root, head, &tmp).and_then(|()| match std::fs::rename(&tmp, &dir) {
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
        let thin_dir = thin(&repo, &h, &cache).unwrap();
        assert_eq!(thin(&repo, &h, &cache).unwrap(), thin_dir, "made once per commit");

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
        assert_eq!(run(&copy, &["describe", "--always", "--dirty"]), commit[..7]);
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
        assert_ne!(thin(&wt, &h2, &cache).unwrap(), thin_dir);
        assert!(head(tmp.path()).is_none());
    }

    fn walk(dir: &Path) -> u64 {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| if e.path().is_dir() { walk(&e.path()) } else { e.metadata().unwrap().len() })
            .sum()
    }
}
