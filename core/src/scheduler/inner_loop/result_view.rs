//! The planner's view of a successful tool result (#677).
//!
//! # Why this exists
//!
//! Until #677 a successful step reached the planner as
//! [`extract_scannable_text`]'s output. That function flattens a JSON value
//! for the **injection guard**: it keeps string leaves and discards every
//! object key, every number and every boolean, so the catalogue cannot fire
//! on JSON shape. For the guard that is right. As the planner's only view of
//! a result it was wrong. A live `mail.search` hit carries `has_attachments`
//! as a boolean, which vanished, and `message_id` as a string that survived
//! only as a bare line, indistinguishable from the account id beside it. The
//! planner in task 186 therefore could not tell which message carried the PDF
//! it had been asked about, and never called `mail.get_attachment_text`.
//!
//! This module keeps the structure instead: [`prune`] returns a copy of the
//! value cut down to size with every kept key, number and boolean intact.
//!
//! This is **not** a security control. The caller screens what reaches the
//! prompt.
//!
//! [`extract_scannable_text`]: crate::cassandra::injection_guard::extract_scannable_text

use serde_json::{Map, Value};

use crate::cassandra::injection_guard::MAX_WALK_DEPTH;

/// The shortest a string is cut to while the budget search still prefers
/// shorter strings over dropping list elements or object fields. Long enough
/// to keep an id, a date, or the head of a subject line.
pub(crate) const LEAF_FLOOR: usize = 64;

/// Elements kept per array before any tightening. Above a typical
/// `limit: 10`, so an ordinary listing is never shortened.
pub(crate) const DEFAULT_ITEMS: usize = 20;

/// Keys kept per object before any tightening. Tool results are records whose
/// fields all potentially matter and rarely number more than a few dozen, so
/// this bounds a pathological map without touching a real record.
pub(crate) const DEFAULT_KEYS: usize = 64;

/// Key added to an object that lost fields; its value is how many it lost.
pub(crate) const OMITTED_KEYS_KEY: &str = "_omitted_keys";

/// Replaces anything nested [`MAX_WALK_DEPTH`] levels deep, for the same
/// reason the injection guard stops there (#143): a pathologically deep value
/// must not overflow the dispatcher thread's stack.
pub(crate) const DEPTH_MARKER: &str = "…nested too deeply to show";

/// Appended to a string that was cut.
const ELLIPSIS: &str = "…";

/// How far one [`prune`] pass cuts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PruneLimits {
    /// Most bytes kept of any string; `usize::MAX` keeps every string whole.
    pub(crate) leaf: usize,
    /// Most elements kept of any array.
    pub(crate) items: usize,
    /// Most keys kept of any object.
    pub(crate) keys: usize,
}

impl PruneLimits {
    /// Containers capped, strings whole.
    pub(crate) const DEFAULT: Self =
        Self { leaf: usize::MAX, items: DEFAULT_ITEMS, keys: DEFAULT_KEYS };
}

/// A copy of `value` cut down to `limits`, with every kept key, number,
/// boolean and null unchanged. Pure and deterministic.
pub(crate) fn prune(value: &Value, limits: PruneLimits) -> Value {
    prune_at(value, limits, 0)
}

/// [`prune`] at nesting level `depth`, which is 0 for the top-level value.
fn prune_at(value: &Value, limits: PruneLimits, depth: usize) -> Value {
    if depth >= MAX_WALK_DEPTH {
        return Value::String(DEPTH_MARKER.to_string());
    }
    match value {
        Value::String(s) => Value::String(cut_leaf(s, limits.leaf)),
        Value::Array(items) => {
            let mut out: Vec<Value> = items
                .iter()
                .take(limits.items)
                .map(|item| prune_at(item, limits, depth + 1))
                .collect();
            let dropped = items.len().saturating_sub(limits.items);
            if dropped > 0 {
                // A marker ELEMENT rather than a wrapper object, so an array
                // stays an array: the planner must emit parameters matching the
                // tool's real schema, and a reshaped list would teach it a wrong one.
                out.push(Value::String(omitted_items_marker(dropped)));
            }
            Value::Array(out)
        }
        Value::Object(map) => {
            let mut out = Map::new();
            // BTreeMap order, so the kept keys are the alphabetically first.
            for (key, item) in map.iter().take(limits.keys) {
                out.insert(key.clone(), prune_at(item, limits, depth + 1));
            }
            let dropped = map.len().saturating_sub(limits.keys);
            if dropped > 0 {
                // A worker's own `_omitted_keys` field, if it sent one, is
                // overwritten. That can only mislabel a count, never reveal
                // anything, so it does not justify a renaming scheme.
                out.insert(OMITTED_KEYS_KEY.to_string(), Value::from(dropped));
            }
            Value::Object(out)
        }
        // Numbers, booleans and null: tiny, and exactly what the keys label.
        scalar => scalar.clone(),
    }
}

/// The element appended to an array that lost `dropped` elements.
fn omitted_items_marker(dropped: usize) -> String {
    format!("…{dropped} more items omitted")
}

/// `s` cut to at most `leaf` bytes plus an ellipsis, but only when that is
/// strictly shorter than `s`. Cutting a 6-byte string to 4 bytes and adding
/// the 3-byte ellipsis would make it longer, so such a string is kept whole.
fn cut_leaf(s: &str, leaf: usize) -> String {
    if s.len() <= leaf {
        return s.to_string();
    }
    let end = floor_char_boundary(s, leaf);
    if end + ELLIPSIS.len() >= s.len() {
        return s.to_string();
    }
    format!("{}{ELLIPSIS}", &s[..end])
}

/// The largest index `<= index` that starts a UTF-8 char, so `&s[..i]` cannot
/// panic. (`str::floor_char_boundary` is still unstable.) A char is at most
/// four bytes, so the loop steps back at most three times, and index 0 is
/// always a boundary, so it cannot underflow. #591 counts this idiom as
/// hand-written across the tree; the tests carry the straddling case.
fn floor_char_boundary(s: &str, index: usize) -> usize {
    let mut i = index.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests;
