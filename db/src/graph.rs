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
    /// Insert-or-update by `(kind, name)`. Returns the entity's id.
    /// `attrs` overwrites on conflict (no JSONB merge — the upserter
    /// is the source of truth for the row's full attribute set).
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
        // ON CONFLICT key matches the `UNIQUE (kind, name)` index in
        // `0001_init.sql`. `updated_at = now()` records the most
        // recent change so a future "stale entity" sweep can use it.
        let row = sqlx::query(
            r#"
            INSERT INTO entities (kind, name, attrs)
            VALUES ($1, $2, $3)
            ON CONFLICT (kind, name) DO UPDATE
              SET attrs = EXCLUDED.attrs,
                  updated_at = now()
            RETURNING id
            "#,
        )
        .bind(kind)
        .bind(name)
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
            Some(row) => Ok(Some(Entity {
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
            })),
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

        rows.into_iter()
            .map(|row| {
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
            })
            .collect()
    }

    async fn path(
        &self,
        src_id: i64,
        dst_id: i64,
        max_hops: u8,
    ) -> Result<Option<Vec<Entity>>, DbError> {
        // Recursive CTE walks outbound edges, tracking the visited set
        // in the row to refuse re-entry on cycles. `depth <= $3` caps
        // the search; we ORDER BY depth ASC LIMIT 1 so the *shortest*
        // satisfying path wins (BFS-like via the planner — Postgres
        // does not actually do BFS but the LIMIT picks min-depth
        // because shorter rows always exist before longer ones in the
        // recursion's natural execution order for a single-source
        // walk).
        //
        // `max_hops` is widened to i32 because Postgres has no native
        // u8; the cap is small enough that overflow is impossible.
        let max_hops_i32: i32 = i32::from(max_hops);
        let row = sqlx::query(
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
            )
            SELECT path
            FROM walk
            WHERE node_id = $2
            ORDER BY depth ASC
            LIMIT 1
            "#,
        )
        .bind(src_id)
        .bind(dst_id)
        .bind(max_hops_i32)
        .fetch_optional(self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;

        let Some(row) = row else { return Ok(None) };
        let ids: Vec<i64> = row
            .try_get(0)
            .map_err(|e| DbError::Query(format!("decode walk.path: {e}")))?;

        // Expand the id list into entities. `ANY($1)` against an
        // index lookup is a single round-trip; we then sort client-
        // side by the path order to preserve the walk sequence.
        let entities = sqlx::query(
            "SELECT id, kind, name, attrs FROM entities WHERE id = ANY($1)",
        )
        .bind(&ids)
        .fetch_all(self.pool)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;

        let mut by_id: std::collections::HashMap<i64, Entity> = std::collections::HashMap::new();
        for r in entities {
            let e = Entity {
                id: r
                    .try_get(0)
                    .map_err(|e| DbError::Query(format!("decode entity.id: {e}")))?,
                kind: r
                    .try_get(1)
                    .map_err(|e| DbError::Query(format!("decode entity.kind: {e}")))?,
                name: r
                    .try_get(2)
                    .map_err(|e| DbError::Query(format!("decode entity.name: {e}")))?,
                attrs: r
                    .try_get(3)
                    .map_err(|e| DbError::Query(format!("decode entity.attrs: {e}")))?,
            };
            by_id.insert(e.id, e);
        }
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            // A foreign-key-cascading delete cannot leave orphans, so
            // any id missing here is a programming error — surface it
            // as a Query error so the daemon fails closed rather than
            // returning a truncated path.
            let e = by_id
                .remove(&id)
                .ok_or_else(|| DbError::Query(format!("path id {id} not found in entities")))?;
            out.push(e);
        }
        Ok(Some(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sanity-pin the value types so a future field rename trips a
    /// compile error in the test before it can leak into a downstream
    /// API change.
    #[test]
    fn entity_struct_field_shape() {
        let e = Entity {
            id: 1,
            kind: "person".into(),
            name: "alice".into(),
            attrs: serde_json::json!({"hello": "world"}),
        };
        assert_eq!(e.id, 1);
        assert_eq!(e.kind, "person");
        assert_eq!(e.name, "alice");
        assert_eq!(e.attrs["hello"], "world");
    }

    #[test]
    fn relation_struct_field_shape() {
        let r = Relation {
            id: 1,
            src_id: 10,
            dst_id: 20,
            kind: "knows".into(),
            attrs: serde_json::json!({}),
        };
        assert_eq!(r.src_id, 10);
        assert_eq!(r.dst_id, 20);
        assert_eq!(r.kind, "knows");
    }
}
