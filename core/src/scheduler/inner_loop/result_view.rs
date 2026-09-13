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
//! # How the budget is met
//!
//! [`render`] looks for caps that fit, trying first the ones that lose the
//! least useful thing:
//!
//! 1. **Containers only.** Default array and object caps, strings whole.
//! 2. **The highest string cap that fits**, by binary search. Every string is
//!    cut to the same "water level", so one large document keeps nearly the
//!    whole budget, while a list of hits keeps every hit with evenly trimmed
//!    snippets.
//! 3. **Narrower containers**, strings at [`LEAF_FLOOR`]: halve the array cap,
//!    then the object cap.
//! 4. **A fallback** ([`fallback_view`]) when nothing fits.
//!
//! Every candidate is **measured** before it is returned, so the size bound
//! does not depend on any argument about how the caps interact.
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

/// Key of the object [`render`] returns when nothing fits.
pub(crate) const VIEW_UNAVAILABLE_KEY: &str = "_view_unavailable";

/// Value of [`VIEW_UNAVAILABLE_KEY`].
const VIEW_UNAVAILABLE_TEXT: &str = "result too large to summarise within budget";

/// The smallest `total` for which [`render`] guarantees its size bound. The
/// fallback serialises to 67 bytes. `summary.rs` pins its budgets above this
/// with a module-level `const` assertion.
pub(crate) const MIN_VIEW_TOTAL: usize = 128;

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

/// Byte length of `value`'s compact JSON, the form the planner prompt embeds.
/// A `Value` always serialises (its keys are strings); if that ever changed,
/// the failure measures as `usize::MAX`, which never "fits".
pub(crate) fn serialised_len(value: &Value) -> usize {
    serde_json::to_string(value).map_or(usize::MAX, |s| s.len())
}

/// The object [`render`] returns when no pruning fits the budget.
pub(crate) fn fallback_view() -> Value {
    let mut map = Map::new();
    map.insert(
        VIEW_UNAVAILABLE_KEY.to_string(),
        Value::String(VIEW_UNAVAILABLE_TEXT.to_string()),
    );
    Value::Object(map)
}

/// The largest version of `value` whose compact JSON is at most `total`
/// bytes, plus that length. See the module docs for the order of the search.
///
/// For `total >= MIN_VIEW_TOTAL` the returned length never exceeds `total`.
/// Below that, the fallback is returned even if it does not fit.
pub(crate) fn render(value: &Value, total: usize) -> (Value, usize) {
    // 1. Containers only.
    let full = PruneLimits::DEFAULT;
    if let Some(found) = fit(value, full, total) {
        return found;
    }

    // 2. The highest string cap that fits. A cap of `longest_leaf` cuts
    //    nothing, so it is step 1 again and known not to fit. Each probe is
    //    measured, and `best` only ever holds a fit.
    let at_floor = PruneLimits { leaf: LEAF_FLOOR, ..full };
    if let Some(mut best) = fit(value, at_floor, total) {
        let (mut fits, mut fails) = (LEAF_FLOOR, longest_leaf(value, 0));
        while fails > fits + 1 {
            let mid = fits + (fails - fits) / 2;
            match fit(value, PruneLimits { leaf: mid, ..full }, total) {
                Some(found) => {
                    best = found;
                    fits = mid;
                }
                None => fails = mid,
            }
        }
        return best;
    }

    // 3. Strings at the floor still do not fit. Narrow lists first: a shorter
    //    list loses whole entries but keeps each entry's labels. A linear,
    //    measured walk rather than a search, because the omitted-items marker
    //    vanishing at a count of zero makes size non-monotone in these caps.
    let mut limits = at_floor;
    while limits.items > 1 {
        limits.items /= 2;
        if let Some(found) = fit(value, limits, total) {
            return found;
        }
    }
    while limits.keys > 1 {
        limits.keys /= 2;
        if let Some(found) = fit(value, limits, total) {
            return found;
        }
    }

    // 4. Nothing fits.
    let fallback = fallback_view();
    let len = serialised_len(&fallback);
    (fallback, len)
}

/// `value` pruned to `limits`, with its length, if that length is within `total`.
fn fit(value: &Value, limits: PruneLimits, total: usize) -> Option<(Value, usize)> {
    let pruned = prune(value, limits);
    let len = serialised_len(&pruned);
    (len <= total).then_some((pruned, len))
}

/// Byte length of the longest string in `value`, looking no deeper than
/// [`MAX_WALK_DEPTH`] (anything deeper is replaced by [`DEPTH_MARKER`] anyway).
fn longest_leaf(value: &Value, depth: usize) -> usize {
    if depth >= MAX_WALK_DEPTH {
        return 0;
    }
    match value {
        Value::String(s) => s.len(),
        Value::Array(items) => items.iter().map(|v| longest_leaf(v, depth + 1)).max().unwrap_or(0),
        Value::Object(map) => map.values().map(|v| longest_leaf(v, depth + 1)).max().unwrap_or(0),
        _ => 0,
    }
}

/// The text the sink screen checks for a view: every object key and every
/// non-empty string, newline-separated.
///
/// Keys are included because, unlike in the old flattened view, they now
/// reach the planner, and a worker writes them. Punctuation, numbers,
/// booleans and null are left out, as `extract_scannable_text` leaves them
/// out, so the catalogue cannot fire on JSON shape.
pub(crate) fn screen_text(view: &Value) -> String {
    let mut parts = Vec::new();
    collect_screen_parts(view, &mut parts, 0);
    parts.join("\n")
}

/// Recursive helper for [`screen_text`].
fn collect_screen_parts<'a>(value: &'a Value, parts: &mut Vec<&'a str>, depth: usize) {
    if depth >= MAX_WALK_DEPTH {
        return;
    }
    match value {
        Value::String(s) if !s.is_empty() => parts.push(s),
        Value::Array(items) => {
            for item in items {
                collect_screen_parts(item, parts, depth + 1);
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                parts.push(key);
                collect_screen_parts(item, parts, depth + 1);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests;
