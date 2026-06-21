//! Read/write helpers for the `entities.embedding` column — the
//! entity-embedding **backfill** scan + guarded updater, and the
//! entity-similarity recall lane (issue: entity-embedding recall lane).
//!
//! Co-located here (rather than in the over-cap `entities.rs` /
//! `memories/search.rs`) so all three entity-embedding SQL helpers share
//! one focused, testable module. Every helper reuses the same dimension
//! chokepoint (`check_embedding_dim`) and `vector(256)` literal encoder
//! (`vector_literal`) the memories lane uses, so a backfilled entity vector
//! is byte-identical to what a future forward path would store.

use sqlx::Row;

use crate::memories::{check_embedding_dim, limit_as_i64, vector_literal};
use crate::DbError;

/// Scan every entity whose `embedding IS NULL`, returning `(id, kind, name)`
/// in ascending-id (stable, resumable) order.
///
/// Returns **all** NULL-embedding entities regardless of `quarantine`:
/// embedding is independent of review state (a quarantined entity may later
/// be approved, and we must not re-embed on approve), and embedding a
/// quarantined row leaks nothing — the recall lane filters quarantined rows
/// at query time. The caller composes the embed text from `(kind, name)`.
pub async fn load_unembedded_entities<'e, E>(
    executor: E,
) -> Result<Vec<(i64, String, String)>, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let rows = sqlx::query(
        "SELECT id, kind, name \
         FROM entities \
         WHERE embedding IS NULL \
         ORDER BY id",
    )
    .fetch_all(executor)
    .await
    .map_err(|e| DbError::Query(format!("load_unembedded_entities: {e}")))?;

    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        let id: i64 = r
            .try_get(0)
            .map_err(|e| DbError::Query(format!("decode entity.id: {e}")))?;
        let kind: String = r
            .try_get(1)
            .map_err(|e| DbError::Query(format!("decode entity.kind: {e}")))?;
        let name: String = r
            .try_get(2)
            .map_err(|e| DbError::Query(format!("decode entity.name: {e}")))?;
        out.push((id, kind, name));
    }
    Ok(out)
}

/// Write `embedding` for entity `id`, but **only if it is still NULL**.
///
/// The `embedding IS NULL` guard makes the write idempotent + race-safe: a
/// row embedded concurrently (by a parallel backfill, or a future forward
/// path) no-ops and returns `false`. Returns `true` iff exactly one row was
/// updated. Dimension-checked before the write — a wrong-width vector is a
/// hard `DbError`, never silently stored. Byte-for-byte mirror of
/// `memories::set_embedding`.
pub async fn set_entity_embedding<'e, E>(
    executor: E,
    id: i64,
    embedding: &[f32],
) -> Result<bool, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    check_embedding_dim("set_entity_embedding", embedding)?;

    let lit = vector_literal(embedding);
    let res = sqlx::query(
        "UPDATE entities \
         SET embedding = $1::vector \
         WHERE id = $2 AND embedding IS NULL",
    )
    .bind(lit)
    .bind(id)
    .execute(executor)
    .await
    .map_err(|e| DbError::Query(format!("set_entity_embedding id={id}: {e}")))?;
    Ok(res.rows_affected() == 1)
}

/// Entity-similarity recall lane: the memories linked to the entities nearest
/// the query embedding.
///
/// Two stages in one statement: (1) the `entity_fanout` entities with the
/// smallest cosine distance (`<=>`) to `query_embedding`, restricted to
/// embedded, non-quarantined rows (unless `include_quarantined`); (2) the
/// memories linked to those entities via `memory_entities`, ranked by each
/// memory's *closest* matching entity (`MIN(dist)`), id-tiebroken for stable
/// order, capped at `k`.
///
/// `include_quarantined = false` is the production posture — it preserves the
/// invariant that operator-unreviewed entities never surface memories into
/// recall (mirrors `memories::graph_search`). The operator CLI may pass
/// `true`.
///
/// `k == 0` → empty, no SQL. `query_embedding.len()` must equal
/// `EMBEDDING_DIM` (hard `DbError`, not a degrade case). An empty result
/// (no embedded/approved entities yet) is normal — the lane simply
/// contributes nothing to fusion.
pub async fn entity_similarity_search<'e, E>(
    executor: E,
    query_embedding: &[f32],
    entity_fanout: i64,
    k: usize,
    include_quarantined: bool,
) -> Result<Vec<i64>, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    if k == 0 {
        return Ok(Vec::new());
    }
    check_embedding_dim("entity query", query_embedding)?;

    let lit = vector_literal(query_embedding);
    let rows = sqlx::query(
        "SELECT me.memory_id \
         FROM ( \
             SELECT id, embedding <=> $1::vector AS dist \
             FROM entities \
             WHERE embedding IS NOT NULL \
               AND ($4 OR quarantine = FALSE) \
             ORDER BY dist \
             LIMIT $2 \
         ) top_e \
         JOIN memory_entities me ON me.entity_id = top_e.id \
         GROUP BY me.memory_id \
         ORDER BY MIN(top_e.dist) ASC, me.memory_id ASC \
         LIMIT $3",
    )
    .bind(lit)
    .bind(entity_fanout)
    .bind(limit_as_i64(k))
    .bind(include_quarantined)
    .fetch_all(executor)
    .await
    .map_err(|e| DbError::Query(format!("entity_similarity_search: {e}")))?;

    rows.into_iter()
        .map(|r| {
            r.try_get::<i64, _>(0)
                .map_err(|e| DbError::Query(format!("decode memory_id: {e}")))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memories::EMBEDDING_DIM;

    /// `set_entity_embedding` rejects a wrong-dimension vector *before* any
    /// I/O — the dim contract is a hard gate, not a degrade case. A lazy
    /// (never-connected) pool proves no round-trip happens on the reject path.
    #[tokio::test]
    async fn set_entity_embedding_rejects_wrong_dim() {
        let pool = sqlx::PgPool::connect_lazy(
            "postgres://invalid:invalid@127.0.0.1:1/nonexistent",
        )
        .expect("lazy pool");
        // EMBEDDING_DIM - 1 components: too short, must be rejected up front.
        let short = vec![0.0f32; EMBEDDING_DIM - 1];
        let err = set_entity_embedding(&pool, 1, &short).await;
        assert!(err.is_err(), "wrong-dim vector must be rejected before I/O");
    }

    /// `k == 0` is a fast-path no-op: returns empty without issuing SQL, so a
    /// lazy pool never connects. (The behaviour against real rows is covered
    /// by `core/tests/entity_reembed_e2e.rs`.)
    #[tokio::test]
    async fn entity_similarity_search_k_zero_is_empty_no_sql() {
        let pool = sqlx::PgPool::connect_lazy(
            "postgres://invalid:invalid@127.0.0.1:1/nonexistent",
        )
        .expect("lazy pool");
        let q = vec![0.0f32; EMBEDDING_DIM];
        let out = entity_similarity_search(&pool, &q, 64, 0, false)
            .await
            .expect("k==0 returns Ok(empty) with no round-trip");
        assert!(out.is_empty());
    }

    /// A wrong-dimension query embedding is rejected before any I/O (mirrors
    /// the semantic lane's hard dim contract).
    #[tokio::test]
    async fn entity_similarity_search_rejects_wrong_dim() {
        let pool = sqlx::PgPool::connect_lazy(
            "postgres://invalid:invalid@127.0.0.1:1/nonexistent",
        )
        .expect("lazy pool");
        let short = vec![0.0f32; EMBEDDING_DIM - 1];
        assert!(
            entity_similarity_search(&pool, &short, 64, 10, false).await.is_err(),
            "wrong-dim query embedding must be rejected before I/O"
        );
    }
}
