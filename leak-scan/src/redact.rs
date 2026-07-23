//! Bounded-buffer, all-hits secret redaction.
//!
//! The streaming [`crate::RollingMatcher`] reports the FIRST leak and is built
//! to BLOCK a tunnel. python-exec output is a bounded in-memory buffer that must
//! instead be SCRUBBED in place, so this finds EVERY occurrence of a secret's
//! verbatim bytes and replaces it with a marker — coincidentally-overlapping
//! matches MERGE into one redacted run so no secret byte survives between two
//! overlapping secrets (see [`redact`]). It reuses the same Rabin pre-filter +
//! SHA-256 confirm as the matcher (so detection cannot drift between the two),
//! specialized to a contiguous slice (direct indexing, no ring buffer).
//!
//! An ENCODED appearance of a secret (base64/hex/url-encoded) is NOT scrubbed —
//! this matches verbatim value bytes only, as does the streaming matcher. The
//! containment boundary for encoded egress is the sandbox + egress proxy, not
//! this fingerprint scanner.

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

/// Write the redaction marker directly into `out`. Lists each contributing
/// secret's first-8 SHA-256 hex chars (joined by `+`) so a redaction correlates
/// to the matching `secret.redeemed` audit row(s) WITHOUT leaking any plaintext.
/// A single-secret run yields exactly `[redacted:<8hex>]` (unchanged format).
fn push_marker(out: &mut Vec<u8>, sha256_hex_strs: &[String]) {
    use std::fmt::Write as _;
    let mut m = String::from("[redacted:");
    for (i, s) in sha256_hex_strs.iter().enumerate() {
        if i > 0 {
            m.push('+');
        }
        let _ = write!(m, "{}", &s[..8]);
    }
    m.push(']');
    out.extend_from_slice(m.as_bytes());
}

/// A maximal run of overlapping redaction spans, merged into one redacted
/// region so no secret byte can survive between two coincidentally-overlapping
/// secrets.
struct Run {
    start: usize,
    end: usize,
    /// Contributing (offset, len, sha256) spans, in scan order.
    spans: Vec<(usize, usize, [u8; 32])>,
}

/// Find every occurrence of any `patterns` value in `input` and replace it with
/// a `[redacted:<8hex>]` marker. Overlapping matches MERGE into one redacted
/// region (no secret byte survives an overlap); adjacent matches stay separate.
/// Empty `patterns` (or none matching) returns `input` unchanged with no hits.
/// Bounded full-buffer scan: O(input.len()) per distinct pattern length.
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

    // Merge overlapping spans into maximal runs so NO secret byte can survive.
    // Sort earliest-start first, longer span first on a tie; then fold each span
    // into the current run when it STRICTLY overlaps it (`off < run.end`).
    // Adjacent spans (`off == run.end`) start a fresh run, preserving
    // back-to-back redaction. A run redacts the union of its spans — a strict
    // superset of the old greedy behaviour, so over-redaction is always safe.
    // This closes the prior gap where two DISTINCT overlapping secrets let the
    // later one's non-overlapping suffix survive in plaintext (a coincidence of
    // two high-entropy values; not adversarially reachable, since agent code
    // cannot control vault secret values). Pinned by
    // `overlapping_distinct_secrets_are_fully_redacted`.
    raw.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    let mut runs: Vec<Run> = Vec::new();
    for (off, len, sha) in raw {
        match runs.last_mut() {
            Some(run) if off < run.end => {
                run.end = run.end.max(off + len);
                run.spans.push((off, len, sha));
            }
            _ => runs.push(Run {
                start: off,
                end: off + len,
                spans: vec![(off, len, sha)],
            }),
        }
    }

    if runs.is_empty() {
        return RedactOutcome {
            bytes: input.to_vec(),
            hits: Vec::new(),
        };
    }

    // Splice one marker per run. The marker lists each DISTINCT contributing
    // secret's 8-hex in first-appearance order (for a non-overlapping run that
    // is exactly one secret → byte-identical to the prior format). One RedactHit
    // is recorded per contributing span (original offsets/lens preserved), so
    // the audit trail sees every secret that appeared.
    let mut bytes = Vec::with_capacity(input.len());
    let mut hits = Vec::new();
    let mut cursor = 0usize;
    for run in runs {
        bytes.extend_from_slice(&input[cursor..run.start]);
        let mut marker_hexes: Vec<String> = Vec::new();
        let mut seen: Vec<[u8; 32]> = Vec::new();
        for (off, len, sha) in &run.spans {
            let sha_hex = sha256_hex(sha);
            if !seen.contains(sha) {
                seen.push(*sha);
                marker_hexes.push(sha_hex.clone());
            }
            hits.push(RedactHit {
                sha256_hex: sha_hex,
                offset: *off,
                len: *len,
            });
        }
        push_marker(&mut bytes, &marker_hexes);
        cursor = run.end;
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
    fn overlapping_candidates_merge_into_one_run() {
        // "abcdefghij" contains "abcdefgh" (len 8) at [0,8) and "cdefghij"
        // (len 8) at [2,10) — they overlap, so they MERGE into one redacted run
        // covering [0,10). Both secrets are recorded; no plaintext survives.
        let a = b"abcdefgh"; // [0,8)
        let b = b"cdefghij"; // [2,10)
        let out = redact(b"abcdefghij", &[fp(a), fp(b)]);
        let body = String::from_utf8(out.bytes).unwrap();
        assert_eq!(out.hits.len(), 2, "both overlapping secrets recorded");
        assert_eq!(out.hits[0].offset, 0);
        assert_eq!(out.hits[1].offset, 2);
        assert_eq!(body, format!("[redacted:{}+{}]", sha8(a), sha8(b)));
        assert!(!body.contains("cdefghij"));
        assert!(!body.contains("abcdefgh"));
    }

    #[test]
    fn nested_spans_same_start_merge_into_one_run() {
        // "abcdefghijklmnop" (len 16) at [0,16) and its prefix "abcdefgh"
        // (len 8) at [0,8) merge into [0,16); both recorded. The sort puts the
        // longer span first on the start tie, so its sha leads the marker.
        let short = b"abcdefgh"; // [0,8)
        let long = b"abcdefghijklmnop"; // [0,16)
        let out = redact(b"abcdefghijklmnop", &[fp(short), fp(long)]);
        let body = String::from_utf8(out.bytes).unwrap();
        assert_eq!(out.hits.len(), 2);
        assert_eq!(body, format!("[redacted:{}+{}]", sha8(long), sha8(short)));
        assert!(!body.contains("abcdefgh"));
    }

    #[test]
    fn overlapping_distinct_secrets_are_fully_redacted() {
        // Two DISTINCT len-8 secrets overlap: A="abcdefgh" [0,8) and
        // B="fghijklm" [5,13). Merging redacts the UNION [0,13), so B's suffix
        // ("ijklm") can no longer survive — the gap the greedy resolution left.
        let a = b"abcdefgh"; // [0,8)
        let b = b"fghijklm"; // [5,13)
        let out = redact(b"abcdefghijklm", &[fp(a), fp(b)]);
        let body = String::from_utf8(out.bytes).unwrap();
        assert_eq!(out.hits.len(), 2, "both overlapping secrets recorded");
        assert_eq!(out.hits[0].offset, 0);
        assert_eq!(out.hits[1].offset, 5);
        assert_eq!(body, format!("[redacted:{}+{}]", sha8(a), sha8(b)));
        assert!(!body.contains("fghijklm"));
        assert!(!body.contains("ijklm"), "the suffix must NOT survive after merge");
    }

    #[test]
    fn three_overlapping_secrets_merge_into_one_run() {
        // A [0,8), B [5,13), C [10,18) form an overlap chain → one run [0,18).
        let a = b"abcdefgh"; // [0,8)
        let b = b"fghijklm"; // [5,13)
        let c = b"klmnopqr"; // [10,18)
        let out = redact(b"abcdefghijklmnopqr", &[fp(a), fp(b), fp(c)]);
        let body = String::from_utf8(out.bytes).unwrap();
        assert_eq!(out.hits.len(), 3);
        assert_eq!(body, format!("[redacted:{}+{}+{}]", sha8(a), sha8(b), sha8(c)));
    }

    #[test]
    fn disjoint_overlapping_pairs_stay_separate_runs() {
        // Two overlap-pairs separated by a gap → two runs, two markers.
        let a = b"abcdefgh"; // pair 1: [0,8)
        let b = b"fghijklm"; // pair 1: [5,13)
        let c = b"ABCDEFGH"; // pair 2: [15,23)
        let d = b"FGHIJKLM"; // pair 2: [20,28)
        let input = b"abcdefghijklm  ABCDEFGHIJKLM";
        let out = redact(input, &[fp(a), fp(b), fp(c), fp(d)]);
        let body = String::from_utf8(out.bytes).unwrap();
        assert_eq!(body.matches("[redacted:").count(), 2, "two separate runs");
        assert_eq!(out.hits.len(), 4, "all four secrets recorded");
        assert_eq!(
            body,
            format!(
                "[redacted:{}+{}]  [redacted:{}+{}]",
                sha8(a), sha8(b), sha8(c), sha8(d)
            )
        );
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
