# Memory layer tagging — L1 always-in-context insight index

**Date:** 2026-05-15
**Status:** Design (pre-implementation, pre-roadmap)
**Branch (proposed):** `feat/memory-layer-l1-index`
**Off:** `main` at HEAD of the day this lands
**Inspired by:** GenericAgent's 5-layer memory hierarchy
(https://github.com/lsdefine/GenericAgent), specifically the L1
"insight index" — a small, hand-curated routing layer that is *always*
in the prompt regardless of similarity score, so the model knows what
kinds of skills/facts exist and how to ask for them.

---

## Goal

Add a `layer` column to `memories` and a single new core surface,
`memory::layers::load_l1`, that returns the (small, bounded) set of L1
rows the prompt assembler must pin into every system prompt. The slice
ships:

- the schema column + GRANT-shape audit on existing rows,
- a typed `MemoryLayer` enum at the db boundary,
- a writer-side `insert_memory_at_layer` helper,
- a read-side `load_layer(layer, cap)` helper,
- the core wrapper that enforces the L1 token / row caps,
- tests pinning the contract.

It deliberately does **not** ship: prompt assembly wiring (no llm_router
yet), automatic skill crystallization (L3), session-digest generation
(L4), or any LLM-driven promotion. Those land later, gated behind this
column existing.

## Why now, why this small

GenericAgent's design thesis is "the right knowledge is always in
scope" — achieved by keeping a tiny, structured index loaded
unconditionally and using it as a routing table for everything else.
kastellan already has the recall lanes (semantic + lexical + graph) but
no notion of "this row is a routing pointer, load it every turn." That
missing primitive blocks every later memory-tier idea (L3 skills,
L4 session digests) because they all need a layer tag to be retrievable
by class. Adding the column now, with one consumer (L1 load), is the
cheapest move that unblocks the rest.

The slice is intentionally one column + two helpers + one core wrapper.
No prompt assembler is wired yet — the consumer is a future
`llm_router` slice. Shipping the storage primitive ahead of the
consumer means the consumer's design can assume the column exists.

## Decisions locked in

1. **Storage shape: dedicated `layer SMALLINT NOT NULL` column with
   CHECK constraint, not `metadata->'layer'`.** Rationale:
   - First-class indexability without a JSONB expression index.
   - CHECK constraint enforces the closed enum at the DB boundary;
     JSONB would only be enforced in Rust.
   - Existing memories backfill to a single value (L2) deterministically.
   - Matches the `tasks.state` pattern (CHECK constraint, not ENUM —
     PG ENUMs are a migration tax we've already chosen against; see
     `0001_init.sql:tasks`).
   Rejected: `layer` as PG ENUM (rename pain on every layer addition);
   `metadata.layer` JSONB key (no CHECK enforcement, awkward filters);
   separate `memory_layers` join table (over-modelled — a memory
   belongs to exactly one layer).

2. **Layer values are 0..=4, mapping to L0..L4** as in GenericAgent:
   - L0 — meta-rules / hard constraints (e.g. "never call rm -rf");
     hand-curated, ships in seed data, never written by the agent.
   - L1 — insight index; small routing pointers ("skills exist at L3
     for: gmail send, stock alert"). Hand-curated *or* programmatically
     promoted; this is the always-in-context layer.
   - L2 — stable accumulated facts (current default; everything in
     `memories` today is L2).
   - L3 — skills / SOPs (parameterized procedures). Reserved; no
     writer in this slice.
   - L4 — session digests. Reserved; no writer in this slice.
   Rejected: `TEXT` enum ("l0".."l4") — wider rows, slower compares,
   no win over a SMALLINT with a CHECK.

3. **Backfill to L2.** Every existing `memories` row becomes L2. This
   is the only defensible default: it preserves "everything currently
   recalled is recallable post-migration" and matches the layer
   semantics (stable accumulated fact). Rejected: backfill to L0 or
   L1 (would inject every existing row into every prompt — token
   blowout); leaving column NULL with a partial constraint (NULL in a
   CHECKed column is a smell, and the column is "always known" by
   construction).

4. **L1 caps are hard, not advisory.** `load_l1` takes a `cap_rows`
   and `cap_bytes` and returns at most `min(cap_rows, ⌊cap_bytes /
   sum(body_len)⌋)` rows in `created_at DESC` order (newest first,
   tie-break by `id DESC`). Both caps default to constants in the core
   module (initial values: 32 rows, 4 KiB). Rationale: L1's whole
   point is "fits in the prompt unconditionally." A soft cap that
   sometimes overshoots defeats the purpose. Rejected: only a row cap
   (one fat L1 row blows the budget); only a byte cap (cheap row count
   bound is useful for quick prompt-size accounting).

5. **No automatic promotion in this slice.** L1 rows are written
   explicitly via `insert_memory_at_layer(L1, ...)`. The "agent
   notices a recurring fact and promotes it to L1" loop is a future
   slice that depends on this one. Rejected: hooking promotion into
   the recall path now — premature, and it would couple write logic
   to read logic before we've measured what's worth promoting.

6. **L1 load is a separate call, not a fourth recall lane.** The three
   existing lanes (semantic / lexical / graph) all use RRF over a
   query. L1 is unconditional and query-independent — fusing it would
   either (a) require synthesising a fake rank, or (b) drop it when
   the query happens to match nothing. Both are wrong. The prompt
   assembler will call `load_l1` and `recall` separately and
   concatenate results. Rejected: extending `RecallParams` with a
   `pin_l1: bool` flag (couples unrelated concerns; recall isn't the
   prompt assembler).

7. **No `delete_l1_row` API.** Deletion goes through the existing
   `memories` DELETE path (which already triggers `deleted_memories`
   audit via migration 0008). The layer column rides along into
   `deleted_memories` for forensic completeness.

## Architecture

```
db/migrations/0013_memories_layer.sql      ← NEW: column + CHECK + backfill + GRANT-shape note
db/migrations/0014_deleted_memories_layer.sql ← NEW: same column on audit table

db/src/memories.rs                         ← extend
  ├─ pub enum MemoryLayer { L0, L1, L2, L3, L4 }    NEW
  ├─ pub fn insert_memory_at_layer(...)              NEW
  └─ pub fn load_layer(executor, layer, cap) -> Vec<Memory>   NEW

core/src/memory/layers.rs                  ← NEW (~80 lines)
  ├─ pub const L1_DEFAULT_CAP_ROWS: usize = 32
  ├─ pub const L1_DEFAULT_CAP_BYTES: usize = 4096
  └─ pub async fn load_l1(pool, cap_rows, cap_bytes) -> Result<Vec<Memory>, DbError>

core/src/memory/mod.rs                     ← extend: pub mod layers
core/tests/memory_layers_e2e.rs            ← NEW (~120 lines)
```

**Chokepoint discipline (unchanged):**
- `db::memories` owns every read/write of the `memories` table.
- `core::memory::layers` composes db helpers; issues no SQL.
- `core::memory::recall` does not import `layers` — different concern.

## Schema — Migration 0013

```sql
-- 0013_memories_layer.sql
--
-- Tag every memory row with a hierarchy layer 0..=4. The L1 layer
-- is the "always-in-context insight index" loaded by
-- core::memory::layers::load_l1; the other layers are reserved
-- writers for future slices (L0 seed rules, L3 skills, L4 session
-- digests). All existing rows are stable accumulated facts → L2.

ALTER TABLE memories
    ADD COLUMN layer SMALLINT NOT NULL DEFAULT 2
        CHECK (layer BETWEEN 0 AND 4);

-- Existing rows already got DEFAULT 2 from the ADD COLUMN above; this
-- statement is a no-op against a virgin schema but documents intent
-- and is idempotent against partial-state recovery.
UPDATE memories SET layer = 2 WHERE layer IS NULL;

CREATE INDEX memories_layer_idx ON memories (layer, created_at DESC);

-- No GRANT change: kastellan_runtime already has full CRUD on memories
-- (migration 0002), and the new column is part of that table.
```

**Notes:**
- `SMALLINT` (2 bytes) over `INT` (4 bytes); 5 distinct values forever.
- CHECK constraint is the canonical defence; the Rust enum is convenience.
- `(layer, created_at DESC)` index supports the L1 hot path
  (`WHERE layer = 1 ORDER BY created_at DESC LIMIT $cap`) and any
  future "show me everything at layer X" query.
- DEFAULT 2 stays on the column (not just the backfill). New writers
  that don't specify a layer get the safest classification.

## Schema — Migration 0014

```sql
-- 0014_deleted_memories_layer.sql
--
-- The deleted_memories audit (migration 0008) must capture the layer
-- of every deleted row, otherwise post-deletion forensics can't
-- reconstruct whether a deleted row was a load-bearing L1 pointer or
-- a routine L2 fact.

ALTER TABLE deleted_memories
    ADD COLUMN layer SMALLINT NOT NULL DEFAULT 2
        CHECK (layer BETWEEN 0 AND 4);

CREATE OR REPLACE FUNCTION audit_memory_delete() RETURNS trigger AS $$
BEGIN
    INSERT INTO deleted_memories (id, body, metadata, embedding, layer, created_at)
    VALUES (OLD.id, OLD.body, OLD.metadata, OLD.embedding, OLD.layer, OLD.created_at);
    RETURN OLD;
END;
$$ LANGUAGE plpgsql;

-- GRANT shape on deleted_memories unchanged: SELECT, INSERT only;
-- UPDATE/DELETE/TRUNCATE remain revoked (migration 0008).
```

**Notes:**
- Trigger function is `CREATE OR REPLACE` — same name, expanded
  payload. Existing trigger binding (`memories_after_delete_audit`)
  picks up the new function body automatically; no `DROP TRIGGER`.
- Backfill of existing `deleted_memories` rows: not needed in dev
  (no rows exist yet); production rollout, when it happens, runs
  this migration before any production deletion path is wired.
- DEFAULT 2 mirrors the source table — same defensible default.

## Library surfaces — `db::memories`

```rust
/// Memory layers, mirroring GenericAgent's 5-layer hierarchy.
///
/// The discriminant values 0..=4 match the SMALLINT stored in
/// `memories.layer` and `deleted_memories.layer`; the CHECK
/// constraint at the DB boundary guarantees no other value is
/// ever read back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i16)]
pub enum MemoryLayer {
    /// Meta-rules / hard constraints. Hand-curated seed data only.
    Meta = 0,
    /// Insight index; always loaded into the prompt by
    /// `core::memory::layers::load_l1`.
    Index = 1,
    /// Stable accumulated facts. Default for plain `insert_memory`.
    Stable = 2,
    /// Skills / SOPs. Reserved; no writer in this slice.
    Skill = 3,
    /// Session digests. Reserved; no writer in this slice.
    Digest = 4,
}

impl MemoryLayer {
    pub fn from_db(raw: i16) -> Result<Self, DbError> { /* match 0..=4, else DbError::Invariant */ }
    pub fn as_db(self) -> i16 { self as i16 }
}

/// Insert a memory row tagged with an explicit layer.
///
/// `insert_memory` (existing) becomes a thin wrapper that calls this
/// with `MemoryLayer::Stable`. Callers that mean L1 must say so.
pub async fn insert_memory_at_layer<'e, E>(
    executor: E,
    body: &str,
    metadata: &serde_json::Value,
    embedding: Option<&[f32]>,
    layer: MemoryLayer,
) -> Result<i64, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>;

/// Load up to `cap` rows at the specified layer, newest first.
///
/// Used by `core::memory::layers::load_l1` (cap = L1 row cap) and
/// by future readers of L0 / L3 / L4. Returns rows in
/// `(created_at DESC, id DESC)` order so the caller gets stable,
/// deterministic ordering across calls with the same `cap`.
pub async fn load_layer<'e, E>(
    executor: E,
    layer: MemoryLayer,
    cap: usize,
) -> Result<Vec<Memory>, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>;
```

SQL shapes:

```sql
-- insert_memory_at_layer
INSERT INTO memories (body, metadata, embedding, layer)
VALUES ($1, $2, $3, $4)
RETURNING id

-- load_layer
SELECT id, body, metadata, embedding, layer, created_at
FROM memories
WHERE layer = $1
ORDER BY created_at DESC, id DESC
LIMIT $2
```

The existing `Memory` struct gains a `pub layer: MemoryLayer` field;
`fetch_by_ids`, `semantic_search`, `lexical_search`, `graph_search`
all populate it from the new column. This is a one-line change at the
SELECT clause for each, plus the struct extension.

## Library surfaces — `core::memory::layers`

```rust
//! L1 insight-index loader.
//!
//! L1 is the "always-in-context" memory layer (GenericAgent's design):
//! a small set of hand-curated or programmatically promoted routing
//! pointers that the prompt assembler concatenates into every system
//! prompt regardless of the user's query. The hard caps below exist
//! because L1's whole purpose is "fits in the prompt unconditionally";
//! a soft cap that sometimes overshoots defeats the design.

/// Default upper bound on L1 row count. Picked to keep the L1 block
/// scannable by the model in a single attention sweep — small enough
/// that the routing pointers don't crowd out the actual task.
pub const L1_DEFAULT_CAP_ROWS: usize = 32;

/// Default upper bound on the byte sum of L1 row bodies. 4 KiB is
/// roughly 1 K tokens at typical English+code density — about 3% of
/// a 30 K token target window (matching GenericAgent's <30 K design).
pub const L1_DEFAULT_CAP_BYTES: usize = 4096;

/// Load L1 rows for prompt pinning. Returns at most `cap_rows`, and
/// truncates earlier if the cumulative `body` byte length would
/// exceed `cap_bytes`. Rows are returned newest-first; the caller
/// concatenates them into the system prompt verbatim.
///
/// Returns an empty Vec when no L1 rows exist — that is the
/// expected state until something writes one. No error.
pub async fn load_l1(
    pool: &sqlx::PgPool,
    cap_rows: usize,
    cap_bytes: usize,
) -> Result<Vec<kastellan_db::memories::Memory>, kastellan_db::DbError>;
```

Implementation:

```rust
pub async fn load_l1(
    pool: &PgPool,
    cap_rows: usize,
    cap_bytes: usize,
) -> Result<Vec<Memory>, DbError> {
    let candidates = kastellan_db::memories::load_layer(
        pool, MemoryLayer::Index, cap_rows,
    ).await?;

    let mut acc: Vec<Memory> = Vec::with_capacity(candidates.len());
    let mut bytes_used: usize = 0;
    for row in candidates {
        let row_bytes = row.body.len();
        if bytes_used.saturating_add(row_bytes) > cap_bytes {
            break;
        }
        bytes_used += row_bytes;
        acc.push(row);
    }
    Ok(acc)
}
```

**Notes:**
- The byte cap is checked against `body` only, not `metadata` or
  `embedding` — the prompt assembler emits `body` text; metadata is
  for filtering and embeddings are not in the prompt.
- The DB caps row count first; the byte loop is a second cap. Two
  caps catch two different failure modes: many tiny rows vs one fat
  row.
- A row whose body alone exceeds `cap_bytes` is dropped silently
  (the loop breaks before pushing it). This is the conservative
  choice; an over-budget single row would blow the prompt. A future
  slice can add `tracing::warn!` when this happens, once we have a
  log-volume budget for warnings of this class.

## Error handling

Zero new error variants. Everything flows through `DbError`:

| Failure                                      | Path                                                              |
| -------------------------------------------- | ----------------------------------------------------------------- |
| `MemoryLayer::from_db(99)` (impossible given CHECK) | `DbError::Invariant("memory layer out of range: 99")`        |
| Empty L1 (no rows yet)                       | `Ok(vec![])` — explicitly not an error                            |
| `load_layer` SQL error                       | `DbError::Query(_)` propagated                                    |
| `insert_memory_at_layer` with CHECK violation | unreachable (Rust enum gates the value)                          |
| Single L1 row body > `cap_bytes`             | dropped silently (see Notes above); future warn                   |
| `cap_bytes = 0` or `cap_rows = 0`            | returns `Ok(vec![])` — caller asked for nothing                   |

## Testing

### DB integration (`db/tests/postgres_e2e.rs`, +3 `#[test]` fns)

1. **`memories_layer_default_is_stable`**
   - `insert_memory(body, meta, emb)` (existing API, no layer arg).
   - `SELECT layer FROM memories WHERE id = $1` → 2.

2. **`insert_memory_at_layer_round_trip`**
   - For each of `L0..=L4`: insert one row at that layer.
   - `load_layer(L1, 100)` returns exactly the L1 row.
   - `load_layer(L3, 100)` returns exactly the L3 row.
   - Cross-layer rows are never returned by the wrong layer query.

3. **`memory_delete_preserves_layer_in_audit`**
   - `insert_memory_at_layer(L1, ...)` → id m.
   - `DELETE FROM memories WHERE id = m`.
   - `SELECT layer FROM deleted_memories WHERE id = m` → 1.

### Core unit (`core/src/memory/layers.rs::tests`, +3)

1. `l1_default_caps_pin` — `L1_DEFAULT_CAP_ROWS == 32` and
   `L1_DEFAULT_CAP_BYTES == 4096`. Pure pin to prevent silent drift.
2. `memory_layer_round_trip_db_value` — `MemoryLayer::Index.as_db() == 1`
   and `from_db(1)` round-trips.
3. `memory_layer_from_db_rejects_out_of_range` — `from_db(5)` returns
   `Err(DbError::Invariant(_))`.

### Core integration (`core/tests/memory_layers_e2e.rs`, NEW, 4 `#[test]` fns)

Each uses `bring_up_pg_cluster` from `kastellan-tests-common` (the
same helper as `memory_recall_e2e.rs`).

1. **`load_l1_empty_returns_empty_vec`**
   - Fresh cluster, no L1 rows. `load_l1(&pool, 32, 4096)` → `Ok(vec![])`.

2. **`load_l1_returns_only_l1_rows_newest_first`**
   - Insert 1 row at each of L0..=L4 in that order.
   - `load_l1(&pool, 32, 4096)` returns exactly 1 row, body equal to
     the L1 insert.

3. **`load_l1_respects_row_cap`**
   - Insert 5 L1 rows.
   - `load_l1(&pool, 3, 4096)` returns 3 rows, all newest-first.

4. **`load_l1_respects_byte_cap`**
   - Insert 3 L1 rows with bodies of ~2 KiB each.
   - `load_l1(&pool, 32, 4096)` returns 2 rows (3rd would exceed).
   - `load_l1(&pool, 32, 100)` returns 0 rows (first row alone exceeds).

### Test count delta

| Surface                              | New tests |
| ------------------------------------ | --------- |
| `db/tests/postgres_e2e.rs`           | +3        |
| `core/src/memory/layers.rs::tests`   | +3        |
| `core/tests/memory_layers_e2e.rs`    | +4        |
| **Workspace total**                  | current → current + 10 |

## What this slice deliberately does NOT do

- **No prompt-assembler wiring.** `load_l1` has no in-tree caller
  outside its tests. The `llm_router` slice (not yet specced) is the
  intended consumer.
- **No L0 / L3 / L4 writers.** Column exists, enum exists, but the
  only API that names a non-default layer is `insert_memory_at_layer`
  used in tests. Promotion / SOP-crystallization / session-digest
  writers are separate slices.
- **No automatic promotion from L2 → L1.** Out of scope; needs
  observation-phase data first to know what to promote.
- **No L1 ordering by salience / hit-count.** `created_at DESC` is
  the simplest defensible order. A "promote-on-recall-hit" counter
  would be premature; we have no data showing recency is the wrong
  ranker.
- **No metadata schema for L1 pointers.** L1 rows reuse the existing
  `metadata JSONB DEFAULT '{}'` column. A future "L1 pointer schema"
  (e.g. `{"points_to": "skill", "skill_id": ...}`) lands when L3
  exists.
- **No deduplication.** Two L1 rows with identical bodies are two
  rows. A `UNIQUE (layer, body)` constraint would block legitimate
  re-insertion patterns we haven't yet imagined.
- **No L1 size-budget telemetry.** When we have a tracing budget for
  this class of warning, the silent-drop path in `load_l1` should
  emit one. Out of scope here.
- **No backfill heuristics.** Every existing memory becomes L2.
  Promoting individual existing rows to L1 is a manual operator job.

## Migration hygiene

Two new files, one concern each (per issue #13's "one concern per
migration"): `0013_memories_layer.sql` adds the column on the live
table; `0014_deleted_memories_layer.sql` mirrors it on the audit
table and re-creates the trigger function. Keeping them split makes
a hypothetical "abandon the layer column" revert reversible
file-by-file.

The trigger `CREATE OR REPLACE FUNCTION` in 0014 silently rebinds the
existing trigger to the new function body. This is the same pattern
used by 0003 (`audit_log_notify`) and is safe: PG looks up trigger
functions by name at execution time, not at trigger-creation time.

## Implementation order (TDD, per CLAUDE.md rule #2)

1. Write `0013_memories_layer.sql`. `cargo test -p kastellan-db` still
   green (no test names the column yet; existing rows backfill to 2).
2. Write the 3 DB integration tests against not-yet-existing
   `MemoryLayer` / `insert_memory_at_layer` / `load_layer` — confirmed red.
3. Add `MemoryLayer` enum + `insert_memory_at_layer` +
   `load_layer` in `db/src/memories.rs`. Extend `Memory` struct with
   `layer: MemoryLayer`, update the four existing SELECT helpers
   (`fetch_by_ids`, `semantic_search`, `lexical_search`,
   `graph_search`) to project the column. DB tests pass.
4. Write `0014_deleted_memories_layer.sql`. The
   `memory_delete_preserves_layer_in_audit` test from step 2 goes green.
5. Create `core/src/memory/layers.rs` with the constants, `load_l1`,
   and the 3 unit tests. Add `pub mod layers;` to
   `core/src/memory/mod.rs`. Unit tests green.
6. Create `core/tests/memory_layers_e2e.rs` with the 4 integration
   tests. All green.
7. Run `cargo test --workspace` — expected current + 10, 0 failed,
   `[SKIP]` lines only on machines without PG.
8. Update HANDOVER + ROADMAP. ROADMAP gains a "Memory layers (L1
   index)" item under Phase 1, ticked in the same commit. Commit.

## Files touched

| File                                              | Action  |
| ------------------------------------------------- | ------- |
| `db/migrations/0013_memories_layer.sql`           | NEW     |
| `db/migrations/0014_deleted_memories_layer.sql`   | NEW     |
| `db/src/memories.rs`                              | extend (+enum, +2 fns, +1 struct field, 4 SELECT updates) |
| `db/tests/postgres_e2e.rs`                        | extend (+3 `#[test]`) |
| `core/src/memory/mod.rs`                          | extend (+`pub mod layers;`) |
| `core/src/memory/layers.rs`                       | NEW (~80 lines + 3 unit tests) |
| `core/tests/memory_layers_e2e.rs`                 | NEW (~120 lines, 4 `#[test]`) |
| `docs/devel/handovers/HANDOVER.md`                | update at end of session |
| `docs/devel/ROADMAP.md`                           | add + tick "Memory layers (L1 index)" under Phase 1 |

## Verification

```sh
source "$HOME/.cargo/env"
cargo build --workspace
cargo test --workspace                                    # +10 vs baseline
cargo test -p kastellan-db memories_layer                   # the 3 db tests
cargo test -p kastellan-core --test memory_layers_e2e       # the 4 e2e tests
```

## Follow-ups this slice unlocks (separate specs)

- **L0 seed data loader.** A startup-time loader that reads a
  hand-edited TOML/YAML of meta-rules into L0 rows, idempotent on
  re-run. Pre-req: this slice.
- **Prompt-assembler `llm_router::build_system_prompt`.** First
  consumer of `load_l1`. Concatenates `[L0 rules]` + `[L1 index]`
  + `[task]` + `[recall(query)]`, enforces a global token cap by
  dropping in priority order L4 → L2 → L3 → L1 → L0. Pre-req: this
  slice + L0 loader.
- **L3 skill crystallization.** A writer that, on observed task
  success (observation-phase signal), distills the trajectory into
  an L3 row whose body is a parameterized JSON-RPC tool-call
  template. Pre-req: this slice + observation-phase captures
  (already specced) + tool-allowlist hygiene (already specced).
- **L4 session digest.** End-of-session summarizer that writes one
  L4 row per finished task; recall pulls them in via the existing
  semantic lane. Pre-req: this slice.
- **L1 promotion heuristic.** A bounded counter on L2 rows hit
  often by recall, with a threshold-based promote-to-L1 step.
  Pre-req: this slice + recall hit telemetry (not yet specced).
