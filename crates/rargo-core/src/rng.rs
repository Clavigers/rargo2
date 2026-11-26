//! A random generator that the caller seeds. Logic draws from one it is
//! handed; the shell seeds it from the OS (`Rng::from_shell`), tests and
//! `rargo-sim` from their own seed, so a run can be replayed from its seed.

/// SplitMix64: small, fast, and fully determined by its seed.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn seeded(seed: u64) -> Rng {
        Rng(seed)
    }

    /// Seeded by the shell's `io::random`: the kernel's bytes, or in a
    /// simulation the simulation's seed.
    pub fn from_shell() -> Rng {
        let mut b = [0u8; 8];
        crate::io::random(&mut b);
        Rng::seeded(u64::from_le_bytes(b))
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

#[cfg(test)]
mod tests {
    use super::Rng;

    #[test]
    fn one_seed_gives_one_sequence() {
        let draw = |seed| {
            let mut r = Rng::seeded(seed);
            (0..8).map(|_| r.next_u64()).collect::<Vec<_>>()
        };
        assert_eq!(draw(7), draw(7));
        assert_ne!(draw(7), draw(8));
    }
}
