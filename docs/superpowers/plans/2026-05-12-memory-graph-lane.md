# Memory Graph Lane (Option P) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the third lane in `core::memory::recall` — graph-anchored memory retrieval — together with the schema (`memory_entities` join table), writer/reader helpers, integration with the existing RRF fusion, and a `deleted_memories` audit trigger on the `memories` table.

**Architecture:** A new `memory_entities` join table (composite PK + entity-id index) backs the lane. The reader computes a 1-hop outbound entity expansion via the existing `Graph::neighbors` chokepoint, then ranks memories by hit-count against `memory_entities`. The lane composes into the existing `recall()` flow via a new `RecallModes::graph` flag and a new `seed_entity_ids` field on `RecallParams`. A separate migration adds an AFTER DELETE trigger on `memories` that journals the deleted row's full shape into `deleted_memories` (append-only by GRANT shape).

**Tech Stack:** Postgres 18 + pgvector, sqlx 0.8 embedded migrator, Rust async (tokio + `futures::future::try_join_all`), the existing `hhagent-tests-common` dev-dep for per-test cluster bring-up.

**Branch:** `feat/memory-graph-lane`, off `main` at `97f2743`. The spec + handover refresh are already committed at `5e68600` on this branch.

**Spec:** [`docs/superpowers/specs/2026-05-12-memory-graph-lane-design.md`](../specs/2026-05-12-memory-graph-lane-design.md)

---

## Pre-flight checks

Before starting, verify the working state matches the plan's assumptions:

```sh
source "$HOME/.cargo/env"
git rev-parse --abbrev-ref HEAD     # expected: feat/memory-graph-lane
git log --oneline -1                # expected: 5e68600 docs: spec for memory graph lane (Option P) + handover refresh
cargo build --workspace             # expected: clean, ~3s incremental
cargo test --workspace 2>&1 | tail -3   # expected: 342 passed, 0 failed (~3 min)
```

If the workspace test count drifts from 342 before any task is started, stop and reconcile against `main` first.

---

## Task 1: Migration 0007 — `memory_entities` join table

**Files:**
- Create: `db/migrations/0007_memory_entities.sql`

**Rationale:** New table backs the graph lane. Composite PK `(memory_id, entity_id)` for natural dedup + a separate `(entity_id)` index for the read-path `WHERE entity_id = ANY($1)` scan. Both FKs ON DELETE CASCADE so deleting a parent removes its link rows; cascades flow downward only (link-row deletion never triggers parent deletion).

- [ ] **Step 1: Write the migration file**

Create `db/migrations/0007_memory_entities.sql`:

```sql
-- Phase 1 — graph lane in `core::memory::recall`.
--
-- Join table linking `memories` rows to `entities` nodes. The graph
-- lane uses this to surface memories tagged with seed entities (and
-- their 1-hop outbound neighbours, expanded in core).
--
-- Why a composite-PK join table (over JSONB on memories.metadata):
--   * Higher-cardinality storage (one row per link)
--   * Clean cascade semantics — deleting an entity drops its links
--     automatically, no manual sweep
--   * Index on entity_id makes `entity_id = ANY($1)` a single index
--     scan, not a JSONB GIN intersection
--   * Lane SQL is a straightforward GROUP BY; JSONB shape would need
--     jsonb_array_elements + casts at every read
--
-- Cascade safety: both FKs are ON DELETE CASCADE. FK cascades flow
-- only from referenced row to referencing row, so a link-row deletion
-- can NEVER trigger a memory or entity deletion. See migration 0008
-- for the trigger that journals memory deletions specifically.

CREATE TABLE memory_entities (
    memory_id  BIGINT NOT NULL
        REFERENCES memories(id) ON DELETE CASCADE,
    entity_id  BIGINT NOT NULL
        REFERENCES entities(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (memory_id, entity_id)
);

-- PK already indexes (memory_id, ...). This second index supports the
-- read path which is `WHERE entity_id = ANY($1)` (no memory_id filter).
CREATE INDEX memory_entities_entity_idx
    ON memory_entities (entity_id);

-- Runtime role gets the same shape as memories/entities/relations
-- (full CRUD). audit_log's REVOKE shape does NOT apply here — this is
-- a mutable derived index, not an immutable audit trail.
GRANT SELECT, INSERT, UPDATE, DELETE ON memory_entities TO hhagent_runtime;
```

- [ ] **Step 2: Verify the migration compiles into MIGRATOR and applies**

The `sqlx::migrate!()` macro embeds every `db/migrations/*.sql` at compile time. Run a focused integration test that exercises `probe::run`:

```sh
source "$HOME/.cargo/env"
cargo test -p hhagent-db --test postgres_e2e probe_runs_migrations_and_graph_happy_path -- --nocapture 2>&1 | tail -20
```

Expected: PASS in ~3s. The probe runs 0001..0007 against a fresh per-test cluster; if the new SQL has a syntax error or references a non-existent symbol, the test fails at MIGRATOR.run with a sqlx error pointing at the offending line.

- [ ] **Step 3: Commit**

```sh
git add db/migrations/0007_memory_entities.sql
git commit -m "$(cat <<'EOF'
feat(db): migration 0007 — memory_entities join table

Adds the join table that backs the graph lane in core::memory::recall.
Composite PK (memory_id, entity_id) + separate index on entity_id for
the read path (WHERE entity_id = ANY($1)). Both FKs ON DELETE CASCADE
flow downward only — link-row deletion can never trigger memory or
entity deletion. Runtime role gets full CRUD (same shape as
memories/entities/relations).

No application code uses the table yet; this migration is the
foundation for the writer-side helper, read-side helper, and graph
lane wiring in subsequent tasks.

Per spec: docs/superpowers/specs/2026-05-12-memory-graph-lane-design.md

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Migration 0008 — `deleted_memories` audit trigger

**Files:**
- Create: `db/migrations/0008_deleted_memories_audit.sql`

**Rationale:** User-requested append-only journal of every memory deletion. AFTER DELETE trigger writes the full deleted row + `deleted_at` timestamp into a dedicated table. Append-only by GRANT shape (same defence as `audit_log` from migration 0002 — UPDATE/DELETE revoked at the DB layer).

- [ ] **Step 1: Write the migration file**

Create `db/migrations/0008_deleted_memories_audit.sql`:

```sql
-- Phase 1 — append-only journal of deleted memories.
--
-- Phase 1 has no caller that deletes memories today, but the cascade
-- infrastructure in 0007 treats memory deletion as a real future
-- operation (e.g. GDPR-style forgetting). When that operation
-- materialises, this trigger guarantees the deleted row is preserved
-- before it vanishes.
--
-- Why a dedicated table and not an audit_log row:
--   * audit_log truncates payloads at 4 KiB; a memory body + metadata
--     + 1024-dim embedding can easily exceed that
--   * Keeping the row's full shape means a future "undelete" or
--     "show me what disappeared" query has everything it needs
--     without joining back to a row that no longer exists
--
-- Why a trigger (not app-level discipline):
--   * Contract is "every DELETE FROM memories journals to
--     deleted_memories" — enforcing at the DB layer means a future
--     contributor's bare DELETE cannot silently bypass the audit

CREATE TABLE deleted_memories (
    id          BIGINT      PRIMARY KEY,    -- preserved from memories.id
    body        TEXT        NOT NULL,
    metadata    JSONB       NOT NULL,
    embedding   vector(1024),                -- nullable, like the source
    created_at  TIMESTAMPTZ NOT NULL,        -- original creation time
    deleted_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX deleted_memories_deleted_at_idx ON deleted_memories (deleted_at);

CREATE OR REPLACE FUNCTION audit_memory_delete() RETURNS trigger AS $$
BEGIN
    INSERT INTO deleted_memories (id, body, metadata, embedding, created_at)
    VALUES (OLD.id, OLD.body, OLD.metadata, OLD.embedding, OLD.created_at);
    RETURN OLD;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER memories_after_delete_audit
    AFTER DELETE ON memories
    FOR EACH ROW
    EXECUTE FUNCTION audit_memory_delete();

-- Runtime needs SELECT (for reads) and INSERT (because the trigger
-- runs as the DELETE issuer's role, SECURITY INVOKER by default).
-- UPDATE/DELETE revoked — same append-only shape as audit_log.
GRANT  SELECT, INSERT ON deleted_memories TO hhagent_runtime;
REVOKE UPDATE, DELETE, TRUNCATE ON deleted_memories FROM hhagent_runtime;
```

- [ ] **Step 2: Verify the migration applies**

```sh
source "$HOME/.cargo/env"
cargo test -p hhagent-db --test postgres_e2e probe_runs_migrations_and_graph_happy_path -- --nocapture 2>&1 | tail -20
```

Expected: PASS. Probe now runs 0001..0008.

- [ ] **Step 3: Commit**

```sh
git add db/migrations/0008_deleted_memories_audit.sql
git commit -m "$(cat <<'EOF'
feat(db): migration 0008 — deleted_memories audit trigger

AFTER DELETE trigger on memories journals the deleted row's full
shape (body, metadata, embedding, original created_at) plus a
deleted_at timestamp into a dedicated deleted_memories table.

Append-only by GRANT shape: SELECT + INSERT for hhagent_runtime
(INSERT needed because the trigger runs as the DELETE issuer's role,
SECURITY INVOKER by default). UPDATE/DELETE/TRUNCATE revoked at the
DB layer — same defence as audit_log from migration 0002.

Phase 1 has no caller that deletes memories today; this is preventive
infrastructure for the future GDPR-style forgetting path. The cascade
direction in 0007 keeps link-row deletion from ever triggering memory
deletion; this trigger captures any future explicit memory deletion.

Per spec: docs/superpowers/specs/2026-05-12-memory-graph-lane-design.md

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Write the failing DB integration tests

**Files:**
- Modify: `db/tests/postgres_e2e.rs` (append three new `#[tokio::test]` fns)

**Rationale:** TDD step — pin the contract for the not-yet-written `link_memory_to_entities` helper plus the trigger from Task 2. The tests compile-fail today (the helper doesn't exist) — that's the RED state.

The existing test file already uses `hhagent_tests_common::{bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, PgCluster}` per the tests-common hoist. Follow the existing import shape and the `tasks_lifecycle_e2e` test as the closest stylistic precedent.

- [ ] **Step 1: Append the three test fns at the bottom of `db/tests/postgres_e2e.rs`**

Add after the existing tests (preserve all existing imports; if `serde_json::json` or `time::OffsetDateTime` aren't already imported in this file, the new tests' `use` lines add them):

```rust
// ─── Graph lane: memory_entities + deleted_memories (0007 + 0008) ────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_entities_link_round_trip_and_idempotency() {
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };
    if !skip_if_no_supervisor() { return; }
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "memory-entities-link",
        "memory-entities-link",
        "hhagent-postgres-memory-entities-link",
    )
    .await
    .expect("bring_up_pg_cluster");

    hhagent_db::probe::run(&cluster.conn_spec, "core", "startup")
        .await
        .expect("probe");

    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("pool");

    // Seed: 1 memory, 3 entities.
    let mem_id = hhagent_db::memories::insert_memory(
        &pool,
        "alpha body",
        &serde_json::json!({}),
        None,
    )
    .await
    .expect("insert memory");

    let graph = hhagent_db::graph::PgGraph::new(&pool);
    let e1 = graph
        .upsert_entity("person", "alice", &serde_json::json!({}))
        .await
        .expect("upsert e1");
    let e2 = graph
        .upsert_entity("person", "bob", &serde_json::json!({}))
        .await
        .expect("upsert e2");
    let e3 = graph
        .upsert_entity("animal", "cat", &serde_json::json!({}))
        .await
        .expect("upsert e3");

    // First link: both new.
    let n = hhagent_db::memories::link_memory_to_entities(&pool, mem_id, &[e1, e2])
        .await
        .expect("link 1");
    assert_eq!(n, 2, "first link of 2 fresh entities must insert 2 rows");

    // Re-link same pair: idempotent.
    let n = hhagent_db::memories::link_memory_to_entities(&pool, mem_id, &[e1, e2])
        .await
        .expect("link 2");
    assert_eq!(n, 0, "re-link of existing pairs must insert 0 rows");

    // Mixed (one new, one dupe): only the new one counts.
    let n = hhagent_db::memories::link_memory_to_entities(&pool, mem_id, &[e1, e3])
        .await
        .expect("link 3");
    assert_eq!(n, 1, "mixed re-link + new must insert 1 row");

    // Final count via raw SQL — defends against the helper's return
    // value lying about idempotency.
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM memory_entities WHERE memory_id = $1",
    )
    .bind(mem_id)
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(row.0, 3, "memory_entities must hold exactly 3 distinct rows");

    drop(pool);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_entities_cascade_on_entity_delete() {
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };
    if !skip_if_no_supervisor() { return; }
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "memory-entities-cascade",
        "memory-entities-cascade",
        "hhagent-postgres-memory-entities-cascade",
    )
    .await
    .expect("bring_up_pg_cluster");

    hhagent_db::probe::run(&cluster.conn_spec, "core", "startup")
        .await
        .expect("probe");
    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("pool");

    let mem_id = hhagent_db::memories::insert_memory(
        &pool,
        "bravo body",
        &serde_json::json!({}),
        None,
    )
    .await
    .expect("insert memory");
    let graph = hhagent_db::graph::PgGraph::new(&pool);
    let e_id = graph
        .upsert_entity("person", "alice", &serde_json::json!({}))
        .await
        .expect("upsert");

    hhagent_db::memories::link_memory_to_entities(&pool, mem_id, &[e_id])
        .await
        .expect("link");

    // Deleting the entity cascades to memory_entities.
    sqlx::query("DELETE FROM entities WHERE id = $1")
        .bind(e_id)
        .execute(&pool)
        .await
        .expect("delete entity");

    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM memory_entities WHERE entity_id = $1",
    )
    .bind(e_id)
    .fetch_one(&pool)
    .await
    .expect("count links");
    assert_eq!(row.0, 0, "entity delete must cascade to memory_entities");

    // Memory itself is untouched (cascade flows downward only).
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM memories WHERE id = $1")
        .bind(mem_id)
        .fetch_one(&pool)
        .await
        .expect("count memory");
    assert_eq!(row.0, 1, "memory survives entity cascade");

    // And not in deleted_memories.
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM deleted_memories WHERE id = $1",
    )
    .bind(mem_id)
    .fetch_one(&pool)
    .await
    .expect("count deleted");
    assert_eq!(row.0, 0, "memory not deleted, so deleted_memories has no row");

    drop(pool);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_delete_writes_deleted_memories_row() {
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };
    if !skip_if_no_supervisor() { return; }
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "memory-delete-audit",
        "memory-delete-audit",
        "hhagent-postgres-memory-delete-audit",
    )
    .await
    .expect("bring_up_pg_cluster");

    hhagent_db::probe::run(&cluster.conn_spec, "core", "startup")
        .await
        .expect("probe");
    let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("pool");

    // Build a memory with an embedding so we exercise the full row shape.
    // Deterministic seeded vector via tests-common.
    let emb = hhagent_tests_common::text_to_embedding("delete-audit-fixture");
    let metadata = serde_json::json!({"k": "v"});
    let mem_id = hhagent_db::memories::insert_memory(
        &pool,
        "audit body",
        &metadata,
        Some(&emb),
    )
    .await
    .expect("insert memory");

    let before: (time::OffsetDateTime,) =
        sqlx::query_as("SELECT created_at FROM memories WHERE id = $1")
            .bind(mem_id)
            .fetch_one(&pool)
            .await
            .expect("fetch created_at");
    let original_created_at = before.0;

    // Delete it.
    sqlx::query("DELETE FROM memories WHERE id = $1")
        .bind(mem_id)
        .execute(&pool)
        .await
        .expect("delete memory");

    // Audit row exists with matching shape.
    let row: (i64, String, serde_json::Value, time::OffsetDateTime, time::OffsetDateTime) =
        sqlx::query_as(
            "SELECT id, body, metadata, created_at, deleted_at \
             FROM deleted_memories WHERE id = $1",
        )
        .bind(mem_id)
        .fetch_one(&pool)
        .await
        .expect("fetch deleted");
    assert_eq!(row.0, mem_id);
    assert_eq!(row.1, "audit body");
    assert_eq!(row.2, metadata);
    assert_eq!(row.3, original_created_at, "created_at preserved verbatim");

    let now = time::OffsetDateTime::now_utc();
    let drift = (now - row.4).whole_seconds().abs();
    assert!(drift < 5, "deleted_at must be within 5s of now (drift = {drift}s)");

    // Append-only invariant: runtime cannot UPDATE or DELETE deleted_memories.
    let upd = sqlx::query("UPDATE deleted_memories SET body = 'tampered' WHERE id = $1")
        .bind(mem_id)
        .execute(&pool)
        .await;
    assert!(upd.is_err(), "UPDATE on deleted_memories must be denied to runtime");

    let del = sqlx::query("DELETE FROM deleted_memories WHERE id = $1")
        .bind(mem_id)
        .execute(&pool)
        .await;
    assert!(del.is_err(), "DELETE on deleted_memories must be denied to runtime");

    drop(pool);
}
```

- [ ] **Step 2: Run the new tests and verify they fail at COMPILE time (RED)**

```sh
source "$HOME/.cargo/env"
cargo test -p hhagent-db --test postgres_e2e memory_entities 2>&1 | tail -20
```

Expected: compile error mentioning `link_memory_to_entities` and `unresolved import hhagent_db::memories::link_memory_to_entities` (or similar). The third test's compile path depends on whether `hhagent_tests_common::text_to_embedding` is re-exported — it is, per the `tests-common/src/lib.rs` flat re-export pattern from the hoist.

If the third test compiles but the first two don't, that's the expected RED state. Do not commit at this step.

---

## Task 4: Implement `link_memory_to_entities`

**Files:**
- Modify: `db/src/memories.rs`

**Rationale:** Make `memory_entities_link_round_trip_and_idempotency` and `memory_entities_cascade_on_entity_delete` go green. One batched INSERT via `unnest($2::bigint[])` with `ON CONFLICT DO NOTHING`. Empty-input fast path.

- [ ] **Step 1: Add the helper at the bottom of `db/src/memories.rs` (after `vector_literal`, before the `#[cfg(test)] mod tests`)**

```rust
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
```

- [ ] **Step 2: Run the two link/cascade tests — verify GREEN**

```sh
source "$HOME/.cargo/env"
cargo test -p hhagent-db --test postgres_e2e memory_entities_link_round_trip_and_idempotency memory_entities_cascade_on_entity_delete 2>&1 | tail -20
```

Expected: 2 passed (each ~2.5 s). The third test (`memory_delete_writes_deleted_memories_row`) still uses the helper indirectly but should also pass now since `link_memory_to_entities` exists.

- [ ] **Step 3: Run the third test to confirm the trigger path works**

```sh
cargo test -p hhagent-db --test postgres_e2e memory_delete_writes_deleted_memories_row 2>&1 | tail -10
```

Expected: PASS in ~2.5 s. The trigger from migration 0008 fires on DELETE FROM memories and writes the journal row.

- [ ] **Step 4: Commit**

```sh
git add db/src/memories.rs db/tests/postgres_e2e.rs
git commit -m "$(cat <<'EOF'
feat(db/memories): link_memory_to_entities + 3 integration tests

Adds the writer-side helper for the graph lane:
  pub async fn link_memory_to_entities(executor, memory_id, &[entity_id]) -> Result<u64, DbError>

One batched INSERT via unnest($2::bigint[]) with ON CONFLICT DO NOTHING.
Returns count of genuinely new rows (idempotent — re-linking returns 0).
Empty entity_ids is a fast-path no-op. FK violations surface as
DbError::Query with the underlying Postgres error context.

Three new integration tests in db/tests/postgres_e2e.rs (all per-test
PG cluster via hhagent-tests-common, skip cleanly on hosts without PG):

1. memory_entities_link_round_trip_and_idempotency — verifies returned
   counts on first link / re-link / mixed-new-and-dupe + final SELECT
   COUNT(*) defends against a lying helper.

2. memory_entities_cascade_on_entity_delete — verifies ON DELETE
   CASCADE drops link rows when an entity is deleted, but the memory
   itself survives (no upward cascade). Pins migration 0008's
   non-effect on this path: deleted_memories has no row when only
   links cascaded.

3. memory_delete_writes_deleted_memories_row — verifies the trigger
   from migration 0008 fires on DELETE FROM memories, journaling the
   row's body/metadata/embedding/original-created_at into
   deleted_memories with a fresh deleted_at. Plus pins the append-only
   GRANT shape: UPDATE and DELETE on deleted_memories as runtime role
   both return permission-denied.

Test count: 342 → 345 (+3 integration).

Per spec: docs/superpowers/specs/2026-05-12-memory-graph-lane-design.md
Per plan: docs/superpowers/plans/2026-05-12-memory-graph-lane.md

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Implement `graph_search` (read helper)

**Files:**
- Modify: `db/src/memories.rs`

**Rationale:** The other half of the DB surface. Counts hits per memory against a pre-expanded `entity_ids` set and returns the top-`k` memory ids in best-first order. No focused DB test for this — exercised end-to-end via the core integration test in Task 8.

- [ ] **Step 1: Add the helper alongside `link_memory_to_entities` in `db/src/memories.rs`**

```rust
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
```

- [ ] **Step 2: Verify the workspace still compiles cleanly**

```sh
source "$HOME/.cargo/env"
cargo build --workspace 2>&1 | tail -5
```

Expected: `Finished dev profile`. No new tests run yet; `graph_search` is unexercised until Task 8.

- [ ] **Step 3: Commit**

```sh
git add db/src/memories.rs
git commit -m "$(cat <<'EOF'
feat(db/memories): graph_search read helper for the graph lane

Adds the read-side helper for the graph lane:
  pub async fn graph_search(executor, entity_ids, k) -> Result<Vec<i64>, DbError>

Single SQL statement against memory_entities:
  SELECT memory_id GROUP BY memory_id ORDER BY COUNT(*) DESC LIMIT k

Returns the top-k memory ids by hit count, ties broken on smaller id
for stable ordering. Empty entity_ids → empty Vec, no SQL issued.
Duplicates harmless (PK on (memory_id, entity_id) guarantees one row
per pair so COUNT(*) is equivalent to COUNT(DISTINCT entity_id)).

The expansion of seed entity ids to 1-hop neighbours happens in
core::memory::recall (next task), not here, because graph traversal
goes through the existing Graph trait chokepoint in db::graph. This
helper is exercised end-to-end by the core integration test in a
later task; no focused db-level test (the SQL is a thin wrapper and
the integration test covers it).

Per spec: docs/superpowers/specs/2026-05-12-memory-graph-lane-design.md
Per plan: docs/superpowers/plans/2026-05-12-memory-graph-lane.md

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Extend `RecallModes` + `RecallParams` + new constant + 5 unit tests (RED)

**Files:**
- Modify: `core/src/memory/recall.rs`

**Rationale:** TDD step. Add the graph flag + new constant + new field + 5 unit tests. The tests fail at compile time (RecallModes::GRAPH_ONLY doesn't exist, the field doesn't exist) — that's the RED state. The structural changes happen in this task; the actual lane wiring is in Task 7.

- [ ] **Step 1: Modify `RecallModes` and add the `GRAPH_ONLY` constant**

In `core/src/memory/recall.rs`, replace the existing `RecallModes` struct definition and the `impl RecallModes` block:

```rust
/// Which retrieval lanes [`recall`] should run.
///
/// Setting a flag to `false` skips the corresponding lane entirely —
/// no SQL is issued, no input is required for that lane. Setting all
/// flags to `false` is permitted but yields an empty fused list; the
/// caller almost always wants at least one lane on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecallModes {
    /// Run the pgvector cosine-distance lane. Requires
    /// [`RecallParams::query_embedding`] to be `Some`.
    pub semantic: bool,
    /// Run the `tsvector` + `ts_rank` lane. Requires
    /// [`RecallParams::query_text`] to be a non-empty string.
    pub lexical: bool,
    /// Run the graph lane (1-hop outbound expansion of
    /// [`RecallParams::seed_entity_ids`] via [`hhagent_db::graph::Graph::neighbors`],
    /// then ranking via [`hhagent_db::memories::graph_search`]).
    /// Requires `seed_entity_ids` to be a non-empty slice.
    pub graph: bool,
}

impl RecallModes {
    /// Run every lane — the most common configuration. Phase 1's
    /// scheduler default.
    pub const ALL: RecallModes = RecallModes {
        semantic: true,
        lexical: true,
        graph: true,
    };

    /// Run only the semantic lane.
    pub const SEMANTIC_ONLY: RecallModes = RecallModes {
        semantic: true,
        lexical: false,
        graph: false,
    };

    /// Run only the lexical lane.
    pub const LEXICAL_ONLY: RecallModes = RecallModes {
        semantic: false,
        lexical: true,
        graph: false,
    };

    /// Run only the graph lane.
    pub const GRAPH_ONLY: RecallModes = RecallModes {
        semantic: false,
        lexical: false,
        graph: true,
    };
}
```

- [ ] **Step 2: Add the `seed_entity_ids` field to `RecallParams`**

Replace the existing `RecallParams` struct and its `impl` block:

```rust
/// Inputs to [`recall`]. Designed as a struct (vs. a positional arg
/// list) so the call site stays readable when the scheduler grows
/// more knobs (filters, recency boost, workspace scope) in later
/// slices — adding a field here is non-breaking.
#[derive(Clone, Debug)]
pub struct RecallParams<'a> {
    /// Free-text query string. Used by the lexical lane; ignored when
    /// [`RecallModes::lexical`] is `false`.
    pub query_text: Option<&'a str>,
    /// Pre-computed query embedding. Used by the semantic lane;
    /// ignored when [`RecallModes::semantic`] is `false`. Must have
    /// length [`EMBEDDING_DIM`] when present and the semantic lane is
    /// enabled.
    pub query_embedding: Option<&'a [f32]>,
    /// Pre-resolved seed entity ids. Used by the graph lane; ignored
    /// when [`RecallModes::graph`] is `false`. The caller resolves
    /// entity names → ids out-of-band (via
    /// [`hhagent_db::graph::Graph::get_entity`] or a future
    /// extraction worker) before invoking recall. An empty slice with
    /// the graph lane enabled is a warn-and-skip, not an error.
    pub seed_entity_ids: Option<&'a [i64]>,
    /// Number of fused results to return. The per-lane queries pull
    /// `k * LANE_FANOUT` candidates so the fusion has enough overlap
    /// to work with even when the lanes disagree heavily — deeper-
    /// than-k per lane is the standard trick for RRF in production
    /// hybrid-search.
    pub k: usize,
    /// Which lanes to run.
    pub modes: RecallModes,
}

impl<'a> RecallParams<'a> {
    /// Common-case constructor: semantic + lexical lanes, default
    /// budget, no graph seeds. Callers that want the graph lane
    /// populate [`RecallParams::seed_entity_ids`] explicitly.
    pub fn new(query_text: &'a str, query_embedding: &'a [f32]) -> Self {
        Self {
            query_text: Some(query_text),
            query_embedding: Some(query_embedding),
            seed_entity_ids: None,
            k: hhagent_db::memories::DEFAULT_RECALL_K,
            modes: RecallModes::ALL,
        }
    }
}
```

- [ ] **Step 3: Add the `GRAPH_FANOUT_CAP_PER_SEED` constant**

Add this near the existing `LANE_FANOUT` constant:

```rust
/// Per-seed cap on outbound neighbour expansion in the graph lane.
///
/// Bounds the worst case: a "hub" entity with thousands of relations
/// (followers, mentions, etc.) cannot flood the expanded set. The
/// value is the order-of-magnitude that [`hhagent_db::graph::Graph::neighbors`]'s
/// `limit` param accepts — generous for typical knowledge graphs,
/// tight against pathological hubs.
pub const GRAPH_FANOUT_CAP_PER_SEED: i64 = 32;
```

- [ ] **Step 4: Add 5 unit tests at the bottom of `core/src/memory/recall.rs`'s existing `mod tests`**

Add inside the `#[cfg(test)] mod tests { ... }` block, after the existing tests:

```rust
    /// `RecallModes::ALL` now includes the graph lane (third lane
    /// added in Option P). If a future fourth lane lands without
    /// updating `ALL`, this trips loudly.
    #[test]
    fn recall_modes_all_includes_graph() {
        assert!(RecallModes::ALL.graph);
        // And the existing flags stay on too.
        assert!(RecallModes::ALL.semantic);
        assert!(RecallModes::ALL.lexical);
    }

    /// `RecallModes::GRAPH_ONLY` exact shape pin.
    #[test]
    fn recall_modes_graph_only_is_only_graph() {
        let m = RecallModes::GRAPH_ONLY;
        assert!(!m.semantic);
        assert!(!m.lexical);
        assert!(m.graph);
    }

    /// `RecallParams::new(text, emb)` leaves `seed_entity_ids = None`
    /// — graph lane stays off implicitly when caller doesn't opt in
    /// via explicit field set. Preserves the no-breaking-call-sites
    /// invariant for `new()` consumers.
    #[test]
    fn recall_params_new_default_seed_entity_ids_is_none() {
        let emb: Vec<f32> = vec![0.0; 1024];
        let params = RecallParams::new("query text", &emb);
        assert!(params.seed_entity_ids.is_none());
    }

    /// Pin `GRAPH_FANOUT_CAP_PER_SEED = 32` so a future tune is an
    /// explicit PR.
    #[test]
    fn graph_fanout_cap_per_seed_is_thirty_two() {
        assert_eq!(GRAPH_FANOUT_CAP_PER_SEED, 32);
    }
```

Also update the existing `recall_modes_default_runs_every_lane` test to assert `graph` too:

Replace:

```rust
    #[test]
    fn recall_modes_default_runs_every_lane() {
        let m = RecallModes::default();
        assert!(m.semantic);
        assert!(m.lexical);
    }
```

with:

```rust
    #[test]
    fn recall_modes_default_runs_every_lane() {
        let m = RecallModes::default();
        assert!(m.semantic);
        assert!(m.lexical);
        assert!(m.graph);
    }
```

And update the existing `recall_modes_all_is_every_lane_on` test:

Replace:

```rust
    #[test]
    fn recall_modes_all_is_every_lane_on() {
        assert_eq!(RecallModes::ALL, RecallModes { semantic: true, lexical: true });
    }
```

with:

```rust
    #[test]
    fn recall_modes_all_is_every_lane_on() {
        assert_eq!(
            RecallModes::ALL,
            RecallModes { semantic: true, lexical: true, graph: true }
        );
    }
```

And update `recall_modes_semantic_only_disables_lexical`:

Replace:

```rust
    #[test]
    fn recall_modes_semantic_only_disables_lexical() {
        let m = RecallModes::SEMANTIC_ONLY;
        assert!(m.semantic);
        assert!(!m.lexical);
    }
```

with:

```rust
    #[test]
    fn recall_modes_semantic_only_disables_lexical() {
        let m = RecallModes::SEMANTIC_ONLY;
        assert!(m.semantic);
        assert!(!m.lexical);
        assert!(!m.graph);
    }
```

And `recall_modes_lexical_only_disables_semantic`:

```rust
    #[test]
    fn recall_modes_lexical_only_disables_semantic() {
        let m = RecallModes::LEXICAL_ONLY;
        assert!(!m.semantic);
        assert!(m.lexical);
        assert!(!m.graph);
    }
```

- [ ] **Step 5: Run the recall unit tests and verify GREEN**

```sh
source "$HOME/.cargo/env"
cargo test -p hhagent-core --lib memory::recall 2>&1 | tail -15
```

Expected: all recall unit tests pass (the existing 11 + 4 new + 3 updated). Net unit test count for the file: 11 → 15 (the 4 new tests; the 3 updated ones replace their predecessors in place).

- [ ] **Step 6: Verify the rest of the workspace still compiles**

The `RecallModes` struct gained a field, so any literal `RecallModes { semantic, lexical }` outside this file would break. Check:

```sh
cargo build --workspace 2>&1 | tail -5
```

Expected: clean build. If the call sites for `RecallModes { semantic: ..., lexical: ... }` exist anywhere (search `grep -rn "RecallModes {" --include='*.rs' .`), update them to include `graph: false` (or `RecallModes::ALL` / `SEMANTIC_ONLY` / etc. as appropriate).

Note: as of the spec, only `core/src/memory/recall.rs` itself and the `memory_recall_e2e.rs` integration test reference `RecallModes`. The integration test uses `RecallModes::ALL` / `SEMANTIC_ONLY` / `LEXICAL_ONLY` (the constants), not field-literal syntax. So no edits needed there.

- [ ] **Step 7: Commit**

```sh
git add core/src/memory/recall.rs
git commit -m "$(cat <<'EOF'
feat(core/memory): RecallModes::graph + GRAPH_ONLY + seed_entity_ids

Structural additions for the graph lane (lane wiring lands next task):

* RecallModes gains `graph: bool` field. RecallModes::ALL now includes
  graph=true; existing constants (SEMANTIC_ONLY / LEXICAL_ONLY) get
  graph=false explicitly. New constant RecallModes::GRAPH_ONLY.

* RecallParams gains `seed_entity_ids: Option<&'a [i64]>` — pre-resolved
  entity ids for the graph lane. Mirrors the existing `query_embedding`
  pattern (caller pre-computes inputs; recall doesn't issue side
  queries). RecallParams::new() keeps seed_entity_ids=None so existing
  callers see no behavioural change (graph lane stays off when seeds
  aren't supplied even if RecallModes::ALL is passed).

* Constant GRAPH_FANOUT_CAP_PER_SEED = 32. Per-seed cap on Graph::neighbors
  expansion; defends against hub-entity flood (an entity with thousands
  of outbound edges contributes at most 32 to the expanded set).

* 4 new unit tests + 4 existing tests updated to also assert on the
  new `graph` field. Net unit test count: 11 → 15.

The recall() function itself is not yet rewired — that lands in the
next task. Workspace builds clean; existing call sites use named
RecallModes constants (ALL/SEMANTIC_ONLY/LEXICAL_ONLY/GRAPH_ONLY) not
field literals, so the struct widening is non-breaking.

Per spec: docs/superpowers/specs/2026-05-12-memory-graph-lane-design.md
Per plan: docs/superpowers/plans/2026-05-12-memory-graph-lane.md

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: Wire the graph lane into `recall()`

**Files:**
- Modify: `core/Cargo.toml` (add `futures` direct dep)
- Modify: `core/src/memory/recall.rs` (extend the `recall` fn)

**Rationale:** Make `recall()` actually execute the graph lane when enabled. Uses `futures::future::try_join_all` to fan `Graph::neighbors` over seeds in parallel, dedups expansion into a HashSet, then calls `db::memories::graph_search`. The lane's id-list joins the others into the existing RRF fusion path unchanged.

- [ ] **Step 1: Add `futures` as a direct dep of `core`**

Modify `core/Cargo.toml`:

In the `[dependencies]` block, after `sqlx = { workspace = true }`, add:

```toml
futures            = { workspace = true }
```

The workspace already declares `futures = "0.3"` at the workspace root, so `workspace = true` is the right shape.

- [ ] **Step 2: Add the graph-lane execution block to `recall()`**

In `core/src/memory/recall.rs`, locate the existing `recall` function body. After the existing `if params.modes.lexical { ... }` block and BEFORE the `if lane_lists.is_empty() { return Ok(Vec::new()); }` early-return, insert:

```rust
    if params.modes.graph {
        match params.seed_entity_ids {
            Some(seeds) if !seeds.is_empty() => {
                // 1-hop outbound expansion via the Graph chokepoint,
                // fanned out in parallel. Per-seed cap defends against
                // hub explosion: an entity with thousands of outbound
                // edges contributes at most GRAPH_FANOUT_CAP_PER_SEED.
                let graph = hhagent_db::graph::PgGraph::new(pool);
                let neighbour_lists = futures::future::try_join_all(
                    seeds.iter().map(|&s| {
                        graph.neighbors(s, None, GRAPH_FANOUT_CAP_PER_SEED)
                    }),
                )
                .await?;

                // Deduped expanded set: seeds ∪ all returned neighbour ids.
                // HashSet strips duplicates when two seeds share a 1-hop
                // hop, or when a seed is also a neighbour of another seed.
                let mut expanded: std::collections::HashSet<i64> =
                    seeds.iter().copied().collect();
                for list in &neighbour_lists {
                    for entity in list {
                        expanded.insert(entity.id);
                    }
                }
                let expanded_vec: Vec<i64> = expanded.into_iter().collect();

                lane_lists.push(
                    hhagent_db::memories::graph_search(pool, &expanded_vec, lane_k)
                        .await?,
                );
            }
            _ => {
                tracing::warn!(
                    target: "hhagent::memory",
                    "graph lane requested but seed_entity_ids is empty or None; skipping"
                );
            }
        }
    }
```

The existing RRF fusion + hydration path below this block is unchanged — it sees `lane_lists` as `Vec<Vec<i64>>` of length 0..=3 instead of 0..=2, and RRF is commutative across lanes.

- [ ] **Step 3: Verify workspace compiles**

```sh
source "$HOME/.cargo/env"
cargo build --workspace 2>&1 | tail -5
```

Expected: clean.

- [ ] **Step 4: Run existing recall tests to verify they still pass**

```sh
cargo test -p hhagent-core --lib memory 2>&1 | tail -10
cargo test -p hhagent-core --test memory_recall_e2e 2>&1 | tail -10
```

Expected: all PASS. The existing integration test doesn't set `seed_entity_ids`, so the graph lane logs a warn-and-skip and the test's existing assertions are unchanged.

- [ ] **Step 5: Commit**

```sh
git add core/Cargo.toml core/src/memory/recall.rs
git commit -m "$(cat <<'EOF'
feat(core/memory): wire graph lane into recall()

The actual lane execution. When RecallModes::graph is true and
seed_entity_ids is non-empty:

  1. for each seed: PgGraph::neighbors(seed, None, GRAPH_FANOUT_CAP_PER_SEED)
     via futures::future::try_join_all (parallel fan-out)
  2. dedup into HashSet<i64> — seeds ∪ all returned neighbour ids
  3. db::memories::graph_search(pool, &expanded_vec, lane_k)
  4. push ranked id-list into the existing lane_lists vector
  5. existing RRF fusion + hydration path consumes it unchanged
     (RRF is commutative across lanes)

Empty / None seed_entity_ids → warn-and-skip (matches the existing
semantic and lexical degrade behaviour for missing inputs).

Adds `futures` as a direct dep of core (was transitively available
via tokio/sqlx but the explicit form makes the dependency surface
truthful).

The graph lane is unexercised end-to-end by tests until the next
task extends memory_recall_e2e.rs; the existing tests in this file
don't set seed_entity_ids so the lane skips, leaving their
assertions byte-identical.

Per spec: docs/superpowers/specs/2026-05-12-memory-graph-lane-design.md
Per plan: docs/superpowers/plans/2026-05-12-memory-graph-lane.md

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: Extend `memory_recall_e2e.rs` with graph-lane assertions (RED → GREEN)

**Files:**
- Modify: `core/tests/memory_recall_e2e.rs`

**Rationale:** End-to-end pin for the graph lane. The existing single `#[test]` fn brings up a per-test PG cluster + seeds 3 memories with deterministic embeddings; we extend it with entity setup, link-table population, and 4 new assertion blocks.

- [ ] **Step 1: Read the existing test to find the splice point**

```sh
cat /home/hherb/src/hhagent/core/tests/memory_recall_e2e.rs | tail -40
```

Identify the line just before the test fn's closing `drop(pool); }` (or equivalent cleanup tail). The new assertion blocks go in just before that cleanup.

- [ ] **Step 2: Add the graph-lane setup + 4 assertion blocks**

Append, inside the existing `#[tokio::test]` fn body and BEFORE the final `drop(pool)` (or pool-close call):

```rust
    // ─── Graph lane: setup ─────────────────────────────────────────
    //
    // alice owns cat (relation); bob is unconnected.
    // mem_a is tagged with {alice, cat}; mem_b with {cat}; mem_c with {bob}.
    use hhagent_db::graph::PgGraph;
    let graph_g = PgGraph::new(&pool);
    let alice_id = graph_g
        .upsert_entity("person", "alice", &serde_json::json!({}))
        .await
        .expect("upsert alice");
    let bob_id = graph_g
        .upsert_entity("person", "bob", &serde_json::json!({}))
        .await
        .expect("upsert bob");
    let cat_id = graph_g
        .upsert_entity("animal", "cat", &serde_json::json!({}))
        .await
        .expect("upsert cat");
    graph_g
        .upsert_relation(alice_id, cat_id, "owns", &serde_json::json!({}))
        .await
        .expect("upsert relation");

    hhagent_db::memories::link_memory_to_entities(&pool, mem_a, &[alice_id, cat_id])
        .await
        .expect("link mem_a");
    hhagent_db::memories::link_memory_to_entities(&pool, mem_b, &[cat_id])
        .await
        .expect("link mem_b");
    hhagent_db::memories::link_memory_to_entities(&pool, mem_c, &[bob_id])
        .await
        .expect("link mem_c");

    // ─── Assertion 1: GRAPH_ONLY with seed=[alice] surfaces A first ─
    //
    // Expanded set = {alice, cat} (alice + alice's 1-hop = cat).
    // mem_a is linked to BOTH alice and cat → hit count 2.
    // mem_b is linked to cat only → hit count 1.
    // mem_c is linked to bob (NOT in expanded) → absent from result.
    let r = hhagent_core::memory::recall(
        &pool,
        &hhagent_core::memory::RecallParams {
            query_text: None,
            query_embedding: None,
            seed_entity_ids: Some(&[alice_id]),
            k: 10,
            modes: hhagent_core::memory::RecallModes::GRAPH_ONLY,
        },
    )
    .await
    .expect("graph-only alice recall");
    assert_eq!(r.len(), 2, "expected mem_a + mem_b only");
    assert_eq!(r[0].id, mem_a, "mem_a (hit=2) must rank first");
    assert_eq!(r[1].id, mem_b, "mem_b (hit=1) must rank second");
    assert!(r.iter().all(|m| m.id != mem_c), "mem_c must be absent");

    // ─── Assertion 2: GRAPH_ONLY with seed=[bob] surfaces C only ────
    //
    // Expanded set = {bob} (bob has no neighbours). Only mem_c links bob.
    let r = hhagent_core::memory::recall(
        &pool,
        &hhagent_core::memory::RecallParams {
            query_text: None,
            query_embedding: None,
            seed_entity_ids: Some(&[bob_id]),
            k: 10,
            modes: hhagent_core::memory::RecallModes::GRAPH_ONLY,
        },
    )
    .await
    .expect("graph-only bob recall");
    assert_eq!(r.len(), 1, "expected mem_c only");
    assert_eq!(r[0].id, mem_c);

    // ─── Assertion 3: ALL fuses graph + semantic + lexical ──────────
    //
    // query_text "alpha" + query_emb(text="alpha") + seed=[alice].
    // Each lane's top-1 is mem_a:
    //   * semantic: mem_a's embedding is exact match (cosine = 0)
    //   * lexical: mem_a's body contains "alpha"
    //   * graph: mem_a has hit count 2 (alice + cat)
    // Fused RRF rank-1 must be mem_a.
    let q_emb = hhagent_tests_common::text_to_embedding("alpha");
    let r = hhagent_core::memory::recall(
        &pool,
        &hhagent_core::memory::RecallParams {
            query_text: Some("alpha"),
            query_embedding: Some(&q_emb),
            seed_entity_ids: Some(&[alice_id]),
            k: 10,
            modes: hhagent_core::memory::RecallModes::ALL,
        },
    )
    .await
    .expect("ALL-lanes alpha+alice recall");
    assert_eq!(r[0].id, mem_a, "fused top-1 must be mem_a");

    // ─── Assertion 4: empty seeds with graph mode on → lane skipped ─
    //
    // Empty seed slice + GRAPH_ONLY → no lane runs → empty fused list,
    // not an error. Matches warn-and-skip semantics for missing inputs.
    let empty: &[i64] = &[];
    let r = hhagent_core::memory::recall(
        &pool,
        &hhagent_core::memory::RecallParams {
            query_text: None,
            query_embedding: None,
            seed_entity_ids: Some(empty),
            k: 10,
            modes: hhagent_core::memory::RecallModes::GRAPH_ONLY,
        },
    )
    .await
    .expect("empty-seeds graph-only recall");
    assert!(r.is_empty(), "empty seeds + graph-only must return empty");
```

The placeholders `mem_a`, `mem_b`, `mem_c` are the variable names the existing test already uses for its three seeded memory ids. Verify by reading the file in Step 1; if they're named differently (e.g. `id_a` / `id_b` / `id_c`), substitute accordingly in the new block.

- [ ] **Step 3: Run the extended integration test**

```sh
source "$HOME/.cargo/env"
cargo test -p hhagent-core --test memory_recall_e2e -- --nocapture 2>&1 | tail -20
```

Expected: PASS in ~2.5–3 s. All four new assertion blocks plus the existing assertions.

- [ ] **Step 4: Run focused determinism check (3 consecutive runs)**

```sh
for i in 1 2 3; do
    echo "=== run $i ==="
    cargo test -p hhagent-core --test memory_recall_e2e 2>&1 | tail -3
done
```

Expected: 3 consecutive PASS results. Per the project convention for new integration tests; flaky test fails this gate.

- [ ] **Step 5: Run the full workspace test suite**

```sh
cargo test --workspace 2>&1 | tail -5
```

Expected: `350 passed; 0 failed; 0 ignored` (modulo the 2 pre-existing `ignored` doctests in hhagent-sandbox / hhagent-worker-prelude, which the spec accounted for). 0 SKIP lines on Linux with the AppArmor profile installed.

- [ ] **Step 6: Commit**

```sh
git add core/tests/memory_recall_e2e.rs
git commit -m "$(cat <<'EOF'
test(core/memory): graph lane e2e — entities, linkage, fused recall

Extends the existing memory_recall_e2e integration test with the
graph lane assertions. The test now seeds:

  * 3 entities: person/alice, person/bob, animal/cat
  * 1 relation: alice --[owns]--> cat
  * 3 memory↔entity links:
      mem_a {alice, cat}, mem_b {cat}, mem_c {bob}

Four new assertion blocks:

  1. RecallModes::GRAPH_ONLY with seed=[alice]:
       expanded = {alice, cat}; mem_a (2 hits) ranks above mem_b
       (1 hit); mem_c absent.

  2. RecallModes::GRAPH_ONLY with seed=[bob]:
       expanded = {bob} (bob has no neighbours); only mem_c returned.

  3. RecallModes::ALL with seed=[alice] + query_text="alpha" +
     query_embedding=text_to_embedding("alpha"):
       each lane's top-1 is mem_a; fused RRF rank-1 must be mem_a.

  4. RecallModes::GRAPH_ONLY with empty seed slice:
       warn-and-skip → empty result, no error. Matches semantic /
       lexical degrade semantics.

Three consecutive focused runs deterministic at ~2.5s each.

Workspace test count: 342 → 350 (+3 db integration in Task 4 +5
core unit in Task 6 +0 new integration fns; the 4 new assertion
blocks are inside the existing single #[test] fn).

Per spec: docs/superpowers/specs/2026-05-12-memory-graph-lane-design.md
Per plan: docs/superpowers/plans/2026-05-12-memory-graph-lane.md

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: Update HANDOVER + ROADMAP

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`
- Modify: `docs/devel/ROADMAP.md`

**Rationale:** End-of-session housekeeping per CLAUDE.md rule #8 and HANDOVER's own update checklist.

- [ ] **Step 1: Update HANDOVER.md header fields**

In `docs/devel/handovers/HANDOVER.md`:

- Set `**Last updated:**` to today's date (2026-05-12, or the actual date if running later).
- Set `**Last commit (main):**` will only be true once merged. For now, on this branch, set:
  `**Branch:** feat/memory-graph-lane (off main at 97f2743).` and note the most recent commit hash on the branch.

- [ ] **Step 2: Add a "Recently completed (this session)" entry at the top of the timeline**

Insert before the existing "Recently completed (previous session, 2026-05-12 — issue #15 hoist...)" entry. The entry should cover:

* What shipped: migrations 0007 + 0008; `link_memory_to_entities`; `graph_search`; `RecallModes::graph` + `GRAPH_ONLY`; `RecallParams::seed_entity_ids`; `GRAPH_FANOUT_CAP_PER_SEED = 32`; graph-lane wiring with `try_join_all` over `Graph::neighbors`; `deleted_memories` trigger.
* Test count delta: 342 → 350.
* Audit-row gap: graph lane does NOT write `actor='?'` audit rows because recall reads are not actions (matches the existing semantic + lexical lanes). The `deleted_memories` table IS itself an audit row but on the memory store, not on `audit_log`.
* What this slice deliberately does NOT do (copy from spec's "What this slice deliberately does NOT do").

- [ ] **Step 3: Refresh the "Working state" tree**

In HANDOVER.md's `Working state` ASCII tree:

* Under `core`: extend the `memory/` description to mention `recall.rs` carries the new graph lane and `seed_entity_ids` field, `GRAPH_FANOUT_CAP_PER_SEED`, and `RecallModes::GRAPH_ONLY`.
* Under `db`: extend the description to mention `link_memory_to_entities`, `graph_search`, and the new `memory_entities` + `deleted_memories` tables from migrations 0007 + 0008.

- [ ] **Step 4: Refresh the test count + suite table**

* Update the bold green line near the working state from `342 tests passed` to `350 tests passed`.
* In the "What's verified" table, update the `core` unit row to mention the 4 new graph-lane shape pins, and update the `core/tests/memory_recall_e2e` row to mention the 4 new graph-lane assertion blocks.
* Update the `db` integration row to mention the 3 new graph-lane / delete-audit tests.

- [ ] **Step 5: Update "Next TODO (pick one)" section**

Remove the "Option P — entity↔memory linkage + graph lane in recall" item from "Existing Phase 1 cont. pickups" (now shipped).

The remaining items become the next session's pickups. Likely candidates worth flagging at the top:

* Production caller wiring: extend `RouterAgent::formulate_plan` to populate `seed_entity_ids` from extracted entities (once an extraction step lands).
* `entities.embedding` population path (still NULL; bge-m3 over kind+name+attrs would seed an entity-similarity lane).
* Issue #16 (WorkerCommand seal hole) and #17 (recall warn-and-degrade tightening) still open.

- [ ] **Step 6: Tick the ROADMAP item**

In `docs/devel/ROADMAP.md`, locate the line under "Phase 1 — Memory & Loop":

```
- [ ] Graph lane in `memory::recall` — entity↔memory linkage ...
```

Change to:

```
- [x] Graph lane in `memory::recall` — entity↔memory linkage (recommended: `memory_entities` join table) + plumb `Graph::neighbors` as a third lane fused alongside semantic + lexical (Phase 1 cont. — Option P) — landed 2026-05-12 on branch `feat/memory-graph-lane`. ...
```

with a paragraph describing what shipped, the test count delta, and pointers to the spec + plan + closing commit hash (to be filled in after the final commit).

- [ ] **Step 7: Verify the docs build cleanly**

There's no docs build step, but skim the rendered Markdown for malformed tables / broken links:

```sh
head -50 docs/devel/handovers/HANDOVER.md
head -20 docs/devel/ROADMAP.md
```

- [ ] **Step 8: Commit the doc updates**

```sh
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "$(cat <<'EOF'
docs(handover,roadmap): graph lane in memory::recall shipped

Closes the ROADMAP item "Graph lane in memory::recall — entity↔memory
linkage + plumb Graph::neighbors as a third lane fused alongside
semantic + lexical". Branch feat/memory-graph-lane; test count
342 → 350 (+3 db integration / +5 core unit / +4 new e2e assertion
blocks in the existing test fn).

Updates HANDOVER's working state, suite-table, Recently-completed
section, and Next-TODO list. Removes the "Option P" pickup from the
queue.

Per spec: docs/superpowers/specs/2026-05-12-memory-graph-lane-design.md
Per plan: docs/superpowers/plans/2026-05-12-memory-graph-lane.md

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Final verification

After Task 9 completes:

```sh
source "$HOME/.cargo/env"
cargo test --workspace 2>&1 | tail -5      # 350 passed / 0 failed / 0 SKIP
git log --oneline main..HEAD               # 8 commits on this branch (spec/handover + 8 tasks - Task 5 had 1 commit, Task 4 had 1 commit, Tasks 6/7/8/9 each had 1 commit, plus Tasks 1+2+3 had 3 commits with Task 3 not yet committed since it's a RED-state test that ships with Task 4)
git status                                 # clean
```

The branch is ready to push and open a PR against `main`. PR title suggestion:

```
feat(memory): graph lane in recall + entity linkage + deleted_memories audit (#NN)
```

where `#NN` matches the ROADMAP item or a fresh tracking issue if one was opened. The PR body should cite the spec at `docs/superpowers/specs/2026-05-12-memory-graph-lane-design.md` and link to the plan.

---

## Plan self-review notes

* **Spec coverage:** every section of the spec has a corresponding task. Migration 0007 → Task 1. Migration 0008 → Task 2. DB tests → Task 3. `link_memory_to_entities` → Task 4. `graph_search` → Task 5. RecallModes/RecallParams structural changes → Task 6. Graph-lane wiring → Task 7. Core integration test → Task 8. HANDOVER/ROADMAP → Task 9.
* **Type consistency:** function signatures match between tasks and the spec (`link_memory_to_entities(executor, memory_id, &[i64]) -> Result<u64, DbError>`; `graph_search(executor, &[i64], usize) -> Result<Vec<i64>, DbError>`; `RecallParams::seed_entity_ids: Option<&'a [i64]>`; `GRAPH_FANOUT_CAP_PER_SEED: i64 = 32`). Identifier shape is consistent across Tasks 4/5/6/7/8.
* **Placeholder scan:** no TBD/TODO/implement-later patterns. Test bodies are complete code blocks. The "subst mem_a/mem_b/mem_c if differently named" note in Task 8 Step 2 is a verification step, not a placeholder — the splice depends on existing names which the engineer reads in Step 1.
