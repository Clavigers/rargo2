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

    pub fn clean_build(&self, tree: &[Option<Vec<u8>>]) -> Vec<Vec<u8>> {
        let mut outputs: Vec<Vec<u8>> = Vec::with_capacity(self.actions.len());
        for action in &self.actions {
            let sources = action.sources.iter().filter_map(|&s| tree[s].as_deref());
            let deps = action.deps.iter().map(|&d| outputs[d].as_slice());
            let inputs: Vec<_> = sources.chain(deps).collect();
            outputs.push(compile(action.id, &inputs));
        }
        outputs
    }

    pub fn ready(&self, done: &dyn Fn(usize) -> bool, requested: &dyn Fn(usize) -> bool) -> Vec<usize> {
        (0..self.actions.len()).filter(|&i| !requested(i) && self.actions[i].deps.iter().all(|&d| done(d))).collect()
    }
}

pub fn source(index: usize, version: u64) -> Vec<u8> {
    format!("source {index} at version {version}\n").into_bytes()
}

pub fn compile(action: u64, inputs: &[&[u8]]) -> Vec<u8> {
    let mut text = format!("output of {action}:").into_bytes();
    for input in inputs {
        text.extend_from_slice(Hash::of(input).hex().as_bytes());
        text.push(b' ');
    }
    text
}

pub fn key(action: u64, inputs: &[Hash]) -> Hash {
    let mut bytes = action.to_le_bytes().to_vec();
    for h in inputs {
        bytes.extend_from_slice(&h.0);
    }
    Hash::of(&bytes)
}
