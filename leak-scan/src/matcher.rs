//! Streaming credential-leak matcher. Feeds an arbitrarily-chunked byte stream
//! through a per-length Rabin-Karp rolling hash (cheap pre-filter) confirmed by
//! SHA-256 (eliminates collisions). State persists across [`RollingMatcher::feed`]
//! calls via a ring buffer of the last `maxLen` bytes, so a secret split across
//! a read boundary (`…AB | CD…`) still matches on the same logical pass.
//!
//! Memory is O(maxLen) regardless of stream size, so the whole connection can be
//! scanned with no body cap.

use std::collections::HashMap;

use sha2::{Digest, Sha256};

use crate::fingerprint::{poly, SecretFingerprint, RABIN_BASE};

/// A confirmed leak: which secret (by its SHA-256, hex) and where in the stream
/// its first byte sat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeakHit {
    pub sha256_hex: String,
    /// Absolute byte offset (0-based) of the matched window's first byte.
    pub offset: u64,
}

/// Per-distinct-length rolling state.
struct LenGroup {
    len: usize,
    /// `RABIN_BASE^(len-1)`, wrapping — the weight of the byte leaving the window.
    pow: u64,
    /// Current rolling hash over the last `len` bytes (valid once `primed`).
    cur: u64,
    primed: bool,
    /// fp64 → the SHA-256(s) of secrets of this length sharing that fp64.
    targets: HashMap<u64, Vec<[u8; 32]>>,
}

/// Streaming matcher over one direction of a tunnel.
pub struct RollingMatcher {
    groups: Vec<LenGroup>,
    /// Ring buffer of the last `cap` bytes; `cap = maxLen + 1` so the byte
    /// *leaving* the widest window is still present for the rolling subtraction.
    ring: Vec<u8>,
    cap: usize,
    /// Total bytes fed so far. The most recent byte sits at absolute index `fed-1`.
    fed: u64,
}

impl RollingMatcher {
    /// Build a matcher for `patterns`. Patterns below the minimum length never
    /// reach here (the provisioner filters them), but any with `len == 0` are
    /// defensively dropped. An empty pattern set makes [`Self::feed`] a near no-op.
    pub fn new(patterns: Vec<SecretFingerprint>) -> Self {
        let mut by_len: HashMap<usize, HashMap<u64, Vec<[u8; 32]>>> = HashMap::new();
        for p in patterns.into_iter().filter(|p| p.len > 0) {
            by_len
                .entry(p.len)
                .or_default()
                .entry(p.fp64)
                .or_default()
                .push(p.sha256);
        }
        let max_len = by_len.keys().copied().max().unwrap_or(0);
        let groups = by_len
            .into_iter()
            .map(|(len, targets)| LenGroup {
                len,
                pow: pow_base(len),
                cur: 0,
                primed: false,
                targets,
            })
            .collect();
        let cap = max_len.saturating_add(1).max(1);
        RollingMatcher {
            groups,
            ring: vec![0u8; cap],
            cap,
            fed: 0,
        }
    }

    /// True when there is nothing to scan for — the caller skips the scanning
    /// relay entirely and uses the plain copy.
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// Feed a chunk; return the first confirmed leak in it (if any). Stateful.
    pub fn feed(&mut self, chunk: &[u8]) -> Option<LeakHit> {
        if self.groups.is_empty() {
            self.fed = self.fed.wrapping_add(chunk.len() as u64);
            return None;
        }
        for &b in chunk {
            let i = self.fed; // absolute index of this byte
            self.ring[(i as usize) % self.cap] = b;
            // Update each length group now that byte `i` has been stored.
            for g in &mut self.groups {
                let l = g.len as u64;
                if i + 1 < l {
                    continue; // window not yet full
                }
                if !g.primed {
                    // First full window [i-l+1 ..= i]: compute directly.
                    g.cur = window_poly(&self.ring, self.cap, i, g.len);
                    g.primed = true;
                } else {
                    // Roll: drop the byte at i-l, shift, add the new byte b.
                    let out = self.ring[((i - l) as usize) % self.cap];
                    g.cur = g
                        .cur
                        .wrapping_sub((out as u64).wrapping_mul(g.pow))
                        .wrapping_mul(RABIN_BASE)
                        .wrapping_add(b as u64);
                }
                if let Some(shas) = g.targets.get(&g.cur) {
                    // fp64 pre-filter hit → confirm with SHA-256 of the window.
                    let window = read_window(&self.ring, self.cap, i, g.len);
                    let mut h = Sha256::new();
                    h.update(&window);
                    let digest: [u8; 32] = h.finalize().into();
                    if shas.contains(&digest) {
                        return Some(LeakHit {
                            sha256_hex: hex(&digest),
                            offset: i + 1 - l,
                        });
                    }
                }
            }
            self.fed = i + 1;
        }
        None
    }
}

/// `RABIN_BASE^(len-1)`, wrapping. `len >= 1` guaranteed by the caller.
fn pow_base(len: usize) -> u64 {
    let mut p = 1u64;
    for _ in 0..len.saturating_sub(1) {
        p = p.wrapping_mul(RABIN_BASE);
    }
    p
}

/// Direct poly hash of the `len`-byte window ending at absolute index `i`.
fn window_poly(ring: &[u8], cap: usize, i: u64, len: usize) -> u64 {
    poly(&read_window(ring, cap, i, len))
}

/// Copy the `len`-byte window ending at absolute index `i` out of the ring.
fn read_window(ring: &[u8], cap: usize, i: u64, len: usize) -> Vec<u8> {
    let start = i + 1 - len as u64; // absolute index of the first window byte
    (0..len)
        .map(|k| ring[((start + k as u64) as usize) % cap])
        .collect()
}

fn hex(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::fingerprint_value;

    fn fp(v: &[u8]) -> SecretFingerprint {
        fingerprint_value(v).expect("test secret long enough")
    }

    fn sha_hex(v: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(v);
        hex(&h.finalize().into())
    }

    #[test]
    fn detects_secret_in_a_single_chunk() {
        let secret = b"alpha-bravo-charlie";
        let mut m = RollingMatcher::new(vec![fp(secret)]);
        let hit = m.feed(b"GET /?x=alpha-bravo-charlie HTTP/1.1").expect("hit");
        assert_eq!(hit.sha256_hex, sha_hex(secret));
        assert_eq!(hit.offset, 8); // index where "alpha..." starts
    }

    #[test]
    fn clean_stream_no_hit() {
        let mut m = RollingMatcher::new(vec![fp(b"alpha-bravo-charlie")]);
        assert!(m.feed(b"nothing to see here, move along please").is_none());
    }

    #[test]
    fn detects_secret_split_across_two_feeds() {
        // The boundary pin: the secret straddles the read boundary.
        let secret = b"split-secret-value";
        let mut m = RollingMatcher::new(vec![fp(secret)]);
        assert!(m.feed(b"prefix split-secret").is_none());
        let hit = m.feed(b"-value suffix").expect("hit across boundary");
        assert_eq!(hit.sha256_hex, sha_hex(secret));
    }

    #[test]
    fn detects_secret_split_byte_by_byte() {
        let secret = b"drip-fed-secret-xy";
        let mut m = RollingMatcher::new(vec![fp(secret)]);
        let mut hit = None;
        for b in b"zz".iter().chain(secret).chain(b"qq") {
            if let Some(h) = m.feed(&[*b]) {
                hit = Some(h);
            }
        }
        assert_eq!(hit.expect("byte-fed hit").sha256_hex, sha_hex(secret));
    }

    #[test]
    fn two_secrets_same_length() {
        let a = b"secret-aaa-1234"; // len 15
        let b = b"secret-bbb-5678"; // len 15
        let mut m = RollingMatcher::new(vec![fp(a), fp(b)]);
        assert_eq!(m.feed(b"xx secret-bbb-5678 yy").unwrap().sha256_hex, sha_hex(b));
    }

    #[test]
    fn two_secrets_different_lengths() {
        let short = b"short-one"; // len 9
        let long = b"a-much-longer-secret-string"; // len 27
        let mut m = RollingMatcher::new(vec![fp(short), fp(long)]);
        assert_eq!(
            m.feed(b"...a-much-longer-secret-string...").unwrap().sha256_hex,
            sha_hex(long)
        );
    }

    #[test]
    fn empty_patterns_is_noop() {
        let mut m = RollingMatcher::new(vec![]);
        assert!(m.is_empty());
        assert!(m.feed(b"anything at all including secrets").is_none());
    }

    #[test]
    fn fp64_collision_is_rejected_by_sha256() {
        // Forge a fingerprint with the SAME fp64 + len as a real secret but a
        // different SHA-256. The pre-filter fires; the SHA-256 confirm rejects it.
        let real = b"real-secret-value-9";
        let mut forged = fp(real);
        forged.sha256 = [0u8; 32]; // wrong digest, identical len + fp64
        let mut m = RollingMatcher::new(vec![forged]);
        assert!(
            m.feed(b"xx real-secret-value-9 yy").is_none(),
            "SHA-256 confirm must reject an fp64-only match"
        );
    }
}
