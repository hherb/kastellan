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
//! ## Phase-1 surface
//!
//! * **Graph lane.** Shipped 2026-05-12. The `memory_entities` join
//!   table (migration 0007) backs entity↔memory linkage; the
//!   writer-side helper [`link_memory_to_entities`] and the read-side
//!   helper [`graph_search`] live in this module. The 1-hop outbound
//!   expansion (via the `db::graph::Graph` chokepoint) happens in
//!   `core::memory::recall`. Future entity-similarity over
//!   `entities.embedding` (still NULL today) is a separate Phase-1
//!   follow-up.
//! * **Embedding worker.** `insert_memory` accepts an `Option<&[f32]>`
//!   and stores NULL when absent. `embed_query` shipped via Option O
//!   in `core::memory::embed`; the production caller routes the body
//!   through the embedding worker before inserting. Tests use the
//!   deterministic SHA-256-seeded helper documented in
//!   `core/tests/memory_recall_e2e.rs`.

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

/// Memory hierarchy layers, mirroring GenericAgent's 5-layer design.
///
/// Discriminant values 0..=4 match the SMALLINT stored in
/// `memories.layer` and `deleted_memories.layer` (migrations 0013 +
/// 0014). The CHECK constraint at the DB boundary guarantees no other
/// value is ever read back, so [`MemoryLayer::from_db`] only needs to
/// defend against a corrupted-row case; production code paths never
/// trip it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i16)]
pub enum MemoryLayer {
    /// L0 — meta-rules / hard constraints (e.g. "never `rm -rf`").
    /// Hand-curated seed data only; never written by the agent itself.
    /// [`insert_memory_at_layer`] **rejects** this variant with
    /// [`DbError::PolicyViolation`]; the only writer path is
    /// [`seed_meta_memory`], deliberately named so a `grep` over the
    /// tree surfaces every L0 write site.
    Meta = 0,
    /// L1 — insight index. Small routing pointers loaded
    /// unconditionally into every system prompt by
    /// `core::memory::layers::load_l1`. The whole point of the layer
    /// is "fits in the prompt regardless of similarity score."
    Index = 1,
    /// L2 — stable accumulated facts. Default for [`insert_memory`]
    /// and the layer every pre-migration row backfills to.
    Stable = 2,
    /// L3 — skills / SOPs (parameterised procedures). Reserved; no
    /// writer in the slice that introduced this enum.
    Skill = 3,
    /// L4 — session digests. Reserved; no writer in the slice that
    /// introduced this enum.
    Digest = 4,
}

impl MemoryLayer {
    /// Decode the SMALLINT stored in `memories.layer` / `deleted_memories.layer`.
    ///
    /// The DB CHECK constraint forbids out-of-range values, so this
    /// only returns `Err` if the column was tampered with via a path
    /// that bypassed the constraint (e.g. a future migration with a
    /// bug). The error type is [`DbError::Invariant`] specifically
    /// because hitting it means the schema invariant was broken —
    /// not a transient query failure.
    pub fn from_db(raw: i16) -> Result<Self, DbError> {
        match raw {
            0 => Ok(Self::Meta),
            1 => Ok(Self::Index),
            2 => Ok(Self::Stable),
            3 => Ok(Self::Skill),
            4 => Ok(Self::Digest),
            other => Err(DbError::Invariant(format!(
                "memory layer out of range: {other}"
            ))),
        }
    }

    /// Encode the layer as the SMALLINT value bound to SQL parameters.
    /// Pair with [`Self::from_db`] for round-trips.
    pub fn as_db(self) -> i16 {
        self as i16
    }
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
    /// Memory hierarchy layer (migrations 0013 + 0014). Defaults to
    /// [`MemoryLayer::Stable`] at the DB level for any row inserted
    /// without an explicit layer; [`insert_memory_at_layer`] is the
    /// writer-side helper for non-default layers.
    pub layer: MemoryLayer,
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
) -> Result<Vec<i64>, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    if k == 0 || entity_ids.is_empty() {
        return Ok(Vec::new());
    }

    let rows = sqlx::query(
        "SELECT memory_id \
         FROM memory_entities \
         WHERE entity_id = ANY($1::bigint[]) \
         GROUP BY memory_id \
         ORDER BY COUNT(*) DESC, memory_id ASC \
         LIMIT $2",
    )
    .bind(entity_ids)
    .bind(limit_as_i64(k))
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
        use sqlx::Row;
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
