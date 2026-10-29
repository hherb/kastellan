//! [`scanned_prefix`]: the part of a JSON value the output screen actually read.
//!
//! # Why this exists
//!
//! `tool_host`'s output screen reads [`extract_scannable_text`] at
//! [`SCAN_BYTE_CAP`](super::SCAN_BYTE_CAP): the string leaves, in walk order,
//! up to 64 KiB. A result bigger than that is stashed behind a handoff
//! placeholder whose head the planner reads (#705). A head built from the
//! **whole** result could show text from past that window, which neither the
//! catalogue nor the guard model ever saw. Building it from `scanned_prefix`
//! instead keeps every string the planner sees inside the screened text.
//!
//! The walk here mirrors `walk` in the parent module step for step (same
//! order, same newline accounting, same depth rule, same UTF-8-safe cut). The
//! tests pin that: `extract_scannable_text` of the prefix, uncapped, is exactly
//! `extract_scannable_text` of the original at the cap.

use serde_json::{Map, Value};

use super::MAX_WALK_DEPTH;
#[cfg(doc)]
use super::extract_scannable_text;

/// A copy of `value` holding only what [`extract_scannable_text`] at
/// `byte_cap` reads. Keys, numbers, booleans and null up to that point are
/// kept (the screen skips them, they cost nothing); the string that crosses the
/// cap is cut where the screen stopped reading; everything after it is dropped.
/// A subtree at [`MAX_WALK_DEPTH`], which the screen skips, becomes `null`.
/// Pure and deterministic.
pub fn scanned_prefix(value: &Value, byte_cap: usize) -> Value {
    let mut used = 0;
    prefix_at(value, byte_cap, 0, &mut used).0.unwrap_or(Value::Null)
}

/// [`scanned_prefix`] at nesting level `depth`. `used` is the length the
/// screen's text has reached so far. Returns the kept part of `value` (`None`
/// when nothing of it was read) and whether the cap was hit, which ends the
/// whole walk, exactly as `walk`'s return value does.
fn prefix_at(value: &Value, cap: usize, depth: usize, used: &mut usize) -> (Option<Value>, bool) {
    if depth >= MAX_WALK_DEPTH {
        // The screen skipped this subtree, so nothing in it may be shown.
        return (Some(Value::Null), false);
    }
    match value {
        Value::String(s) if !s.is_empty() => {
            // A newline separates every string after the first.
            let sep = usize::from(*used > 0);
            if *used + sep + s.len() <= cap {
                *used += sep + s.len();
                return (Some(value.clone()), false);
            }
            let remaining = cap.saturating_sub(*used + sep);
            let mut take = remaining.min(s.len());
            while !s.is_char_boundary(take) {
                take -= 1;
            }
            let kept = (take > 0).then(|| Value::String(s[..take].to_string()));
            (kept, true)
        }
        Value::Array(items) => {
            let mut out = Vec::new();
            for item in items {
                let (kept, stop) = prefix_at(item, cap, depth + 1, used);
                out.extend(kept);
                if stop {
                    return (Some(Value::Array(out)), true);
                }
            }
            (Some(Value::Array(out)), false)
        }
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, item) in map {
                let (kept, stop) = prefix_at(item, cap, depth + 1, used);
                if let Some(kept) = kept {
                    out.insert(key.clone(), kept);
                }
                if stop {
                    return (Some(Value::Object(out)), true);
                }
            }
            (Some(Value::Object(out)), false)
        }
        // Empty strings, numbers, booleans, null: the screen reads none of them.
        other => (Some(other.clone()), false),
    }
}

#[cfg(test)]
mod tests {
    use super::super::extract_scannable_text;
    use super::*;
    use serde_json::json;

    /// The property the module rests on: the screen, run uncapped over the
    /// prefix, reads exactly what it read from the original at the cap.
    fn assert_mirrors_the_screen(v: &Value, cap: usize) {
        let (screened, _) = extract_scannable_text(v, cap);
        let (shown, _) = extract_scannable_text(&scanned_prefix(v, cap), usize::MAX);
        assert_eq!(shown, screened, "cap {cap}, value {v}");
    }

    /// A value deep enough that `walk` skips its innermost string.
    fn nested(depth: usize, leaf: Value) -> Value {
        (0..depth).fold(leaf, |inner, _| json!([inner]))
    }

    #[test]
    fn the_prefix_reads_exactly_what_the_screen_read() {
        let values = [
            json!("plain"),
            json!({"a": "first", "b": ["second", "", 3, true, null, {"c": "third"}], "d": "fourth"}),
            json!(["aé€😀", "tail"]),
            json!({"a": nested(MAX_WALK_DEPTH + 2, json!("too deep")), "b": "after the deep one"}),
        ];
        for v in &values {
            let full = extract_scannable_text(v, usize::MAX).0.len();
            for cap in 0..=full + 2 {
                assert_mirrors_the_screen(v, cap);
            }
        }
    }

    #[test]
    fn nothing_after_the_cap_survives() {
        let v = json!({"a": "x".repeat(10), "b": "never screened", "c": 7});
        assert_eq!(scanned_prefix(&v, 4), json!({"a": "xxxx"}));
    }

    #[test]
    fn a_cut_never_splits_a_character() {
        // "é" is two bytes: a cap landing between them keeps only "a".
        assert_eq!(scanned_prefix(&json!(["aé"]), 2), json!(["a"]));
    }

    #[test]
    fn a_skipped_deep_subtree_becomes_null() {
        let deep = nested(MAX_WALK_DEPTH + 1, json!("never screened"));
        assert!(!scanned_prefix(&deep, usize::MAX).to_string().contains("never screened"));
    }

    #[test]
    fn under_the_cap_the_value_is_unchanged() {
        let v = json!({"message_id": "3327", "has_attachments": true, "subject": "Itinerary"});
        assert_eq!(scanned_prefix(&v, usize::MAX), v);
    }
}
