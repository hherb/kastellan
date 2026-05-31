//! Write-path helpers for the `memories` table — inserts, the
//! layer-tagged writers, the deliberately-named L0 seed entry point,
//! the layer-guarded delete, and the entity-linkage writer.
//!
//! Split out of the parent [`crate::memories`] module (2026-05-30) to
//! keep each file under the 500-LOC cap. Every public function here is
//! re-exported from the parent, so the call-site paths
//! `db::memories::{insert_memory, insert_memory_at_layer,
//! delete_memory_at_layer, seed_meta_memory, link_memory_to_entities}`
//! are byte-for-byte unchanged. See the parent module doc for the
//! chokepoint discipline and the `vector(1024)` text-encoding decision.
//!
//! Shared vocabulary lives in the parent and is imported below: the
//! `check_embedding_dim` guard, the [`vector_literal`] pgvector
//! encoder, and the [`MemoryLayer`] enum. A child module can reach
//! these parent-private items via `super::`.

use sqlx::Row;

use crate::DbError;

use super::{check_embedding_dim, vector_literal, MemoryLayer};

/// Insert a new memory row and return its id.
///
/// `embedding` may be `None` (the column is nullable today); when
/// `Some`, its length MUST equal [`EMBEDDING_DIM`](super::EMBEDDING_DIM) — the wrapped helper
/// rejects mismatches up front so the operator-facing error is "wrong
/// dimensionality" rather than an opaque Postgres `column type error`.
///
/// `executor` is generic over `sqlx::Executor<'_, Database = Postgres>`
/// so the same helper works against `&PgPool` (production) and
/// `&mut PgConnection` (deterministic test setup).
pub async fn insert_memory<'e, E>(
    executor: E,
    body: &str,
    metadata: &serde_json::Value,
    embedding: Option<&[f32]>,
) -> Result<i64, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    if let Some(v) = embedding {
        check_embedding_dim("insert", v)?;
    }

    // Bind the embedding as text and let Postgres cast it.
    //
    // Rationale documented on [`vector_literal`]: keeps the dep set
    // minimal, and the throughput at Phase-0 scale is dominated by
    // the network/UDS round-trip, not the parse cost. The `::vector`
    // cast happens inside the SQL so the column type is preserved.
    //
    // Splitting into two query shapes (with vs. without embedding) is
    // marginally cleaner than passing `NULL::vector` through the same
    // statement — `NULL` casts work, but the planner's decision tree
    // is simpler when the column reference is a literal column.
    let row = if let Some(v) = embedding {
        let lit = vector_literal(v);
        sqlx::query(
            "INSERT INTO memories (body, metadata, embedding) \
             VALUES ($1, $2, $3::vector) RETURNING id",
        )
        .bind(body)
        .bind(metadata)
        .bind(lit)
        .fetch_one(executor)
        .await
    } else {
        sqlx::query(
            "INSERT INTO memories (body, metadata) \
             VALUES ($1, $2) RETURNING id",
        )
        .bind(body)
        .bind(metadata)
        .fetch_one(executor)
        .await
    }
    .map_err(|e| DbError::Query(format!("insert memory: {e}")))?;
    row.try_get::<i64, _>(0)
        .map_err(|e| DbError::Query(format!("decode memory.id: {e}")))
}

/// Link a memory to a set of entities. Idempotent: re-linking the same
/// pair is a no-op via ON CONFLICT DO NOTHING.
///
/// Returns the count of genuinely new links inserted — zero on a full
/// re-link, partial counts on mixed (some new, some pre-existing).
///
/// Empty `entity_ids` is a fast-path no-op (no SQL issued, returns 0).
/// FK violation (unknown memory_id or entity_id) surfaces as
/// [`DbError::Query`]; ON CONFLICT DO NOTHING does not suppress FK
/// failures — the whole batch fails atomically with zero rows inserted.
///
/// `executor` is generic over `sqlx::Executor` so the same helper works
/// against `&PgPool` (production) and `&mut PgConnection` (test setup).
pub async fn link_memory_to_entities<'e, E>(
    executor: E,
    memory_id: i64,
    entity_ids: &[i64],
) -> Result<u64, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    if entity_ids.is_empty() {
        return Ok(0);
    }

    let result = sqlx::query(
        "INSERT INTO memory_entities (memory_id, entity_id) \
         SELECT $1::bigint, eid FROM unnest($2::bigint[]) AS t(eid) \
         ON CONFLICT (memory_id, entity_id) DO NOTHING",
    )
    .bind(memory_id)
    .bind(entity_ids)
    .execute(executor)
    .await
    .map_err(|e| DbError::Query(format!("link_memory_to_entities: {e}")))?;

    Ok(result.rows_affected())
}

/// Insert a memory row tagged with an explicit layer.
///
/// [`insert_memory`] is the shorthand for the L2 (Stable) case; callers
/// that genuinely mean L1 / L3 / L4 must use this helper and say so.
/// The DB-level `DEFAULT 2` on the column belongs to the plain
/// `insert_memory` SQL shape — this helper passes the layer explicitly
/// so a future column-default change can't silently affect L1 writers.
///
/// **L0 ([`MemoryLayer::Meta`]) is rejected here** with
/// [`DbError::PolicyViolation`]. L0 is reserved for hand-curated
/// meta-rules ("never `rm -rf`") that constrain the agent itself; the
/// agent loop must never grow its own constraints. Seed inserts go
/// through [`seed_meta_memory`] instead — a separate, explicitly named
/// admin path so a code review can see L0 writes at a glance.
///
/// Layer-CHECK violation is unreachable through this signature: the
/// [`MemoryLayer`] enum is the only producer of the bound value, and
/// every discriminant is within the CHECK range. Embedding dimension
/// mismatch is rejected up front by the shared [`check_embedding_dim`]
/// helper (same operator-readable shape as [`insert_memory`]).
pub async fn insert_memory_at_layer<'e, E>(
    executor: E,
    body: &str,
    metadata: &serde_json::Value,
    embedding: Option<&[f32]>,
    layer: MemoryLayer,
) -> Result<i64, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    if matches!(layer, MemoryLayer::Meta) {
        return Err(DbError::PolicyViolation(
            "L0 (Meta) writes must go through seed_meta_memory; \
             insert_memory_at_layer is for L1/L3/L4 only"
                .to_string(),
        ));
    }
    insert_row_at_layer_unchecked(executor, body, metadata, embedding, layer).await
}

/// Delete one row from `memories` by id, but **only** if its layer
/// matches `layer`. Returns `true` if a row was deleted; `false` if
/// no row matched (id absent or layer mismatch).
///
/// The layer guard exists so that `hhagent-cli memory l1 remove <id>`
/// cannot accidentally delete an L0 / L2 / L3 row through this path —
/// the operator subcommand passes `MemoryLayer::Index` here.
///
/// The existing AFTER DELETE trigger on `memories` (migration
/// `0008_deleted_memories_audit.sql`) journals the deleted row's
/// body, metadata, embedding, and `original_created_at` into the
/// `deleted_memories` table for the audit trail.
pub async fn delete_memory_at_layer<'e, E>(
    executor: E,
    id: i64,
    layer: MemoryLayer,
) -> Result<bool, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let rows = sqlx::query("DELETE FROM memories WHERE id = $1 AND layer = $2")
        .bind(id)
        .bind(layer.as_db())
        .execute(executor)
        .await
        .map_err(|e| DbError::Query(format!("delete_memory_at_layer id={id}: {e}")))?;
    Ok(rows.rows_affected() == 1)
}

/// Flip a layer-3 (`MemoryLayer::Skill`) row's metadata `trust` field via
/// `jsonb_set` (other metadata keys untouched). Layer-guarded so an
/// L0/L1/L2 id — or a non-existent id — is a no-op. Returns `true` iff a
/// row was updated. Takes a `&str` trust value: the `db` crate sits below
/// `core` and cannot depend on the `core`-owned `SkillTrust` enum.
pub async fn set_skill_trust<'e, E>(
    executor: E,
    id: i64,
    trust: &str,
) -> Result<bool, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let rows = sqlx::query(
        "UPDATE memories \
         SET metadata = jsonb_set(metadata, '{trust}', to_jsonb($2::text), true) \
         WHERE id = $1 AND layer = $3",
    )
    .bind(id)
    .bind(trust)
    .bind(MemoryLayer::Skill.as_db())
    .execute(executor)
    .await
    .map_err(|e| DbError::Query(format!("set_skill_trust id={id}: {e}")))?;
    Ok(rows.rows_affected() == 1)
}

/// Insert an L0 (meta-rule) memory row.
///
/// Separate from [`insert_memory_at_layer`] on purpose: L0 rows are
/// hard agent-constraints (e.g. "never `rm -rf`") and a `grep` for this
/// function name is the auditable record of every place the codebase
/// chose to grow L0. The agent loop must not call this — only operator
/// tooling / migrations / seed scripts should.
///
/// The body of this function is intentionally a thin pass-through to
/// the shared writer; the value-add is the named entry point.
pub async fn seed_meta_memory<'e, E>(
    executor: E,
    body: &str,
    metadata: &serde_json::Value,
    embedding: Option<&[f32]>,
) -> Result<i64, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    insert_row_at_layer_unchecked(executor, body, metadata, embedding, MemoryLayer::Meta).await
}

/// Internal writer shared by [`insert_memory_at_layer`] and
/// [`seed_meta_memory`]. Bypasses the L0 policy check — callers above
/// are responsible for upholding the policy.
async fn insert_row_at_layer_unchecked<'e, E>(
    executor: E,
    body: &str,
    metadata: &serde_json::Value,
    embedding: Option<&[f32]>,
    layer: MemoryLayer,
) -> Result<i64, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    if let Some(v) = embedding {
        check_embedding_dim("insert", v)?;
    }

    // Two SQL shapes (with vs. without embedding) — same rationale as
    // [`insert_memory`]: NULL-vector casts work, but the planner's
    // decision tree is simpler when the column reference is a literal
    // column. The `layer` bind is added to both shapes.
    let row = if let Some(v) = embedding {
        let lit = vector_literal(v);
        sqlx::query(
            "INSERT INTO memories (body, metadata, embedding, layer) \
             VALUES ($1, $2, $3::vector, $4) RETURNING id",
        )
        .bind(body)
        .bind(metadata)
        .bind(lit)
        .bind(layer.as_db())
        .fetch_one(executor)
        .await
    } else {
        sqlx::query(
            "INSERT INTO memories (body, metadata, layer) \
             VALUES ($1, $2, $3) RETURNING id",
        )
        .bind(body)
        .bind(metadata)
        .bind(layer.as_db())
        .fetch_one(executor)
        .await
    }
    .map_err(|e| DbError::Query(format!("insert memory at layer {layer:?}: {e}")))?;
    row.try_get::<i64, _>(0)
        .map_err(|e| DbError::Query(format!("decode memory.id: {e}")))
}
