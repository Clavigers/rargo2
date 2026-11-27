mod disk;
pub mod model;
pub mod rng;
mod nextest;
pub mod workspace;
pub mod worktree;
pub mod world;

mod transport;

pub use world::{Bug, Failure, Fault, Kind, Off, Report, run, run_off};

pub fn search(seeds: std::ops::Range<u64>, bug: Option<Bug>) -> Vec<Failure> {
    seeds.filter_map(|seed| run(seed, bug).err()).collect()
}

#[cfg(test)]
mod tests;
