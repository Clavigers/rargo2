//! The content-addressed description of a worktree.
//!
//! A directory is a [`Tree`]: its entries sorted by name, each a file (by
//! content hash), a symlink, or another directory (by the digest of its
//! tree). The digest of the root tree therefore names the complete content
//! of a worktree, two worktrees share every subtree they have in common, and
//! after an edit only the trees on the path to the edited file are new.

use crate::Digest;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Node {
    File { hash: Digest, size: u64, exec: bool },
    Symlink(Vec<u8>),
    Dir(Digest),
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tree {
    /// Sorted by name; names are single path components.
    pub entries: Vec<(Vec<u8>, Node)>,
}

impl Tree {
    pub fn encode(&self) -> Vec<u8> {
        crate::encode(self)
    }

    /// Decodes a tree and checks that it is well formed: names sorted,
    /// unique, and each a single component.
    pub fn decode(bytes: &[u8]) -> std::io::Result<Tree> {
        let t: Tree = crate::decode(bytes)?;
        let bad = |why: &str| std::io::Error::other(format!("malformed tree: {why}"));
        for (i, (name, _)) in t.entries.iter().enumerate() {
            if name.is_empty() || name.contains(&b'/') || name.contains(&0) || name == b"." || name == b".." {
                return Err(bad("an entry name is not a path component"));
            }
            if i > 0 && t.entries[i - 1].0 >= *name {
                return Err(bad("entries are not sorted"));
            }
        }
        Ok(t)
    }

    pub fn get(&self, name: &[u8]) -> Option<&Node> {
        self.entries.binary_search_by(|(n, _)| n.as_slice().cmp(name)).ok().map(|i| &self.entries[i].1)
    }
}

/// The digest of an encoded tree: the plain content hash, so that a tree is
/// stored and fetched like any other content.
pub fn digest(encoded: &[u8]) -> Digest {
    *blake3::hash(encoded).as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_rejects_unsorted_and_odd_names() {
        let f = Node::File { hash: [0; 32], size: 0, exec: false };
        let ok = Tree { entries: vec![(b"a".to_vec(), f.clone()), (b"b".to_vec(), f.clone())] };
        assert_eq!(Tree::decode(&ok.encode()).unwrap(), ok);
        assert!(ok.get(b"b").is_some() && ok.get(b"c").is_none());
        for names in [vec!["b", "a"], vec!["a", "a"], vec!["a/b"], vec![".."], vec![""]] {
            let t = Tree { entries: names.iter().map(|n| (n.as_bytes().to_vec(), f.clone())).collect() };
            assert!(Tree::decode(&t.encode()).is_err(), "{names:?}");
        }
    }
}
