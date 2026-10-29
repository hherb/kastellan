//! [`screened_head`]: a small head of a JSON value, drawn only from text the
//! output screen read.
//!
//! # Why this exists
//!
//! `tool_host`'s output screen reads [`extract_scannable_text`] at
//! [`SCAN_BYTE_CAP`](super::SCAN_BYTE_CAP): the string leaves, in walk order,
//! up to 64 KiB. A result bigger than that is stashed behind a handoff
//! placeholder, and the planner reads a head of it (#705). The planner's view
//! trims strings evenly across whatever it is given, so given the whole result
//! it would show text from past that window, which neither the catalogue nor
//! the guard model ever saw. Given this head instead, every string it can show
//! lies inside the screened text.
//!
//! The head is raw, not rendered: the planner summary renders every step
//! result exactly once, and a second render garbles the first one's omission
//! markers (found by review of #705's first version).
//!
//! The walk mirrors `walk` in the parent module (same order, same depth rule,
//! same UTF-8-safe cut), but charges **more** to the budget: every key's bytes
//! and one byte per number, boolean, null, empty string and container, on top
//! of the strings and separators `walk` counts. That bounds the head's size and
//! the work spent on it whatever the result's shape, and it only ever stops the
//! walk earlier than `walk` would, so the head's text is always a prefix of the
//! text the screen read at the same cap. The tests pin that property.

use serde_json::{Map, Value};

use super::MAX_WALK_DEPTH;
#[cfg(doc)]
use super::extract_scannable_text;

/// A head of `value` in walk order whose total charge (see the module docs) is
/// at most `byte_cap`. The string that crosses the cap is cut on a character
/// boundary and nothing after it is kept. A subtree at [`MAX_WALK_DEPTH`],
/// which the screen skips, becomes `null`. Pure and deterministic.
///
/// The head's text, as [`extract_scannable_text`] reads it, is a prefix of
/// `extract_scannable_text(value, cap)` for any `cap >= byte_cap`.
pub fn screened_head(value: &Value, byte_cap: usize) -> Value {
    let mut used = 0;
    head_at(value, byte_cap, 0, &mut used).0.unwrap_or(Value::Null)
}

/// [`screened_head`] at nesting level `depth`; `used` is the charge so far.
/// Returns the kept part of `value` (`None` when none of it fits) and whether
/// the budget ran out, which ends the whole walk.
fn head_at(value: &Value, cap: usize, depth: usize, used: &mut usize) -> (Option<Value>, bool) {
    if depth >= MAX_WALK_DEPTH {
        // The screen skipped this subtree, so nothing in it may be shown.
        return charge_node(Value::Null, cap, used);
    }
    match value {
        Value::String(s) if !s.is_empty() => {
            // `walk` puts a newline before every string but the first. Charging
            // it whenever anything came before is at least as much.
            let sep = usize::from(*used > 0);
            if *used + sep + s.len() <= cap {
                *used += sep + s.len();
                return (Some(value.clone()), false);
            }
            let mut take = cap.saturating_sub(*used + sep).min(s.len());
            while !s.is_char_boundary(take) {
                take -= 1;
            }
            *used = cap;
            ((take > 0).then(|| Value::String(s[..take].to_string())), true)
        }
        Value::Array(items) => {
            if let (None, stop) = charge_node(Value::Null, cap, used) {
                return (None, stop);
            }
            let mut out = Vec::new();
            for item in items {
                let (kept, stop) = head_at(item, cap, depth + 1, used);
                out.extend(kept);
                if stop {
                    return (Some(Value::Array(out)), true);
                }
            }
            (Some(Value::Array(out)), false)
        }
        Value::Object(map) => {
            if let (None, stop) = charge_node(Value::Null, cap, used) {
                return (None, stop);
            }
            let mut out = Map::new();
            for (key, item) in map {
                if *used + key.len() > cap {
                    return (Some(Value::Object(out)), true);
                }
                *used += key.len();
                let (kept, stop) = head_at(item, cap, depth + 1, used);
                if let Some(kept) = kept {
                    out.insert(key.clone(), kept);
                }
                if stop {
                    return (Some(Value::Object(out)), true);
                }
            }
            (Some(Value::Object(out)), false)
        }
        // Empty strings, numbers, booleans, null: no text, one byte of budget.
        other => charge_node(other.clone(), cap, used),
    }
}

/// Charge one byte for a node that holds no text, keeping `node` if it fits.
/// The byte is what bounds a result of a million numbers or empty arrays.
fn charge_node(node: Value, cap: usize, used: &mut usize) -> (Option<Value>, bool) {
    if *used < cap {
        *used += 1;
        (Some(node), false)
    } else {
        (None, true)
    }
}

#[cfg(test)]
mod tests {
    use super::super::extract_scannable_text;
    use super::*;
    use serde_json::json;

    /// The property the module rests on: the screen, run uncapped over the
    /// head, reads a prefix of what it read from the original at the cap.
    fn assert_inside_the_screened_text(v: &Value, cap: usize) {
        let (screened, _) = extract_scannable_text(v, cap);
        let (shown, _) = extract_scannable_text(&screened_head(v, cap), usize::MAX);
        assert!(screened.starts_with(&shown), "cap {cap}: showed {shown:?}, screened {screened:?}");
    }

    /// A value deep enough that `walk` skips its innermost string.
    fn nested(depth: usize, leaf: Value) -> Value {
        (0..depth).fold(leaf, |inner, _| json!([inner]))
    }

    #[test]
    fn the_head_shows_only_text_the_screen_read() {
        let values = [
            json!("plain"),
            json!({"a": "first", "b": ["second", "", 3, true, null, {"c": "third"}], "d": "fourth"}),
            json!(["aé€😀", "tail"]),
            // A cut after a separator, before a multi-byte character (review).
            json!(["ab", "é"]),
            json!({"a": nested(MAX_WALK_DEPTH + 2, json!("too deep")), "b": "after the deep one"}),
        ];
        for v in &values {
            let full = serde_json::to_string(v).unwrap().len();
            for cap in 0..=full + 2 {
                assert_inside_the_screened_text(v, cap);
            }
        }
    }

    #[test]
    fn nothing_after_the_cap_survives() {
        // Charge: 1 (object) + 1 ("a") + 1 (separator) + 4 ("xxxx") = 7.
        let v = json!({"a": "x".repeat(10), "b": "never screened", "c": 7});
        assert_eq!(screened_head(&v, 7), json!({"a": "xxxx"}));
    }

    #[test]
    fn a_cut_never_splits_a_character() {
        // Charge: 1 (array) + 1 ("a") leaves 1 byte, and "é" is two.
        assert_eq!(screened_head(&json!(["aé"]), 3), json!(["a"]));
    }

    #[test]
    fn a_skipped_deep_subtree_becomes_null() {
        let deep = nested(MAX_WALK_DEPTH + 1, json!("never screened"));
        assert!(!screened_head(&deep, usize::MAX).to_string().contains("never screened"));
    }

    #[test]
    fn a_result_with_no_text_is_still_bounded() {
        // A million numbers would pass the screen's text budget whole.
        let head = screened_head(&json!(vec![0; 1_000_000]), 100);
        assert_eq!(head.as_array().unwrap().len(), 99, "the array costs one byte, each number one");
        let long_key = json!({ "k".repeat(1_000_000): 1, "a": 2 });
        assert_eq!(screened_head(&long_key, 100), json!({"a": 2}));
    }

    #[test]
    fn under_the_cap_the_value_is_unchanged() {
        let v = json!({"message_id": "3327", "has_attachments": true, "subject": "Itinerary"});
        assert_eq!(screened_head(&v, usize::MAX), v);
    }
}
