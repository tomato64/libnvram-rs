//! FNV-1a, 64-bit.
//!
//! Chosen for being three lines and dependency-free. Key quality only affects
//! probe-chain length and seqlock-slot collision rate, never correctness.

pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    // 0 is reserved as the "empty bucket" marker in the index, so never
    // hand it out.
    if h == 0 {
        1
    } else {
        h
    }
}
