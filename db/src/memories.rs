//! Typed sqlx helpers for the `memories` table.
//!
//! ## What this module owns
//!
//! Every read and write of `memories` goes through one of the helpers
//! below — same chokepoint discipline `db::audit` and `db::secrets`
//! follow. Outside callers (today: `core::memory::recall`) never write
//! raw SQL against the table. Two payoffs:
//!
//!   1. The `vector(1024)` bind shape lives in *one* place. We choose
//!      to encode embeddings as their canonical Postgres-text form
//!      (`'[0.12, 0.34, ...]'::vector`) rather than pull in the
//!      `pgvector` Rust crate. The reasons are documented on
//!      [`vector_literal`]; if a future call site grows enough
//!      embedding-traffic to make the dep worthwhile, the swap is
//!      strictly local.
//!   2. Fusion (RRF) and per-lane retrieval are decoupled. Each `*_search`
//!      helper returns a `Vec<i64>` of memory ids in best-first order;
//!      the fusion in `core::memory` is then a pure function over those
//!      ranked id-lists. That makes the fusion unit-testable without a
//!      DB and pins the per-lane shape to "ranked id-list," which is
//!      exactly what RRF needs.
//!
//! ## Why no HNSW index in this slice
//!
//! `0001_init.sql` deliberately omits the HNSW index on
//! `memories.embedding`; HNSW build cost is dominated by the row count
//! at index-creation time, so building against an empty table just to
//! grow it row-by-row is strictly worse than building once after the
//! first batch ingest. Phase 1's first-load step is where the index
//! materialises. Until then the `<=>` cosine-distance ORDER BY is a
//! sequential scan, which is fine at the corpus sizes this slice is
//! exercised against (the integration test seeds 3 rows).
//!
//! ## Phase-1 holes deliberately left
//!
//! * **Graph lane.** "Three independent score lists, fused per-call"
//!   originally included a graph traversal over `entities`/`relations`,
//!   but the schema has no entity-to-memory linkage today. Adding one
//!   (likely `memories.metadata->>'entities'` with a GIN-indexed JSONB
//!   array, or a join table) is a separate design decision and a
//!   separate slice. This module ships the two lanes (semantic +
//!   lexical) that the existing schema already supports.
//! * **Embedding worker.** `insert_memory` accepts an `Option<&[f32]>`
//!   and stores NULL when absent. The first production caller will
//!   route the body through the (future) embedding worker before
//!   inserting. Tests use the deterministic SHA-256-seeded helper
//!   documented in `core/tests/memory_recall_e2e.rs`.

use std::fmt::Write as _;

use sqlx::Row;

use crate::DbError;

/// Required dimensionality of every embedding written to `memories`.
///
/// Pinned by `0001_init.sql`'s `vector(1024)` column type — bge-m3's
/// natural output dim. A mismatch surfaces as a Postgres error at
/// INSERT time (`expected 1024 dimensions, not <N>`); the
/// application-layer check in [`insert_memory`] catches it earlier
/// with an operator-readable message.
pub const EMBEDDING_DIM: usize = 1024;

/// Default fusion budget when a caller hasn't specified one.
///
/// 10 is the order-of-magnitude that Phase 1's scheduler will start
/// with — small enough that the LLM's context budget is undisturbed,
/// large enough that RRF has multiple candidates per lane to fuse.
pub const DEFAULT_RECALL_K: usize = 10;

/// Reject embeddings whose length doesn't match [`EMBEDDING_DIM`].
///
/// Shared by [`insert_memory`] (write path) and [`semantic_search`]
/// (read path) so the operator-readable error message is identical at
/// both ends. The check fires before any sqlx call, so unit tests can
/// exercise it without a live executor.
fn check_embedding_dim(label: &str, v: &[f32]) -> Result<(), DbError> {
    if v.len() != EMBEDDING_DIM {
        return Err(DbError::Query(format!(
            "{label} embedding dim mismatch: got {}, expected {}",
            v.len(),
            EMBEDDING_DIM
        )));
    }
    Ok(())
}

/// `usize` → `i64` for SQL `LIMIT` binds. Saturates at `i64::MAX`
/// rather than wrapping to a negative value (which Postgres would
/// reject with a runtime error far from the call site).
fn limit_as_i64(k: usize) -> i64 {
    i64::try_from(k).unwrap_or(i64::MAX)
}

/// One row from `memories` returned from a fully hydrated query.
///
/// `embedding` is intentionally NOT decoded back into a `Vec<f32>` —
/// callers that need the raw vector should be retrieving it through a
/// dedicated path that opts in to the (future) `pgvector` Rust crate's
/// decode. Recall does not need the bytes; the column existence is
/// enough.
#[derive(Clone, Debug)]
pub struct Memory {
    /// Strictly monotonic `BIGSERIAL` from the table.
    pub id: i64,
    /// Free-form body. Phase 1's scheduler renders this into the
    /// LLM context.
    pub body: String,
    /// JSONB metadata. Phase 1's caller may store workspace, channel,
    /// source URL, originator entity, etc. The schema enforces no
    /// shape — that's by design.
    pub metadata: serde_json::Value,
    /// `now()`-derived insertion timestamp. The recall path returns it
    /// unsorted (the caller may sort by recency as a tiebreaker
    /// downstream).
    pub created_at: time::OffsetDateTime,
}

/// Insert a new memory row and return its id.
///
/// `embedding` may be `None` (the column is nullable today); when
/// `Some`, its length MUST equal [`EMBEDDING_DIM`] — the wrapped helper
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

/// Semantic recall: nearest-neighbour search over `memories.embedding`
/// using pgvector's cosine-distance operator (`<=>`).
///
/// Returns up to `k` memory ids in best-first order (smallest cosine
/// distance first). Rows with NULL embedding are filtered out at the
/// SQL level — they cannot participate in this lane. Pass `k = 0` to
/// get an empty result without round-tripping.
///
/// `query_embedding.len()` must equal [`EMBEDDING_DIM`].
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
        "SELECT id, body, metadata, created_at \
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
        let created_at: time::OffsetDateTime = r
            .try_get(3)
            .map_err(|e| DbError::Query(format!("decode memory.created_at: {e}")))?;
        by_id.insert(id, Memory { id, body, metadata, created_at });
    }

    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(m) = by_id.remove(id) {
            out.push(m);
        }
    }
    Ok(out)
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

/// Format a `Vec<f32>` as the canonical pgvector text representation.
///
/// pgvector's text input format is `[v0,v1,...,vN-1]` with a trailing
/// `]`, no whitespace, and standard floating-point literals. The
/// extension's parser accepts both decimal (`0.5`) and scientific
/// (`5e-1`) forms; we delegate to Rust's `f32::Display`, which emits
/// the shortest round-trippable representation — usually decimal for
/// human-scale magnitudes (`0.5`, `-1.25`) but scientific for very
/// small or very large values (`1e-10`, `3.4e38`). Both forms are
/// accepted by pgvector and round-trip losslessly, so the choice is
/// invisible to correctness; the only operator-visible effect is the
/// shape of values they read in EXPLAIN.
///
/// **Why text-cast and not the `pgvector` Rust crate.** The crate
/// wraps the same string round-trip with stronger types and a sqlx
/// `Encode`/`Decode` impl. We avoid the dep for two reasons:
///
///   1. **Dep audit surface.** Every workspace dep is licence-checked
///      and pulled in across all build targets. The `pgvector` crate
///      is MIT (AGPL-compatible) but pulls `byteorder` and an extra
///      sqlx-feature shim; until a second consumer needs decode, the
///      text-cast is strictly cheaper.
///   2. **Throughput shape.** Phase-0 scale: a handful of recall calls
///      per minute. The cost is dominated by the network round-trip
///      and the index lookup, not the formatter.
///
/// When the embedding worker (Phase 1+) lands and starts streaming
/// vectors at higher rates, swap this for `pgvector::Vector::from(v)`
/// + `.bind(...)`. The swap is strictly local to this module.
///
/// Pure: no I/O, deterministic — same input, same string every call.
pub fn vector_literal(v: &[f32]) -> String {
    // Heuristic capacity: each f32 prints to ~10 chars on average; the
    // exact value doesn't matter for correctness, just allocation
    // pressure on a hot path.
    let mut s = String::with_capacity(v.len() * 10 + 2);
    s.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        // `f32` Display gives the shortest round-trippable
        // representation (decimal for human-scale values, scientific
        // for very small/large) — both are valid pgvector input.
        // NaN/Inf produce strings pgvector rejects, but we never
        // expect those: embeddings come from a normalised model
        // output and are pre-validated by the embedding worker.
        // Defense in depth: a future caller that introduces
        // unsanitised floats will get a clear pgvector error at
        // INSERT time, not silent corruption.
        write!(&mut s, "{}", x).expect("write to String cannot fail");
    }
    s.push(']');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin the embedding dim. Cluster-side `vector(1024)` and Rust-side
    /// constant must agree; if either drifts the integration test will
    /// trip immediately.
    #[test]
    fn embedding_dim_is_1024() {
        assert_eq!(EMBEDDING_DIM, 1024);
    }

    /// Default fusion budget is non-zero. A zero default would let a
    /// caller construct an "empty modes" recall that returns nothing
    /// without an obvious cause — we'd rather force the explicit
    /// passing of `k = 0` if someone genuinely wants that.
    #[test]
    fn default_recall_k_is_at_least_one() {
        assert!(DEFAULT_RECALL_K >= 1);
    }

    /// Empty vector is still valid input — pgvector rejects it at
    /// INSERT (it expects exactly 1024 dimensions), but the formatter
    /// emits the canonical `[]` shape regardless. Rejecting it here
    /// would mask the operator-readable error.
    #[test]
    fn vector_literal_handles_empty_slice() {
        assert_eq!(vector_literal(&[]), "[]");
    }

    /// Single-element shape. The bracket-comma shape with no trailing
    /// comma matches the pgvector parser's expectation.
    #[test]
    fn vector_literal_single_element_no_trailing_comma() {
        assert_eq!(vector_literal(&[0.5]), "[0.5]");
    }

    /// Multi-element ordering preserved. `<=>` similarity is sensitive
    /// to position, so a permutation of the input would silently corrupt
    /// the cosine distance — this test pins the order.
    #[test]
    fn vector_literal_preserves_order() {
        let v = [1.0_f32, 2.0, 3.0];
        assert_eq!(vector_literal(&v), "[1,2,3]");
    }

    /// Negative values flow through verbatim. Embedding components are
    /// signed; if a refactor ever tried to abs() them the cosine
    /// similarity would be silently wrong. (Defensive: caught by
    /// integration test too, but this is faster feedback.)
    #[test]
    fn vector_literal_passes_through_negatives() {
        assert_eq!(vector_literal(&[-0.5_f32, 0.5]), "[-0.5,0.5]");
    }

    /// Dim-check shape pin: the shared helper rejects a too-short
    /// vector with a `Query` error whose message names both expected
    /// and actual dim, plus the call-site label so an operator can
    /// tell INSERT-side from query-side errors apart. Pure — runs
    /// without a DB. Both `insert_memory` and `semantic_search` route
    /// through this same helper, so this is the real production path.
    #[test]
    fn check_embedding_dim_rejects_too_short() {
        let too_short: Vec<f32> = vec![0.0; 10];
        let err = check_embedding_dim("insert", &too_short).unwrap_err();
        match err {
            DbError::Query(msg) => {
                assert!(msg.contains("dim mismatch"), "msg: {msg}");
                assert!(msg.contains("insert"), "label missing in: {msg}");
                assert!(msg.contains("10"), "got-dim missing in: {msg}");
                assert!(msg.contains("1024"), "expected-dim missing in: {msg}");
            }
            other => panic!("expected DbError::Query, got {other:?}"),
        }
    }

    /// Same helper accepts an exact-length input.
    #[test]
    fn check_embedding_dim_accepts_correct_length() {
        let ok: Vec<f32> = vec![0.0; EMBEDDING_DIM];
        check_embedding_dim("query", &ok).expect("exact-length input must pass");
    }

    /// `limit_as_i64` saturates at `i64::MAX` rather than wrapping.
    /// Realistic `k` values (≤ a few hundred) flow through unchanged;
    /// the saturation is defense-in-depth against a future caller
    /// passing an unreasonably large `k` from a config file.
    #[test]
    fn limit_as_i64_saturates_at_i64_max() {
        assert_eq!(limit_as_i64(0), 0);
        assert_eq!(limit_as_i64(40), 40);
        assert_eq!(limit_as_i64(usize::MAX), i64::MAX);
    }
}
