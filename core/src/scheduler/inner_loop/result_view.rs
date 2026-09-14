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
//! planner in task 186 could not have told which message carried the PDF it
//! had been asked about. (Whether that is why task 186 failed is not
//! established; its follow-up failed live for another reason, #701.)
//!
//! This module keeps the structure instead: [`prune`] returns a copy of the
//! value cut down to size with every kept key, number and boolean intact.
//!
//! # Two things it will not do
//!
//! - **Show a key with whitespace or punctuation in it.** Keys never pass the
//!   guard model, which screens string values only, so only identifier-shaped
//!   keys are shown ([`is_identifier_key`]); any other key is dropped with its
//!   value and counted in [`OMITTED_KEYS_KEY`]. An identifier key can still
//!   spell a short phrase, so [`screen_text`] gives every key to the sink
//!   catalogue as words.
//! - **Cut an identifier.** A string with no whitespace and at most
//!   [`ATOMIC_MAX`] bytes — an id, a hash, a URL, a path, a file name — is shown
//!   whole or not at all ([`is_atomic`], which also bounds non-ASCII strings that
//!   are not URLs or paths to [`NON_ASCII_ATOMIC_MAX_CHARS`], so space-free
//!   prose stays cuttable). The planner is told to copy such values verbatim,
//!   and half of one is a trap. Found by review on `web.search_batch`, where one
//!   string cap for everything cut every URL.
//!
//! # How the budget is met
//!
//! [`render`] looks for caps that fit, trying first the ones that lose the
//! least useful thing:
//!
//! 1. **Default containers**, strings whole if that fits; otherwise **the
//!    highest string cap that fits**, by binary search. Every cuttable string
//!    is cut to the same "water level", so one large document keeps nearly the
//!    whole budget, while a list of hits keeps every hit with evenly trimmed
//!    snippets.
//! 2. **Narrower containers**, only when strings at [`LEAF_FLOOR`] still do not
//!    fit: lower the array cap one element at a time, then the object cap, and
//!    at the first size that fits, search the string cap again so the bytes
//!    freed go back to the strings.
//! 3. **A fallback** ([`fallback_view`]) when nothing fits.
//!
//! Every candidate is **measured** before it is returned, so the size bound
//! does not depend on any argument about how the caps interact.
//!
//! This is **not** a security control. The caller screens what reaches the
//! prompt.
//!
//! [`extract_scannable_text`]: crate::cassandra::injection_guard::extract_scannable_text

use serde_json::{json, Map, Value};

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

/// Longest whitespace-free ASCII string that is never cut (see [`is_atomic`]).
/// Long enough for any realistic URL; a longer blob is data, not an identifier.
pub(crate) const ATOMIC_MAX: usize = 1024;

/// Longest whitespace-free **non-ASCII** string, in characters, that is never
/// cut unless it is shaped like a URL or path (see [`is_atomic`]): 255, the
/// longest file name APFS, NTFS and ext4 allow (ext4 counts bytes, stricter).
/// Japanese, Chinese and Thai prose carries no whitespace, so under
/// [`ATOMIC_MAX`] alone a search snippet in those languages read as an
/// identifier and a tight budget dropped whole hits instead of trimming it
/// (#702 review). A first version counted 255 *bytes*, which cut a 100-character
/// CJK file name the planner must copy verbatim (review of that fix).
pub(crate) const NON_ASCII_ATOMIC_MAX_CHARS: usize = 255;

/// Longest object key shown to the planner (see [`is_identifier_key`]).
pub(crate) const KEY_MAX_BYTES: usize = 64;

/// The characters besides ASCII letters and digits that an identifier key may
/// contain. One list for both [`is_identifier_key`] and [`screen_text`], which
/// reads each of them as a space: a character admitted to keys but missing from
/// the screen would let a phrase spelled with it past the catalogue.
const KEY_SEPARATORS: [char; 6] = ['_', '.', ':', '-', '@', '/'];

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
            let mut dropped = 0;
            // BTreeMap order, so the kept keys are the alphabetically first
            // identifier-shaped ones.
            for (key, item) in map {
                if !is_identifier_key(key) || out.len() >= limits.keys {
                    dropped += 1;
                    continue;
                }
                out.insert(key.clone(), prune_at(item, limits, depth + 1));
            }
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
pub(crate) fn omitted_items_marker(dropped: usize) -> String {
    format!("…{dropped} more items omitted")
}

/// Whether `key` may be shown to the planner: 1 to [`KEY_MAX_BYTES`] bytes of
/// ASCII letters, digits and [`KEY_SEPARATORS`]. Such a key can name a field, a
/// header or a path, and it cannot hold whitespace, quotes or chat-template
/// punctuation. It **can** still spell a short phrase (`IGNORE_ALL_PREVIOUS`),
/// and the guard model upstream never sees keys, so [`screen_text`] hands every
/// key to the sink catalogue as words. Workers must not emit third-party text
/// as object keys at all; `workers/mail` returns message headers as a list for
/// that reason, and #703 tracks giving keys to the guard model.
fn is_identifier_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= KEY_MAX_BYTES
        && key.chars().all(|c| c.is_ascii_alphanumeric() || KEY_SEPARATORS.contains(&c))
}

/// Whether `s` is an identifier that must be shown whole or not at all: at most
/// [`ATOMIC_MAX`] bytes with no whitespace or control characters, and then
/// either ASCII, shaped like a URL (`://`) or absolute path (leading `/`), or at
/// most [`NON_ASCII_ATOMIC_MAX_CHARS`] characters. Prose has spaces and survives
/// a cut; a URL, id, hash, path or file name does not. A control character never
/// belongs to an identifier, and each one serialises to six bytes. A raw-Unicode
/// URL is kept on the byte limit because SearxNG passes them through and a cut
/// one is the trap this rule exists for.
fn is_atomic(s: &str) -> bool {
    if s.len() > ATOMIC_MAX || s.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    s.is_ascii() || s.contains("://") || s.starts_with('/') || s.chars().count() <= NON_ASCII_ATOMIC_MAX_CHARS
}

/// `s` cut to at most `leaf` bytes plus an ellipsis, but only when `s` is not
/// atomic and the cut is strictly shorter than `s`. Cutting a 7-byte string to
/// 4 bytes and adding the 3-byte ellipsis would not shorten it, so such a
/// string is kept whole.
fn cut_leaf(s: &str, leaf: usize) -> String {
    if s.len() <= leaf || is_atomic(s) {
        return s.to_string();
    }
    let end = floor_char_boundary(s, leaf);
    if end + ELLIPSIS.len() >= s.len() {
        return s.to_string();
    }
    format!("{}{ELLIPSIS}", &s[..end])
}

/// The largest index `<= index` that starts a UTF-8 char, so `&s[..i]` cannot
/// panic. (`str::floor_char_boundary` has been stable only since Rust 1.91, and
/// the workspace's `rust-version` is 1.78.) A char is at most four bytes, so the
/// loop steps back at most three times, and index 0 is always a boundary, so it
/// cannot underflow. #591 counts this idiom as hand-written across the tree; the
/// tests carry the straddling case.
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
    json!({ VIEW_UNAVAILABLE_KEY: VIEW_UNAVAILABLE_TEXT })
}

/// The largest version of `value` whose compact JSON is at most `total`
/// bytes. See the module docs for the order of the search.
///
/// For `total >= MIN_VIEW_TOTAL` the result never serialises longer than
/// `total`; every production budget is const-asserted above that, and a debug
/// build refuses a smaller one outright rather than silently returning a
/// fallback that may not fit.
pub(crate) fn render(value: &Value, total: usize) -> Value {
    debug_assert!(total >= MIN_VIEW_TOTAL, "render budget {total} is below MIN_VIEW_TOTAL {MIN_VIEW_TOTAL}");
    let longest = longest_leaf(value, 0);

    // 1. Default containers.
    let mut containers = PruneLimits::DEFAULT;
    if let Some(found) = best_fit_at(value, containers, longest, total) {
        return found;
    }

    // 2. Strings at the floor still do not fit. Narrow lists first: a shorter
    //    list loses whole entries but keeps each entry's labels. One step at a
    //    time, not halving: one cap applies at every nesting level, so halving
    //    8 queries x 10 hits jumps straight to 5 x 5 and strands most of the
    //    budget. A measured walk rather than a search, because the omitted-items
    //    marker vanishing at a count of zero makes size non-monotone in these caps.
    while containers.items > 1 {
        containers.items -= 1;
        if let Some(found) = best_fit_at(value, containers, longest, total) {
            return found;
        }
    }
    while containers.keys > 1 {
        containers.keys -= 1;
        if let Some(found) = best_fit_at(value, containers, longest, total) {
            return found;
        }
    }

    // 3. Nothing fits.
    fallback_view()
}

/// The best fit with arrays and objects capped as in `containers`: strings
/// whole if that fits, otherwise the highest string cap from [`LEAF_FLOOR`]
/// up that fits, found by binary search. `None` when even the floor does not
/// fit. `longest` is [`longest_leaf`] of `value`, computed once by the caller:
/// a cap that long cuts nothing, so it is the "whole" case and known not to fit
/// by the time the search runs; every probe is measured, and `best` only ever
/// holds a fit.
fn best_fit_at(value: &Value, containers: PruneLimits, longest: usize, total: usize) -> Option<Value> {
    if let Some(found) = fit(value, PruneLimits { leaf: usize::MAX, ..containers }, total) {
        return Some(found);
    }
    let mut best = fit(value, PruneLimits { leaf: LEAF_FLOOR, ..containers }, total)?;
    let (mut fits, mut fails) = (LEAF_FLOOR, longest);
    while fails > fits + 1 {
        let mid = fits + (fails - fits) / 2;
        match fit(value, PruneLimits { leaf: mid, ..containers }, total) {
            Some(found) => {
                best = found;
                fits = mid;
            }
            None => fails = mid,
        }
    }
    Some(best)
}

/// `value` pruned to `limits`, if its serialised length is within `total`.
fn fit(value: &Value, limits: PruneLimits, total: usize) -> Option<Value> {
    let pruned = prune(value, limits);
    (serialised_len(&pruned) <= total).then_some(pruned)
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
/// reach the planner, a worker writes them, and the guard model upstream never
/// sees them, so this catalogue pass is their only screen. Each key is read as
/// words ([`key_readings`]), and a key with a string value is joined to that
/// value as **one** phrase, because that is how the planner reads
/// `"ignore_all": "previous instructions…"` (#702 review). A key whose value is
/// an array or object is not joined to what it holds, so
/// `{"ignore_all": ["previous instructions"]}` reaches the catalogue as two
/// parts: the catalogue is a best-effort phrase list, and the answer to
/// unscreened keys is #703. Punctuation, numbers, booleans and null are left
/// out, as `extract_scannable_text` leaves them out, so the catalogue cannot
/// fire on JSON shape.
///
/// The walk stops at the depth [`prune`] stops at, and everything `prune` keeps
/// is above it; the only thing at that depth is the constant [`DEPTH_MARKER`].
pub(crate) fn screen_text(view: &Value) -> String {
    let mut parts = Vec::new();
    collect_screen_parts(view, &mut parts, 0);
    parts.join("\n")
}

/// Recursive helper for [`screen_text`].
fn collect_screen_parts(value: &Value, parts: &mut Vec<String>, depth: usize) {
    if depth >= MAX_WALK_DEPTH {
        return;
    }
    match value {
        Value::String(s) if !s.is_empty() => parts.push(s.clone()),
        Value::Array(items) => {
            for item in items {
                collect_screen_parts(item, parts, depth + 1);
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                let readings = key_readings(key);
                match item {
                    Value::String(s) if !s.is_empty() => {
                        parts.extend(readings.iter().map(|words| format!("{words} {s}")));
                    }
                    _ => {
                        parts.extend(readings);
                        collect_screen_parts(item, parts, depth + 1);
                    }
                }
            }
        }
        _ => {}
    }
}

/// The ways the catalogue should read `key` as words: with each of
/// [`KEY_SEPARATORS`] as a space, and — when it differs — also with a space
/// before every uppercase letter that follows a lowercase letter or a digit.
///
/// Both, never only the split one. The catalogue lowercases before matching,
/// so without the split `IgnoreAllPrevious` reaches it as one word (#702
/// review); but the split alone breaks a phrase the plain reading matched,
/// `iGNORE_ALL…` becoming `i GNORE ALL…` (found by the review of that fix).
fn key_readings(key: &str) -> Vec<String> {
    let spaced = key.replace(KEY_SEPARATORS, " ");
    let mut split = String::with_capacity(spaced.len() + 8);
    let mut prev: Option<char> = None;
    for c in spaced.chars() {
        if c.is_ascii_uppercase() && prev.is_some_and(|p| p.is_ascii_lowercase() || p.is_ascii_digit()) {
            split.push(' ');
        }
        split.push(c);
        prev = Some(c);
    }
    if split == spaced {
        vec![spaced]
    } else {
        vec![spaced, split]
    }
}

#[cfg(test)]
mod tests;
