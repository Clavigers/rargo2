//! A test's write to the worktree never silently replaces a local file that changed while the command ran.

use crate::place::within;
use crate::scan::{hash_file, Index};
use rargo_proto::Write;
use std::collections::BTreeSet;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// The name a test's version of `path` gets when the local file is kept.
pub fn aside(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".rargo-test");
    path.with_file_name(name)
}

/// What the worktree holds at `rel` now differs from what the snapshot sent: `sent` is the snapshot's
/// index, and a path it does not hold was not sent, so it changed if it was made or changed since `since_ns`.
fn changed(abs: &Path, rel: &[u8], sent: &Index, since_ns: i64) -> bool {
    let was = sent.files.get(rel);
    let Ok(md) = rargo_net::io::file::symlink_metadata(abs) else { return was.is_some() };
    match was {
        Some(entry) => !md.is_file() || md.len() != entry.size || hash_file(abs, md.len()).ok() != Some(entry.hash),
        None => !md.is_dir() && md.mtime_ns().max(md.ctime() * 1_000_000_000 + md.ctime_nsec()) >= since_ns,
    }
}

fn path_of(w: &mut Write) -> &mut String {
    match w {
        Write::Dir { path } | Write::Remove { path } | Write::Symlink { path, .. } | Write::File { path, .. } | Write::Text { path, .. } => path,
    }
}

/// Holds back the writes into the worktree `root` (outside `targets`) whose local file changed since the
/// snapshot was taken: the local file stays, and a test's new content goes beside it, at `aside`. `written`
/// is every path this command has written so far, which a later write may replace. Returns the writes to
/// place and one warning per file kept.
pub fn keep_edits(
    root: &Path,
    targets: &[String],
    sent: &Index,
    since_ns: i64,
    written: &mut BTreeSet<PathBuf>,
    writes: Vec<Write>,
) -> (Vec<Write>, Vec<String>) {
    let mut out = Vec::with_capacity(writes.len());
    let mut notes = Vec::new();
    for mut w in writes {
        let abs = PathBuf::from(path_of(&mut w).as_str());
        let ours = !within(root, &abs)
            || matches!(w, Write::Dir { .. })
            || written.contains(&abs)
            || within(&root.join("target"), &abs)
            || within(&root.join(".git"), &abs)
            || targets.iter().any(|t| Path::new(t) == abs || within(Path::new(t), &abs));
        let rel = abs.strip_prefix(root).map(|r| r.as_os_str().as_bytes().to_vec()).unwrap_or_default();
        if ours || !changed(&abs, &rel, sent, since_ns) {
            written.insert(abs);
            out.push(w);
            continue;
        }
        let shown = abs.strip_prefix(root).unwrap_or(&abs).display().to_string();
        if let Write::File { hash, .. } = &w
            && sent.files.get(rel.as_slice()).is_some_and(|e| e.hash == *hash)
        {
            continue;
        }
        match w {
            Write::Remove { .. } | Write::Text { text: None, .. } => {
                notes.push(format!("warning: `{shown}` changed while rargo ran, so rargo kept it, though a test removed it\n"));
            }
            mut w => {
                let to = aside(&abs);
                notes.push(format!(
                    "warning: `{shown}` changed while rargo ran, so rargo kept it; what a test wrote there is in `{}`\n",
                    to.strip_prefix(root).unwrap_or(&to).display()
                ));
                *path_of(&mut w) = to.to_string_lossy().into_owned();
                written.insert(to);
                out.push(w);
            }
        }
    }
    (out, notes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::Entry;

    fn entry(bytes: &[u8]) -> Entry {
        Entry { size: bytes.len() as u64, mtime_ns: 0, ctime_ns: 0, ino: 0, hash: *blake3::hash(bytes).as_bytes(), hashed_at_ns: 0 }
    }

    fn file(path: &Path, bytes: &[u8]) -> Write {
        Write::File { path: path.to_string_lossy().into_owned(), hash: *blake3::hash(bytes).as_bytes(), size: bytes.len() as u64, exec: false }
    }

    fn paths(writes: &[Write]) -> Vec<String> {
        writes.iter().map(|w| { let mut w = w.clone(); path_of(&mut w).clone() }).collect()
    }

    #[test]
    fn a_file_edited_while_the_test_ran_is_kept_and_the_tests_version_goes_beside_it() {
        rargo_net::io::sim::test(|| {
            let root = rargo_net::io::sim::tempdir();
            let s = |n: &str| root.join(n).to_string_lossy().into_owned();
            let w = |n: &str| rargo_net::io::file::write(root.join(n), n.as_bytes()).unwrap();
            for n in ["notes.txt", "golden.txt", "same.txt", "ignored.log"] {
                rargo_net::io::file::write(root.join(n), b"original\n").unwrap();
            }
            let mut sent = Index::default();
            for n in ["notes.txt", "golden.txt", "same.txt", "gone.txt"] {
                sent.files.insert(n.as_bytes().to_vec(), entry(b"original\n"));
            }
            rargo_net::io::sleep(std::time::Duration::from_millis(10));
            let since = rargo_net::io::system_now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as i64;
            rargo_net::io::sleep(std::time::Duration::from_millis(10));
            w("notes.txt");
            w("same.txt");
            w("new.txt");
            let mut written = BTreeSet::new();
            let writes = vec![
                file(&root.join("notes.txt"), b"original\nappended by test\n"),
                file(&root.join("golden.txt"), b"regenerated\n"),
                file(&root.join("same.txt"), b"original\n"),
                file(&root.join("ignored.log"), b"log\n"),
                file(&root.join("new.txt"), b"made by test\n"),
                file(&root.join("gone.txt"), b"made by test\n"),
                Write::Remove { path: s("notes.txt") },
                Write::Dir { path: s("gen") },
                file(&root.join("target/debug/out"), b"x"),
            ];
            let (kept, notes) = keep_edits(&root, &[], &sent, since, &mut written, writes);
            assert_eq!(
                paths(&kept),
                [
                    s("notes.txt.rargo-test"),
                    s("golden.txt"),
                    s("ignored.log"),
                    s("new.txt.rargo-test"),
                    s("gone.txt.rargo-test"),
                    s("gen"),
                    s("target/debug/out"),
                ],
                "an unchanged file and one the snapshot did not hold that predates the run are written in place; a test that \
                 rewrote what it was sent writes nothing"
            );
            expect_test::expect![[r#"
                warning: `notes.txt` changed while rargo ran, so rargo kept it; what a test wrote there is in `notes.txt.rargo-test`
                warning: `new.txt` changed while rargo ran, so rargo kept it; what a test wrote there is in `new.txt.rargo-test`
                warning: `gone.txt` changed while rargo ran, so rargo kept it; what a test wrote there is in `gone.txt.rargo-test`
                warning: `notes.txt` changed while rargo ran, so rargo kept it, though a test removed it
            "#]]
            .assert_eq(&notes.concat());
            let (again, notes) = keep_edits(&root, &[], &sent, since, &mut written, vec![file(&root.join("golden.txt"), b"twice\n")]);
            assert_eq!((paths(&again), notes.len()), (vec![s("golden.txt")], 0), "a file this command wrote may be written again");
        });
    }
}
