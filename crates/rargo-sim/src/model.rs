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
