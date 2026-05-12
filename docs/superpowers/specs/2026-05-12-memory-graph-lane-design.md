# Memory recall — graph lane (Option P)

**Date:** 2026-05-12
**Status:** Design (pre-implementation)
**Branch (proposed):** `feat/memory-graph-lane`
**Off:** `main` at `97f2743`
**Closes:** ROADMAP "Phase 1 — Memory & Loop" item "Graph lane in `memory::recall`"

---

## Goal

Add the third lane to `core::memory::recall` so the fused recall result respects entity-anchored relevance, not just text/semantic similarity. A query carrying seed entity ids (and their 1-hop outbound neighbours) joins through a new `memory_entities` table to surface memories tagged with any of those entities; the ranked id-list fuses with the existing semantic + lexical lanes via Reciprocal Rank Fusion.

The slice ships the schema, the writer-side helper, the read-side helper, the lane wiring in `recall`, the cascade-aware delete-audit infrastructure for `memories`, and the integration tests that pin the contract end-to-end.

## Decisions locked in (brainstorming output)

1. **Linkage storage:** new `memory_entities` join table (option P1 in HANDOVER). Rejected: `memories.metadata->'entities'` JSONB array (option P2) — couples linkage shape to memory shape, can't cascade on entity delete, awkward count semantics.
2. **Traversal:** 1-hop outbound expansion via `Graph::neighbors`, with a per-seed fanout cap of 32. Rejected: direct-only (no traversal — degenerate "graph" lane); N-hop expansion (unpredictable cost on dense subgraphs).
3. **Writer API:** standalone `link_memory_to_entities(pool, memory_id, &[entity_id])` helper. Rejected: extending `insert_memory` signature (forces every call site to know entity ids up front; future "insert now, link later when extraction finishes" becomes awkward).
4. **Seed shape:** pre-resolved `seed_entity_ids: Option<&[i64]>` on `RecallParams`. Rejected: natural-key `(kind, name)` input (silently drops unknowns, adds round-trips in the hot path); dual-shape API (premature, no two real callers differ today).
5. **Delete audit:** trigger-driven `deleted_memories` append-only journal (responding to user's "memories never get deleted in a cascade" requirement, expanded). Rejected: REVOKE DELETE on memories from `hhagent_runtime` (blocks all future legitimate memory deletion); RESTRICT FK direction (link-rows-must-be-cleared-first awkwardness).

## Architecture

```
core/src/memory/recall.rs              ← extended: new graph lane + seed param
       │
       │ composes:
       ▼
db::graph::PgGraph::neighbors          ← existing chokepoint for graph traffic
db::memories::link_memory_to_entities  ← NEW writer-side helper
db::memories::graph_search             ← NEW count-and-rank read helper

db/migrations/0007_memory_entities.sql      ← NEW table + index + GRANT
db/migrations/0008_deleted_memories_audit.sql ← NEW table + trigger + GRANT
```

**Chokepoint discipline:**
- `db::graph` owns every read/write of `entities` and `relations` (existing rule, unchanged).
- `db::memories` now owns every read/write of `memories`, `memory_entities`, and `deleted_memories`. The join-table SQL lives here (not in `db::graph`) because its purpose is memory recall.
- `core::memory::recall` composes them — no SQL of its own, no direct graph SQL.

**Read flow** (when graph lane is on and seeds are non-empty):

```
recall(pool, params { seed_entity_ids: Some([alice_id]), ... })
  │
  ├─ semantic_search()     ─┐
  ├─ lexical_search()       ├─ ranked id-lists ─→ RRF ─→ top-k ─→ fetch_by_ids
  │                         │
  └─ graph lane:            │
     1. for each seed: Graph::neighbors(seed, None, 32)  via try_join_all
     2. dedup into HashSet<i64> — seeds + their 1-hop neighbours
     3. memories::graph_search(pool, &expanded_set, lane_k)  ──┘
```

## Schema — Migration 0007

```sql
-- 0007_memory_entities.sql
--
-- Join table linking `memories` rows to `entities` nodes. Powers the
-- graph lane in `core::memory::recall`: a query carrying seed entity
-- ids (and their 1-hop neighbours) joins through this table to find
-- the memories tagged with any of those entities.

CREATE TABLE memory_entities (
    memory_id  BIGINT NOT NULL
        REFERENCES memories(id) ON DELETE CASCADE,
    entity_id  BIGINT NOT NULL
        REFERENCES entities(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (memory_id, entity_id)
);

CREATE INDEX memory_entities_entity_idx
    ON memory_entities (entity_id);

GRANT SELECT, INSERT, UPDATE, DELETE ON memory_entities TO hhagent_runtime;
```

**Notes:**
- PK on `(memory_id, entity_id)` indexes the writer's `ON CONFLICT` path and supports `WHERE memory_id = $1` lookups.
- Separate `(entity_id)` index supports the read path `WHERE entity_id = ANY($1)`.
- `ON DELETE CASCADE` on both sides keeps the table internally consistent. Cascade direction flows from referenced row to referencing row, so deleting a link row can never delete a parent memory or entity.
- Runtime role gets full CRUD (matches `memories` / `entities` / `relations` posture).

## Schema — Migration 0008

```sql
-- 0008_deleted_memories_audit.sql
--
-- Trigger-driven append-only journal of every row deleted from
-- `memories`. Phase 1 has no caller that deletes memories today, but
-- the cascade infrastructure in 0007 already treats memory deletion
-- as a real future operation. When that operation materialises, the
-- trigger guarantees the row is preserved before it vanishes.

CREATE TABLE deleted_memories (
    id          BIGINT      PRIMARY KEY,
    body        TEXT        NOT NULL,
    metadata    JSONB       NOT NULL,
    embedding   vector(1024),
    created_at  TIMESTAMPTZ NOT NULL,
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

GRANT  SELECT, INSERT ON deleted_memories TO hhagent_runtime;
REVOKE UPDATE, DELETE, TRUNCATE ON deleted_memories FROM hhagent_runtime;
```

**Notes:**
- Append-only by GRANT shape — same defence as `audit_log` (migration 0002). UPDATE/DELETE revoked at the DB layer.
- Trigger is `SECURITY INVOKER` (default) — runs as the DELETE issuer's role. Runtime needs INSERT grant for the trigger to fire successfully.
- PK on `id` preserves the original `memories.id`. A row can only be deleted once, so no duplicates possible.
- `deleted_by` column omitted — runtime role is the only login role today. Future multi-actor world can add a column populated from a session GUC.

## Library surfaces — `db::memories`

```rust
/// Link a memory to a set of entities. Idempotent: re-linking the same
/// pair is a no-op via ON CONFLICT DO NOTHING. Returns the count of
/// genuinely new links (zero on a full re-link).
///
/// Empty `entity_ids` is a fast-path no-op (no SQL issued). FK violation
/// (unknown memory_id or entity_id) surfaces as DbError::Query.
pub async fn link_memory_to_entities<'e, E>(
    executor: E,
    memory_id: i64,
    entity_ids: &[i64],
) -> Result<u64, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>;
```

SQL shape:

```sql
INSERT INTO memory_entities (memory_id, entity_id)
SELECT $1::bigint, eid FROM unnest($2::bigint[]) AS t(eid)
ON CONFLICT (memory_id, entity_id) DO NOTHING
```

```rust
/// Graph lane: rank memories by how many of the supplied entity ids
/// they're linked to.
///
/// Returns up to `k` memory ids in best-first order (highest hit count
/// first; ties broken by smaller id for stable ordering). `entity_ids`
/// is the *already-expanded* set (seeds + 1-hop neighbours); expansion
/// happens in `core::memory::recall`, not here, because graph
/// traversal goes through the `Graph` chokepoint.
///
/// Empty `entity_ids` → empty Vec, no SQL issued.
pub async fn graph_search<'e, E>(
    executor: E,
    entity_ids: &[i64],
    k: usize,
) -> Result<Vec<i64>, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>;
```

SQL shape:

```sql
SELECT memory_id
FROM memory_entities
WHERE entity_id = ANY($1::bigint[])
GROUP BY memory_id
ORDER BY COUNT(*) DESC, memory_id ASC
LIMIT $2
```

## Library surfaces — `core::memory::recall`

```rust
/// Per-seed cap on outbound neighbour expansion in the graph lane.
/// Bounds the worst case: a "hub" entity with thousands of relations
/// cannot flood the expanded set.
const GRAPH_FANOUT_CAP_PER_SEED: i64 = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecallModes {
    pub semantic: bool,
    pub lexical:  bool,
    pub graph:    bool,   // NEW
}

impl RecallModes {
    pub const ALL: RecallModes = RecallModes {
        semantic: true, lexical: true, graph: true,
    };
    pub const SEMANTIC_ONLY: RecallModes = RecallModes {
        semantic: true,  lexical: false, graph: false,
    };
    pub const LEXICAL_ONLY:  RecallModes = RecallModes {
        semantic: false, lexical: true,  graph: false,
    };
    pub const GRAPH_ONLY:    RecallModes = RecallModes {     // NEW
        semantic: false, lexical: false, graph: true,
    };
}

#[derive(Clone, Debug)]
pub struct RecallParams<'a> {
    pub query_text:       Option<&'a str>,
    pub query_embedding:  Option<&'a [f32]>,
    pub seed_entity_ids:  Option<&'a [i64]>,    // NEW
    pub k:                usize,
    pub modes:            RecallModes,
}
```

`RecallParams::new(text, embedding)` keeps `seed_entity_ids: None` so the graph lane stays off implicitly when the caller doesn't opt in — no breaking call sites in `core/tests/memory_recall_e2e.rs` or any other existing consumer.

**Graph lane execution** is slotted in `recall()` between the lexical lane and the RRF fusion:

```rust
if params.modes.graph {
    match params.seed_entity_ids {
        Some(seeds) if !seeds.is_empty() => {
            let graph = hhagent_db::graph::PgGraph::new(pool);
            let neighbour_lists = futures::future::try_join_all(
                seeds.iter().map(|&s| graph.neighbors(s, None, GRAPH_FANOUT_CAP_PER_SEED))
            ).await?;
            let mut expanded: std::collections::HashSet<i64> =
                seeds.iter().copied().collect();
            for list in &neighbour_lists {
                for entity in list {
                    expanded.insert(entity.id);
                }
            }
            let expanded_vec: Vec<i64> = expanded.into_iter().collect();
            lane_lists.push(
                hhagent_db::memories::graph_search(pool, &expanded_vec, lane_k).await?,
            );
        }
        _ => {
            tracing::warn!(
                target: "hhagent::memory",
                "graph lane requested but seed_entity_ids is empty; skipping"
            );
        }
    }
}
```

**Notes:**
- `futures::future::try_join_all` parallelises `Graph::neighbors` calls across seeds. Small wins at N=1–10 seeds.
- HashSet dedup strips overlapping neighbours (two seeds sharing a 1-hop hop) and avoids inflating a seed that's also a neighbour of another seed.
- `graph_search` doesn't dedupe `entity_ids` itself — set-semantic SQL handles duplicates harmlessly, but the caller's HashSet already ensures uniqueness.

## Error handling

The new surfaces add **zero new error variants**. Everything flows through the existing paths:

| Failure                                      | Path                                                                              |
| -------------------------------------------- | --------------------------------------------------------------------------------- |
| Empty seed list while graph mode is on       | `tracing::warn!` + skip lane (matches semantic / lexical degrade)                 |
| `Graph::neighbors` SQL error                 | `DbError::Query(_)` → propagated from `recall` via `?`                            |
| `graph_search` SQL error                     | `DbError::Query(_)` → propagated from `recall` via `?`                            |
| `link_memory_to_entities` SQL error          | `DbError::Query(_)` → caller decides                                              |
| One seed id doesn't exist in `entities`      | `Graph::neighbors` returns empty list for that seed (predicate match, no row). Silently dropped. No error. |
| One entity_id passed to `link_memory_to_entities` doesn't exist | FK violation → `DbError::Query`. ON CONFLICT DO NOTHING doesn't suppress FK failures; the whole batch fails atomically (zero rows inserted). |
| Delete memory while linked entities exist    | Cascade drops `memory_entities` rows; trigger writes a `deleted_memories` row. Link rows are NOT recorded in `deleted_memories` (we audit deleted memories, not links). |
| Delete entity while linked memories exist    | Cascade drops `memory_entities` rows. Memories untouched. No audit row.           |

**One degrade-vs-error semantics decision** documented for future reference: if `Graph::neighbors` succeeds but returns an empty list for every seed, the expanded set still contains the seeds themselves, so `graph_search` still runs against them. A query about "alice" with no neighbours still surfaces alice-tagged memories. Only an empty *input* set skips the entire lane.

The slice extends the existing "warn and degrade on missing input" pattern; it does not address open issue #17 (which discusses tightening the degrade behaviour for ALL lanes — independent change).

## Testing

### DB integration (`db/tests/postgres_e2e.rs`, +3 `#[test]` fns)

1. **`memory_entities_link_round_trip_and_idempotency`**
   - Insert 1 memory, 2 entities.
   - `link_memory_to_entities(m, [e1, e2])` → returns 2.
   - `link_memory_to_entities(m, [e1, e2])` → returns 0 (idempotent).
   - `link_memory_to_entities(m, [e1, e3])` → returns 1 (e3 new, e1 dupe).
   - `SELECT COUNT(*) FROM memory_entities WHERE memory_id = m` → 3.

2. **`memory_entities_cascade_on_entity_delete`**
   - Insert memory, entity, link them.
   - `DELETE FROM entities WHERE id = e`.
   - `SELECT COUNT(*) FROM memory_entities WHERE entity_id = e` → 0.
   - Memory row still exists. `SELECT COUNT(*) FROM deleted_memories WHERE id = m` → 0.

3. **`memory_delete_writes_deleted_memories_row`**
   - Insert memory with body + metadata + 1024-dim embedding.
   - `DELETE FROM memories WHERE id = m`.
   - `SELECT * FROM deleted_memories WHERE id = m` — body, metadata, embedding, created_at all match; `deleted_at` within 5s of now().
   - Direct `INSERT INTO deleted_memories` as runtime role succeeds (matches `audit_log` shape — discipline, not enforcement).
   - Direct `UPDATE` and `DELETE` on `deleted_memories` as runtime role both fail (REVOKE shape enforced).

### Core unit (`core/src/memory/recall.rs::tests`, +5)

1. `recall_modes_all_includes_graph` — `RecallModes::ALL` has graph=true.
2. `recall_modes_graph_only_is_only_graph` — `RecallModes::GRAPH_ONLY` shape pin.
3. `recall_modes_default_runs_every_lane` — existing test updated to also assert graph=true.
4. `recall_params_seed_entity_ids_default_is_none` — `RecallParams::new(text, emb)` leaves seed_entity_ids=None.
5. `graph_fanout_cap_per_seed_is_thirty_two` — pins the constant.

### Core integration (`core/tests/memory_recall_e2e.rs`, extends existing `#[test]`)

The single existing test in this file is the canonical "lanes fuse correctly" test; the graph lane is the third lane it should now cover. Extending in place reuses the per-test PG cluster bring-up (~2 s). At the end of the existing assertions, add a graph-lane setup block + 4 assertion blocks:

```rust
// Graph setup:
//   alice owns cat (relation), bob is unconnected
//   mem_A tagged {alice, cat}, mem_B tagged {cat}, mem_C tagged {bob}
```

1. **`GRAPH_ONLY` with seed=[alice]** — expanded set = {alice, cat}; A ranks above B (2 hits vs. 1); C absent.
2. **`GRAPH_ONLY` with seed=[bob]** — expanded set = {bob}; only C returned.
3. **`ALL` with seed=[alice]** — all three lanes rank A first; fused top-1 is A.
4. **`GRAPH_ONLY` with empty seed slice** — lane skipped, returns empty (matches the warn-and-skip semantics).

### Test count delta

| Surface                            | New tests |
| ---------------------------------- | --------- |
| `db/tests/postgres_e2e.rs`         | +3        |
| `db/src/memories.rs::tests`        | 0         |
| `core/src/memory/recall.rs::tests` | +5        |
| `core/tests/memory_recall_e2e.rs`  | 0 new fns (+4 assertion blocks) |
| **Workspace total**                | 342 → ~**350** |

## What this slice deliberately does NOT do

- **No entity extraction from memory body.** A future "extraction worker" or LLM-prompted extraction step will populate `memory_entities` at memory-insert time. For now, callers (today: tests; future: an extraction pipeline) opt in by calling `link_memory_to_entities` explicitly.
- **No graph traversal beyond 1-hop.** N-hop expansion via `Graph::path` is deferred until observation phase shows 1-hop is insufficient.
- **No entity-similarity lane.** `entities.embedding` stays NULL today; an "entity semantic search" lane is a Phase-1 follow-up that needs the embedding worker to populate `entities.embedding` first.
- **No atomic `insert_memory_with_links` helper.** Adds API surface without a known caller demanding atomicity.
- **No `seed_entity_keys` natural-key input shape.** Caller is responsible for resolving names → ids via `Graph::get_entity` before invoking recall.
- **No production caller wiring.** The scheduler's `RouterAgent::formulate_plan` does not pass `seed_entity_ids` yet; that wiring lands when an entity-extraction step exists.
- **No `memory_entities` audit trail.** Inserting/deleting link rows is high-cardinality and low-stakes; the `deleted_memories` trigger captures the only deletion that really matters.
- **No fix to issue #17.** The graph lane's warn-and-skip-on-empty-seeds matches the existing pattern; tightening recall's missing-input behaviour for ALL lanes is a separate change.
- **No fix to issue #32 (stale dead-code warning).** Pre-existing; orthogonal.

## Migration hygiene

Two new migration files: `0007_memory_entities.sql` and `0008_deleted_memories_audit.sql`. Per issue #13's "one concern per migration" guidance, keeping them separate makes a future revert of one without the other clean. sqlx's `_sqlx_migrations` tracks them by `(version, slug)` so no rename hygiene issue is introduced.

## Implementation order (TDD per CLAUDE.md rule #2)

1. Write migration `0007_memory_entities.sql`. Cluster comes up; sqlx migrator runs; per-test PG cluster works.
2. Write the 3 DB integration tests against the not-yet-existing helpers — confirmed red.
3. Implement `link_memory_to_entities` and `graph_search` in `db/src/memories.rs`. DB integration tests pass.
4. Write migration `0008_deleted_memories_audit.sql` + the `memory_delete_writes_deleted_memories_row` test — should already be in place from step 2.
5. Add `RecallModes::graph` field + `RecallModes::GRAPH_ONLY` constant + `RecallParams::seed_entity_ids` + `GRAPH_FANOUT_CAP_PER_SEED` constant. Add the 5 unit tests — confirmed red (the new constants don't exist yet) → green after the additions.
6. Wire the graph lane into `recall()`. Extend `memory_recall_e2e.rs` with the 4 new assertion blocks — confirmed red.
7. Run full `cargo test --workspace`. Expected: **350 passed / 0 failed / 0 SKIP**.
8. Update HANDOVER + ROADMAP. Commit.

## Files touched

| File                                              | Action  |
| ------------------------------------------------- | ------- |
| `db/migrations/0007_memory_entities.sql`          | NEW     |
| `db/migrations/0008_deleted_memories_audit.sql`   | NEW     |
| `db/src/memories.rs`                              | extend (+2 fns) |
| `db/tests/postgres_e2e.rs`                        | extend (+3 `#[test]`) |
| `core/src/memory/recall.rs`                       | extend (+graph lane, +RecallModes::graph, +GRAPH_ONLY, +seed_entity_ids, +constant, +5 tests) |
| `core/tests/memory_recall_e2e.rs`                 | extend (+4 assertion blocks) |
| `core/Cargo.toml`                                 | possibly +`futures` dep (TBD — only if `try_join_all` isn't already reachable transitively) |
| `docs/devel/handovers/HANDOVER.md`                | update at end of session |
| `docs/devel/ROADMAP.md`                           | tick "Graph lane in memory::recall" |

## Verification

```sh
source "$HOME/.cargo/env"
cargo build --workspace
cargo test --workspace                     # expected: 350 passed / 0 failed / 0 SKIP
cargo test -p hhagent-db                   # ~80 tests incl. 3 new integration
cargo test -p hhagent-core --test memory_recall_e2e   # 1 test, extended assertions
```

Skip-as-pass paths on macOS (no PG available out of the box) remain valid: every new test goes through `bring_up_pg_cluster` from `hhagent-tests-common`, which prints `[SKIP]` to stderr when PG bin dir or supervisor isn't available.
