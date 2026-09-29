//! Read-path helpers for the `memories` table — three of the four
//! recall lanes (semantic / lexical / graph; the fourth,
//! entity-similarity, is `crate::entity_embedding`) and the
//! order-preserving hydration of ranked id-lists. The layer-load
//! queries moved to the sibling `layer_load` module (2026-09-29).
//!
//! Split out of the parent [`crate::memories`] module (2026-05-30) to
//! keep each file under the 500-LOC cap. Every public function here is
//! re-exported from the parent, so the call-site paths
//! `db::memories::{semantic_search, lexical_search, graph_search,
//! fetch_by_ids}` are byte-for-byte unchanged.
//!
//! Each `*_search` helper returns a `Vec<i64>` of memory ids in
//! best-first order; the RRF fusion over those ranked id-lists is a
//! pure function in `core::memory`. Shared vocabulary (the dim-check,
//! the `limit_as_i64` saturating cast, the [`vector_literal`] encoder,
//! the [`Memory`] row and [`MemoryLayer`] enum) lives in the parent and
//! is imported below via `super::`.

use sqlx::Row;

use crate::DbError;

use super::{
    check_embedding_dim, limit_as_i64, recallable_layer_codes, vector_literal, Memory, MemoryLayer,
};

/// Semantic recall: nearest-neighbour search over `memories.embedding`
/// using pgvector's cosine-distance operator (`<=>`).
///
/// Returns up to `k` memory ids in best-first order (smallest cosine
/// distance first). Rows with NULL embedding are filtered out at the
/// SQL level — they cannot participate in this lane. Pass `k = 0` to
/// get an empty result without round-tripping.
///
/// `query_embedding.len()` must equal [`EMBEDDING_DIM`](super::EMBEDDING_DIM).
///
/// **Layer gate (#785):** only layers for which
/// [`MemoryLayer::is_recallable`] holds are searched, so L0 (meta-rules)
/// and L3 (skills) rows never come back from this lane.
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
           AND layer = ANY($3) \
         ORDER BY embedding <=> $1::vector \
         LIMIT $2",
    )
    .bind(lit)
    .bind(limit_as_i64(k))
    .bind(recallable_layer_codes())
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
///
/// **Layer gate (#785):** only layers for which
/// [`MemoryLayer::is_recallable`] holds are searched, so L0 (meta-rules)
/// and L3 (skills) rows never come back from this lane.
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
           AND m.layer = ANY($3) \
         ORDER BY ts_rank(m.tsv, query) DESC, m.id ASC \
         LIMIT $2",
    )
    .bind(query_text)
    .bind(limit_as_i64(k))
    .bind(recallable_layer_codes())
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
///
/// **Layer gate (#785):** only layers for which
/// [`MemoryLayer::is_recallable`] holds are searched, so L0 (meta-rules)
/// and L3 (skills) rows never come back from this lane.
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
         JOIN memories m ON m.id = me.memory_id \
         WHERE me.entity_id = ANY($1::bigint[]) \
           AND ($3 OR e.quarantine = FALSE) \
           AND m.layer = ANY($4) \
         GROUP BY me.memory_id \
         ORDER BY COUNT(*) DESC, me.memory_id ASC \
         LIMIT $2",
    )
    .bind(entity_ids)
    .bind(limit_as_i64(k))
    .bind(include_quarantined)
    .bind(recallable_layer_codes())
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
