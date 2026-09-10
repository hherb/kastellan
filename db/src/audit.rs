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
//! Pure: returns a new `serde_json::Value`, performs no I/O. Tested
//! with deterministic-fingerprint regression pins.

use sqlx::Row;

use crate::DbError;

pub mod req_summary;

pub use req_summary::{HEAD_MAX_BYTES, REQ_KEY, REQ_SUMMARY_KEY};

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

/// Payload keys carried THROUGH truncation instead of being replaced by
/// the fingerprint envelope.
///
/// A key earns a place here only if it is all three of:
///
/// 1. **bounded by construction** — a fixed set of scalars, so it cannot
///    itself push the envelope over [`PAYLOAD_MAX_BYTES`];
/// 2. **a decision record, not data** — the outcome of a control, not the
///    document the control ran on. The cap exists to stop bodies dominating
///    the heap, and preserving a body under another name would defeat it;
/// 3. **irrecoverable** — it exists nowhere else *on the path that matters*.
///    `req` and `result` can be reconstructed from the worker and the
///    surrounding rows. A guard score is computed once, in memory; a Block
///    mirrors its `p`/`tau` onto the forensic `policy` / `injection.blocked`
///    row, but a **clear writes no second row at all**, and the cleared half
///    is exactly the half D5 needs.
///
/// `guard` is the wiring slice's per-dispatch guard-tier report
/// (`{state, p, tau, ms, body_byte_len, truncated, error_kind}`). Before it was listed
/// here, a tool result over the cap took the score with it — measured live
/// on 2026-08-23 at 85,352 bytes — which silently inverted spec D5: blocked
/// dispatches kept their score (their result is a short placeholder) while
/// *cleared* ones lost theirs, leaving a size-selected sample that reads
/// like a score distribution.
///
/// This is a **wire contract** in the same sense as
/// [`TRUNCATED_MARKER_KEY`]: readers may rely on a preserved key meaning
/// exactly what it meant in the untruncated payload.
///
/// **Array order is priority order.** Keys are admitted left to right
/// against a shared budget, so an earlier member can starve a later one.
/// Moot at one member; decide it deliberately before adding a second.
pub const PRESERVED_KEYS: &[&str] = &[GUARD_KEY, REQ_SUMMARY_KEY];

/// The payload key under which `core::tool_host::post_process` records the
/// guard tier's per-dispatch verdict — and the sole member of
/// [`PRESERVED_KEYS`].
///
/// It is a `const` rather than two independent string literals because the
/// producer lives in another crate. Spelled twice, a rename on either side
/// would compile, and preservation would stop silently: cleared scores
/// would vanish above the cap while blocked ones survived, which is
/// precisely the defect [`PRESERVED_KEYS`] was added to fix, restored
/// without a single failing test. Spelled once, that rename is a compile
/// error.
pub const GUARD_KEY: &str = "guard";

/// Payload key naming the [`PRESERVED_KEYS`] that did **not** fit.
///
/// Without it, an envelope whose preserved key was dropped for size carries
/// the *same key set* as one whose payload never carried that key (the
/// fingerprints differ, but they describe inputs a reader does not have) —
/// so a reader cannot tell "the control never ran" from "the control ran,
/// and its verdict was discarded here". That ambiguity *is* the defect this
/// allowlist exists to fix, one function further down; leaving it in place
/// would be fixing the loss and keeping the silence.
///
/// A **wire contract** in the same sense as [`TRUNCATED_MARKER_KEY`]:
/// present only on an envelope, and only when something was actually lost.
pub const DROPPED_PRESERVED_KEY: &str = "_dropped_preserved";

/// Bytes held back from [`PAYLOAD_MAX_BYTES`] so [`DROPPED_PRESERVED_KEY`]
/// is *always* affordable.
///
/// A drop that could not be recorded would be a silent drop again, so the
/// room for the record is reserved before any preserved key is admitted
/// rather than hoped for afterwards. [`drop_marker_worst_case`] is checked
/// against this at compile time.
///
/// Public because [`truncate_payload`]'s one hard postcondition is stated
/// in terms of it: a caller reasoning about what will survive the cap
/// cannot do so without this number.
pub const DROP_MARKER_RESERVE: usize = 64;

/// The envelope's fingerprint keys. Private — [`is_truncation_envelope`] is
/// the supported way to read an envelope — but named once so the producer
/// and the compile-time shadow check below cannot drift apart.
const FINGERPRINT_SHA256_KEY: &str = "sha256";
/// See [`FINGERPRINT_SHA256_KEY`].
const FINGERPRINT_LEN_KEY: &str = "len";

/// `a == b` for `&str` in const context, which `PartialEq` is not.
const fn str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Worst-case serialised cost of adding [`DROPPED_PRESERVED_KEY`] to an
/// object that already has keys: one separating comma, the quoted key, a
/// colon, and an array naming every member of [`PRESERVED_KEYS`].
///
/// The comma is counted, not located. `serde_json`'s `Map` is a `BTreeMap`
/// unless `preserve_order` is enabled, so `_dropped_preserved` actually
/// sorts *before* `_truncated` and its separator is emitted after the pair
/// rather than before it — exactly one comma either way, and the count is
/// what the reserve is spent on.
///
/// Deliberately an over-estimate: the per-member term charges a trailing
/// comma to every element while `serde_json` emits only `N-1` of them.
const fn drop_marker_worst_case() -> usize {
    // `,"<key>":[]`
    let mut n = DROPPED_PRESERVED_KEY.len() + 6;
    let mut i = 0;
    while i < PRESERVED_KEYS.len() {
        // `"<key>",`
        n += PRESERVED_KEYS[i].len() + 3;
        i += 1;
    }
    n
}

/// The envelope's own keys are reserved, and the marker is affordable.
///
/// [`PRESERVED_KEYS`]' three admission criteria are prose, and a key can
/// satisfy all three and still be catastrophic: `len` or `sha256` would
/// silently overwrite the fingerprint — so two rows for the same body stop
/// comparing equal, which is precisely what
/// `a_preserved_key_does_not_change_the_fingerprint` was written to protect
/// and cannot catch for a key it does not name. [`TRUNCATED_MARKER_KEY`] is
/// worse still: shadowing it makes [`is_truncation_envelope`] report
/// whatever the payload happened to carry, resurrecting the issue-#62
/// misclassification the predicate exists to prevent.
///
/// Prose cannot enforce that. This can, at compile time, for every future
/// member — which is the same move the size postcondition already makes.
///
/// [`DROPPED_PRESERVED_KEY`] is checked against the same reserved names, and
/// not only against [`PRESERVED_KEYS`]: it is written onto the envelope by
/// the same `insert`, so pointing it at `len` would overwrite the
/// fingerprint just as surely — the failure this block exists to make
/// impossible, reachable through the one key it used not to look at.
const _: () = {
    // The three keys the envelope builds itself from. Shadowing any of
    // them means writing over the fingerprint, or over the marker that
    // makes the row self-declaring.
    //
    // `plan` is not one of them, and is here for a different reason:
    // promoting it would falsify
    // `core::observation::CapturedPlan::source_truncated`'s documented
    // guarantee that a null `plan_json` on a truncated row is an artefact
    // of truncation. A future member meets that reader here rather than in
    // production.
    let reserved =
        [TRUNCATED_MARKER_KEY, FINGERPRINT_SHA256_KEY, FINGERPRINT_LEN_KEY, "plan"];

    let mut j = 0;
    while j < reserved.len() {
        let mut i = 0;
        while i < PRESERVED_KEYS.len() {
            assert!(
                !str_eq(PRESERVED_KEYS[i], reserved[j]),
                "a PRESERVED_KEYS member may not shadow a reserved payload key"
            );
            i += 1;
        }
        // The marker is written onto the same envelope by the same
        // `insert`, so pointing it at `len` would overwrite the
        // fingerprint just as surely. Checking only PRESERVED_KEYS left
        // this reachable through the one key the block did not look at.
        assert!(
            !str_eq(DROPPED_PRESERVED_KEY, reserved[j]),
            "DROPPED_PRESERVED_KEY may not shadow a reserved payload key"
        );
        j += 1;
    }

    let mut i = 0;
    while i < PRESERVED_KEYS.len() {
        assert!(
            !str_eq(PRESERVED_KEYS[i], DROPPED_PRESERVED_KEY),
            "a PRESERVED_KEYS member may not shadow the dropped-key marker"
        );
        i += 1;
    }

    assert!(
        drop_marker_worst_case() <= DROP_MARKER_RESERVE,
        "DROP_MARKER_RESERVE no longer covers the marker PRESERVED_KEYS can produce"
    );
};

/// Payload key that marks a [`truncate_payload`] envelope. This is a **wire
/// contract**: readers in other crates (e.g. `kastellan-core`'s observation
/// capture, issue #62) detect truncation via [`is_truncation_envelope`], so
/// the writer and every reader share this single definition rather than
/// re-spelling the literal.
pub const TRUNCATED_MARKER_KEY: &str = "_truncated";

/// True iff `payload` is a truncation envelope produced by
/// [`truncate_payload`] — i.e. the original payload was over budget and
/// its keys were replaced by the `{_truncated, sha256, len}` fingerprint,
/// except any listed in [`PRESERVED_KEYS`], which ride along unchanged (and
/// [`DROPPED_PRESERVED_KEY`], which names those that could not).
///
/// The predicate deliberately tests only the marker, so adding a preserved
/// key stays additive for every existing reader.
///
/// Lives next to the producer so the two cannot drift: a shape change to the
/// envelope must update this predicate (and the shape-pin test below) in the
/// same file. Pure.
pub fn is_truncation_envelope(payload: &serde_json::Value) -> bool {
    payload
        .get(TRUNCATED_MARKER_KEY)
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

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
/// the lowercase-hex SHA-256 digest of the same bytes — **of the input,
/// not of the envelope**, so two rows for the same body still compare
/// equal whatever else they carry.
///
/// Any [`PRESERVED_KEYS`] present in the input are then copied onto the
/// envelope **verbatim, whatever their value** — including a `null` or a
/// scalar, since this function judges keys and not shapes — because a
/// bounded decision record is not what the cap is defending against and is
/// worth more than the bytes it costs.
///
/// Keys are admitted **one at a time**, each against the budget less
/// [`DROP_MARKER_RESERVE`]. Two properties follow that an all-or-nothing
/// copy does not have: one oversized key cannot take a bounded sibling
/// down with it, and anything refused can still be *named*, under
/// [`DROPPED_PRESERVED_KEY`]. Nothing in this workspace can produce an
/// oversized preserved key — the one producer is
/// `core::tool_host::post_process`, via `GuardReport::audit_value`, which
/// emits a fixed handful of small scalars — but the signature permits one,
/// and
/// [`preserve_onto`] is tested against multi-key lists that exercise it.
///
/// The budget postcondition is therefore structural rather than checked
/// after the fact: every admitted key left [`DROP_MARKER_RESERVE`] bytes
/// free, and the compile-time assertion above pins the marker's worst case
/// under that reserve.
///
/// Pure: deterministic, no I/O, no global state. Same input → same
/// output, every call.
pub fn truncate_payload(mut payload: serde_json::Value) -> serde_json::Value {
    // `to_vec` is infallible for `serde_json::Value` (the value is
    // already valid JSON in memory). The serialised form is what
    // Postgres will see — so that's the form we measure.
    let bytes = serde_json::to_vec(&payload).expect("serde_json::Value cannot fail to serialise");
    if bytes.len() <= PAYLOAD_MAX_BYTES {
        return payload;
    }

    // One renderer, shared with the request summary's digest: readers
    // compare both across rows, and two hex spellings that disagreed on
    // zero-padding would make rows silently incomparable.
    let hex = req_summary::sha256_hex(&bytes);

    let mut bare = serde_json::Map::new();
    bare.insert(TRUNCATED_MARKER_KEY.to_string(), serde_json::Value::Bool(true));
    bare.insert(FINGERPRINT_SHA256_KEY.to_string(), serde_json::Value::String(hex));
    bare.insert(FINGERPRINT_LEN_KEY.to_string(), serde_json::json!(bytes.len()));

    // ── Derive the bounded request summary (issue #617). ──
    //
    // Strictly after the fingerprint above, which is computed over `bytes`
    // — the serialisation of the payload as it arrived. Inserting the
    // summary first would fold it into the digest and two rows for one
    // request body would stop comparing equal, which is the one thing the
    // fingerprint exists to do. `the_req_summary_does_not_change_the_
    // envelope_fingerprint` pins that order.
    //
    // Written onto the payload rather than handed to `preserve_onto`
    // separately so the summary is admitted, budgeted and — if it ever did
    // not fit — *named* by exactly the same machinery as every other
    // preserved key, with no second code path to keep in step. `payload` is
    // owned here and about to be dropped, so the insert costs one small
    // value and no clone of the oversized body.
    //
    // It overwrites any `req_summary` the producer supplied: there is one
    // producer of this rule by design, and a payload must not be able to
    // put its own answer on the row.
    if let Some(summary) = req_summary::summarize_req(&payload) {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert(REQ_SUMMARY_KEY.to_string(), summary);
        }
    }

    // Only an object can have keys to preserve; a bare string or array
    // over the cap is all data by definition. Nothing is lost silently
    // here that the fingerprint does not already record: a non-object
    // payload has no keys, so no preserved key can have gone missing.
    let Some(source) = payload.as_object() else {
        return serde_json::Value::Object(bare);
    };

    preserve_onto(bare, source, PRESERVED_KEYS)
}

/// Copy each of `keys` from `source` onto `envelope`, admitting one at a
/// time against [`PAYLOAD_MAX_BYTES`] less [`DROP_MARKER_RESERVE`], and
/// naming under [`DROPPED_PRESERVED_KEY`] any that would not fit.
///
/// Split out of [`truncate_payload`] so `keys` can be a **parameter**.
/// With [`PRESERVED_KEYS`] at one member, the interesting half of this
/// function is unreachable from its only production caller: a running
/// envelope is indistinguishable from a fixed one, a starved sibling
/// cannot exist, and [`DROPPED_PRESERVED_KEY`] can never name more than a
/// single key. Those are the properties [`truncate_payload`]'s doc claims,
/// so they are tested here against multi-key lists rather than argued.
///
/// Measurement is exact rather than estimated: the candidate is inserted,
/// the whole object is serialised, and a key that does not fit is taken
/// back out. `displaced` restores a value this would otherwise clobber —
/// unreachable for [`PRESERVED_KEYS`], which the compile-time block above
/// forbids from shadowing an envelope key, but `keys` is a parameter and a
/// helper that silently ate a fingerprint under test would be its own
/// small version of the defect this file is about.
///
/// Pure.
fn preserve_onto(
    mut envelope: serde_json::Map<String, serde_json::Value>,
    source: &serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
) -> serde_json::Value {
    let mut dropped: Vec<&str> = Vec::new();
    for &key in keys {
        let Some(value) = source.get(key) else { continue };
        let displaced = envelope.insert(key.to_string(), value.clone());
        // Infallible for a reason stronger than "Values serialise": this
        // subtree was already serialised whole by the caller, so a second
        // pass over a subset of it cannot fail.
        let grown = serde_json::to_vec(&envelope).expect("a JSON object cannot fail to serialise");
        // The reserve is what keeps the ELSE arm affordable: a key refused
        // for size must still leave room to say so.
        if grown.len() + DROP_MARKER_RESERVE <= PAYLOAD_MAX_BYTES {
            continue;
        }
        match displaced {
            Some(prev) => envelope.insert(key.to_string(), prev),
            None => envelope.remove(key),
        };
        dropped.push(key);
    }

    if !dropped.is_empty() {
        envelope.insert(DROPPED_PRESERVED_KEY.to_string(), serde_json::json!(dropped));
    }
    serde_json::Value::Object(envelope)
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
mod tests;
