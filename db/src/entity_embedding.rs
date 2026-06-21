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

use crate::memories::{check_embedding_dim, vector_literal};
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
}
