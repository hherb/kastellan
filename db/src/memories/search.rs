//! Read-path helpers for the `memories` table — the three per-lane
//! recall searches (semantic / lexical / graph), the order-preserving
//! hydration of ranked id-lists, and the layer-load queries.
//!
//! Split out of the parent [`crate::memories`] module (2026-05-30) to
//! keep each file under the 500-LOC cap. Every public function here is
//! re-exported from the parent, so the call-site paths
//! `db::memories::{semantic_search, lexical_search, graph_search,
//! fetch_by_ids, load_layer, load_active_l0}` are byte-for-byte
//! unchanged.
//!
//! Each `*_search` helper returns a `Vec<i64>` of memory ids in
//! best-first order; the RRF fusion over those ranked id-lists is a
//! pure function in `core::memory`. Shared vocabulary (the dim-check,
//! the `limit_as_i64` saturating cast, the [`vector_literal`] encoder,
//! the [`Memory`] row and [`MemoryLayer`] enum) lives in the parent and
//! is imported below via `super::`.

use sqlx::Row;

use crate::DbError;

use super::{check_embedding_dim, limit_as_i64, vector_literal, Memory, MemoryLayer};

/// Semantic recall: nearest-neighbour search over `memories.embedding`
/// using pgvector's cosine-distance operator (`<=>`).
///
/// Returns up to `k` memory ids in best-first order (smallest cosine
/// distance first). Rows with NULL embedding are filtered out at the
/// SQL level — they cannot participate in this lane. Pass `k = 0` to
/// get an empty result without round-tripping.
///
/// `query_embedding.len()` must equal [`EMBEDDING_DIM`](super::EMBEDDING_DIM).
pub async fn semantic_search<'e, E>(
    executor: E,
    query_embedding: &[f32],
    k: usize,
) -> Result<Vec<i64>, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    if k == 0 {
        return Ok(Vec::new());
    }
    check_embedding_dim("query", query_embedding)?;

    let lit = vector_literal(query_embedding);
    let rows = sqlx::query(
        "SELECT id \
         FROM memories \
         WHERE embedding IS NOT NULL \
         ORDER BY embedding <=> $1::vector \
         LIMIT $2",
    )
    .bind(lit)
    .bind(limit_as_i64(k))
    .fetch_all(executor)
    .await
    .map_err(|e| DbError::Query(format!("semantic_search: {e}")))?;

    rows.into_iter()
        .map(|r| {
            r.try_get::<i64, _>(0)
                .map_err(|e| DbError::Query(format!("decode memory.id: {e}")))
        })
        .collect()
}

/// Lexical recall: full-text search over `memories.tsv` using
/// `plainto_tsquery('simple', $1)` and `ts_rank` for ordering.
///
/// `plainto_tsquery` is the operator-friendly query parser — it
/// tokenises the input, drops stopwords (none in `'simple'` config),
/// and ANDs the remaining lexemes. We deliberately use `'simple'` to
/// match the column's `GENERATED ALWAYS AS (to_tsvector('simple',
/// body)) STORED` definition; mixing configurations would yield a
/// query that doesn't match any rows.
///
/// Returns up to `k` memory ids in best-first order (highest
/// `ts_rank` first). Documents with no overlapping lexemes don't appear
/// in the result set — they are excluded by the `tsv @@ query`
/// filter, not just ranked low.
pub async fn lexical_search<'e, E>(
    executor: E,
    query_text: &str,
    k: usize,
) -> Result<Vec<i64>, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    if k == 0 || query_text.trim().is_empty() {
        return Ok(Vec::new());
    }

    // The CROSS JOIN with `plainto_tsquery(...) AS query` materialises
    // the parsed query once per statement; subsequent references in
    // SELECT and ORDER BY share the same `tsquery` value. Doing it as
    // a subquery in WHERE would re-parse it for ts_rank.
    let rows = sqlx::query(
        "SELECT m.id \
         FROM memories m, plainto_tsquery('simple', $1) AS query \
         WHERE m.tsv @@ query \
         ORDER BY ts_rank(m.tsv, query) DESC, m.id ASC \
         LIMIT $2",
    )
    .bind(query_text)
    .bind(limit_as_i64(k))
    .fetch_all(executor)
    .await
    .map_err(|e| DbError::Query(format!("lexical_search: {e}")))?;

    rows.into_iter()
        .map(|r| {
            r.try_get::<i64, _>(0)
                .map_err(|e| DbError::Query(format!("decode memory.id: {e}")))
        })
        .collect()
}

/// Fetch the bodies + metadata for a list of memory ids, preserving
/// caller-supplied id order.
///
/// Recall returns ranked id-lists for memory; the final hydration step
/// looks up the bodies. We do this in one round-trip via
/// `WHERE id = ANY($1)` then re-sort the result client-side to match
/// `ids` — Postgres' `ANY` does not preserve input order, and adding a
/// `WITH ORDINALITY` join would obscure the simple shape for a marginal
/// win.
///
/// Ids that do not exist (e.g. the row was deleted between the lane
/// query and hydration) are silently skipped — the caller observes a
/// shorter list rather than an error, matching the
/// "ranked id-list" + "best-effort hydration" contract.
///
/// Duplicate ids in `ids` are deduped to the first occurrence: the
/// internal `HashMap::remove` strips the row on first lookup, so a
/// later occurrence finds nothing and is dropped. RRF (the only
/// production caller today) cannot produce duplicates because its
/// score map is keyed by id, but a future caller passing arbitrary
/// id lists should not rely on `fetch_by_ids` to expand them.
pub async fn fetch_by_ids<'e, E>(executor: E, ids: &[i64]) -> Result<Vec<Memory>, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    if ids.is_empty() {
        return Ok(Vec::new());
    }

    let rows = sqlx::query(
        "SELECT id, body, metadata, layer, created_at \
         FROM memories \
         WHERE id = ANY($1)",
    )
    .bind(ids)
    .fetch_all(executor)
    .await
    .map_err(|e| DbError::Query(format!("fetch_by_ids: {e}")))?;

    let mut by_id: std::collections::HashMap<i64, Memory> =
        std::collections::HashMap::with_capacity(rows.len());
    for r in rows {
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
        by_id.insert(id, Memory { id, body, metadata, layer, created_at });
    }

    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(m) = by_id.remove(id) {
            out.push(m);
        }
    }
    Ok(out)
}

/// Graph lane: rank memories by how many of the supplied entity ids
/// they're linked to.
///
/// Returns up to `k` memory ids in best-first order (highest hit count
/// first; ties broken by smaller id for stable ordering). `entity_ids`
/// is the *already-expanded* set (seeds + 1-hop neighbours); expansion
/// happens in `core::memory::recall`, not here, because graph
/// traversal goes through the [`crate::graph::Graph`] chokepoint.
///
/// Empty `entity_ids` → empty Vec, no SQL issued. Duplicates in
/// `entity_ids` are harmless: the PK on `memory_entities(memory_id,
/// entity_id)` guarantees one row per pair, so `COUNT(*)` is
/// equivalent to `COUNT(DISTINCT entity_id)` regardless of input
/// duplication. The caller's expansion logic should dedup via
/// `HashSet` anyway, but this helper does not enforce it.
pub async fn graph_search<'e, E>(
    executor: E,
    entity_ids: &[i64],
    k: usize,
    include_quarantined: bool,
) -> Result<Vec<i64>, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    if k == 0 || entity_ids.is_empty() {
        return Ok(Vec::new());
    }

    // JOIN entities + filter on quarantine. When include_quarantined
    // is TRUE, the predicate short-circuits via `OR $3` so the planner
    // skips the entity-table probe on the operator-CLI path.
    let rows = sqlx::query(
        "SELECT me.memory_id \
         FROM memory_entities me \
         JOIN entities e ON me.entity_id = e.id \
         WHERE me.entity_id = ANY($1::bigint[]) \
           AND ($3 OR e.quarantine = FALSE) \
         GROUP BY me.memory_id \
         ORDER BY COUNT(*) DESC, me.memory_id ASC \
         LIMIT $2",
    )
    .bind(entity_ids)
    .bind(limit_as_i64(k))
    .bind(include_quarantined)
    .fetch_all(executor)
    .await
    .map_err(|e| DbError::Query(format!("graph_search: {e}")))?;

    rows.into_iter()
        .map(|r| {
            r.try_get::<i64, _>(0)
                .map_err(|e| DbError::Query(format!("decode memory_id: {e}")))
        })
        .collect()
}

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
    for r in rows {
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
        out.push(Memory { id, body, metadata, layer, created_at });
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
