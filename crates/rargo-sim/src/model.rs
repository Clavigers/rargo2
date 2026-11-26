use crate::rng::Rng;
use rargo_core::hash::Hash;

#[derive(Clone, Debug)]
pub struct Action {
    pub id: u64,
    pub sources: Vec<usize>,
    pub deps: Vec<usize>,
    pub ms: u64,
    pub mem_kb: u64,
}

#[derive(Clone, Debug)]
pub struct Project {
    pub sources: usize,
    pub actions: Vec<Action>,
}

impl Project {
    pub fn generate(rng: &mut Rng, sources: usize, actions: usize) -> Project {
        let actions = (0..actions)
            .map(|i| {
                let mut srcs: Vec<usize> = (0..rng.range(0, 3)).map(|_| rng.below(sources as u64) as usize).collect();
                srcs.sort();
                srcs.dedup();
                let mut deps: Vec<usize> =
                    if i == 0 { Vec::new() } else { (0..rng.range(0, 3)).map(|_| rng.below(i as u64) as usize).collect() };
                deps.sort();
                deps.dedup();
                if srcs.is_empty() && deps.is_empty() {
                    srcs.push(rng.below(sources as u64) as usize);
                }
                Action { id: 1000 + i as u64, sources: srcs, deps, ms: rng.range(5, 200), mem_kb: rng.range(1, 6) * 1024 }
            })
            .collect();
        Project { sources, actions }
    }

    pub fn ready(&self, done: &dyn Fn(usize) -> bool, requested: &dyn Fn(usize) -> bool) -> Vec<usize> {
        (0..self.actions.len()).filter(|&i| !requested(i) && self.actions[i].deps.iter().all(|&d| done(d))).collect()
    }
}

pub fn source(index: usize, version: u64) -> Vec<u8> {
    format!("source {index} at version {version}\n").into_bytes()
}

pub(crate) const LONG_RUN_MS: u64 = 1_000;
pub(crate) const FAILED: i32 = 101;
pub(crate) const CANCELLED: i32 = 130;

pub fn compile(seed: u64, action: u64, inputs: &[&[u8]]) -> Result<Vec<u8>, Vec<u8>> {
    let hashes: Vec<_> = inputs.iter().map(|input| Hash::of(input)).collect();
    if profile(seed, key(action, &hashes)).is_multiple_of(23) {
        return Err(format!("error: model compiler failed for action {action}\n").into_bytes());
    }
    let mut text = format!("output of {action}:").into_bytes();
    for hash in hashes {
        text.extend_from_slice(hash.hex().as_bytes());
        text.push(b' ');
    }
    Ok(text)
}

pub(crate) fn duration(seed: u64, key: Hash, base_ms: u64) -> u64 {
    if profile(seed, key).is_multiple_of(5) { base_ms + LONG_RUN_MS } else { base_ms }
}

fn profile(seed: u64, key: Hash) -> u64 {
    let mut bytes = seed.to_le_bytes().to_vec();
    bytes.extend_from_slice(&key.0);
    u64::from_le_bytes(Hash::of(&bytes).0[..8].try_into().expect("eight bytes"))
}

pub fn key(action: u64, inputs: &[Hash]) -> Hash {
    let mut bytes = action.to_le_bytes().to_vec();
    for h in inputs {
        bytes.extend_from_slice(&h.0);
    }
    Hash::of(&bytes)
}
