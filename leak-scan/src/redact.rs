//! Bounded-buffer, all-hits secret redaction.
//!
//! The streaming [`crate::RollingMatcher`] reports the FIRST leak and is built
//! to BLOCK a tunnel. python-exec output is a bounded in-memory buffer that must
//! instead be SCRUBBED in place, so this finds EVERY non-overlapping occurrence
//! of a secret's verbatim bytes and replaces it with a marker. It reuses the
//! same Rabin pre-filter + SHA-256 confirm as the matcher (so detection cannot
//! drift between the two), specialized to a contiguous slice (direct indexing,
//! no ring buffer).

use std::collections::HashMap;

use sha2::{Digest, Sha256};

use crate::fingerprint::{poly, pow_base, sha256_hex, SecretFingerprint, RABIN_BASE};

/// One redacted span: which secret (by SHA-256, hex) and where it sat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RedactHit {
    pub sha256_hex: String,
    /// Byte offset of the matched span's first byte in the ORIGINAL input.
    pub offset: usize,
    /// Byte length of the matched (now replaced) span.
    pub len: usize,
}

/// Result of [`redact`]: the rewritten bytes + the spans that were replaced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RedactOutcome {
    pub bytes: Vec<u8>,
    pub hits: Vec<RedactHit>,
}

/// Write the redaction marker for `sha256_hex` directly into `out`. Carries the
/// first 8 hex chars of the secret's SHA-256 so a redaction correlates to the
/// matching `secret.redeemed` audit row WITHOUT leaking any plaintext.
fn push_marker(out: &mut Vec<u8>, sha256_hex_str: &str) {
    use std::fmt::Write as _;
    let mut m = String::with_capacity(19); // "[redacted:" + 8 + "]"
    let _ = write!(m, "[redacted:{}]", &sha256_hex_str[..8]);
    out.extend_from_slice(m.as_bytes());
}

/// Find every non-overlapping occurrence of any `patterns` value in `input` and
/// replace it with a `[redacted:<8hex>]` marker. Earliest match wins; on equal
/// start the longer
/// span wins; scanning resumes past a chosen span. Empty `patterns` (or none
/// matching) returns `input` unchanged with no hits. Bounded full-buffer scan:
/// O(input.len()) per distinct pattern length.
pub fn redact(input: &[u8], patterns: &[SecretFingerprint]) -> RedactOutcome {
    // Group target SHA-256s by (len, fp64), skipping patterns longer than the
    // input (they cannot match) and any defensive len == 0.
    let mut by_len: HashMap<usize, HashMap<u64, Vec<[u8; 32]>>> = HashMap::new();
    for p in patterns
        .iter()
        .filter(|p| p.len > 0 && p.len <= input.len())
    {
        by_len
            .entry(p.len)
            .or_default()
            .entry(p.fp64)
            .or_default()
            .push(p.sha256);
    }

    // Collect all confirmed (offset, len, sha256) hits with a per-length rolling
    // Rabin scan (cheap pre-filter) confirmed by SHA-256 (eliminates collisions).
    let mut raw: Vec<(usize, usize, [u8; 32])> = Vec::new();
    for (len, targets) in &by_len {
        let len = *len;
        let pow = pow_base(len);
        let mut cur = poly(&input[0..len]);
        let mut i = 0usize;
        loop {
            if let Some(shas) = targets.get(&cur) {
                let digest: [u8; 32] = Sha256::digest(&input[i..i + len]).into();
                if shas.contains(&digest) {
                    raw.push((i, len, digest));
                }
            }
            if i + len >= input.len() {
                break;
            }
            // Roll the window forward one byte: drop input[i], add input[i+len].
            let out = input[i] as u64;
            cur = cur
                .wrapping_sub(out.wrapping_mul(pow))
                .wrapping_mul(RABIN_BASE)
                .wrapping_add(input[i + len] as u64);
            i += 1;
        }
    }

    // Resolve overlaps: earliest start first, longer span first on a tie; then
    // greedily keep non-overlapping spans.
    //
    // Accepted limitation: when two DISTINCT secrets overlap (the tail of one is
    // the head of the other), the earlier-start span is redacted and the later
    // one is dropped, so the later secret's non-overlapping suffix survives in
    // plaintext. This is not adversarially reachable — agent-authored code cannot
    // control vault secret values, so it cannot engineer such an alignment; it can
    // only occur by genuine coincidence of two high-entropy secrets (negligible).
    // The streaming `RollingMatcher` has the equivalent first-hit limitation, and
    // this matches conventional non-overlapping find/replace semantics. Pinned by
    // `overlapping_distinct_secrets_leave_second_suffix`.
    raw.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    let mut next_free = 0usize;
    let mut chosen: Vec<(usize, usize, [u8; 32])> = Vec::new();
    for (off, len, sha) in raw {
        if off >= next_free {
            next_free = off + len;
            chosen.push((off, len, sha));
        }
    }

    if chosen.is_empty() {
        return RedactOutcome {
            bytes: input.to_vec(),
            hits: Vec::new(),
        };
    }

    // Splice the markers in, recording one RedactHit per replaced span.
    let mut bytes = Vec::with_capacity(input.len());
    let mut hits = Vec::with_capacity(chosen.len());
    let mut cursor = 0usize;
    for (off, len, sha) in chosen {
        bytes.extend_from_slice(&input[cursor..off]);
        let sha_hex = sha256_hex(&sha);
        push_marker(&mut bytes, &sha_hex);
        hits.push(RedactHit {
            sha256_hex: sha_hex,
            offset: off,
            len,
        });
        cursor = off + len;
    }
    bytes.extend_from_slice(&input[cursor..]);
    RedactOutcome { bytes, hits }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::fingerprint_value;

    fn fp(v: &[u8]) -> SecretFingerprint {
        fingerprint_value(v).expect("test secret >= MIN_SECRET_LEN")
    }

    fn sha8(v: &[u8]) -> String {
        let d: [u8; 32] = Sha256::digest(v).into();
        let mut s = String::new();
        for b in &d[..4] {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    #[test]
    fn no_patterns_returns_input_unchanged() {
        let out = redact(b"nothing to hide here", &[]);
        assert_eq!(out.bytes, b"nothing to hide here");
        assert!(out.hits.is_empty());
    }

    #[test]
    fn no_match_returns_input_unchanged() {
        let out = redact(b"clean output line", &[fp(b"super-secret-value")]);
        assert_eq!(out.bytes, b"clean output line");
        assert!(out.hits.is_empty());
    }

    #[test]
    fn single_occurrence_is_replaced_with_marker() {
        let secret = b"super-secret-value";
        let out = redact(b"x super-secret-value y", &[fp(secret)]);
        let expect = format!("x [redacted:{}] y", sha8(secret));
        assert_eq!(String::from_utf8(out.bytes).unwrap(), expect);
        assert_eq!(out.hits.len(), 1);
        assert_eq!(out.hits[0].offset, 2);
        assert_eq!(out.hits[0].len, secret.len());
    }

    #[test]
    fn multiple_occurrences_all_replaced() {
        let secret = b"super-secret-value";
        let out = redact(b"super-secret-value and super-secret-value", &[fp(secret)]);
        let body = String::from_utf8(out.bytes).unwrap();
        assert!(!body.contains("super-secret-value"));
        assert_eq!(body.matches("[redacted:").count(), 2);
        assert_eq!(out.hits.len(), 2);
    }

    #[test]
    fn adjacent_occurrences_both_replaced() {
        let secret = b"super-secret-value";
        let mut input = secret.to_vec();
        input.extend_from_slice(secret);
        let out = redact(&input, &[fp(secret)]);
        assert!(!String::from_utf8(out.bytes).unwrap().contains("super-secret-value"));
        assert_eq!(out.hits.len(), 2);
    }

    #[test]
    fn match_at_start_and_at_end() {
        let secret = b"super-secret-value";
        let mut input = secret.to_vec();
        input.extend_from_slice(b" mid ");
        input.extend_from_slice(secret);
        let out = redact(&input, &[fp(secret)]);
        assert_eq!(out.hits.len(), 2);
        assert_eq!(out.hits[0].offset, 0);
    }

    #[test]
    fn two_secrets_different_lengths_both_redacted() {
        let short = b"short-one"; // len 9
        let long = b"a-much-longer-secret-string"; // len 27
        let input = b"..short-one..a-much-longer-secret-string..";
        let out = redact(input, &[fp(short), fp(long)]);
        let body = String::from_utf8(out.bytes).unwrap();
        assert!(!body.contains("short-one"));
        assert!(!body.contains("a-much-longer-secret-string"));
        assert_eq!(out.hits.len(), 2);
    }

    #[test]
    fn overlapping_candidates_resolve_earliest_start() {
        // "abcdefghij" contains "abcdefgh" (len 8) at offset 0 and "cdefghij"
        // (len 8) at offset 2; the earlier-start match wins.
        let a = b"abcdefgh";
        let b = b"cdefghij";
        let out = redact(b"abcdefghij", &[fp(a), fp(b)]);
        assert_eq!(out.hits.len(), 1);
        assert_eq!(out.hits[0].offset, 0);
        assert_eq!(out.hits[0].sha256_hex.len(), 64);
    }

    #[test]
    fn overlapping_candidates_resolve_longer_span_on_tie() {
        // "abcdefghijklmnop" (len 16) starts at offset 0 and "abcdefgh" (len 8)
        // is a prefix of it, also at offset 0. The longer span must win.
        let short = b"abcdefgh"; // len 8, offset 0
        let long = b"abcdefghijklmnop"; // len 16, offset 0
        let out = redact(b"abcdefghijklmnop", &[fp(short), fp(long)]);
        assert_eq!(out.hits.len(), 1, "longer span must win on equal start offset");
        assert_eq!(out.hits[0].len, long.len());
        assert_eq!(out.hits[0].offset, 0);
    }

    #[test]
    fn overlapping_distinct_secrets_leave_second_suffix() {
        // Accepted-limitation characterization (see the greedy-resolution comment
        // in `redact`): two DISTINCT len-8 secrets overlap in the input —
        // A="abcdefgh" at [0,8) and B="fghijklm" at [5,13). Greedy keeps the
        // earlier-start span (A) and drops B, so B's non-overlapping suffix
        // ("ijklm") survives in plaintext. Not adversarially reachable: agent
        // code cannot align two vault secret values like this. If overlap
        // semantics ever change to redact the union, update this test.
        let a = b"abcdefgh"; // [0,8)
        let b = b"fghijklm"; // [5,13)
        let out = redact(b"abcdefghijklm", &[fp(a), fp(b)]);
        let body = String::from_utf8(out.bytes).unwrap();
        assert_eq!(out.hits.len(), 1, "only the earlier-start span is chosen");
        assert_eq!(out.hits[0].offset, 0);
        assert_eq!(out.hits[0].len, a.len());
        assert_eq!(body, format!("[redacted:{}]ijklm", sha8(a)));
        // The full second secret is gone, but its suffix coincidentally survives.
        assert!(!body.contains("fghijklm"));
        assert!(body.contains("ijklm"));
    }

    #[test]
    fn marker_carries_first_8_hex_of_sha256() {
        let secret = b"super-secret-value";
        let out = redact(secret, &[fp(secret)]);
        let body = String::from_utf8(out.bytes).unwrap();
        assert_eq!(body, format!("[redacted:{}]", sha8(secret)));
    }

    #[test]
    fn sub_min_len_value_is_never_fingerprinted_so_never_redacted() {
        // 7 bytes < MIN_SECRET_LEN: fingerprint_value returns None, so it can
        // never be a pattern and "secret7" stays in the output.
        assert!(fingerprint_value(b"secret7").is_none());
        let out = redact(b"leak secret7 here", &[]);
        assert_eq!(out.bytes, b"leak secret7 here");
    }
}
