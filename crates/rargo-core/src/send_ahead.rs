use crate::chunk::Recipe;
use crate::hash::Hash;
use crate::view::{Obs, VPath, vpath_starts_with};

pub const BATCH_BYTES: usize = 1 << 20;
pub const AHEAD_MAX: u64 = 256 << 20;
pub const UNKNOWN_SIZE: u64 = 256 << 10;

pub fn profile_obs(profile: &[(VPath, Obs)], exclude: &[Vec<u8>]) -> Vec<(VPath, Obs)> {
    profile.iter().filter(|(p, _)| !exclude.iter().any(|x| vpath_starts_with(p, x))).cloned().collect()
}

pub fn incoming(files: Vec<Hash>, has: &dyn Fn(&Hash) -> bool, in_flight: &dyn Fn(&Hash) -> bool) -> Vec<Hash> {
    let mut v: Vec<Hash> = files.into_iter().filter(|h| !has(h) || in_flight(h)).collect();
    v.sort();
    v.dedup();
    v
}

pub fn to_push(incoming: &[Hash], has: &dyn Fn(&Hash) -> bool) -> Vec<Hash> {
    incoming.iter().filter(|h| !has(h)).copied().collect()
}

pub fn trace_bytes(
    obs: &[(VPath, Obs)],
    own: bool,
    exclude: &[Vec<u8>],
    has: &dyn Fn(&Hash) -> bool,
    size_of: &dyn Fn(&Hash) -> Option<u64>,
) -> u64 {
    obs.iter()
        .filter(|(p, _)| own || !exclude.iter().any(|x| vpath_starts_with(p, x)))
        .filter_map(|(_, o)| match o {
            Obs::File { hash, .. } if !has(hash) => Some(size_of(hash).unwrap_or(UNKNOWN_SIZE)),
            _ => None,
        })
        .sum()
}

pub fn state_to_send(
    blobs: &[(Hash, u64)],
    has: &dyn Fn(&Hash) -> bool,
    here: &dyn Fn(&Hash) -> bool,
    budget: u64,
) -> Option<Vec<(Hash, u64)>> {
    let missing: Vec<(Hash, u64)> = blobs.iter().filter(|(h, _)| !has(h) && here(h)).copied().collect();
    let bytes: u64 = missing.iter().map(|(_, s)| *s).sum();
    (!missing.is_empty() && bytes <= budget).then_some(missing)
}

pub fn missing_chunks(recipe: &Recipe, has: &dyn Fn(&Hash) -> bool) -> Vec<u32> {
    (0..recipe.len() as u32).filter(|i| !has(&recipe[*i as usize].hash)).collect()
}

pub fn batches<T>(items: Vec<T>, len: impl Fn(&T) -> usize, budget: usize) -> Vec<Vec<T>> {
    let mut out: Vec<Vec<T>> = Vec::new();
    let mut cur: Vec<T> = Vec::new();
    let mut bytes = 0usize;
    for it in items {
        let n = len(&it);
        if bytes + n > budget && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            bytes = 0;
        }
        bytes += n;
        cur.push(it);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

pub fn push_parts(recipe: &Recipe, has_chunk: &dyn Fn(&Hash) -> bool) -> Vec<Vec<u32>> {
    let parts = batches(missing_chunks(recipe, has_chunk), |i| recipe[*i as usize].len as usize, BATCH_BYTES);
    if parts.is_empty() { vec![Vec::new()] } else { parts }
}

pub fn inline_indices(recipe: &Recipe, has: &dyn Fn(&Hash) -> bool) -> Vec<u32> {
    let mut out = Vec::new();
    let mut bytes = 0usize;
    for (i, c) in recipe.iter().enumerate() {
        if has(&c.hash) {
            continue;
        }
        if bytes + c.len as usize > BATCH_BYTES && !out.is_empty() {
            break;
        }
        bytes += c.len as usize;
        out.push(i as u32);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::ChunkRef;
    use std::collections::BTreeSet;

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
    }

    fn h(i: u64) -> Hash {
        let mut b = [0u8; 32];
        b[..8].copy_from_slice(&i.to_le_bytes());
        Hash(b)
    }

    fn recipe(rng: &mut Rng) -> Recipe {
        (0..rng.below(40))
            .map(|_| ChunkRef {
                hash: h(rng.below(30)),
                len: match rng.below(4) {
                    0 => (BATCH_BYTES + 5) as u32,
                    1 => (BATCH_BYTES / 3) as u32,
                    _ => 1 + rng.below(64 << 10) as u32,
                },
            })
            .collect()
    }

    #[test]
    fn a_trace_s_files_the_worker_lacks_are_sent_once_in_a_stable_order() {
        let files = vec![h(3), h(1), h(2), h(3), h(4)];
        let has = |x: &Hash| *x == h(2) || *x == h(4);
        let flying = |x: &Hash| *x == h(4);
        let inc = incoming(files, &has, &flying);
        assert_eq!(inc, vec![h(1), h(3), h(4)], "sorted, once each, and a blob still on its way counts as coming");
        assert_eq!(to_push(&inc, &has), vec![h(1), h(3)], "what the worker has or is being sent is not sent again");
        let obs: Vec<(VPath, Obs)> = vec![
            (VPath::from(b"/w/src/lib.rs".to_vec()), Obs::File { hash: h(1), exec: false }),
            (VPath::from(b"/c/registry/x.rs".to_vec()), Obs::File { hash: h(2), exec: false }),
            (VPath::from(b"/w/src".to_vec()), Obs::Dir),
        ];
        let exclude = vec![b"/w".to_vec()];
        assert_eq!(profile_obs(&obs, &exclude).len(), 1, "another crate's profile says nothing about this worktree");
        let size = |x: &Hash| (*x == h(1)).then_some(10);
        assert_eq!(trace_bytes(&obs, true, &exclude, &|_| false, &size), 10 + UNKNOWN_SIZE);
        assert_eq!(trace_bytes(&obs, false, &exclude, &|_| false, &size), UNKNOWN_SIZE);
        assert_eq!(trace_bytes(&obs, true, &exclude, &|_| true, &size), 0, "what the worker has costs nothing");
    }

    #[test]
    fn a_state_is_sent_ahead_only_within_the_budget() {
        let blobs = vec![(h(1), 100), (h(2), 50), (h(3), 70)];
        assert_eq!(state_to_send(&blobs, &|x| *x == h(2), &|_| true, 200), Some(vec![(h(1), 100), (h(3), 70)]));
        assert_eq!(state_to_send(&blobs, &|x| *x == h(2), &|_| true, 169), None, "over the budget, nothing is sent");
        assert_eq!(
            state_to_send(&blobs, &|_| false, &|x| *x != h(1), 200),
            Some(vec![(h(2), 50), (h(3), 70)]),
            "only what is here"
        );
        assert_eq!(state_to_send(&blobs, &|_| true, &|_| true, 200), None, "nothing when the worker has it all");
    }

    #[test]
    fn generated_inputs_send_nothing_the_worker_has_keep_to_the_budget_and_keep_their_order() {
        for seed in 1..=300u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let held: BTreeSet<Hash> = (0..30).filter(|_| rng.below(3) == 0).map(h).collect();
            let has = |x: &Hash| held.contains(x);
            let r = recipe(&mut rng);

            let parts = push_parts(&r, &has);
            assert!(!parts.is_empty(), "seed {seed}: a push has a first part, which carries the recipe");
            let flat: Vec<u32> = parts.iter().flatten().copied().collect();
            assert_eq!(flat, missing_chunks(&r, &has), "seed {seed}: every missing chunk once, in recipe order");
            for i in &flat {
                assert!(!held.contains(&r[*i as usize].hash), "seed {seed}: a chunk the worker has is not sent");
            }
            for p in &parts {
                let bytes: usize = p.iter().map(|i| r[*i as usize].len as usize).sum();
                assert!(p.len() <= 1 || bytes <= BATCH_BYTES, "seed {seed}: a part keeps to the budget");
            }
            assert_eq!(parts, push_parts(&r, &has), "seed {seed}: the same inputs give the same parts");

            let inline = inline_indices(&r, &has);
            assert!(inline.windows(2).all(|w| w[0] < w[1]), "seed {seed}: inline chunks keep recipe order");
            assert!(inline.iter().all(|i| !held.contains(&r[*i as usize].hash)), "seed {seed}: nothing the worker has");
            let bytes: usize = inline.iter().map(|i| r[*i as usize].len as usize).sum();
            assert!(inline.len() <= 1 || bytes <= BATCH_BYTES, "seed {seed}: inline chunks keep to the budget");

            let files: Vec<Hash> = (0..rng.below(20)).map(|_| h(rng.below(30))).collect();
            let flying: BTreeSet<Hash> = held.iter().filter(|_| rng.below(4) == 0).copied().collect();
            let inc = incoming(files.clone(), &has, &|x| flying.contains(x));
            assert!(inc.windows(2).all(|w| w[0] < w[1]), "seed {seed}: incoming is sorted and has no repeats");
            let mut reversed = files.clone();
            reversed.reverse();
            assert_eq!(
                inc,
                incoming(reversed, &has, &|x| flying.contains(x)),
                "seed {seed}: order does not depend on the trace's"
            );
            let push = to_push(&inc, &has);
            assert!(push.iter().all(|x| !held.contains(x)), "seed {seed}: nothing the worker has is pushed");
            assert!(
                files.iter().filter(|x| !held.contains(x)).all(|x| push.contains(x)),
                "seed {seed}: what it lacks is pushed"
            );

            let state: Vec<(Hash, u64)> =
                (0..rng.below(12)).map(|_| (h(rng.below(30)), rng.below(100 << 20))).collect();
            let budget = rng.below(400 << 20);
            let here: BTreeSet<Hash> = (0..30).filter(|_| rng.below(5) != 0).map(h).collect();
            match state_to_send(&state, &has, &|x| here.contains(x), budget) {
                Some(sent) => {
                    assert!(sent.iter().map(|(_, s)| s).sum::<u64>() <= budget, "seed {seed}: the budget is kept");
                    assert!(sent.iter().all(|(x, _)| !held.contains(x) && here.contains(x)), "seed {seed}");
                    let kept: Vec<&(Hash, u64)> =
                        state.iter().filter(|(x, _)| !held.contains(x) && here.contains(x)).collect();
                    assert_eq!(sent.iter().collect::<Vec<_>>(), kept, "seed {seed}: in the state's own order");
                }
                None => {
                    let bytes: u64 =
                        state.iter().filter(|(x, _)| !held.contains(x) && here.contains(x)).map(|(_, s)| s).sum();
                    let any = state.iter().any(|(x, _)| !held.contains(x) && here.contains(x));
                    assert!(!any || bytes > budget, "seed {seed}: a state that fits is sent");
                }
            }
        }
    }
}
