//! Knowledge-graph abstraction over the relational `entities` +
//! `relations` tables.
//!
//! ## Why the abstraction
//!
//! The roadmap commits to *relational* storage for nodes and edges
//! (closed issue #9 — Apache AGE deferred won't-fix). The trade-off
//! we accepted: we get to keep pgvector + tsvector indexing on the
//! same table, and we accept that variable-length traversal goes
//! through recursive CTEs instead of native graph syntax. To stop
//! that decision from leaking everywhere, *every* graph operation in
//! `core` (and any future worker that needs the graph) goes through
//! [`Graph`]. That gives us one chokepoint to swap for AGE / Neo4j /
//! Memgraph if a measured bottleneck ever materialises — same
//! discipline as `tool_host::dispatch()` is for tools.
//!
//! ## What's NOT here yet
//!
//! - **Embeddings.** `entities.embedding` is `vector(1024)` in the
//!   schema, but writing/reading it requires either the `pgvector`
//!   crate or a `::vector`-cast text representation. Phase 1 picks
//!   one when the embedding worker lands; until then `upsert_entity`
//!   leaves the column NULL.
//! - **Subgraph extract / GraphML export.** Filed for whenever an
//!   actual call site asks for it — premature today.
//!
//! ## Cycle handling in `path`
//!
//! The recursive CTE in [`PgGraph::path`] tracks the visited-set in
//! the row (`visited` array) and joins with `NOT (r.dst_id = ANY(...))`
//! to refuse to re-enter a node. Without that guard a cycle in the
//! graph (`A -> B -> A`) would diverge.

use sqlx::Row;

use crate::DbError;

/// Decode an `entities`-shaped row into an [`Entity`].
///
/// All four `try_get` sites had near-identical wording before; centralising
/// keeps the error strings consistent (so a `grep` on operator logs lands
/// in one place) and means a future column rename only needs to touch one
/// site. Caller is responsible for selecting columns in the order
/// `(id, kind, name, attrs)`.
fn decode_entity(row: &sqlx::postgres::PgRow) -> Result<Entity, DbError> {
    Ok(Entity {
        id: row
            .try_get(0)
            .map_err(|e| DbError::Query(format!("decode entity.id: {e}")))?,
        kind: row
            .try_get(1)
            .map_err(|e| DbError::Query(format!("decode entity.kind: {e}")))?,
        name: row
            .try_get(2)
            .map_err(|e| DbError::Query(format!("decode entity.name: {e}")))?,
        attrs: row
            .try_get(3)
            .map_err(|e| DbError::Query(format!("decode entity.attrs: {e}")))?,
    })
}

/// Decode a row from the `walk_*_edges` recursive-CTE SELECT into a
/// [`WalkedEdge`]. The SELECT projects columns in this order:
///
/// `(depth, edge_id, src_id, src_kind, src_name, src_quarantine,
///   dst_id, dst_kind, dst_name, dst_quarantine, kind)`.
///
/// `depth` is read as `i32` (Postgres has no native `u8`) and narrowed
/// via [`u8::try_from`]. `max_depth` is capped by [`MAX_WALK_DEPTH`] in
/// callers so `depth <= MAX_WALK_DEPTH <= u8::MAX` always — but we
/// enforce the invariant at the boundary with a checked conversion so a
/// future direct caller that synthesizes a row with an out-of-range
/// depth surfaces a typed error instead of silently truncating.
fn decode_walked_edge(row: &sqlx::postgres::PgRow) -> Result<WalkedEdge, DbError> {
    let depth_i32: i32 = row
        .try_get(0)
        .map_err(|e| DbError::Query(format!("decode walked_edge.depth: {e}")))?;
    let depth = u8::try_from(depth_i32).map_err(|_| {
        DbError::Query(format!(
            "decode walked_edge.depth: value {depth_i32} out of range 0..=255 \
             (CTE depth counter is bounded by MAX_WALK_DEPTH={MAX_WALK_DEPTH})",
        ))
    })?;
    Ok(WalkedEdge {
        depth,
        edge_id: row
            .try_get(1)
            .map_err(|e| DbError::Query(format!("decode walked_edge.edge_id: {e}")))?,
        src_id: row
            .try_get(2)
            .map_err(|e| DbError::Query(format!("decode walked_edge.src_id: {e}")))?,
        src_kind: row
            .try_get(3)
            .map_err(|e| DbError::Query(format!("decode walked_edge.src_kind: {e}")))?,
        src_name: row
            .try_get(4)
            .map_err(|e| DbError::Query(format!("decode walked_edge.src_name: {e}")))?,
        src_quarantine: row
            .try_get(5)
            .map_err(|e| DbError::Query(format!("decode walked_edge.src_quarantine: {e}")))?,
        dst_id: row
            .try_get(6)
            .map_err(|e| DbError::Query(format!("decode walked_edge.dst_id: {e}")))?,
        dst_kind: row
            .try_get(7)
            .map_err(|e| DbError::Query(format!("decode walked_edge.dst_kind: {e}")))?,
        dst_name: row
            .try_get(8)
            .map_err(|e| DbError::Query(format!("decode walked_edge.dst_name: {e}")))?,
        dst_quarantine: row
            .try_get(9)
            .map_err(|e| DbError::Query(format!("decode walked_edge.dst_quarantine: {e}")))?,
        kind: row
            .try_get(10)
            .map_err(|e| DbError::Query(format!("decode walked_edge.kind: {e}")))?,
    })
}

/// Column index of the trailing `direction` discriminant column appended
/// by [`Graph::walk_edges_around`]'s UNION ALL projection. Sits one
/// column past the 11 columns (0..=10) that [`decode_walked_edge`]
/// consumes — keep this in sync with both the SQL projection and
/// `decode_walked_edge`'s column contract. A future change inserting a
/// column between `kind` (col 10) and `direction` must bump this
/// constant; on accidental drift, the `try_get::<String, _>` decode
/// surfaces a typed error rather than silently misreading a `bigint` or
/// `bool` as a `text` direction value.
const WALK_EDGES_AROUND_DIRECTION_COL: usize = 11;

/// Hard ceiling on `max_depth` accepted by [`Graph::walk_outbound_edges`]
/// and [`Graph::walk_inbound_edges`]. Matches the budget convention used
/// by [`Graph::path`] (its `max_hops` is a `u8` callers typically pass
/// as 5 or below). The cap exists to keep the recursive CTE's worst-case
/// row count bounded — at depth 5 on a 10-fan-out graph we already see
/// 10^5 = 100_000 rows before the `LIMIT` clause clips them.
///
/// `max_depth` strictly greater than this constant is clamped down by
/// the impl (with a `tracing::warn!`) rather than rejected; the
/// operator-facing CLI takes a `u8` from the command line, and a sharp
/// cap-vs-error boundary in the protocol would force every consumer to
/// re-implement the same clamping logic.
pub const MAX_WALK_DEPTH: u8 = 5;

/// A node in the knowledge graph. The `id` is the BIGSERIAL primary
/// key from `entities`; the `(kind, name)` pair is the natural key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entity {
    pub id: i64,
    pub kind: String,
    pub name: String,
    pub attrs: serde_json::Value,
}

/// An edge between two entities.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Relation {
    pub id: i64,
    pub src_id: i64,
    pub dst_id: i64,
    pub kind: String,
    pub attrs: serde_json::Value,
}

/// One edge walked by [`Graph::walk_outbound_edges`] or
/// [`Graph::walk_inbound_edges`], carrying *both* endpoints' natural
/// keys plus the edge kind plus the hop depth.
///
/// `depth` is `1` for direct neighbours of the seed (the edge between
/// the seed and a 1-hop neighbour), `2` for two-hop, etc. It is never
/// `0` — the seed itself is never returned as an "edge".
///
/// `src_quarantine` / `dst_quarantine` carry the `entities.quarantine`
/// column for each endpoint so the operator-facing `relations show`
/// command can flag quarantined entities visually (eg. with `[Q]`)
/// without an extra round-trip.
///
/// This struct is deliberately self-contained: it duplicates `kind`/
/// `name` from the joined `entities` rows so that downstream rendering
/// is a pure transformation of `Vec<WalkedEdge>` and doesn't need a
/// secondary lookup table to translate ids to display strings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalkedEdge {
    pub depth: u8,
    pub edge_id: i64,
    pub src_id: i64,
    pub src_kind: String,
    pub src_name: String,
    pub src_quarantine: bool,
    pub dst_id: i64,
    pub dst_kind: String,
    pub dst_name: String,
    pub dst_quarantine: bool,
    pub kind: String,
}

/// Return shape for [`Graph::walk_edges_around`]: the seed's outbound
/// and inbound walks, pre-partitioned. Each field carries
/// [`WalkedEdge`]s in `(depth ASC, edge_id ASC)` order, with the same
/// shortest-path-per-unique-edge dedupe semantics as
/// [`Graph::walk_outbound_edges`] / [`Graph::walk_inbound_edges`].
///
/// Returning a pre-partitioned struct (rather than a
/// `Vec<(Direction, WalkedEdge)>`) means consumers like
/// `relations show` consume each list directly without an extra
/// partition step. The trade-off — a future consumer that wants
/// interleaved ordering across both directions would have to merge
/// the two `Vec`s itself — is fine: every consumer we have today
/// renders the two directions as separate output sections.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct EdgesAround {
    pub outbound: Vec<WalkedEdge>,
    pub inbound: Vec<WalkedEdge>,
}

/// Read/write surface for the knowledge graph.
///
/// All graph traffic in `core` and downstream workers goes through
/// this trait — no module outside `db` writes raw SQL against
/// `entities` or `relations`. See module docs for the rationale.
///
/// Async-fn-in-trait (Rust 1.75+) is used directly instead of
/// `async-trait` to avoid the `Box<Pin<...>>` allocation for every
/// call. The cost: trait objects (`dyn Graph`) are not directly
/// usable; if Phase 1 needs them, wrap with an explicit
/// `async-trait` shim at the call site.
pub trait Graph {
    /// Insert-or-update by `(kind, name_norm)`. Returns the entity's id.
    /// `attrs` overwrites on conflict (no JSONB merge — the upserter
    /// is the source of truth for the row's full attribute set).
    ///
    /// **Quarantine default (migration 0015).** Inserted rows ship with
    /// `quarantine=TRUE` per the column default; this writer does not
    /// override it. Production `graph_search` filters quarantined rows
    /// out, so entities created here are invisible to recall until an
    /// operator (or a future maintenance-CLI path) flips the column.
    /// Existing rows hit on conflict keep their current `quarantine`
    /// state — neither path silently un-quarantines.
    fn upsert_entity(
        &self,
        kind: &str,
        name: &str,
        attrs: &serde_json::Value,
    ) -> impl std::future::Future<Output = Result<i64, DbError>> + Send;

    /// Insert-or-update an edge. Multi-edges are allowed (see
    /// `0001_init.sql` — `relations` has no UNIQUE on the triple);
    /// "upsert" here means "INSERT, returning id". The trait shape
    /// keeps the door open for a future variant that dedupes on
    /// `(src_id, dst_id, kind)` if a call site needs it.
    fn upsert_relation(
        &self,
        src_id: i64,
        dst_id: i64,
        kind: &str,
        attrs: &serde_json::Value,
    ) -> impl std::future::Future<Output = Result<i64, DbError>> + Send;

    /// Look up an entity by its natural key.
    fn get_entity(
        &self,
        kind: &str,
        name: &str,
    ) -> impl std::future::Future<Output = Result<Option<Entity>, DbError>> + Send;

    /// 1-hop outbound neighbors of `src_id`. `kind = Some("knows")`
    /// filters to a single edge type; `None` returns all edges.
    /// `limit` is honoured at the SQL level so the worst case stays
    /// bounded; pass a generous value (1000) when in doubt.
    fn neighbors(
        &self,
        src_id: i64,
        kind: Option<&str>,
        limit: i64,
    ) -> impl std::future::Future<Output = Result<Vec<Entity>, DbError>> + Send;

    /// Shortest outbound path from `src_id` to `dst_id`, up to
    /// `max_hops` edges. Returns the entity sequence (start..=end)
    /// or `None` when no path within budget exists.
    fn path(
        &self,
        src_id: i64,
        dst_id: i64,
        max_hops: u8,
    ) -> impl std::future::Future<Output = Result<Option<Vec<Entity>>, DbError>> + Send;

    /// Walk outbound edges from `src_id` up to `max_depth` hops.
    ///
    /// Returns one [`WalkedEdge`] per **unique edge** reached from the
    /// seed, anchored to the **shortest path's depth**. Endpoint
    /// kind/name/quarantine on both sides is included so the caller can
    /// render the result without a secondary lookup. Rows are sorted by
    /// `(depth ASC, edge_id ASC)` for deterministic operator-facing
    /// output.
    ///
    /// **When you want both directions:** prefer
    /// [`Graph::walk_edges_around`] which issues one UNION ALL round-trip
    /// instead of two separate queries. This single-direction method
    /// stays as a stable surface for callers that genuinely need only
    /// one direction.
    ///
    /// **Semantics:**
    /// - `max_depth == 0` returns an empty `Vec` (no edges to walk).
    /// - `max_depth == 1` returns the seed's direct outbound edges
    ///   (parallel to [`Graph::neighbors`] but with edge metadata).
    /// - Cycles are bounded by a visited-set tracked in the recursive
    ///   CTE: a node already reached on the current path will not be
    ///   re-entered (`A → B → A` does not diverge).
    /// - **Diamond dedupe (issue #114):** an edge reachable by multiple
    ///   paths from the seed appears exactly **once** in the result,
    ///   carrying the shortest-path depth. On `A→B, A→C, B→C, C→D`
    ///   walked from `A` at `max_depth=3`, the `C→D` edge surfaces at
    ///   `depth=2` (via `A-C`), not also at `depth=3` (via `A-B-C`).
    ///   Implemented via `DISTINCT ON (edge_id) ORDER BY edge_id,
    ///   depth ASC` in the SQL.
    /// - `limit` is honoured SQL-side so a high-fan-out seed cannot
    ///   exhaust memory. Pass a generous value (e.g. `10_000`) when in
    ///   doubt; the operator-facing CLI is the typical caller. The
    ///   `LIMIT` applies *after* dedupe, so it bounds the final row
    ///   count rather than the intermediate traversal-row count.
    ///
    /// **Quarantine:** quarantined entities are **not** filtered. The
    /// only consumer today is the operator-facing `relations show`
    /// command, which deliberately surfaces quarantined entities (tagged
    /// `[Q]` in its output) because they are exactly the rows the
    /// operator may be about to review. If a future caller needs
    /// quarantine-filtered output, layer the filter on the returned
    /// `Vec` rather than threading another flag through this method.
    fn walk_outbound_edges(
        &self,
        src_id: i64,
        max_depth: u8,
        limit: i64,
    ) -> impl std::future::Future<Output = Result<Vec<WalkedEdge>, DbError>> + Send;

    /// Walk inbound edges into `dst_id` up to `max_depth` hops.
    ///
    /// Mirror of [`Graph::walk_outbound_edges`] for the reverse
    /// direction: at depth 1 returns edges that point to the seed; at
    /// depth 2 returns edges that point to *those* sources; and so on.
    /// Each [`WalkedEdge`] still records the canonical `(src_id, kind,
    /// dst_id)` triple in the same orientation as it lives in
    /// `relations`, so the operator-facing renderer can render every
    /// edge in the same `src --[kind]--> dst` shape regardless of which
    /// walk direction surfaced it.
    ///
    /// See [`Graph::walk_outbound_edges`] for cycle handling, depth-0
    /// semantics, the `limit` contract, quarantine policy, and the
    /// shortest-path dedupe semantics.
    ///
    /// **When you want both directions:** prefer
    /// [`Graph::walk_edges_around`] which issues one UNION ALL round-trip
    /// instead of two separate queries.
    fn walk_inbound_edges(
        &self,
        dst_id: i64,
        max_depth: u8,
        limit: i64,
    ) -> impl std::future::Future<Output = Result<Vec<WalkedEdge>, DbError>> + Send;

    /// Walk *both* outbound and inbound edges around `seed_id` in a
    /// single SQL round-trip via UNION ALL, returning an
    /// [`EdgesAround`] with the two directions pre-partitioned.
    ///
    /// Equivalent to calling [`Graph::walk_outbound_edges`] +
    /// [`Graph::walk_inbound_edges`] back-to-back, but issues exactly
    /// one query against the DB instead of two. Used by the operator-
    /// facing `kastellan-cli relations show` command which always needs
    /// both directions.
    ///
    /// **Semantics:**
    /// - `per_direction_limit` is honoured *per direction*: an
    ///   outbound-heavy and inbound-light seed will return up to
    ///   `per_direction_limit` outbound rows AND up to
    ///   `per_direction_limit` inbound rows.
    /// - The same diamond-dedupe (issue #114) applies within each
    ///   direction: each unique `edge_id` reachable via multiple paths
    ///   appears exactly once, at its shortest depth.
    /// - `max_depth == 0` short-circuits to an empty [`EdgesAround`]
    ///   without a SQL round-trip.
    /// - Cycle handling, quarantine policy, sorting (`(depth ASC,
    ///   edge_id ASC)` within each direction) all match the per-
    ///   direction methods.
    ///
    /// Closes [issue #115](https://github.com/hherb/kastellan/issues/115).
    fn walk_edges_around(
        &self,
        seed_id: i64,
        max_depth: u8,
        per_direction_limit: i64,
    ) -> impl std::future::Future<Output = Result<EdgesAround, DbError>> + Send;
}

/// Postgres implementation of [`Graph`]. Holds a borrowed pool/
/// connection so the same connection lifecycle as the rest of the
/// daemon applies (no stowed connection leaking past Drop).
///
/// Constructed with a `&sqlx::PgPool` in production code, or with a
/// `&mut sqlx::PgConnection` in tests. The blanket impl is over
/// `&PgPool` for now; if a test wants `&mut PgConnection` we add a
/// second constructor.
pub struct PgGraph<'a> {
    pool: &'a sqlx::PgPool,
}

impl<'a> PgGraph<'a> {
    /// Borrow a pool. The pool's lifetime must outlive the graph
    /// reference; the daemon owns the pool for the duration of its
    /// lifetime, so this is straightforward at the call site.
    pub fn new(pool: &'a sqlx::PgPool) -> Self {
        Self { pool }
    }
}

impl<'a> Graph for PgGraph<'a> {
    async fn upsert_entity(
        &self,
        kind: &str,
        name: &str,
        attrs: &serde_json::Value,
    ) -> Result<i64, DbError> {
        // Migration 0015 replaced the (kind, name) UNIQUE with
        // (kind, name_norm) — case/whitespace/NFC-insensitive dedup.
        // `name` keeps the FIRST writer's display form; `attrs`
        // updates on conflict. `name_norm` is computed via the
        // canonical `normalize_entity_name` helper so this writer
        // matches the v2 extractor's `upsert_entities_and_relations`
        // exactly (same input → same dedup key).
        let name_norm = crate::normalize_entity_name(name);
        let row = sqlx::query(
            r#"
            INSERT INTO entities (kind, name, name_norm, attrs)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (kind, name_norm) DO UPDATE
              SET attrs = EXCLUDED.attrs,
                  updated_at = now()
            RETURNING id
            "#,
        )
        .bind(kind)
        .bind(name)
        .bind(&name_norm)
        .bind(attrs)
        .fetch_one(self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;
        row.try_get::<i64, _>(0)
            .map_err(|e| DbError::Query(format!("decode entity.id: {e}")))
    }

    async fn upsert_relation(
        &self,
        src_id: i64,
        dst_id: i64,
        kind: &str,
        attrs: &serde_json::Value,
    ) -> Result<i64, DbError> {
        let row = sqlx::query(
            r#"
            INSERT INTO relations (src_id, dst_id, kind, attrs)
            VALUES ($1, $2, $3, $4)
            RETURNING id
            "#,
        )
        .bind(src_id)
        .bind(dst_id)
        .bind(kind)
        .bind(attrs)
        .fetch_one(self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;
        row.try_get::<i64, _>(0)
            .map_err(|e| DbError::Query(format!("decode relation.id: {e}")))
    }

    async fn get_entity(
        &self,
        kind: &str,
        name: &str,
    ) -> Result<Option<Entity>, DbError> {
        let opt = sqlx::query("SELECT id, kind, name, attrs FROM entities WHERE kind = $1 AND name = $2")
            .bind(kind)
            .bind(name)
            .fetch_optional(self.pool)
            .await
            .map_err(|e| DbError::Query(e.to_string()))?;
        match opt {
            Some(row) => Ok(Some(decode_entity(&row)?)),
            None => Ok(None),
        }
    }

    async fn neighbors(
        &self,
        src_id: i64,
        kind: Option<&str>,
        limit: i64,
    ) -> Result<Vec<Entity>, DbError> {
        // Two query shapes (`kind IS NULL` vs filtered) so the planner
        // gets the predicate at parse time. A single query with
        // `($3::text IS NULL OR r.kind = $3)` would also work but is
        // marginally less index-friendly — both rows in the
        // `relations` indexes are `(src_id, kind)`-shaped.
        let rows = if let Some(k) = kind {
            sqlx::query(
                r#"
                SELECT e.id, e.kind, e.name, e.attrs
                FROM relations r
                JOIN entities e ON e.id = r.dst_id
                WHERE r.src_id = $1 AND r.kind = $2
                ORDER BY r.id
                LIMIT $3
                "#,
            )
            .bind(src_id)
            .bind(k)
            .bind(limit)
            .fetch_all(self.pool)
            .await
        } else {
            sqlx::query(
                r#"
                SELECT e.id, e.kind, e.name, e.attrs
                FROM relations r
                JOIN entities e ON e.id = r.dst_id
                WHERE r.src_id = $1
                ORDER BY r.id
                LIMIT $2
                "#,
            )
            .bind(src_id)
            .bind(limit)
            .fetch_all(self.pool)
            .await
        }
        .map_err(|e| DbError::Query(e.to_string()))?;

        rows.iter().map(decode_entity).collect()
    }

    async fn path(
        &self,
        src_id: i64,
        dst_id: i64,
        max_hops: u8,
    ) -> Result<Option<Vec<Entity>>, DbError> {
        // Recursive CTE walks outbound edges, tracking the visited set
        // in the row to refuse re-entry on cycles. `depth < $3` caps
        // the recursion budget; the final `ORDER BY depth ASC LIMIT 1`
        // on the materialised CTE result picks the shortest satisfying
        // path. Execution order in the recursive term doesn't matter —
        // the sort happens after the full reachable set (within
        // max_hops) is built.
        //
        // `max_hops` is widened to i32 because Postgres has no native
        // u8; the cap is small enough that overflow is impossible.
        //
        // The single statement uses a follow-up CTE (`hits`) to pick the
        // shortest path and `unnest WITH ORDINALITY` to expand it to
        // entities in path order. Doing it server-side closes the race
        // window between "select ids" and "expand to entities" that a
        // two-statement variant has against a concurrent
        // `DELETE FROM entities` — under FK CASCADE, the relations row
        // would also have vanished, so a half-deleted path can't slip
        // through the same snapshot here.
        let max_hops_i32: i32 = i32::from(max_hops);
        let rows = sqlx::query(
            r#"
            WITH RECURSIVE walk(node_id, depth, path) AS (
                SELECT $1::bigint, 0, ARRAY[$1::bigint]
                UNION ALL
                SELECT r.dst_id,
                       w.depth + 1,
                       w.path || r.dst_id
                FROM walk w
                JOIN relations r ON r.src_id = w.node_id
                WHERE w.depth < $3
                  AND NOT (r.dst_id = ANY(w.path))
            ),
            hits AS (
                SELECT path
                FROM walk
                WHERE node_id = $2
                ORDER BY depth ASC
                LIMIT 1
            )
            SELECT e.id, e.kind, e.name, e.attrs, ord
            FROM hits,
                 unnest(hits.path) WITH ORDINALITY AS p(id, ord)
                 JOIN entities e ON e.id = p.id
            ORDER BY ord
            "#,
        )
        .bind(src_id)
        .bind(dst_id)
        .bind(max_hops_i32)
        .fetch_all(self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;

        if rows.is_empty() {
            return Ok(None);
        }
        rows.into_iter()
            .map(|r| decode_entity(&r))
            .collect::<Result<Vec<_>, _>>()
            .map(Some)
    }

    async fn walk_outbound_edges(
        &self,
        src_id: i64,
        max_depth: u8,
        limit: i64,
    ) -> Result<Vec<WalkedEdge>, DbError> {
        // Depth-0 has no edges to traverse. Short-circuit before the
        // CTE so a degenerate caller doesn't pay for a SQL round-trip.
        if max_depth == 0 {
            return Ok(Vec::new());
        }
        let max_depth = clamp_walk_depth(max_depth);
        let max_depth_i32: i32 = i32::from(max_depth);

        // Recursive CTE walks edges, not nodes. The base case projects
        // every edge leaving the seed (depth=1). The recursive case
        // joins each row's `dst_id` against `relations.src_id` to
        // expand one level further, accumulating both the visited
        // *node* set (to refuse re-entering nodes on cycles) and the
        // hop depth.
        //
        // The recursive term emits one row per *traversal path*, so the
        // same `edge_id` can be reached via multiple paths on a diamond
        // topology (`A→B, A→C, B→C, C→D` walked from `A` at depth ≥ 3
        // surfaces `C→D` twice: once via `A-C` at depth 2 and once via
        // `A-B-C` at depth 3). The intermediate `deduped` CTE applies
        // `DISTINCT ON (edge_id) ORDER BY edge_id, depth ASC` to keep
        // exactly one row per unique edge — the one with the shortest
        // path. This is the [issue #114] fix.
        //
        // The outer SELECT re-applies the operator-facing sort
        // `(depth ASC, edge_id ASC)` after the dedupe. Sorting only
        // happens in named SELECTs (never the recursive term) — Postgres
        // treats the recursive term as a bag with non-deterministic
        // enumeration.
        //
        // [issue #114]: https://github.com/hherb/kastellan/issues/114
        let rows = sqlx::query(
            r#"
            WITH RECURSIVE walk(edge_id, src_id, dst_id, kind, depth, visited) AS (
                SELECT r.id, r.src_id, r.dst_id, r.kind, 1,
                       ARRAY[r.src_id::bigint, r.dst_id::bigint]
                FROM relations r
                WHERE r.src_id = $1::bigint
                UNION ALL
                SELECT r.id, r.src_id, r.dst_id, r.kind, w.depth + 1,
                       w.visited || r.dst_id
                FROM walk w
                JOIN relations r ON r.src_id = w.dst_id
                WHERE w.depth < $2
                  AND NOT (r.dst_id = ANY(w.visited))
            ),
            deduped AS (
                SELECT DISTINCT ON (edge_id)
                    edge_id, src_id, dst_id, kind, depth
                FROM walk
                ORDER BY edge_id, depth ASC
            )
            SELECT
                d.depth,
                d.edge_id,
                d.src_id, es.kind, es.name, es.quarantine,
                d.dst_id, ed.kind, ed.name, ed.quarantine,
                d.kind
            FROM deduped d
            JOIN entities es ON es.id = d.src_id
            JOIN entities ed ON ed.id = d.dst_id
            ORDER BY d.depth ASC, d.edge_id ASC
            LIMIT $3
            "#,
        )
        .bind(src_id)
        .bind(max_depth_i32)
        .bind(limit)
        .fetch_all(self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;

        rows.iter().map(decode_walked_edge).collect()
    }

    async fn walk_inbound_edges(
        &self,
        dst_id: i64,
        max_depth: u8,
        limit: i64,
    ) -> Result<Vec<WalkedEdge>, DbError> {
        if max_depth == 0 {
            return Ok(Vec::new());
        }
        let max_depth = clamp_walk_depth(max_depth);
        let max_depth_i32: i32 = i32::from(max_depth);

        // Symmetric to `walk_outbound_edges` but walking in the reverse
        // direction. The base case projects every edge *arriving at*
        // the seed; the recursive case joins each row's `src_id`
        // against `relations.dst_id` to expand one level upstream.
        //
        // Critical: each emitted `WalkedEdge` still records the
        // canonical `(src_id, kind, dst_id)` orientation (the same way
        // the row lives in `relations`). So a `B --[knows]--> A`
        // edge surfaced by `walk_inbound_edges(A, ...)` is returned as
        // `WalkedEdge { src=B, kind=knows, dst=A, depth=1 }`, not as
        // an inverted `A --[knows]--> B`. This means the caller can
        // mix outbound + inbound results in one rendering loop without
        // having to track which walk produced each row.
        //
        // Same `DISTINCT ON (edge_id) ORDER BY edge_id, depth ASC`
        // dedupe as the outbound walk — see that method's body comment
        // for the [issue #114] rationale.
        //
        // [issue #114]: https://github.com/hherb/kastellan/issues/114
        let rows = sqlx::query(
            r#"
            WITH RECURSIVE walk(edge_id, src_id, dst_id, kind, depth, visited) AS (
                SELECT r.id, r.src_id, r.dst_id, r.kind, 1,
                       ARRAY[r.dst_id::bigint, r.src_id::bigint]
                FROM relations r
                WHERE r.dst_id = $1::bigint
                UNION ALL
                SELECT r.id, r.src_id, r.dst_id, r.kind, w.depth + 1,
                       w.visited || r.src_id
                FROM walk w
                JOIN relations r ON r.dst_id = w.src_id
                WHERE w.depth < $2
                  AND NOT (r.src_id = ANY(w.visited))
            ),
            deduped AS (
                SELECT DISTINCT ON (edge_id)
                    edge_id, src_id, dst_id, kind, depth
                FROM walk
                ORDER BY edge_id, depth ASC
            )
            SELECT
                d.depth,
                d.edge_id,
                d.src_id, es.kind, es.name, es.quarantine,
                d.dst_id, ed.kind, ed.name, ed.quarantine,
                d.kind
            FROM deduped d
            JOIN entities es ON es.id = d.src_id
            JOIN entities ed ON ed.id = d.dst_id
            ORDER BY d.depth ASC, d.edge_id ASC
            LIMIT $3
            "#,
        )
        .bind(dst_id)
        .bind(max_depth_i32)
        .bind(limit)
        .fetch_all(self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;

        rows.iter().map(decode_walked_edge).collect()
    }

    async fn walk_edges_around(
        &self,
        seed_id: i64,
        max_depth: u8,
        per_direction_limit: i64,
    ) -> Result<EdgesAround, DbError> {
        if max_depth == 0 {
            return Ok(EdgesAround::default());
        }
        let max_depth = clamp_walk_depth(max_depth);
        let max_depth_i32: i32 = i32::from(max_depth);

        // One round-trip combining both walks via UNION ALL. The
        // structure mirrors `walk_outbound_edges` and `walk_inbound_edges`
        // twice over: each direction gets its own recursive walk CTE
        // and its own DISTINCT ON dedupe + LIMIT + entity-join, then
        // we UNION ALL the two renderings.
        //
        // The trailing `direction` text column is the partition
        // discriminant the Rust decoder uses to drop each row into
        // `EdgesAround::{outbound, inbound}`. Outer `ORDER BY direction
        // ASC, depth ASC, edge_id ASC` keeps rows for the same
        // direction adjacent and within each direction sorted by
        // `(depth, edge_id)` — which preserves the per-direction sort
        // every existing caller expects.
        //
        // Note: lexicographically `'inbound' < 'outbound'`, so inbound
        // rows arrive on the cursor first. This is an incidental
        // property of the literal strings, NOT an API contract — the
        // Rust decoder partitions row-by-row via the discriminant, so
        // consumers must never depend on which direction's `Vec`
        // populates first. If the literals ever changed (e.g. to a
        // `direction_enum` with a different sort key), the partition
        // contract still holds.
        //
        // `LIMIT` is applied per-direction (inside each rendered CTE)
        // rather than across the union, so an outbound-heavy seed
        // doesn't starve inbound rows out of the result.
        //
        // [issue #115]: https://github.com/hherb/kastellan/issues/115
        let rows = sqlx::query(
            r#"
            WITH RECURSIVE outbound_walk(edge_id, src_id, dst_id, kind, depth, visited) AS (
                SELECT r.id, r.src_id, r.dst_id, r.kind, 1,
                       ARRAY[r.src_id::bigint, r.dst_id::bigint]
                FROM relations r
                WHERE r.src_id = $1::bigint
                UNION ALL
                SELECT r.id, r.src_id, r.dst_id, r.kind, w.depth + 1,
                       w.visited || r.dst_id
                FROM outbound_walk w
                JOIN relations r ON r.src_id = w.dst_id
                WHERE w.depth < $2
                  AND NOT (r.dst_id = ANY(w.visited))
            ),
            inbound_walk(edge_id, src_id, dst_id, kind, depth, visited) AS (
                SELECT r.id, r.src_id, r.dst_id, r.kind, 1,
                       ARRAY[r.dst_id::bigint, r.src_id::bigint]
                FROM relations r
                WHERE r.dst_id = $1::bigint
                UNION ALL
                SELECT r.id, r.src_id, r.dst_id, r.kind, w.depth + 1,
                       w.visited || r.src_id
                FROM inbound_walk w
                JOIN relations r ON r.dst_id = w.src_id
                WHERE w.depth < $2
                  AND NOT (r.src_id = ANY(w.visited))
            ),
            outbound_deduped AS (
                SELECT DISTINCT ON (edge_id)
                    edge_id, src_id, dst_id, kind, depth
                FROM outbound_walk
                ORDER BY edge_id, depth ASC
            ),
            inbound_deduped AS (
                SELECT DISTINCT ON (edge_id)
                    edge_id, src_id, dst_id, kind, depth
                FROM inbound_walk
                ORDER BY edge_id, depth ASC
            ),
            outbound_rendered AS (
                SELECT
                    d.depth,
                    d.edge_id,
                    d.src_id, es.kind AS src_kind, es.name AS src_name, es.quarantine AS src_quarantine,
                    d.dst_id, ed.kind AS dst_kind, ed.name AS dst_name, ed.quarantine AS dst_quarantine,
                    d.kind,
                    'outbound'::text AS direction
                FROM outbound_deduped d
                JOIN entities es ON es.id = d.src_id
                JOIN entities ed ON ed.id = d.dst_id
                ORDER BY d.depth ASC, d.edge_id ASC
                LIMIT $3
            ),
            inbound_rendered AS (
                SELECT
                    d.depth,
                    d.edge_id,
                    d.src_id, es.kind AS src_kind, es.name AS src_name, es.quarantine AS src_quarantine,
                    d.dst_id, ed.kind AS dst_kind, ed.name AS dst_name, ed.quarantine AS dst_quarantine,
                    d.kind,
                    'inbound'::text AS direction
                FROM inbound_deduped d
                JOIN entities es ON es.id = d.src_id
                JOIN entities ed ON ed.id = d.dst_id
                ORDER BY d.depth ASC, d.edge_id ASC
                LIMIT $3
            )
            SELECT * FROM outbound_rendered
            UNION ALL
            SELECT * FROM inbound_rendered
            ORDER BY direction ASC, depth ASC, edge_id ASC
            "#,
        )
        .bind(seed_id)
        .bind(max_depth_i32)
        .bind(per_direction_limit)
        .fetch_all(self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;

        let mut around = EdgesAround::default();
        for row in &rows {
            let edge = decode_walked_edge(row)?;
            let direction: String = row
                .try_get::<String, _>(WALK_EDGES_AROUND_DIRECTION_COL)
                .map_err(|e| DbError::Query(format!("decode walked_edge_around.direction: {e}")))?;
            match direction.as_str() {
                "outbound" => around.outbound.push(edge),
                "inbound" => around.inbound.push(edge),
                other => {
                    return Err(DbError::Query(format!(
                        "decode walked_edge_around.direction: unexpected value '{other}' \
                         (expected 'outbound' or 'inbound' — SQL projection drift?)"
                    )));
                }
            }
        }
        Ok(around)
    }
}

/// Clamp `max_depth` to [`MAX_WALK_DEPTH`], emitting a `tracing::warn`
/// on out-of-range input so an operator who passes a huge depth on the
/// CLI sees a one-line breadcrumb instead of a silent ceiling. Pure
/// function so the warn-emission is easy to unit-test without touching
/// the DB.
fn clamp_walk_depth(requested: u8) -> u8 {
    if requested > MAX_WALK_DEPTH {
        tracing::warn!(
            requested,
            clamped_to = MAX_WALK_DEPTH,
            "graph::walk depth exceeds cap; clamping",
        );
        MAX_WALK_DEPTH
    } else {
        requested
    }
}

#[cfg(test)]
mod tests;
