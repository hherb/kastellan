//! NUL handling for every Postgres write (issues #816, #818).
//!
//! Postgres refuses U+0000 anywhere in a `jsonb` value — string or object
//! key — with `unsupported Unicode escape sequence`, and anywhere in a `text`
//! column as an invalid byte sequence. One NUL fails the **whole statement**,
//! so a worker or peer that can put one into a written string can stop that
//! row from being written.
//!
//! This module holds the escape that rewrites each NUL to [`NUL_ESCAPE`]
//! (`␀`). [`crate::audit::nul_escape`] layers the audit row's count marker
//! on top of it.
//!
//! Pure: no I/O, no global state.

use std::borrow::Cow;

use serde_json::{Map, Value};

/// What every NUL in an audit row is replaced with: U+2400 SYMBOL FOR NULL.
///
/// Not a space (the display-side choice in `core::untrusted_text`, for logs
/// and planner-bound text): a reader needs the control *gone*, but an audit
/// row needs to keep saying a NUL was there. Not U+FFFD either, which
/// already means "an undecodable byte" — a different fact.
pub const NUL_ESCAPE: char = '\u{2400}';

/// `s` with every NUL replaced by [`NUL_ESCAPE`], and how many there were.
///
/// For `actor` / `action`, which are `text` columns and refuse NUL just as
/// `jsonb` does. They have nowhere to carry a count; the glyph is the record.
///
/// Borrowed when there is nothing to replace, so the common case — every
/// string in a clean, possibly 85 KB tool result — costs a scan and no copy.
pub fn escape_str(s: &str) -> (Cow<'_, str>, u64) {
    let count = count_nuls(s);
    if count == 0 {
        return (Cow::Borrowed(s), 0);
    }
    (Cow::Owned(s.replace('\0', NUL_ESCAPE_STR)), count)
}

/// [`NUL_ESCAPE`] as a `&str`, for `str::replace`.
const NUL_ESCAPE_STR: &str = "\u{2400}";

fn count_nuls(s: &str) -> u64 {
    // A NUL is one byte in UTF-8 and never part of a longer sequence, so a
    // byte count is a character count.
    s.bytes().filter(|&b| b == 0).count() as u64
}

/// Escape `v` in place; return how many NULs were replaced.
pub fn escape_value(v: &mut Value) -> u64 {
    match v {
        Value::String(s) => {
            let (escaped, count) = escape_str(s);
            if count > 0 {
                *s = escaped.into_owned();
            }
            count
        }
        Value::Array(items) => items.iter_mut().map(escape_value).sum(),
        Value::Object(map) => escape_object(map),
        Value::Null | Value::Bool(_) | Value::Number(_) => 0,
    }
}

/// Escape an object's values and keys in place; return the NUL count.
///
/// **Keys can collide once escaped**, and a hostile worker can arrange it:
/// `{"k\0": evil, "k␀": benign}` maps both to `k␀`, and a plain re-insert
/// would silently keep one value. So every key *without* a NUL keeps its
/// spelling, and each escaped key that lands on an existing one gets another
/// [`NUL_ESCAPE`] appended until it is unique. Nothing is overwritten. The
/// appended glyphs replace no NUL, so they are not counted: the count is
/// always the number of NULs the worker sent.
///
/// What is **not** recoverable is which key was renamed: `{"k\0": A,
/// "k␀": B}` and `{"k␀\0": A, "k␀": B}` are stored alike. That is ambiguity
/// in evidence, not loss of it — both values and the true count survive.
fn escape_object(map: &mut Map<String, Value>) -> u64 {
    let mut count: u64 = map.values_mut().map(escape_value).sum();

    // Listed first because the map cannot be re-keyed while it is iterated.
    // One at a time is safe: an escaped key holds no NUL, so it can only
    // land on a key that never had one or on one already re-inserted —
    // never on a NUL key still waiting its turn.
    let nul_keys: Vec<String> = map.keys().filter(|k| k.contains('\0')).cloned().collect();
    for key in nul_keys {
        let value = map.remove(&key).expect("key was just listed from this map");
        let (escaped, n) = escape_str(&key);
        let mut escaped = escaped.into_owned();
        count += n;
        while map.contains_key(&escaped) {
            escaped.push(NUL_ESCAPE);
        }
        map.insert(escaped, value);
    }
    count
}
