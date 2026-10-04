//! NUL escaping for audit rows (issue #816).
//!
//! ## Why this exists
//!
//! Postgres refuses U+0000 anywhere in a `jsonb` value — string or object
//! key — with `unsupported Unicode escape sequence`, and anywhere in a `text`
//! column as an invalid byte sequence. The audit insert is one statement, so
//! a single NUL anywhere in a row fails the **whole row**.
//!
//! Audit payloads carry worker-written strings: a channel peer id, a skipped
//! message id, a tool's entire result. A compromised worker is in scope
//! (`docs/threat-model.md`), so before #816 a hostile Matrix peer could put a
//! NUL in its own id and every row about it — `rejected_unpaired`,
//! `channel.received`, … — would fail to insert. The peer erased its own
//! audit trail, and since #808 it also bought one `[audit-lost]` line per
//! attempt, unthinned.
//!
//! ## What it does
//!
//! Every NUL becomes [`NUL_ESCAPE`] (U+2400 `␀`, SYMBOL FOR NULL — the
//! Unicode glyph that exists to *depict* a NUL). Replaced, never deleted:
//! deleting would join the characters on either side into a string the
//! worker never sent, and an audit row is evidence.
//!
//! Because a hostile worker can also send a literal `␀`, the glyph alone
//! cannot say which characters were rewritten. So a payload object also
//! gets [`NUL_ESCAPED_KEY`], the number of NULs replaced. An **object**
//! payload with that key was rewritten and says by how much; one without it
//! (and with no `_dropped_preserved` naming it) had no NUL in its payload.
//! Three things say so only through the glyph: `actor` and `action`, which
//! have no payload slot; a non-object payload, which has nowhere to put a
//! key; and which of several colliding keys was renamed (see
//! `escape_object`).
//!
//! [`escape_payload`] is the step [`super::truncate_payload`] runs **first**,
//! so the cap, the fingerprint and the request summary all see the stored
//! form, and the count rides through truncation as a
//! [`super::PRESERVED_KEYS`] member.
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

/// Payload key holding how many NULs [`escape_payload`] replaced.
///
/// Present only when at least one was. A **wire contract** in the same sense
/// as [`super::TRUNCATED_MARKER_KEY`].
///
/// When the payload holds NULs, [`escape_payload`] writes the true count over
/// any value already there. When it holds none, the payload — this key
/// included — is left alone. That second rule is what makes the escape
/// **idempotent**, and production depends on it: the tool path applies
/// [`super::truncate_payload`] twice (`core::tool_host::audit_sink`), and a
/// second pass that stripped the key would store `␀` with nothing left to
/// say it had been a NUL.
///
/// ⚠️ **Leaving a marker alone is safe only because core spells every
/// top-level payload key** — each `audit::insert` site builds its payload
/// with worker data nested beneath its own keys. A site that stored a
/// worker-returned object *as* the payload would let that worker forge this
/// key on a NUL-free row. Nothing enforces the convention; keep it when
/// adding a write site.
pub const NUL_ESCAPED_KEY: &str = "_nul_escaped";

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

/// The payload with every NUL — in any string, at any depth, in any object
/// key — replaced by [`NUL_ESCAPE`], and [`NUL_ESCAPED_KEY`] set to how many
/// were replaced.
///
/// A payload with no NUL comes back **unchanged** — including a
/// [`NUL_ESCAPED_KEY`] it already carried, which is what makes this
/// idempotent (see that key's doc).
///
/// Only an object can carry the count. A bare string or array payload is
/// still escaped — the row still lands — but says so only through the glyph.
/// Every production payload is an object.
pub fn escape_payload(mut payload: Value) -> Value {
    let count = escape_value(&mut payload);
    if count > 0 {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert(NUL_ESCAPED_KEY.to_string(), Value::from(count));
        }
    }
    payload
}

/// Escape `v` in place; return how many NULs were replaced.
fn escape_value(v: &mut Value) -> u64 {
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

#[cfg(test)]
mod tests;
