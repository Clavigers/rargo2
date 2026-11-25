pub mod model;
pub mod rng;
pub mod world;

pub use world::{Bug, Failure, Kind, Report, run};

pub fn search(seeds: std::ops::Range<u64>, bug: Option<Bug>) -> Vec<Failure> {
    seeds.filter_map(|seed| run(seed, bug).err()).collect()
}

#[cfg(test)]
mod tests;
