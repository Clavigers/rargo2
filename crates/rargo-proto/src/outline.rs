use blake3::hazmat::{merge_subtrees_non_root, merge_subtrees_root, ChainingValue, HasherExt, Mode};

pub const GROUP: u64 = 64 << 10;
const _: () = assert!(GROUP.is_power_of_two() && GROUP >= blake3::CHUNK_LEN as u64);

pub type Cv = ChainingValue;

pub fn groups(size: u64) -> usize {
    size.div_ceil(GROUP) as usize
}

pub fn group_range(i: usize, size: u64) -> (u64, u64) {
    let start = i as u64 * GROUP;
    assert!(start < size, "group {i} of a {size}-byte content");
    (start, (start + GROUP).min(size))
}

pub fn group_cv(i: usize, data: &[u8]) -> Cv {
    assert!(!data.is_empty() && data.len() as u64 <= GROUP);
    let mut h = blake3::Hasher::new();
    h.set_input_offset(i as u64 * GROUP);
    h.update(data);
    h.finalize_non_root()
}

pub fn outline(data: &[u8]) -> Vec<Cv> {
    if data.len() as u64 <= GROUP {
        return Vec::new();
    }
    data.chunks(GROUP as usize).enumerate().map(|(i, g)| group_cv(i, g)).collect()
}

pub fn root(cvs: &[Cv]) -> [u8; 32] {
    assert!(cvs.len() >= 2, "an outline has at least two groups");
    let (l, r) = cvs.split_at(left_len(cvs.len()));
    *merge_subtrees_root(&subtree(l), &subtree(r), Mode::Hash).as_bytes()
}

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
        let sizes = [
            g + 1, 2 * g, 2 * g + 1, 3 * g - 5, 4 * g, 4 * g + 1, 5 * g + 1024,
            7 * g + 1, 8 * g, 8 * g + 1, 9 * g + 17, 33 * g - 1, 100 * g + 3,
        ];
        for n in sizes {
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
        let mut bad = data[GROUP as usize..2 * GROUP as usize].to_vec();
        bad[100] ^= 1;
        assert_ne!(group_cv(1, &bad), cvs[1]);
        assert_ne!(group_cv(2, &data[GROUP as usize..2 * GROUP as usize]), cvs[1]);
        cvs[3][0] ^= 1;
        assert!(!proves(&cvs, data.len() as u64, &hash));
        cvs[3][0] ^= 1;
        assert!(proves(&cvs, data.len() as u64, &hash));
        assert!(!proves(&cvs, data.len() as u64 + GROUP, &hash));
        assert!(!proves(&cvs[..5], data.len() as u64, &hash));
    }
}
