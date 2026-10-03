//! Append-only audit-log writes and reads.
//!
//! ## Where rows come from
//!
//! Write sites are many and growing — the daemon's bring-up row
//! ([`crate::probe::run`]), every tool call (`core::tool_host::dispatch`),
//! memory writes, egress verdicts, secrets administration, channel I/O.
//! An enumeration here only ever went stale (it said "exactly two" long
//! after there were twenty). The invariant worth stating is that **every
//! one of them goes through [`insert`]** — so every one of them is capped
//! by [`truncate_payload`], and every one of them writes as
//! [`crate::conn::RUNTIME_ROLE`], which is what makes the GRANT below
//! load-bearing rather than advisory.
//!
//! *How* that role is assumed differs at exactly one site, and the
//! difference is worth knowing before trusting the sentence above: the
//! runtime pool sets it once per connection through the `after_connect`
//! hook on [`crate::pool::connect_runtime_pool`], whereas
//! [`crate::probe::run`] issues an explicit `SET ROLE` on the single bare
//! connection it migrates over — the pool does not exist yet at that point
//! in bring-up. Same role, same [`insert`], different mechanism.
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
//! **The request is summarised rather than lost.** The premise above —
//! that operators want "who did what" and not the body — holds for a tool
//! whose bulk is a fetched document, and fails for `shell.exec`, where the
//! argv *is* the act being audited. So an over-cap payload that carried a
//! request keeps a bounded [`req_summary`] naming what ran, derived here
//! rather than at each producer because there is more than one producer and
//! a rule each of them can forget is a rule that will be forgotten
//! (issue #617). See [`req_summary`] for the shape and the reasoning.
//!
//! **[`PRESERVED_KEYS`] ride through that replacement.** "Who did what"
//! includes the outcome of a control that ran on the payload, and such a
//! record is bounded, tiny, and — unlike a request body — recoverable
//! from nowhere else. Dropping it was measured live: on 2026-08-23 two
//! 85 KB `web.fetch` rows took the guard tier's score down with them.
//! A preserved key that still cannot be afforded is named by
//! [`DROPPED_PRESERVED_KEY`] rather than vanishing, because an
//! unrecorded loss is the shape of the defect above.
//!
//! ## NUL is escaped before anything else (issue #816)
//!
//! Postgres refuses U+0000 in `jsonb` and `text`, so one NUL in a
//! worker-written string used to fail the whole row — and a hostile channel
//! peer could erase its own audit trail by putting one in its id.
//! [`truncate_payload`] therefore runs [`nul_escape::escape_payload`] first:
//! each NUL becomes [`NUL_ESCAPE`] (`␀`) and the payload records how many
//! under [`NUL_ESCAPED_KEY`], which rides through truncation as a
//! [`PRESERVED_KEYS`] member. [`insert`] escapes `actor` and `action` too.
//!
//! Pure: returns a new `serde_json::Value`, performs no I/O. Tested
//! with deterministic-fingerprint regression pins.

use sqlx::Row;

use crate::DbError;

pub mod nul_escape;
pub mod req_summary;
mod truncate;

pub use nul_escape::{NUL_ESCAPE, NUL_ESCAPED_KEY};
pub use req_summary::{HEAD_MAX_BYTES, REQ_KEY, REQ_SUMMARY_KEY};
pub use truncate::{
    is_truncation_envelope, truncate_payload, DROPPED_PRESERVED_KEY, DROP_MARKER_RESERVE, GUARD_KEY,
    PAYLOAD_MAX_BYTES, PRESERVED_KEYS, TRUNCATED_MARKER_KEY,
};

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

/// Insert one row into `audit_log` and return its `id`.
///
/// `payload` flows through [`truncate_payload`] so the caller does not
/// have to enforce the cap — or escape NUL — themselves. `actor` and
/// `action` are `text` columns, which refuse NUL just as `jsonb` does, so
/// they are escaped too ([`nul_escape::escape_str`]). Both are spelled by
/// code today, so this is a backstop; with no payload of their own to
/// carry a count, the `␀` glyph is their record.
///
/// The insert is a single round-trip (`INSERT … RETURNING id`) — there is
/// no separate SELECT.
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
    let (actor, _) = nul_escape::escape_str(actor);
    let (action, _) = nul_escape::escape_str(action);
    let row = sqlx::query(
        "INSERT INTO audit_log (actor, action, payload) \
         VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(&actor)
    .bind(&action)
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
