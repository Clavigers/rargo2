use super::*;
use hegel::{Database, Hegel, Settings, TestCase, generators as gs};
use std::fs;
use std::os::unix::fs::symlink;

const CASES: u64 = 64;
const MAX_OPS: usize = 16;
const SLOTS: [&str; 4] = ["left/a", "right/a", "left/b", "right/b"];

#[derive(Debug)]
enum Op {
    Create(usize),
    Edit(usize),
    Rename(usize, usize),
    Delete(usize),
    Link(usize, usize),
}

fn apply(root: &Path, op: &Op, step: usize) -> HashSet<PathBuf> {
    let mut changed = HashSet::new();
    let mut touch = |slot: usize| {
        let path = root.join(SLOTS[slot]);
        changed.insert(path.parent().unwrap().to_path_buf());
        path
    };
    match *op {
        Op::Create(slot) => {
            let path = touch(slot);
            if fs::symlink_metadata(&path).is_err_and(|err| err.kind() == io::ErrorKind::NotFound) {
                fs::write(path, vec![b'c'; step + 2]).unwrap();
            }
        }
        Op::Edit(slot) => {
            let path = touch(slot);
            if fs::symlink_metadata(&path).is_ok_and(|md| md.is_file()) {
                fs::write(path, vec![b'e'; step + 2]).unwrap();
            }
        }
        Op::Rename(from, to) => {
            let source = touch(from);
            let dest = touch(to);
            if fs::symlink_metadata(&source).is_ok() {
                fs::rename(source, dest).unwrap();
            }
        }
        Op::Delete(slot) => remove(&touch(slot)),
        Op::Link(slot, target) => {
            let path = touch(slot);
            remove(&path);
            symlink(Path::new("..").join(SLOTS[target]), path).unwrap();
        }
    }
    changed
}

fn remove(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => panic!("remove {}: {err}", path.display()),
    }
}

fn settled(scan: &mut Scan) {
    // Model a settled index entry without waiting for the wall clock.
    for entry in scan.index.files.values_mut() {
        entry.hashed_at_ns = entry.mtime_ns.max(entry.ctime_ns) + RACY_NS + 1;
    }
}

fn check(tc: TestCase) {
    let encoded = tc.draw(
        gs::vecs(gs::integers::<u8>().max_value(79))
            .min_size(1)
            .max_size(MAX_OPS),
    );
    let ops: Vec<_> = encoded
        .into_iter()
        .map(|code| {
            let (kind, slot, other) = (code % 5, usize::from(code / 5 % 4), usize::from(code / 20));
            match kind {
                0 => Op::Create(slot),
                1 => Op::Edit(slot),
                2 => Op::Rename(slot, other),
                3 => Op::Delete(slot),
                _ => Op::Link(slot, other),
            }
        })
        .collect();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    for dir in ["left", "right"] {
        fs::create_dir(root.join(dir)).unwrap();
        fs::write(root.join(dir).join("anchor"), b"unchanged").unwrap();
    }
    fs::write(root.join(SLOTS[0]), b"x").unwrap();
    let fresh = || scan(root, &Index::default(), &[], None).unwrap();
    let mut prev = fresh();
    settled(&mut prev);
    let dirty = [root.join("left"), root.join("right")]
        .into_iter()
        .collect();
    prev = rescan(root, prev, &dirty, &[], None).unwrap();
    assert_eq!(
        prev.hashed_files, 0,
        "settled files in visited directories are reused"
    );
    for (step, op) in ops.iter().enumerate() {
        let changed = apply(root, op, step);
        prev = rescan(root, prev, &changed, &[], None).unwrap();
        let full = fresh();
        assert_eq!(
            prev.root,
            full.root,
            "step {step}, operations {:?}",
            &ops[..=step]
        );
        assert!(
            prev.hashed_files < full.hashed_files,
            "always reuse the anchors"
        );
        settled(&mut prev);
    }
}

#[test]
fn hegel_scan_oracle() {
    let settings = Settings::new()
        .test_cases(CASES)
        .seed(Some(118))
        .database(Database::Disabled);
    let test = Hegel::new(check).settings(settings);
    if let Ok(blob) = std::env::var("RARGO_HEGEL_REPLAY") {
        test.reproduce_failure(blob).run();
    } else {
        test.run();
    }
}

#[test]
fn hegel_scan_control() {
    std::hint::black_box(());
}
