//! NUL handling for every Postgres write (issues #816, #818).
//!
//! Postgres refuses U+0000 anywhere in a `jsonb` value — string or object
//! key — with `unsupported Unicode escape sequence`, and anywhere in a `text`
//! column as an invalid byte sequence. One NUL fails the **whole statement**,
//! so a worker or peer that can put one into a written string can stop that
//! row from being written. A compromised worker is in scope
//! (`docs/threat-model.md`), so every column that stores text it can
//! influence has to decide, *before* the bind, what a NUL means.
//!
//! ## The rule: escape records, refuse identities
//!
//! * **A record** — a column that says what happened (`audit_log`, a task's
//!   `result` and `turn_record`, an ask's `body` and `resume_state`) — is
//!   **escaped**: each NUL becomes [`NUL_ESCAPE`] (`␀`) and the row lands.
//!   Losing a record is worse than rewriting one character of it, and for
//!   these columns losing the row also strands what depends on it (a task
//!   that never leaves `running`, an escalation that cannot be raised).
//!   Use [`escape_json`] / [`escape_str`].
//! * **An identity** — a column that is *matched* (`pairings.peer`, a task
//!   payload's `peer`/`conversation`, `entities.name_norm`) — is **refused**.
//!   Escaping would be an identity decision: a peer `a\0` stored as `a␀`
//!   would then match a different, real peer `a␀`. Use [`refuse_nul_in_text`]
//!   / [`refuse_nul_in_json`], which fail with [`DbError::NulRefused`] before
//!   any SQL runs, naming the column and never the value.
//! * **Long-lived knowledge** — a `memories` row — is refused too: it is
//!   recalled and replayed later (a crystallised skill's parameters), so a
//!   silently rewritten value would be a different instruction, not a
//!   faithful record.
//!
//! [`crate::audit::nul_escape`] layers the audit row's count marker on top
//! of the escape. The non-audit records add no marker: their columns are
//! read back by code that expects their own shape, so the write site logs
//! the count instead.
//!
//! ⚠️ The escape glyph is U+2400, which needs a UTF8 cluster — see
//! [`crate::InitDbOptions::encoding`].
//!
//! Pure: no I/O, no global state.

use std::borrow::Cow;

use serde_json::{Map, Value};

use crate::DbError;

/// What every NUL in an escaped record is replaced with: U+2400 SYMBOL FOR
/// NULL.
///
/// Not a space (the display-side choice in `core::untrusted_text`, for logs
/// and planner-bound text): a reader needs the control *gone*, but a record
/// needs to keep saying a NUL was there. Not U+FFFD either, which
/// already means "an undecodable byte" — a different fact.
pub const NUL_ESCAPE: char = '\u{2400}';

/// `s` with every NUL replaced by [`NUL_ESCAPE`], and how many there were.
///
/// For `text` record columns (the audit row's `actor` / `action`, an ask's
/// `body`), which refuse NUL just as `jsonb` does.
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

/// `v` with every NUL escaped (see [`escape_value`]), and how many there
/// were. For a **record** column; adds no marker key (see the module doc).
pub fn escape_json(mut v: Value) -> (Value, u64) {
    let count = escape_value(&mut v);
    (v, count)
}

/// [`escape_json`] over an optional column value; `None` stays `None` with
/// a zero count.
pub fn escape_opt_json(v: Option<Value>) -> (Option<Value>, u64) {
    match v {
        Some(v) => {
            let (v, n) = escape_json(v);
            (Some(v), n)
        }
        None => (None, 0),
    }
}

/// True when any string or object key anywhere in `v` holds a NUL.
///
/// A walk, not a substring search of the serialisation: a string holding
/// the six characters `\u0000` serialises with that text in it, so a search
/// would report a NUL that is not there.
pub fn json_contains_nul(v: &Value) -> bool {
    match v {
        Value::String(s) => s.contains('\0'),
        Value::Array(items) => items.iter().any(json_contains_nul),
        Value::Object(map) => map.iter().any(|(k, v)| k.contains('\0') || json_contains_nul(v)),
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

/// Refuse a NUL in an **identity** `text` column, before any SQL runs.
///
/// `column` is the `table.column` the value was bound for; the error names
/// it and deliberately not the value, which may be a hostile peer's id.
pub fn refuse_nul_in_text(column: &'static str, s: &str) -> Result<(), DbError> {
    if s.contains('\0') {
        return Err(DbError::NulRefused { column });
    }
    Ok(())
}

/// [`refuse_nul_in_text`] for a `jsonb` column: any NUL in any string or
/// key refuses the whole value.
pub fn refuse_nul_in_json(column: &'static str, v: &Value) -> Result<(), DbError> {
    if json_contains_nul(v) {
        return Err(DbError::NulRefused { column });
    }
    Ok(())
}

#[cfg(test)]
mod tests;
