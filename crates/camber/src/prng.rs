//! Thread-local SplitMix64 PRNG seeded from OS entropy via `RandomState`.

use std::cell::Cell;

thread_local! {
    static STATE: Cell<u64> = Cell::new({
        use std::hash::{BuildHasher, Hasher};
        let state = std::collections::hash_map::RandomState::new();
        let mut hasher = state.build_hasher();
        hasher.write_usize(0);
        hasher.finish()
    });
}

/// Return the next `u64` from the thread-local SplitMix64 generator.
pub(crate) fn next_u64() -> u64 {
    STATE.with(|cell| {
        let mut s = cell.get();
        s = s.wrapping_add(0x9e37_79b9_7f4a_7c15);
        cell.set(s);
        let mut z = s;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    })
}

/// Fill a fixed-size byte array with PRNG output.
#[cfg(feature = "otel")]
pub(crate) fn random_bytes<const N: usize>() -> [u8; N] {
    let mut result = [0u8; N];
    let mut offset = 0;
    while offset < N {
        let bytes = next_u64().to_ne_bytes();
        let to_copy = (N - offset).min(8);
        result[offset..offset + to_copy].copy_from_slice(&bytes[..to_copy]);
        offset += to_copy;
    }
    result
}
