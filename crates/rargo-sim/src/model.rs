use crate::rng::Rng;
use crate::workspace::{self, Workspace};
use rargo_core::build::pure;
use rargo_core::buildview::LEVEL_NONE;
use std::sync::Arc;
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
    /// Plans `ws` with the model planner, then asks rargo's pure plan code which units each
    /// unit sees: an action reads its own sources and the outputs of every unit it sees.
    pub fn plan(rng: &mut Rng, ws: &Workspace) -> Project {
        let planned = workspace::plan(ws);
        let units = &planned.plan.units;
        let roots = &planned.plan.roots;
        let facts = pure::plan_facts(units, roots);
        let seen: Vec<Vec<u8>> = (0..units.len()).map(|i| pure::levels(units, &facts, i)).collect();
        let need = pure::needs(
            units,
            roots,
            &|i| Arc::new(pure::meta_levels(units, &facts, i)),
            &|i| Arc::new(seen[i].clone()),
        );
        let kept: Vec<usize> = (0..units.len()).filter(|&i| need[i] != LEVEL_NONE).collect();
        let index = |i: usize| kept.binary_search(&i).expect("a needed unit sees only needed units");
        let actions: Vec<Action> = kept
            .iter()
            .enumerate()
            .map(|(at, &i)| {
                let deps: Vec<usize> =
                    seen[i].iter().enumerate().filter(|&(_, &lv)| lv != LEVEL_NONE).map(|(j, _)| index(j)).collect();
                assert!(deps.iter().all(|&d| d < at), "the plan lists a unit after what it sees");
                let sources = planned.sources[i].clone();
                assert!(!sources.is_empty() || !deps.is_empty(), "an action reads something");
                Action { id: 1000 + at as u64, sources, deps, ms: rng.range(5, 200), mem_kb: rng.range(1, 6) * 1024 }
            })
            .collect();
        assert!(!actions.is_empty());
        Project { sources: ws.sources, actions }
    }

    pub fn clean_build(&self, tree: &[Option<Vec<u8>>]) -> Vec<Vec<u8>> {
        let mut outputs: Vec<Vec<u8>> = Vec::with_capacity(self.actions.len());
        for action in &self.actions {
            let sources = action.sources.iter().filter_map(|&s| tree[s].as_deref());
            let deps = action.deps.iter().map(|&d| outputs[d].as_slice());
            let inputs: Vec<_> = sources.chain(deps).collect();
            outputs.push(render(action.id, &inputs.iter().map(|input| Hash::of(input)).collect::<Vec<_>>()));
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

pub(crate) const LONG_RUN_MS: u64 = 1_000;
pub(crate) const FAILED: i32 = 101;
pub(crate) const CANCELLED: i32 = 130;

pub fn compile(seed: u64, action: u64, inputs: &[&[u8]]) -> Result<Vec<u8>, Vec<u8>> {
    let hashes: Vec<_> = inputs.iter().map(|input| Hash::of(input)).collect();
    if profile(seed, key(action, &hashes)).is_multiple_of(23) {
        return Err(format!("error: model compiler failed for action {action}\n").into_bytes());
    }
    Ok(render(action, &hashes))
}

fn render(action: u64, hashes: &[Hash]) -> Vec<u8> {
    let mut text = format!("output of {action}:").into_bytes();
    for hash in hashes {
        text.extend_from_slice(hash.hex().as_bytes());
        text.push(b' ');
    }
    text
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
