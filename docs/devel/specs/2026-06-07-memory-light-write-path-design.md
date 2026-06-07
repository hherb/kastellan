# Design: memory two-tier write path — `insert_memory_light`

**Date:** 2026-06-07
**ROADMAP item:** "Memory two-tier write path: `put_doc()` vs `put_doc_light()`" (ROADMAP.md:130)
**Status:** approved, pre-implementation

## Problem

Today every `memories` row is written through `db::memories::insert_memory`
(L2 shorthand) or `insert_memory_at_layer` (explicit layer), both of which take
a caller-provided `embedding: Option<&[f32]>`. The production callers always
compute and pass an embedding. For *future* high-frequency, ephemeral writers —
channel inbound, browser observations, screen capture if it ever materialises —
embedding every row is wasteful: those rows would never be useful semantic-search
targets, and the embed call is the expensive step.

openhuman's `docs/memory-sync-functions.md` draws exactly this line: a heavyweight
`put_doc()` ("embed + async graph-extract") versus a `put_doc_light()` that skips
embedding for high-frequency ephemeral data. This spec adds the `light` surface.

## Decision summary

Add a deliberately-named, embedding-skipping writer:

```rust
pub async fn insert_memory_light<'e, E>(
    executor: E,
    body: &str,
    metadata: &serde_json::Value,
    layer: MemoryLayer,
) -> Result<i64, DbError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>;
```

It is a **thin named delegate** to the existing chokepoint:

```rust
insert_memory_at_layer(executor, body, metadata, None, layer).await
```

There is no `embedding` parameter — the whole point of `light` is to *not* embed.
The value-add is the intent-signalling name plus a documented degradation
contract, exactly mirroring how `seed_meta_memory` is a named pass-through whose
value is the auditable entry point rather than new logic.

### Why delegate rather than write fresh SQL

`insert_memory_at_layer` already owns:
- the no-embedding `INSERT` SQL shape (the `embedding IS NULL` branch), and
- the L0 (`MemoryLayer::Meta`) `PolicyViolation` guard.

Delegating inherits both for free and keeps a single insert chokepoint, so there
is no duplicated SQL to drift. Writing fresh SQL would re-implement the
NULL-embedding branch and re-state the L0 check — rejected as needless
duplication.

### L0 handling

`insert_memory_light` rejects `MemoryLayer::Meta` (L0) with the same
`DbError::PolicyViolation` as `insert_memory_at_layer`, inherited automatically
via delegation. This preserves the invariant that a `grep` for `seed_meta_memory`
is the complete auditable record of every L0 write. `light` is for L1/L2/L3/L4
ephemeral data only.

## Degradation contract (documented on the function)

A row written with `insert_memory_light` has `embedding IS NULL` and no
`memory_entities` links (entity extraction is skipped by contract — it is a
caller/`core`-side step, not a `db`-side one). Therefore:

- ✅ **Lexical lane** (full-text search on `body`) works normally — it never
  touches `embedding`.
- ✅ **`metadata @>` containment** lookups work normally — also embedding-free.
- ⚠️ **Semantic lane** silently skips the row: `semantic_search` already filters
  `WHERE embedding IS NOT NULL`, so a NULL-embedding row degrades gracefully
  rather than erroring.
- ⚠️ **Graph lane** never surfaces it: with no `memory_entities` links, the
  1-hop entity expansion has nothing to find.

This is graceful degradation, not breakage: the row is retrievable by the two
embedding-free lanes and invisible to the two embedding/graph lanes.

## Components

- **`db/src/memories/write.rs`** — add `insert_memory_light`. One function,
  delegating to `insert_memory_at_layer`. Doc comment carries the degradation
  contract above.
- **`db/src/memories.rs`** (parent) — add `insert_memory_light` to the
  `pub use write::{…}` re-export list so the call site is
  `db::memories::insert_memory_light`.

No schema change. No migration. The `memories.metadata` column is already the
natural namespace selector for the deferred caps/eviction follow-up.

## Testing (TDD — written before the implementation)

1. **Happy path (PG integration, `db/tests/postgres_e2e.rs`):**
   `insert_memory_light` returns an id; the persisted row has `embedding IS NULL`
   and the requested `layer`.
2. **L0 rejection (fast unit, `db/src/memories/write.rs` tests):**
   `MemoryLayer::Meta` returns `DbError::PolicyViolation` — the guard fires
   before any SQL, so no PG needed. Pins the delegation to the guarded path.
3. **Degradation pin (PG integration, `db/tests/postgres_e2e.rs`):**
   a light-written row is **absent** from `semantic_search` results yet
   **present** via a lexical / `metadata @>` query — proving the documented
   contract on a live row.

## Out of scope (deferred follow-ups)

- **Core-side caller wiring.** No high-frequency writer exists yet; this is
  plumbing for when one lands (Phase 2 channels / Phase 3 browser).
- **Per-namespace caps + oldest-eviction** (openhuman quotes "max 50 KV entries,
  max 200 docs"). Fits naturally on `memories.metadata` as the namespace
  selector with no schema change, but does not block this surface.
