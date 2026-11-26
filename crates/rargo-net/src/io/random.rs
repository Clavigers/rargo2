//! Where the link's random values come from. The shell gives an endpoint the
//! operating system's generator; tests and the simulator give it a seeded one,
//! so a run is a function of its seed.

use crate::Random;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// The operating system's generator: what a real endpoint draws from. In a
/// simulation (`io::sim`) it is the simulation's seed instead.
pub fn os_random() -> Arc<dyn Random> {
    Arc::new(Os)
}

/// A generator that gives the same values, in the same order, for the same seed.
pub fn seeded_random(seed: u64) -> Arc<dyn Random> {
    Arc::new(Seeded(AtomicU64::new(seed ^ 0x5851_f42d_4c95_7f2d)))
}

struct Os;

impl Random for Os {
    fn fill(&self, out: &mut [u8]) {
        super::random(out);
    }
}

struct Seeded(AtomicU64);

impl Random for Seeded {
    fn fill(&self, out: &mut [u8]) {
        for chunk in out.chunks_mut(8) {
            let mut z = self.0.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed).wrapping_add(0x9e37_79b9_7f4a_7c15);
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^= z >> 31;
            chunk.copy_from_slice(&z.to_le_bytes()[..chunk.len()]);
        }
    }
}
