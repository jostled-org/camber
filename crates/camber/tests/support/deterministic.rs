use std::collections::BTreeSet;
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

    /// Builds one case, then rebuilds it from the same seed and index.
    ///
    /// Both builds must yield the same value and leave the same stream state,
    /// so a failure reported as `seed=… case=…` reproduces from those two
    /// numbers alone.
    pub fn reproducible<T, F>(&self, index: u64, build: F) -> (DeterministicCase, T)
    where
        T: PartialEq + fmt::Debug,
        F: Fn(&mut DeterministicCase) -> T,
    {
        let mut case = self.case(index);
        let built = build(&mut case);
        let mut replay = self.case(index);
        let rebuilt = build(&mut replay);
        assert_eq!(built, rebuilt, "{case}: a rebuild yields the same case");
        assert_eq!(case, replay, "{case}: a rebuild draws the same stream");
        (case, built)
    }

    /// Fails with this seed when `cases` cases left a required label unreached.
    pub fn assert_reached<'a>(
        &self,
        cases: u64,
        required: impl IntoIterator<Item = &'a str>,
        reached: &BTreeSet<&str>,
    ) {
        let unreached: Box<[&str]> = required
            .into_iter()
            .filter(|label| !reached.contains(*label))
            .collect();
        assert!(
            unreached.is_empty(),
            "seed={:#x}: {cases} cases never reached {unreached:?}",
            self.seed
        );
    }
}

/// One named family: a seed, and the rule that turns each of its cases into a row.
pub struct Family<Row> {
    pub name: &'static str,
    pub seed: u64,
    pub generate: fn(u64, &mut DeterministicCase) -> Row,
}

impl<Row: PartialEq + fmt::Debug> Family<Row> {
    /// Yields cases `0..count` of this family, each with its reproducible row.
    pub fn rows(&self, count: u64) -> impl Iterator<Item = (DeterministicCase, Row)> + '_ {
        let generator = DeterministicGenerator::new(self.seed);
        (0..count)
            .map(move |index| generator.reproducible(index, |case| (self.generate)(index, case)))
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

    /// Borrows one item from a generator table that must not be empty.
    pub fn pick<'a, T>(&mut self, values: &'a [T]) -> &'a T {
        self.select(values)
            .unwrap_or_else(|| panic!("{self}: a generator table is empty"))
    }

    /// Returns a value in `0..upper_exclusive`, which must not be zero.
    pub fn below(&mut self, upper_exclusive: usize) -> usize {
        let upper = NonZeroUsize::new(upper_exclusive)
            .unwrap_or_else(|| panic!("{self}: a generated bound is zero"));
        self.bounded(upper)
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
