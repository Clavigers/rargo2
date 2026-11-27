//! The table of virtual files in a client's `target/`, and how a delivery edits it. The output
//! filesystem serves this table; a simulation keeps one in memory.

use rargo_machine::StateMachine;
use rargo_proto::Digest;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VFile {
    pub hash: Digest,
    pub size: u64,
    pub exec: bool,
    pub mtime_ns: i64,
}

pub type Table = BTreeMap<Vec<u8>, VFile>;

pub enum Edit {
    Put { path: Vec<u8>, file: VFile, replaced_real: bool },
    Remove(Vec<u8>),
    Shadow(Vec<u8>),
}

pub struct OutputTree {
    pub files: Table,
    pub generation: u64,
}

impl OutputTree {
    pub fn new(files: Table) -> OutputTree {
        OutputTree { files, generation: 0 }
    }

    pub fn same(&self, rel: &[u8], file: &VFile) -> bool {
        self.files.get(rel).is_some_and(|v| v.hash == file.hash && v.exec == file.exec)
    }

    pub fn has_below(&self, rel: &[u8]) -> bool {
        let prefix = below(rel);
        self.files.range(prefix.clone()..).next().is_some_and(|(k, _)| k.starts_with(&prefix))
    }

    pub fn apply(&mut self, generation: u64, edits: Vec<Edit>) -> Vec<Vec<u8>> {
        if generation <= self.generation {
            return Vec::new();
        }
        self.generation = generation;
        let mut changed = Vec::new();
        for edit in edits {
            let path = match edit {
                Edit::Put { path, file, replaced_real } => {
                    if !replaced_real && self.same(&path, &file) {
                        continue;
                    }
                    self.remove_below(&path);
                    let mut at = path.as_slice();
                    while let Some(slash) = at.iter().rposition(|&b| b == b'/') {
                        at = &at[..slash];
                        self.files.remove(at);
                    }
                    self.files.insert(path.clone(), file);
                    assert!(!self.has_below(&path), "a file put in place has nothing below it");
                    path
                }
                Edit::Remove(path) => {
                    self.files.remove(&path);
                    self.remove_below(&path);
                    assert!(!self.files.contains_key(&path), "a removed path is gone");
                    path
                }
                Edit::Shadow(path) => {
                    self.files.remove(&path);
                    path
                }
            };
            changed.push(path);
        }
        changed
    }

    pub fn remove_below(&mut self, path: &[u8]) {
        let prefix = below(path);
        let removed: Vec<_> =
            self.files.range(prefix.clone()..).take_while(|(key, _)| key.starts_with(&prefix)).map(|(key, _)| key.clone()).collect();
        for key in removed {
            self.files.remove(&key);
        }
        assert!(!self.has_below(path), "nothing is left below {:?}", String::from_utf8_lossy(path));
    }
}

impl StateMachine for OutputTree {
    fn invariants(&self) {
        for path in self.files.keys() {
            let at = String::from_utf8_lossy(path);
            assert!(!path.is_empty(), "no file has an empty path");
            assert!(!path.split(|&b| b == b'/').any(|part| part.is_empty()), "{at:?} is relative and has no empty parts");
            assert!(!self.has_below(path), "file {at:?} has nothing below it");
        }
    }
}


pub fn below(prefix: &[u8]) -> Vec<u8> {
    let mut p = prefix.to_vec();
    p.push(b'/');
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(n: u8) -> VFile {
        VFile { hash: [n; 32], size: 1, exec: false, mtime_ns: 0 }
    }

    fn put(path: &str, n: u8) -> Edit {
        Edit::Put { path: path.as_bytes().to_vec(), file: file(n), replaced_real: false }
    }

    #[test]
    fn a_delivery_replaces_what_is_in_the_way_and_an_old_one_changes_nothing() {
        let mut tree = OutputTree::new(Table::new());
        let changed = tree.apply(1, vec![put("debug/a", 1), put("debug/b/c", 2), put("debug/a", 1)]);
        tree.invariants();
        assert_eq!(changed, vec![b"debug/a".to_vec(), b"debug/b/c".to_vec()], "a file already in place is no change");

        let changed = tree.apply(2, vec![put("debug/b", 3), Edit::Remove(b"debug/a".to_vec())]);
        tree.invariants();
        assert_eq!(changed.len(), 2);
        assert_eq!(tree.files.keys().collect::<Vec<_>>(), vec![b"debug/b"]);
        assert_eq!(tree.files[b"debug/b".as_slice()].hash, [3; 32]);

        assert!(tree.apply(2, vec![put("debug/late", 4)]).is_empty(), "a delivery of an older generation is dropped");
        tree.invariants();
        assert!(!tree.files.contains_key(b"debug/late".as_slice()));

        tree.apply(3, vec![put("debug/b/d", 5)]);
        tree.invariants();
        assert_eq!(tree.files.keys().collect::<Vec<_>>(), vec![b"debug/b/d"], "a file becomes a directory");
    }
}
