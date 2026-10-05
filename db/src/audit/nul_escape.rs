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
//! The escape itself is [`crate::nul`]'s; this module adds the audit-only
//! count marker.
//!
//! Every NUL becomes [`crate::nul::NUL_ESCAPE`] (U+2400 `␀`, SYMBOL FOR
//! NULL — the Unicode glyph that exists to *depict* a NUL). Replaced, never deleted:
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
//! `escape_object` in [`crate::nul`]).
//!
//! [`escape_payload`] is the step [`super::truncate_payload`] runs **first**,
//! so the cap, the fingerprint and the request summary all see the stored
//! form, and the count rides through truncation as a
//! [`super::PRESERVED_KEYS`] member.
//!
//! Pure: no I/O, no global state.

use serde_json::Value;

use crate::nul::escape_value;

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

/// The payload with every NUL — in any string, at any depth, in any object
/// key — replaced by [`crate::nul::NUL_ESCAPE`], and [`NUL_ESCAPED_KEY`] set to how many
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

#[cfg(test)]
mod tests;
