//! Append-only audit-log writes and reads.
//!
//! ## Where rows come from
//!
//! Today exactly two callers write into `audit_log`:
//!
//!   1. [`crate::probe::run`] — the daemon's bring-up row, written
//!      under [`crate::conn::RUNTIME_ROLE`] right after migrations.
//!   2. `core::tool_host::dispatch` (Phase 0 Option I) — one row per
//!      tool call, again under the runtime role via the
//!      `after_connect` SET ROLE hook on
//!      [`crate::pool::connect_runtime_pool`].
//!
//! The shape `(actor, action, payload)` is deliberately schema-less so
//! every future write site (memory writer, channel I/O, scheduler
//! transitions) can use the same single insert path.
//!
//! ## Append-only by *both* convention and database GRANT
//!
//! Migration `0002_runtime_role.sql` REVOKEs `UPDATE, DELETE,
//! TRUNCATE` on `audit_log` from [`crate::conn::RUNTIME_ROLE`]. So a
//! compromised dispatcher path running under the runtime role gets a
//! `permission denied` from Postgres if it tries to rewrite a row.
//! The application-level discipline of "only this module writes
//! audit rows" is layered on top — defense in depth.
//!
//! ## Truncation policy
//!
//! Tool-call payloads can be arbitrarily large (a `web-fetch` worker
//! could in principle return a megabyte of HTML). Storing the entire
//! body as JSONB inflates the table, the WAL, and the JSONL mirror
//! file with no operational value — operators tail the audit log to
//! see *who did what*, not to recover request bodies.
//!
//! [`truncate_payload`] enforces a 4 KiB cap (after JSON serialisation):
//! oversize payloads are replaced with a small envelope carrying a
//! SHA-256 fingerprint of the original bytes plus the original byte
//! length. The fingerprint lets two truncated rows be compared for
//! equality without storing the bytes themselves; the length tells an
//! operator how much was elided.
//!
//! Pure: returns a new `serde_json::Value`, performs no I/O. Tested
//! with deterministic-fingerprint regression pins.

use sqlx::Row;

use crate::DbError;

/// Maximum size in bytes of a serialised `audit_log.payload` JSONB
/// value before [`truncate_payload`] replaces it with a fingerprint
/// envelope.
///
/// 4 KiB is the same threshold called out in HANDOVER's Option I
/// brief. It comfortably holds a typical tool-call request/response
/// summary (`{"req": {...}, "result": {...}, "ms": 12}`) while
/// preventing any single row from dominating the `audit_log` heap or
/// the JSONL mirror line count.
pub const PAYLOAD_MAX_BYTES: usize = 4096;

/// One decoded `audit_log` row.
///
/// `payload` is whatever the writer stored — a `serde_json::Value`
/// (which may itself be a [`truncate_payload`] envelope). Decoding
/// happens through sqlx's `JsonValue` codec, which is enabled via the
/// workspace `sqlx` feature `"json"`.
#[derive(Clone, Debug)]
pub struct AuditRow {
    /// Strictly monotonic `BIGSERIAL` from the table.
    pub id: i64,
    /// `now()`-derived TIMESTAMPTZ from the row's `DEFAULT`. The
    /// audit-mirror task ships this verbatim (RFC 3339-ish via
    /// `time::OffsetDateTime`'s default `Display`).
    pub ts: time::OffsetDateTime,
    /// Free-form short string identifying who wrote the row.
    /// Conventions: `"core"` for daemon-internal events,
    /// `"tool:<name>"` for dispatcher-mediated tool calls,
    /// `"channel:<adapter>"` for channel I/O (Phase 2+).
    pub actor: String,
    /// Verb describing what happened: `"startup"`, `"call"`,
    /// `"deny"`, etc. Free-form, paired with `actor`.
    pub action: String,
    /// Structured details. May be a [`truncate_payload`] envelope.
    pub payload: serde_json::Value,
}

/// Returns the JSONB payload to *actually store* for a given input.
///
/// If the input serialises to ≤ [`PAYLOAD_MAX_BYTES`], the input is
/// returned unchanged. Otherwise it is replaced with:
///
/// ```json
/// { "_truncated": true, "sha256": "<64 hex>", "len": <bytes> }
/// ```
///
/// where `len` is the original serialised byte length and `sha256` is
/// the lowercase-hex SHA-256 digest of the same bytes. The envelope
/// itself is well under the budget so the return value is always
/// within budget.
///
/// Pure: deterministic, no I/O, no global state. Same input → same
/// output, every call.
pub fn truncate_payload(payload: serde_json::Value) -> serde_json::Value {
    // `to_vec` is infallible for `serde_json::Value` (the value is
    // already valid JSON in memory). The serialised form is what
    // Postgres will see — so that's the form we measure.
    let bytes = serde_json::to_vec(&payload).expect("serde_json::Value cannot fail to serialise");
    if bytes.len() <= PAYLOAD_MAX_BYTES {
        return payload;
    }

    use sha2::Digest;
    let digest = sha2::Sha256::digest(&bytes);
    let mut hex = String::with_capacity(64);
    for b in digest.iter() {
        // Two lowercase hex chars per byte. Width-padded so a leading
        // zero in any byte is preserved — `format!("{b:02x}")` is the
        // canonical idiom for reproducible hex.
        use std::fmt::Write;
        write!(&mut hex, "{:02x}", b).expect("write to String cannot fail");
    }

    serde_json::json!({
        "_truncated": true,
        "sha256": hex,
        "len": bytes.len(),
    })
}

/// Insert one row into `audit_log` and return its `id`.
///
/// `payload` flows through [`truncate_payload`] so the caller does not
/// have to enforce the cap themselves. The insert is a single round-trip
/// (`INSERT … RETURNING id`) — there is no separate SELECT.
///
/// `executor` is generic so this works against both a `&PgPool`
/// (production: dispatcher write site) and a `&mut PgConnection`
/// (tests: deterministic single-connection setup against a per-test
/// cluster). Both implement [`sqlx::Executor`] for the
/// [`sqlx::Postgres`] backend.
///
/// Errors propagate as [`DbError::Query`] — the wrapped message includes
/// the underlying sqlx error so a `permission denied` from the runtime
/// role's REVOKEs is operator-readable in the daemon log.
pub async fn insert<'e, E>(
    executor: E,
    actor: &str,
    action: &str,
    payload: serde_json::Value,
) -> Result<i64, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let payload = truncate_payload(payload);
    let row = sqlx::query(
        "INSERT INTO audit_log (actor, action, payload) \
         VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(actor)
    .bind(action)
    .bind(payload)
    .fetch_one(executor)
    .await
    .map_err(|e| DbError::Query(format!("audit_log insert: {e}")))?;
    row.try_get::<i64, _>(0)
        .map_err(|e| DbError::Query(format!("decode audit_log.id: {e}")))
}

/// Fetch one row by `id`. Used by the audit-mirror task to expand a
/// NOTIFY payload (which carries only the id) into the full row that
/// gets written to the JSONL file.
///
/// Returns [`DbError::Query`] if the row does not exist — which can
/// happen legitimately when the listener catches a NOTIFY for a row
/// that was rolled back between trigger fire and SELECT. Callers
/// should treat "row not found" as a benign skip, not a hard error.
pub async fn fetch_by_id<'e, E>(executor: E, id: i64) -> Result<AuditRow, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let row = sqlx::query(
        "SELECT id, ts, actor, action, payload \
         FROM audit_log WHERE id = $1",
    )
    .bind(id)
    .fetch_one(executor)
    .await
    .map_err(|e| DbError::Query(format!("audit_log fetch_by_id({id}): {e}")))?;
    decode_audit_row(&row)
}

/// Fetch every row with `id > since`, ordered by `id`. The mirror task
/// uses this on first start (since=0 → drain the whole table) and on
/// listener reconnect (since=last_seen_id → catch up on rows committed
/// while we weren't listening).
///
/// `limit` caps the number of rows pulled in one call so a multi-day
/// outage doesn't OOM the listener. The caller loops until the result
/// is shorter than `limit`.
pub async fn fetch_since<'e, E>(
    executor: E,
    since: i64,
    limit: i64,
) -> Result<Vec<AuditRow>, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let rows = sqlx::query(
        "SELECT id, ts, actor, action, payload \
         FROM audit_log WHERE id > $1 ORDER BY id LIMIT $2",
    )
    .bind(since)
    .bind(limit)
    .fetch_all(executor)
    .await
    .map_err(|e| DbError::Query(format!("audit_log fetch_since({since}): {e}")))?;
    rows.iter().map(decode_audit_row).collect()
}

fn decode_audit_row(row: &sqlx::postgres::PgRow) -> Result<AuditRow, DbError> {
    Ok(AuditRow {
        id: row
            .try_get(0)
            .map_err(|e| DbError::Query(format!("decode audit_log.id: {e}")))?,
        ts: row
            .try_get(1)
            .map_err(|e| DbError::Query(format!("decode audit_log.ts: {e}")))?,
        actor: row
            .try_get(2)
            .map_err(|e| DbError::Query(format!("decode audit_log.actor: {e}")))?,
        action: row
            .try_get(3)
            .map_err(|e| DbError::Query(format!("decode audit_log.action: {e}")))?,
        payload: row
            .try_get(4)
            .map_err(|e| DbError::Query(format!("decode audit_log.payload: {e}")))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Small payloads pass through unchanged — the truncation envelope
    /// must not be wrapped around already-fitting values.
    #[test]
    fn small_payload_is_not_truncated() {
        let v = serde_json::json!({"actor": "core", "ms": 12});
        let out = truncate_payload(v.clone());
        assert_eq!(out, v);
    }

    /// Empty object is the canonical default and must stay byte-for-byte.
    #[test]
    fn empty_object_passes_through() {
        let v = serde_json::json!({});
        assert_eq!(truncate_payload(v.clone()), v);
    }

    /// A payload at exactly the threshold byte count must NOT be
    /// truncated — the bound is inclusive. (Off-by-one regression
    /// guard: an earlier draft used `<` instead of `<=`.)
    #[test]
    fn payload_at_exact_threshold_is_not_truncated() {
        // Build a string whose JSON serialisation is exactly
        // `PAYLOAD_MAX_BYTES`. The serialisation of `"...payload..."`
        // adds 2 bytes for the surrounding double quotes.
        let inner_len = PAYLOAD_MAX_BYTES - 2;
        let s: String = "x".repeat(inner_len);
        let v = serde_json::Value::String(s);
        // Sanity: serialised length is exactly the bound.
        assert_eq!(serde_json::to_vec(&v).unwrap().len(), PAYLOAD_MAX_BYTES);
        let out = truncate_payload(v.clone());
        assert_eq!(out, v, "boundary is inclusive: == max must not truncate");
    }

    /// One byte over the threshold must be truncated. The envelope
    /// shape (`_truncated: true` + `sha256` + `len`) is the wire
    /// contract the JSONL mirror relies on; a downstream parser will
    /// notice if any of these keys go missing.
    #[test]
    fn over_threshold_payload_is_replaced_with_envelope() {
        let s: String = "y".repeat(PAYLOAD_MAX_BYTES);
        let v = serde_json::Value::String(s);
        let original_len = serde_json::to_vec(&v).unwrap().len();
        assert!(original_len > PAYLOAD_MAX_BYTES);

        let out = truncate_payload(v);
        let obj = out.as_object().expect("envelope must be a JSON object");
        assert_eq!(obj.get("_truncated"), Some(&serde_json::Value::Bool(true)));
        assert_eq!(
            obj.get("len").and_then(|v| v.as_i64()),
            Some(original_len as i64)
        );
        let sha = obj
            .get("sha256")
            .and_then(|v| v.as_str())
            .expect("sha256 must be a string");
        assert_eq!(sha.len(), 64, "sha256 hex must be 64 chars");
        assert!(
            sha.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "sha256 must be lowercase hex: got {sha}"
        );
    }

    /// Same input → same fingerprint. This is what makes truncated
    /// rows comparable: two operator queries that returned the same
    /// big body show the same `sha256`, even though the body itself
    /// is gone. Regression guard against accidentally salting the
    /// hash.
    #[test]
    fn truncate_is_deterministic_for_same_input() {
        let s = "z".repeat(PAYLOAD_MAX_BYTES + 100);
        let v1 = serde_json::Value::String(s.clone());
        let v2 = serde_json::Value::String(s);
        let a = truncate_payload(v1);
        let b = truncate_payload(v2);
        assert_eq!(a, b);
    }

    /// Different inputs at the same length must produce different
    /// fingerprints. Catches a silly mistake like hashing the *length*
    /// instead of the bytes.
    #[test]
    fn truncate_fingerprint_distinguishes_different_payloads() {
        let a = serde_json::Value::String("a".repeat(PAYLOAD_MAX_BYTES + 50));
        let b = serde_json::Value::String("b".repeat(PAYLOAD_MAX_BYTES + 50));
        let oa = truncate_payload(a);
        let ob = truncate_payload(b);
        assert_ne!(
            oa.get("sha256"),
            ob.get("sha256"),
            "different bodies must produce different SHA-256s"
        );
    }
}
