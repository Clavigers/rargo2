//! A content's outline: the BLAKE3 chaining values of its groups, so that
//! any part of it can be checked against its hash before the rest arrives.
//!
//! A content hash is plain BLAKE3, which is a Merkle tree over 1 KiB
//! chunks. Cut a content into groups of [`GROUP`] bytes at multiples of
//! [`GROUP`] (the last one shorter): each group is a subtree of that tree,
//! and the tree above the groups has BLAKE3's own shape (the left subtree
//! of every node holds the largest power of two of groups that is less
//! than all of them). So the chaining values of the groups give the root
//! hash by [`root`], and one comparison with the content's hash proves all
//! of them; after that, the bytes of a group are proven by its chaining
//! value alone ([`group_cv`]).
//!
//! An outline is 32 bytes per 64 KiB (0.05% of the content), and only a
//! content of more than one group has one.

use blake3::hazmat::{merge_subtrees_non_root, merge_subtrees_root, ChainingValue, HasherExt, Mode};

/// Bytes per group: 64 BLAKE3 chunks.
pub const GROUP: u64 = 64 << 10;
const _: () = assert!(GROUP.is_power_of_two() && GROUP >= blake3::CHUNK_LEN as u64);

pub type Cv = ChainingValue;

/// How many groups a content of `size` bytes has.
pub fn groups(size: u64) -> usize {
    size.div_ceil(GROUP) as usize
}

/// The bytes of group `i` of a content of `size` bytes.
pub fn group_range(i: usize, size: u64) -> (u64, u64) {
    let start = i as u64 * GROUP;
    assert!(start < size, "group {i} of a {size}-byte content");
    (start, (start + GROUP).min(size))
}

/// The chaining value of group `i`, whose bytes are `data`. Only for a
/// content of more than one group: the only group of a smaller one is the
/// root, whose hash is [`blake3::hash`].
pub fn group_cv(i: usize, data: &[u8]) -> Cv {
    assert!(!data.is_empty() && data.len() as u64 <= GROUP);
    let mut h = blake3::Hasher::new();
    h.set_input_offset(i as u64 * GROUP);
    h.update(data);
    h.finalize_non_root()
}

/// The outline of `data`, group by group. Empty for a content of one
/// group or less.
pub fn outline(data: &[u8]) -> Vec<Cv> {
    if data.len() as u64 <= GROUP {
        return Vec::new();
    }
    data.chunks(GROUP as usize).enumerate().map(|(i, g)| group_cv(i, g)).collect()
}

/// The content hash that an outline of at least two groups gives.
pub fn root(cvs: &[Cv]) -> [u8; 32] {
    assert!(cvs.len() >= 2, "an outline has at least two groups");
    let (l, r) = cvs.split_at(left_len(cvs.len()));
    *merge_subtrees_root(&subtree(l), &subtree(r), Mode::Hash).as_bytes()
}

/// Whether `cvs` is the outline of a content of `size` bytes with `hash`.
pub fn proves(cvs: &[Cv], size: u64, hash: &[u8; 32]) -> bool {
    size > GROUP && cvs.len() == groups(size) && root(cvs) == *hash
}

fn subtree(cvs: &[Cv]) -> Cv {
    if cvs.len() == 1 {
        return cvs[0];
    }
    let (l, r) = cvs.split_at(left_len(cvs.len()));
    merge_subtrees_non_root(&subtree(l), &subtree(r), Mode::Hash)
}

/// The number of groups under the left child of a node over `n` groups:
/// the largest power of two less than `n`.
fn left_len(n: usize) -> usize {
    assert!(n >= 2);
    1 << (usize::BITS - 1 - (n - 1).leading_zeros())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(n: usize, seed: u64) -> Vec<u8> {
        let mut x = seed | 1;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    #[test]
    fn the_outline_gives_the_content_hash() {
        let g = GROUP as usize;
        for n in [g + 1, 2 * g, 2 * g + 1, 3 * g - 5, 4 * g, 5 * g + 1024, 7 * g + 1, 8 * g, 9 * g + 17, 33 * g - 1, 100 * g + 3] {
            let data = noise(n, n as u64);
            let cvs = outline(&data);
            assert_eq!(cvs.len(), groups(n as u64));
            assert_eq!(root(&cvs), *blake3::hash(&data).as_bytes(), "size {n}");
            assert!(proves(&cvs, n as u64, blake3::hash(&data).as_bytes()));
        }
    }

    #[test]
    fn a_changed_byte_or_value_is_caught() {
        let data = noise(5 * GROUP as usize + 99, 7);
        let hash = *blake3::hash(&data).as_bytes();
        let mut cvs = outline(&data);
        // A wrong group is caught by its value.
        let mut bad = data[GROUP as usize..2 * GROUP as usize].to_vec();
        bad[100] ^= 1;
        assert_ne!(group_cv(1, &bad), cvs[1]);
        // The right bytes at the wrong place too.
        assert_ne!(group_cv(2, &data[GROUP as usize..2 * GROUP as usize]), cvs[1]);
        // A wrong value is caught by the root.
        cvs[3][0] ^= 1;
        assert!(!proves(&cvs, data.len() as u64, &hash));
        cvs[3][0] ^= 1;
        assert!(proves(&cvs, data.len() as u64, &hash));
        // So is a wrong size or a missing value.
        assert!(!proves(&cvs, data.len() as u64 + GROUP, &hash));
        assert!(!proves(&cvs[..5], data.len() as u64, &hash));
    }

    #[test]
    fn left_subtrees_hold_the_largest_power_of_two_below() {
        assert_eq!(left_len(2), 1);
        assert_eq!(left_len(3), 2);
        assert_eq!(left_len(4), 2);
        assert_eq!(left_len(5), 4);
        assert_eq!(left_len(8), 4);
        assert_eq!(left_len(9), 8);
    }
}
