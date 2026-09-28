//! Layer-load queries for the `memories` table — the query-independent
//! loaders behind the dedicated per-layer prompt blocks
//! (`<l0_meta_rules>`, `<l1_insights>`, `<skills>`) and the L1 re-embed
//! backfill scan.
//!
//! Split out of the sibling `search` module (2026-09-29, movement only)
//! to keep each file under the 500-LOC cap: `search` holds the recall
//! lanes and hydration, this module the loaders that select one layer by
//! name. Every public function here is re-exported from the parent, so
//! the `db::memories::<name>` call-site paths are unchanged.

use sqlx::Row;

use crate::DbError;

use super::{limit_as_i64, Memory, MemoryLayer};

/// Load up to `cap` rows at the specified layer, newest first.
///
/// Returns rows in `(created_at DESC, id DESC)` order. The `id DESC`
/// tiebreaker is deliberate: `created_at` is `now()`-sourced at insert
/// time and Postgres clock resolution is microseconds, so two L1 rows
/// inserted in the same `tokio::spawn` burst can collide on
/// `created_at`. The tiebreaker keeps `load_layer` deterministic for
/// tests that seed rows sequentially without sleeping. The
/// `(layer, created_at DESC)` index from migration 0013 covers the
/// filter and the primary sort; the `id DESC` tiebreaker is resolved
/// in memory over the already-narrow result set (no second index
/// needed at L1's expected cardinality — if L4 / Digest grows large,
/// reconsider).
///
/// `cap = 0` is a fast-path no-op (no SQL issued). Rows whose layer
/// column reads back as an out-of-range SMALLINT surface as
/// [`DbError::Invariant`] via [`MemoryLayer::from_db`] — the schema
/// CHECK forbids that case, so hitting it means an operator must
/// investigate.
pub async fn load_layer<'e, E>(
    executor: E,
    layer: MemoryLayer,
    cap: usize,
) -> Result<Vec<Memory>, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    if cap == 0 {
        return Ok(Vec::new());
    }

    let rows = sqlx::query(
        "SELECT id, body, metadata, layer, created_at \
         FROM memories \
         WHERE layer = $1 \
         ORDER BY created_at DESC, id DESC \
         LIMIT $2",
    )
    .bind(layer.as_db())
    .bind(limit_as_i64(cap))
    .fetch_all(executor)
    .await
    .map_err(|e| DbError::Query(format!("load_layer {layer:?}: {e}")))?;

    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        out.push(row_to_memory(r)?);
    }
    Ok(out)
}

/// Load the `(id, body)` of every row at `layer` whose `embedding IS NULL`
/// — the scan behind the `kastellan-cli memory l1 reembed` backfill.
///
/// These rows are invisible to the semantic recall lane ([`super::semantic_search`]
/// filters `WHERE embedding IS NOT NULL`): pre-#324 rows and operator-added
/// rows (`memory l1 add` stores no embedding). The backfill re-embeds each
/// body and writes the vector back via [`super::set_embedding`].
///
/// Ordered by `id` for a stable, resumable scan: embedded rows drop out of
/// this `IS NULL` filter, so a re-run after a crash simply skips them.
/// Returns every match unpaged — at Phase-0 scale the L1 layer is small
/// (capped in-prompt); add a `LIMIT`/keyset cursor here if that ever changes.
/// `executor` is generic so the same helper serves `&PgPool` and
/// `&mut PgConnection` (test setup).
pub async fn load_unembedded_at_layer<'e, E>(
    executor: E,
    layer: MemoryLayer,
) -> Result<Vec<(i64, String)>, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let rows = sqlx::query(
        "SELECT id, body \
         FROM memories \
         WHERE layer = $1 AND embedding IS NULL \
         ORDER BY id",
    )
    .bind(layer.as_db())
    .fetch_all(executor)
    .await
    .map_err(|e| DbError::Query(format!("load_unembedded_at_layer {layer:?}: {e}")))?;

    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        let id: i64 = r
            .try_get(0)
            .map_err(|e| DbError::Query(format!("decode memory.id: {e}")))?;
        let body: String = r
            .try_get(1)
            .map_err(|e| DbError::Query(format!("decode memory.body: {e}")))?;
        out.push((id, body));
    }
    Ok(out)
}

/// Decode one `(id, body, metadata, layer, created_at)` row — in that
/// column order — into a [`Memory`]. Shared by the layer loaders so the
/// column order and decode-error shape stay identical across them.
fn row_to_memory(r: &sqlx::postgres::PgRow) -> Result<Memory, DbError> {
    let id: i64 = r
        .try_get(0)
        .map_err(|e| DbError::Query(format!("decode memory.id: {e}")))?;
    let body: String = r
        .try_get(1)
        .map_err(|e| DbError::Query(format!("decode memory.body: {e}")))?;
    let metadata: serde_json::Value = r
        .try_get(2)
        .map_err(|e| DbError::Query(format!("decode memory.metadata: {e}")))?;
    let layer_raw: i16 = r
        .try_get(3)
        .map_err(|e| DbError::Query(format!("decode memory.layer: {e}")))?;
    let layer = MemoryLayer::from_db(layer_raw)?;
    let created_at: time::OffsetDateTime = r
        .try_get(4)
        .map_err(|e| DbError::Query(format!("decode memory.created_at: {e}")))?;
    Ok(Memory { id, body, metadata, layer, created_at })
}

/// Load rows at `layer` whose `metadata->>'trust'` matches one of
/// `trusts`, newest-first (`created_at DESC, id DESC`), capped at `cap`.
///
/// The trust filter runs **in SQL**, so the result set — and the work
/// this does — is bounded by the number of *matching* rows, not the
/// whole layer. This matters for [`MemoryLayer::Skill`]: the L3
/// crystallisation writer appends a `trust:"untrusted"` row on every
/// completed multi-step task, so that layer grows with task history,
/// yet only the handful of operator-approved/pinned rows ever surface to
/// the planner. The `WHERE layer = $1 AND metadata->>'trust' = ANY($2)`
/// push-down keeps prompt assembly off that growth curve — the caller no
/// longer fetches (and JSON-decodes) the entire layer on every plan
/// formulation just to discard nearly all of it.
///
/// Trust matching is exact and case-sensitive, mirroring the Rust gate
/// (`SkillTrust::from_metadata_str`): a row with an absent, null,
/// non-string, or unrecognised `trust` marker yields a `metadata->>'trust'`
/// that is NULL or outside `trusts`, so `= ANY` excludes it — the same
/// fail-safe "unknown ⇒ not surfaced" posture, enforced in the query.
///
/// `cap == 0` or an empty `trusts` is a fast-path no-op (no SQL issued).
pub async fn load_layer_by_trust<'e, E>(
    executor: E,
    layer: MemoryLayer,
    trusts: &[&str],
    cap: usize,
) -> Result<Vec<Memory>, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    if cap == 0 || trusts.is_empty() {
        return Ok(Vec::new());
    }

    let trust_list: Vec<String> = trusts.iter().map(|s| s.to_string()).collect();
    let rows = sqlx::query(
        "SELECT id, body, metadata, layer, created_at \
         FROM memories \
         WHERE layer = $1 AND metadata->>'trust' = ANY($2) \
         ORDER BY created_at DESC, id DESC \
         LIMIT $3",
    )
    .bind(layer.as_db())
    .bind(&trust_list)
    .bind(limit_as_i64(cap))
    .fetch_all(executor)
    .await
    .map_err(|e| DbError::Query(format!("load_layer_by_trust {layer:?}: {e}")))?;

    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        out.push(row_to_memory(r)?);
    }
    Ok(out)
}

/// Load the currently-active L0 rule set, deduplicated by
/// `metadata->>'l0_rule_id'`.
///
/// L0 rows are append-only by `seed_meta_memory`; an edited rule
/// produces a *new* row with the same `l0_rule_id` and a different
/// `body_sha256`. The active set is the newest row per
/// `l0_rule_id`. Rows missing the `l0_rule_id` metadata key (e.g.
/// hand-written test rows or future legacy fixtures) are excluded —
/// they're not part of the seed-loader's universe.
///
/// Returns up to `cap_rows` rows ordered by
/// `(l0_rule_id ASC, created_at DESC, id DESC)` for stable per-rule
/// dedup, but the *outer* return order is `created_at DESC, id DESC`
/// across the deduplicated set so the caller can drop oldest-first
/// when budgeting. The `id DESC` tiebreaker matches `load_layer` for
/// microsecond-clock collisions.
///
/// `cap_rows = 0` is a fast-path no-op (no SQL issued). Saturating
/// cast on `cap_rows` via `limit_as_i64` matches `load_layer`.
pub async fn load_active_l0<'e, E>(
    executor: E,
    cap_rows: usize,
) -> Result<Vec<Memory>, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    if cap_rows == 0 {
        return Ok(Vec::new());
    }
    let limit = limit_as_i64(cap_rows);

    // Two-step SELECT:
    //   1. DISTINCT ON (rule_id) ORDER BY rule_id, created_at DESC,
    //      id DESC — newest row per rule.
    //   2. Outer wrapper re-orders by created_at DESC across the
    //      deduplicated set so the caller's byte-budget drop logic
    //      cuts oldest-first (consistent with load_layer).
    //
    // The `metadata ? 'l0_rule_id'` predicate excludes any L0 rows
    // written without the rule_id metadata key. Such rows are not
    // part of the seed-loader's universe and would otherwise produce
    // a NULL group from the DISTINCT ON.
    let rows = sqlx::query(
        "SELECT id, body, metadata, layer, created_at \
         FROM ( \
             SELECT DISTINCT ON (metadata->>'l0_rule_id') \
                    id, body, metadata, layer, created_at \
               FROM memories \
              WHERE layer = 0 \
                AND metadata ? 'l0_rule_id' \
              ORDER BY metadata->>'l0_rule_id', created_at DESC, id DESC \
         ) AS dedup \
         ORDER BY created_at DESC, id DESC \
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(executor)
    .await
    .map_err(|e| DbError::Query(format!("load_active_l0: {e}")))?;

    let mut out: Vec<Memory> = Vec::with_capacity(rows.len());
    for row in rows {
        let id: i64 = row
            .try_get("id")
            .map_err(|e| DbError::Query(format!("decode id: {e}")))?;
        let body: String = row
            .try_get("body")
            .map_err(|e| DbError::Query(format!("decode body: {e}")))?;
        let metadata: serde_json::Value = row
            .try_get("metadata")
            .map_err(|e| DbError::Query(format!("decode metadata: {e}")))?;
        let layer_raw: i16 = row
            .try_get("layer")
            .map_err(|e| DbError::Query(format!("decode layer: {e}")))?;
        let layer = MemoryLayer::from_db(layer_raw)?;
        let created_at: time::OffsetDateTime = row
            .try_get("created_at")
            .map_err(|e| DbError::Query(format!("decode created_at: {e}")))?;
        out.push(Memory {
            id,
            body,
            metadata,
            layer,
            created_at,
        });
    }
    Ok(out)
}
