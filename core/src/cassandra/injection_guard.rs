//! Worker-output prompt-injection guard.
//!
//! Pure-function catalogue scan called from `tool_host::dispatch`
//! after `worker.call` returns Ok, before the result is appended to
//! the scheduler's conversation history. The chokepoint pattern
//! (Option M, issue #16) means every worker result passes through
//! exactly one screen, with no bypass path.
//!
//! On `InjectionDecision::Block` the caller replaces the worker
//! result with a redacted placeholder and writes a second audit row
//! carrying only the SHA-256 of the scanned body + length + score +
//! class codes — never the raw scanned text. See
//! [`docs/superpowers/specs/2026-05-28-worker-output-prompt-injection-guard-design.md`](../../../docs/superpowers/specs/2026-05-28-worker-output-prompt-injection-guard-design.md)
//! for the full design.
//!
//! ## Why a separate module
//!
//! The screen is a pure function over `&str` so the catalogue stays
//! greppable (one weight + pattern + class per entry) and the helper
//! is exercisable without the async dispatcher machinery.
//!
//! ## Scope (Slice 1, deliberately narrow)
//!
//! - Substring matching after `normalize` (lowercase + strip
//!   zero-width). No regex, no leetspeak fold, no multilingual
//!   coverage. The catalogue is meant to be read in one sitting.
//! - Two-tier verdict (`Allow` / `Block`). A future Review tier slots
//!   in via the `#[non_exhaustive]` enum.
//! - Per-rule weights summed (cap 1.0); threshold `BLOCK_THRESHOLD`.

use serde_json::Value;

/// Verdict returned by [`screen`].
///
/// `score` is the sum of per-rule weights that fired (cap 1.0).
/// `reason_codes` carries the deduped, lexicographically-sorted set
/// of class names whose entries fired — the operator-visible signal.
#[derive(Debug, Clone, PartialEq)]
pub struct InjectionVerdict {
    pub score: f32,
    pub decision: InjectionDecision,
    pub reason_codes: Vec<&'static str>,
}

/// Decision side of [`InjectionVerdict`].
///
/// `#[non_exhaustive]` so a future `Review` variant (HANDOVER Item
/// 30's 0.45–0.70 tier) does not break out-of-crate callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InjectionDecision {
    Allow,
    Block,
}

/// Score `>=` this triggers `InjectionDecision::Block`.
pub const BLOCK_THRESHOLD: f32 = 0.70;

/// Byte cap on the body [`extract_scannable_text`] returns. Prevents
/// pathological-size worker outputs (e.g. a future `web-fetch`
/// returning 10 MB of HTML) from burning CPU in the substring scan.
pub const SCAN_BYTE_CAP: usize = 64 * 1024;

// ----- Public surface placeholders (filled in by later tasks) -----

/// Catalogue scan over `text`. Returns an [`InjectionVerdict`] whose
/// `score` is the sum of per-rule weights that fired (cap 1.0) and
/// whose `decision` is `Block` iff `score >= BLOCK_THRESHOLD`.
///
/// The match is **case-insensitive** and **zero-width-stripped**: the
/// helper lowercases the input and removes ZWJ/ZWNJ/ZWSP/BOM once at
/// the top so callers don't have to.
pub fn screen(_text: &str) -> InjectionVerdict {
    unimplemented!("filled in by Task 3")
}

/// Extract a flat string body for [`screen`] from a worker's JSON
/// result. Walks `value` recursively, concatenating `Value::String`
/// nodes with `'\n'` between them. Non-string nodes (numbers, bools,
/// null, JSON keys, structural punctuation) are skipped so the
/// catalogue cannot fire on JSON shape itself.
///
/// Returns `(body, truncated)` where `truncated == true` iff the
/// concatenation reached `byte_cap` before all string nodes were
/// emitted. Forensic SHA-256 is computed over the **returned** body,
/// truncated or not — so the audit row's `body_byte_len` field and
/// the SHA are always self-consistent.
pub fn extract_scannable_text(value: &Value, byte_cap: usize) -> (String, bool) {
    let mut out = String::new();
    let truncated = walk(value, &mut out, byte_cap);
    (out, truncated)
}

/// Recursive helper for [`extract_scannable_text`]. Returns `true`
/// iff the cap was hit during this subtree.
///
/// Strings get a leading `'\n'` separator iff `out` is non-empty,
/// so consecutive emitted values are newline-separated but the body
/// has no leading newline. The truncation check happens **before**
/// appending so we never overshoot the cap.
fn walk(value: &Value, out: &mut String, byte_cap: usize) -> bool {
    match value {
        Value::String(s) => {
            if s.is_empty() {
                return false;
            }
            // Reserve room for the separator if we'd add one.
            let sep_len = if out.is_empty() { 0 } else { 1 };
            let want = out.len() + sep_len + s.len();
            if want <= byte_cap {
                if sep_len == 1 {
                    out.push('\n');
                }
                out.push_str(s);
                false
            } else {
                // Append as much as fits, then signal truncation.
                let remaining = byte_cap.saturating_sub(out.len() + sep_len);
                if sep_len == 1 && remaining > 0 {
                    out.push('\n');
                }
                // Take up to `remaining` bytes from `s`, respecting
                // UTF-8 boundaries (find the largest valid prefix).
                let take = s
                    .char_indices()
                    .take_while(|(i, c)| i + c.len_utf8() <= remaining)
                    .last()
                    .map(|(i, c)| i + c.len_utf8())
                    .unwrap_or(0);
                out.push_str(&s[..take]);
                true
            }
        }
        Value::Array(items) => {
            for item in items {
                if walk(item, out, byte_cap) {
                    return true;
                }
            }
            false
        }
        Value::Object(map) => {
            for (_k, v) in map.iter() {
                if walk(v, out, byte_cap) {
                    return true;
                }
            }
            false
        }
        // Numbers, bools, null contribute nothing scannable.
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn block_threshold_is_zero_point_seven_zero() {
        // Pin the threshold against silent drift. If you bump this,
        // expect a wave of false-positive or false-negative reports.
        assert_eq!(BLOCK_THRESHOLD, 0.70);
    }

    #[test]
    fn scan_byte_cap_is_64_kib() {
        // 64 KiB matches the spec; bumping it widens the CPU footprint
        // of every dispatch call.
        assert_eq!(SCAN_BYTE_CAP, 64 * 1024);
    }

    #[test]
    fn extract_scannable_text_concats_strings_with_newline_sep() {
        // Object with two string fields: both included, newline-joined.
        // Order follows serde_json's BTreeMap-backed Map iteration
        // (alphabetic over keys), so this assertion is stable.
        let v = json!({"a": "hello", "b": "world"});
        let (body, truncated) = extract_scannable_text(&v, 1024);
        assert_eq!(body, "hello\nworld");
        assert!(!truncated);
    }

    #[test]
    fn extract_scannable_text_recurses_into_arrays_and_objects() {
        // Deep-nested string. Recursion must reach it.
        let v = json!({"x": [{"y": "deep"}]});
        let (body, _) = extract_scannable_text(&v, 1024);
        assert_eq!(body, "deep");
    }

    #[test]
    fn extract_scannable_text_ignores_non_string_values() {
        // Numbers, bools, nulls contribute nothing. Empty result.
        let v = json!({"n": 42, "b": true, "z": null});
        let (body, _) = extract_scannable_text(&v, 1024);
        assert_eq!(body, "");
    }

    #[test]
    fn extract_scannable_text_truncates_at_byte_cap() {
        // 100 KiB of 'a's; cap 1024 → exactly 1024 bytes + truncated.
        let big = "a".repeat(100 * 1024);
        let v = json!({"payload": big});
        let (body, truncated) = extract_scannable_text(&v, 1024);
        assert_eq!(body.len(), 1024);
        assert!(truncated, "100 KiB into cap=1024 must report truncated");
    }

    #[test]
    fn extract_scannable_text_under_cap_reports_truncated_false() {
        // 500 bytes into cap 1024: full body returned, not truncated.
        let medium = "a".repeat(500);
        let v = json!({"payload": medium});
        let (body, truncated) = extract_scannable_text(&v, 1024);
        assert_eq!(body.len(), 500);
        assert!(!truncated);
    }

    #[test]
    fn extract_scannable_text_truncates_at_utf8_boundary() {
        // '€' is U+20AC, 3 bytes in UTF-8. With cap = 5, exactly one '€'
        // fits (3 bytes); a second would need 6 bytes total. The returned
        // body must be valid UTF-8 (Rust String guarantee) and the byte
        // length must not exceed the cap.
        let euros = "€€€€";
        let v = json!({"x": euros});
        let (body, truncated) = extract_scannable_text(&v, 5);
        assert!(truncated);
        assert!(body.len() <= 5);
        // One euro sign = 3 bytes; fits in the 5-byte cap.
        assert_eq!(body, "€");
    }

    #[test]
    fn extract_scannable_text_skips_empty_string_separator() {
        // Empty string values do not emit a stray separator. Without the
        // is_empty guard, this would produce "hello\n\nworld".
        let v = json!({"a": "hello", "b": "", "c": "world"});
        let (body, truncated) = extract_scannable_text(&v, 1024);
        assert_eq!(body, "hello\nworld");
        assert!(!truncated);
    }
}
