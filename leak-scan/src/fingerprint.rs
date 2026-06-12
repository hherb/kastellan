//! Secret-value fingerprinting: a one-way [`SecretFingerprint`] (length +
//! 64-bit Rabin polynomial hash + SHA-256) computed from a secret's plaintext
//! bytes. Provisioned to the egress proxy so it can detect the verbatim bytes
//! without ever holding the secret value.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Secrets shorter than this are never fingerprinted/provisioned: trivially
/// short values produce high false-positive match rates against arbitrary
/// egress traffic and are not real credentials.
pub const MIN_SECRET_LEN: usize = 8;

/// Secrets longer than this are never fingerprinted/provisioned. Generous enough
/// for any real credential (API keys, JWTs, full PEM private keys all sit well
/// under 16 KiB), while bounding `len`: the scanning side sizes its rolling-window
/// ring buffer at `maxLen + 1`, so capping here keeps a corrupt/oversized
/// provisioning entry from driving a large allocation (defense-in-depth — the
/// file is host-owned, but [`crate::parse_hashes`] also re-checks this bound).
pub const MAX_SECRET_LEN: usize = 16 * 1024;

/// Base of the Rabin-Karp polynomial rolling hash. MUST be identical on the
/// provisioning side ([`fingerprint_value`]) and the scanning side
/// ([`super::matcher::RollingMatcher`]) — they live in this one crate so they
/// cannot drift. Arithmetic is wrapping (mod 2^64).
pub(crate) const RABIN_BASE: u64 = 257;

/// One-way fingerprint of a secret value. Carries no plaintext: `fp64` and
/// `sha256` are both irreversible for a high-entropy secret; only `len` leaks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretFingerprint {
    /// Byte length of the secret value (== the scan window width).
    pub len: usize,
    /// 64-bit Rabin polynomial hash of the value — the cheap pre-filter.
    pub fp64: u64,
    /// SHA-256 of the value — the confirmation that eliminates Rabin collisions.
    pub sha256: [u8; 32],
}

/// Direct Rabin polynomial hash of `bytes`: `sum(b_k * BASE^(len-1-k))`, wrapping.
/// The [`super::matcher::RollingMatcher`] rolling state converges to this exact
/// value for any window equal to `bytes`.
pub(crate) fn poly(bytes: &[u8]) -> u64 {
    let mut h = 0u64;
    for &b in bytes {
        h = h.wrapping_mul(RABIN_BASE).wrapping_add(b as u64);
    }
    h
}

/// Fingerprint `value`. Returns `None` if its length is outside
/// `[MIN_SECRET_LEN, MAX_SECRET_LEN]`.
pub fn fingerprint_value(value: &[u8]) -> Option<SecretFingerprint> {
    if value.len() < MIN_SECRET_LEN || value.len() > MAX_SECRET_LEN {
        return None;
    }
    let mut h = Sha256::new();
    h.update(value);
    let sha256: [u8; 32] = h.finalize().into();
    Some(SecretFingerprint {
        len: value.len(),
        fp64: poly(value),
        sha256,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_a_long_enough_value() {
        let fp = fingerprint_value(b"super-secret-token-1234").expect("long enough");
        assert_eq!(fp.len, 23);
        // sha256 matches an independent computation.
        let mut h = Sha256::new();
        h.update(b"super-secret-token-1234");
        let expected: [u8; 32] = h.finalize().into();
        assert_eq!(fp.sha256, expected);
        // fp64 matches the direct polynomial.
        assert_eq!(fp.fp64, poly(b"super-secret-token-1234"));
    }

    #[test]
    fn rejects_values_below_min_len() {
        assert!(fingerprint_value(b"").is_none());
        assert!(fingerprint_value(b"1234567").is_none()); // 7 < 8
        assert!(fingerprint_value(b"12345678").is_some()); // 8 == MIN
    }

    #[test]
    fn rejects_values_above_max_len() {
        assert!(fingerprint_value(&vec![b'x'; MAX_SECRET_LEN]).is_some()); // == MAX
        assert!(fingerprint_value(&vec![b'x'; MAX_SECRET_LEN + 1]).is_none()); // > MAX
    }

    #[test]
    fn poly_is_position_sensitive() {
        // Anagram inputs must not collide trivially (sanity on the base choice).
        assert_ne!(poly(b"ab"), poly(b"ba"));
    }
}
