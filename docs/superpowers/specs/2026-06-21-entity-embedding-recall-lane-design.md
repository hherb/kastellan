# Entity-embedding backfill + entity-similarity recall lane

**Date:** 2026-06-21
**Status:** approved (design); implementation pending
**Relates to:** HANDOVER "Next TODO" → entity-embedding backfill / `entities.embedding`
similarity lane; "Design notes for parked work" → Option P (secondary deferral:
`entities.embedding` populated column → entity-similarity lane); [#40] (graph
quarantine-default policy).

## Problem

The `entities` table has had a nullable `embedding` column since migration 0001
(narrowed to `vector(256)` in 0019), but **no write path ever populates it** and
**no read path consumes it**. Every entity row therefore has `embedding IS NULL`,
and the recall system has no entity-similarity signal.

Recall today fuses three lanes (`semantic`, `lexical`, `graph`). The `graph` lane
is the only entity-aware lane, and it is seeded by `seed_entity_ids` — ids the
agent resolved out-of-band from explicit entity *extraction* on the query. When
extraction finds no seeds (the common cli_ask path), recall has no entity signal
at all, even if the store holds entities semantically close to the query.

## Goal

Populate `entities.embedding` and add a fourth recall lane that consumes it, so a
query surfaces memories linked to entities that are *embedding-similar* to the
query — end-to-end and observable. This is symmetric with the L1 memory embedding
arc: a backfill (mirrors #325) plus a consuming lane (mirrors the semantic lane).

Scope is **backfill + lane**. A *forward* embed path on entity insert
(`batch_upsert`) is deliberately out of scope this session and tracked as a
follow-up (see "Out of scope").

## Non-goals / out of scope

- **Forward embed-on-insert path.** Newly-extracted entities written by
  `entity_extraction::batch_upsert` will keep `embedding IS NULL` until the next
  `entities reembed` run. This mirrors the L1 split where #324 (forward) and #325
  (backfill) shipped separately; here we ship backfill + lane first and defer the
  forward path. Documented as a follow-up.
- **An ANN index** (ivfflat / hnsw) on `entities.embedding`. A sequential cosine
  scan over the entity table is acceptable at current cardinality, matching the
  memories semantic lane, whose index 0001 also defers. A later optimization once
  entity counts grow.
- **Schema change / migration.** `entities.embedding vector(256)` already exists.
- **Changing the graph lane.** The entity-similarity lane is a *separate* lane,
  not a modification of `graph_search`'s seed expansion.

## Architecture

Four layers, bottom-up. Each unit is independently testable.

### Layer 1 — `db` helpers: new module `db/src/entity_embedding.rs`

Co-locates the three new entity-embedding helpers in one focused module rather
than growing the already-over-cap `entities.rs` (653 LOC) or `memories/search.rs`
(508 LOC). Re-exported from `lib.rs` as `kastellan_db::entity_embedding::*`.

1. **`load_unembedded_entities(executor) -> Result<Vec<(i64, String, String)>, DbError>`**
   ```sql
   SELECT id, kind, name FROM entities WHERE embedding IS NULL ORDER BY id
   ```
   Returns `(id, kind, name)` so the core layer composes the embed text. Scans
   **all** NULL-embedding entities regardless of `quarantine` (embedding is
   review-independent — see "Quarantine semantics"). Stable `ORDER BY id` →
   resumable.

2. **`set_entity_embedding(executor, id, &[f32]) -> Result<bool, DbError>`**
   ```sql
   UPDATE entities SET embedding = $1::vector WHERE id = $2 AND embedding IS NULL
   ```
   Re-asserts `embedding IS NULL` → idempotent + race-safe (a row embedded
   concurrently no-ops and returns `false`). Dim-checked via the shared
   `check_embedding_dim` chokepoint before the write. Byte-for-byte mirror of
   `memories::set_embedding`.

3. **`entity_similarity_search(executor, query_embedding, entity_fanout, k, include_quarantined) -> Result<Vec<i64>, DbError>`**
   The lane query. Top-`entity_fanout` entities nearest the query embedding, then
   the memories linked to them, ranked by each memory's closest matching entity:
   ```sql
   SELECT me.memory_id
   FROM (
       SELECT id, embedding <=> $1::vector AS dist
       FROM entities
       WHERE embedding IS NOT NULL
         AND ($4 OR quarantine = FALSE)
       ORDER BY dist
       LIMIT $2                       -- entity_fanout
   ) top_e
   JOIN memory_entities me ON me.entity_id = top_e.id
   GROUP BY me.memory_id
   ORDER BY MIN(top_e.dist) ASC, me.memory_id ASC
   LIMIT $3                           -- k
   ```
   Returns up to `k` memory ids in best-first order. `k == 0` or
   `query_embedding.len() != EMBEDDING_DIM` handled the same way the existing
   search helpers handle them (`k==0` → empty no-SQL; dim mismatch →
   `check_embedding_dim` error). The `($4 OR quarantine = FALSE)` predicate mirrors
   `graph_search`'s `include_quarantined` parameter so the operator-CLI path can
   opt into quarantined rows while production stays fail-closed.

### Layer 2 — `core`: shared report + entity backfill

1. **Extract shared report** — move `ReembedReport`, `format_reembed_report`,
   `reembed_batch_failed` (and their unit tests) out of
   `core/src/memory/l1_reembed.rs` into a new `core/src/memory/reembed.rs`.
   `l1_reembed` imports them from `super::reembed`; the memory facade
   (`mod.rs`) re-exports from `reembed` instead of `l1_reembed`, so the public
   paths `kastellan_core::memory::{ReembedReport, format_reembed_report,
   reembed_batch_failed}` are unchanged. Both backfills then share one report type
   (rule 1 — pure helpers in a reusable module).

2. **New `core/src/memory/entity_reembed.rs`** (mirrors `l1_reembed.rs`):
   - **`entity_embedding_text(kind: &str, name: &str) -> String`** — pure:
     `format!("{kind}: {name}")` (e.g. `"person: Horst Herb"`). Test-pinnable; the
     single source of truth for what goes into an entity vector so a future forward
     path embeds identically.
   - **`reembed_entities_null(pool, embedder: &dyn Embedder) -> Result<ReembedReport, DbError>`**
     — `load_unembedded_entities` → for each `(id, kind, name)`: compose text via
     `entity_embedding_text`, `embedder.embed_for_storage(&text)`,
     `set_entity_embedding(pool, id, &vec)`. Per-row degrade-and-warn (embed
     `None` / lost `IS NULL` race / write error all count as `skipped`, never fail
     the batch); only an initial-scan failure returns `Err`. Aggregate WARN +
     `reembed_batch_failed` exactly as `reembed_l1_null`.

### Layer 3 — recall lane (`core/src/memory/recall.rs`)

- Add **`entity: bool`** to `RecallModes`.
- `RecallModes::ALL` = `{ semantic, lexical, graph, entity }` all `true`.
- Add `entity: false` (mechanical, no behaviour change) to `SEMANTIC_ONLY`,
  `LEXICAL_ONLY`, `GRAPH_ONLY`, `SEMANTIC_AND_LEXICAL`.
- Add a new no-seeds default preset
  **`SEMANTIC_LEXICAL_ENTITY = { semantic, lexical, entity, graph:false }`** and
  have `RecallParams::new` use it (instead of `SEMANTIC_AND_LEXICAL`), so the entity
  lane runs on the common no-seeds cli_ask path. The entity lane requires only
  `query_embedding` (which both `new` and `with_seeds` supply); it is *most*
  valuable on the no-seeds path where the graph lane is off.
- Add `ENTITY_ONLY` preset for tests.
- New const **`ENTITY_SIMILARITY_FANOUT: i64`** — how many nearest entities to
  consider (e.g. 64; bounded, generous, analogous to `GRAPH_FANOUT_CAP_PER_SEED`).
- New entity-lane block in `recall`, mirroring the **semantic** lane's
  input-handling exactly (reuse `query_embedding`; `Some(emb) if len==DIM` → run,
  `Some(_)` → hard `DbError::Query` dim error, `None` → warn-and-skip). Production
  passes `include_quarantined = false`. The returned id-list is pushed into
  `lane_lists` and fused by RRF unchanged. An empty result (no embedded/approved
  entities yet) is a *ran* lane contributing nothing — it does **not** trip the
  "no lanes ran" guard, because the lane ran; it simply pushes an empty list.

### Layer 4 — CLI (`core/src/bin/kastellan-cli/entities.rs`)

Add a **`reembed`** action to `run_entities` (alongside
`list|show|approve|reject|merge|kinds`), mirroring `memory l1 reembed`:
builds the real `RouterEmbedder` from `RouterConfig::from_env()` (same config as
the daemon), calls `reembed_entities_null`, prints
`format_reembed_report` (`scanned=<n> embedded=<n> skipped=<n>`), and exits
non-zero when `reembed_batch_failed` (a wholly-failed batch) so a scripted
`reembed && next-step` chain does not proceed; the idempotent no-op
(`scanned==0`) exits 0. Takes no args.

## Data flow

**Backfill:** `kastellan-cli entities reembed` → `reembed_entities_null` →
`load_unembedded_entities` → per row `entity_embedding_text(kind,name)` →
`RouterEmbedder.embed_for_storage` (Matryoshka-truncate to `EMBEDDING_DIM` +
`action='embed'` audit row) → `set_entity_embedding` (guarded) → `ReembedReport`.

**Recall:** query text → `embed_query` → `recall` → entity lane:
`entity_similarity_search(query_embedding, ENTITY_SIMILARITY_FANOUT, lane_k,
include_quarantined=false)` → ranked memory ids → RRF-fused with the
semantic/lexical/graph lanes → hydrated top-`k` `Memory` rows.

## Quarantine semantics

- **Backfill embeds every entity** regardless of `quarantine`. Embedding is
  independent of review state; an entity later approved must not need re-embedding,
  and embedding a quarantined row leaks nothing (the lane filters it).
- **The lane filters `quarantine = FALSE`** in production
  (`include_quarantined=false`), mirroring `graph_search`. This preserves the
  invariant that unreviewed entities never surface memories into recall ([#40]).

## Error handling

- Backfill: per-row degrade-and-warn (mirrors `reembed_l1_null`); only an
  initial-scan `Err` aborts. Aggregate WARN + non-zero CLI exit on a wholly-failed
  batch.
- Lane: a wrong-dimension `query_embedding` is a hard `DbError::Query` (mirrors the
  semantic lane); a `None` embedding warns-and-skips.
- `set_entity_embedding` dim-checks before the write (cannot store a wrong-width
  vector).

## Testing (TDD)

- **db unit** (PG-free, lazy pool): `set_entity_embedding` rejects a wrong-dim
  vector before any I/O.
- **db integration** (`postgres_e2e` or a focused suite): seed entities (some
  embedded, some NULL, some quarantined) → `load_unembedded_entities` returns only
  NULL rows in id order; `set_entity_embedding` writes once and no-ops on re-run;
  `entity_similarity_search` returns linked memory ids ranked by nearest entity and
  **excludes quarantined entities** when `include_quarantined=false`.
- **core unit:** `entity_embedding_text` composition (`"kind: name"`, empty-kind,
  unicode); shared `ReembedReport` reuse still pins its invariant + formatting.
- **core e2e `entity_reembed_e2e`** (live PG 18): backfill populates an entity
  embedding and the lane then finds the linked memory; idempotent re-run embeds
  nothing; degrade-and-warn (a `NoOp`/failing embedder leaves the row NULL, batch
  succeeds); a quarantined entity's linked memory is **not** surfaced by the lane.
- **recall unit:** `RecallModes::ALL.entity` is true; `RecallParams::new` enables
  the entity lane (new default preset); `ENTITY_ONLY` shape; the lane's ranked
  id-list participates in RRF fusion.

## File / cap impact

- New: `db/src/entity_embedding.rs` (~150 LOC), `core/src/memory/reembed.rs`
  (~120 LOC, lifted from `l1_reembed.rs`), `core/src/memory/entity_reembed.rs`
  (~160 LOC), `core/tests/entity_reembed_e2e.rs`.
- Edited: `db/src/lib.rs` (re-export), `core/src/memory/l1_reembed.rs` (import
  shared report — drops ~70 LOC), `core/src/memory/mod.rs` (module decls +
  re-exports), `core/src/memory/recall.rs` (+ lane block, ~40 LOC; recall.rs is
  406 → ~450, under cap), `core/src/memory/recall/tests.rs`,
  `core/src/bin/kastellan-cli/entities.rs` (+ reembed action).
- No file pushed newly over the 500-LOC cap.

## Platform

Pure-Rust, no migration, no OS-gated (sandbox/seccomp/Landlock) code. macOS live
PG 18 exercises pgvector + the exact lane SQL. **DGX not required** as an
acceptance gate for this change.

## Follow-ups (filed, not in scope)

- **Forward entity-embed path** — embed on insert in
  `entity_extraction::batch_upsert` (through the same `entity_embedding_text`
  chokepoint), so fresh entities are searchable without a backfill run.
- **ANN index** on `entities.embedding` once entity cardinality warrants it.
- **`entities reembed` after `approve`** is unnecessary (backfill already embeds
  quarantined rows), but if the forward path lands quarantined-but-embedded, no
  action is needed on approve — the lane simply starts including the row.
