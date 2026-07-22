use std::fmt;
use std::num::NonZeroUsize;

/// The fixed seed used by [`DeterministicGenerator::stable`].
pub const STABLE_SEED: u64 = 0x4341_4d42_4552_0007;

const SPLITMIX_INCREMENT: u64 = 0x9e37_79b9_7f4a_7c15;
const SPLITMIX_MULTIPLIER_1: u64 = 0xbf58_476d_1ce4_e5b9;
const SPLITMIX_MULTIPLIER_2: u64 = 0x94d0_49bb_1331_11eb;

/// Creates independently addressable deterministic cases from one explicit seed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeterministicGenerator {
    seed: u64,
}

impl DeterministicGenerator {
    pub const fn new(seed: u64) -> Self {
        Self { seed }
    }

    pub const fn stable() -> Self {
        Self::new(STABLE_SEED)
    }

    pub const fn seed(&self) -> u64 {
        self.seed
    }

    pub const fn case(&self, index: u64) -> DeterministicCase {
        // A golden-ratio stride gives each case a reproducible starting state.
        let state = self.seed ^ index.wrapping_mul(SPLITMIX_INCREMENT);
        DeterministicCase {
            seed: self.seed,
            index,
            state,
        }
    }
}

/// One mutable SplitMix64 stream, identified by its immutable seed and case index.
#[derive(Debug, Eq, PartialEq)]
pub struct DeterministicCase {
    seed: u64,
    index: u64,
    state: u64,
}

impl DeterministicCase {
    pub const fn seed(&self) -> u64 {
        self.seed
    }

    pub const fn index(&self) -> u64 {
        self.index
    }

    /// Returns a value in `0..upper_exclusive`.
    pub fn bounded(&mut self, upper_exclusive: NonZeroUsize) -> usize {
        (self.next_u64() % upper_exclusive.get() as u64) as usize
    }

    pub fn boolean(&mut self) -> bool {
        self.next_u64() & 1 == 1
    }

    /// Borrows one item, or returns `None` without advancing an empty case.
    pub fn select<'a, T>(&mut self, values: &'a [T]) -> Option<&'a T> {
        let upper = NonZeroUsize::new(values.len())?;
        values.get(self.bounded(upper))
    }

    fn next_u64(&mut self) -> u64 {
        // SplitMix64 by Steele, Lea, and Flood. These constants and operations
        // are the stable sequence contract; changing either changes all cases.
        self.state = self.state.wrapping_add(SPLITMIX_INCREMENT);
        let mixed = (self.state ^ (self.state >> 30)).wrapping_mul(SPLITMIX_MULTIPLIER_1);
        let mixed = (mixed ^ (mixed >> 27)).wrapping_mul(SPLITMIX_MULTIPLIER_2);
        mixed ^ (mixed >> 31)
    }
}

impl fmt::Display for DeterministicCase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "seed={:#x} case={}", self.seed, self.index)
    }
}
