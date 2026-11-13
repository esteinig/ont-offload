// SPDX-License-Identifier: AGPL-3.0-or-later
//! Small deterministic PRNG (`SplitMix64`) and string hashing.
//!
//! Deterministic so that a seed reproduces the same runs, names and content.

/// `SplitMix64` generator.
#[derive(Debug, Clone)]
pub struct SplitMix64(u64);

impl SplitMix64 {
    /// New generator from a seed.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// Next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Fill a buffer with random bytes.
    pub fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let v = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }

    /// Uniform value in `0..n` (`n > 0`).
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }

    /// Uniform float in `[0, 1)`.
    #[allow(clippy::cast_precision_loss)]
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// `value` with up to `±frac` relative jitter.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn jitter(&mut self, value: u64, frac: f64) -> u64 {
        let f = 1.0 + frac * (2.0 * self.unit() - 1.0);
        (value as f64 * f).round().max(1.0) as u64
    }

    /// Eight lowercase hex characters.
    pub fn hex8(&mut self) -> String {
        format!("{:08x}", self.next_u64() & 0xFFFF_FFFF)
    }
}

/// FNV-1a hash of a string, for deriving per-file seeds.
#[must_use]
pub fn hash_str(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_spread() {
        let mut a = SplitMix64::new(7);
        let mut b = SplitMix64::new(7);
        assert_eq!(a.next_u64(), b.next_u64());
        let mut buf = [0u8; 13];
        a.fill(&mut buf);
        assert!(buf.iter().any(|x| *x != 0));
        for _ in 0..1000 {
            let u = a.unit();
            assert!((0.0..1.0).contains(&u));
            let j = a.jitter(1000, 0.1);
            assert!((900..=1100).contains(&j));
        }
        assert_eq!(a.hex8().len(), 8);
        assert_ne!(hash_str("a"), hash_str("b"));
    }
}
