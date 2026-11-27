//! The server's random numbers: a generator the caller seeds. The shell seeds
//! it from the OS; tests and `rargo-sim` seed it from their own seed, so the
//! same seed gives the same IDs.

#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed ^ 0x5851_f42d_4c95_7f2d)
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
    use super::*;

    #[test]
    fn the_same_seed_draws_the_same_numbers_and_another_seed_others() {
        let draw = |seed| {
            let mut r = Rng::new(seed);
            (0..16).map(|_| r.next_u64()).collect::<Vec<_>>()
        };
        assert_eq!(draw(7), draw(7));
        assert_ne!(draw(7), draw(8));
        let a = draw(0);
        assert!(a.iter().all(|&x| x != 0) && a.windows(2).all(|w| w[0] != w[1]));
    }
}
