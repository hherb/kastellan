# Forward entity embed-on-insert path

**Date:** 2026-06-21
**Status:** approved (design); implementation pending
**Relates to:** HANDOVER "Next TODO" → "forward entity embed-on-insert path"
(natural follow-up to the entity-embedding backfill + recall lane, PR #335);
the deliberate "Out of scope" item in
`2026-06-21-entity-embedding-recall-lane-design.md`. Symmetric with the L1 split
`#324` (forward) / `#325` (backfill).

## Problem

PR #335 shipped the entity-embedding **backfill** (`kastellan-cli entities
reembed`) plus the **entity-similarity recall lane** that consumes
`entities.embedding`. But there is still **no forward embed path**: every entity
written by `entity_extraction::batch_upsert` (the single chokepoint all entity
inserts flow through — query-time seed extraction in `formulate_plan` *and*
memory auto-linking in `link_memory_entities`) lands with `embedding IS NULL`.
A freshly-seen entity is therefore invisible to the new similarity lane until an
operator re-runs `entities reembed`.

This mirrors exactly where the L1 memory arc stood between #325 (backfill) and
#324 (forward): the lane exists, the backfill exists, but new rows don't embed
themselves.

## Goal

Embed **newly-inserted** entities at upsert time, through the *same*
`entity_embedding_text` chokepoint the backfill uses, so an on-insert vector is
byte-identical to a backfilled one. After this, a new entity is searchable via
the similarity lane without any manual `entities reembed` run.

## Non-goals / out of scope

- **Re-embedding conflict-hit (already-existing) entities.** The upsert's
  `ON CONFLICT DO UPDATE SET name_norm = entities.name_norm` never touches
  `embedding`, so an existing row keeps whatever it had. If an existing row still
  has `embedding IS NULL` (pre-feature rows, or a prior failed embed), that stays
  the **backfill's** job — exactly the #324/#325 division of labour. The forward
  path embeds *only* rows it just created (`inserted == true`, the `xmax = 0`
  discriminator the upsert already returns).
- **Batching the embed calls.** Like the backfill, the forward path embeds each
  new entity with one `embed_for_storage` call in a sequential loop. New entities
  per call are few; a batch-embed seam is a later optimization.
- **An ANN index** on `entities.embedding` (deferred, as in #335).
- **Schema / migration changes.** `entities.embedding vector(256)` already exists.

## Architecture

Bottom-up, each unit independently testable.

### Layer 1 — pure selector (`entity_extraction/batch_upsert.rs`)

`select_new_entities(deduped, upsert_map) -> Vec<(i64, &str, &str)>` — pure, no
I/O. Walks the deduped entity inputs, looks each up in the upsert result map
(`(kind, name_norm) -> (id, inserted)`), and returns `(id, kind, name)` for every
row with `inserted == true`. `kind` = the input label, `name` = the input display
text — identical to what the backfill reads back from `entities.name`, so
`entity_embedding_text(kind, name)` produces the same string either way.

Unit tests (no DB): all-new, all-conflict, mixed batch, empty.

### Layer 2 — forward embed loop (`entity_extraction/batch_upsert.rs`)

`embed_new_entities(pool, embedder, new_entities)` — async. For each
`(id, kind, name)`:
- `embed_for_storage(entity_embedding_text(kind, name))`;
- on `Some(vec)` → `set_entity_embedding(pool, id, &vec)` (the guarded,
  `embedding IS NULL`-checked, race-safe writer reused from the backfill);
- **degrade-and-warn** (mirrors `reembed_entities_null` / `promote_l1`): an embed
  `None` (the `RouterEmbedder` already logged the WARN), a lost `IS NULL` race
  (`Ok(false)` — a concurrent backfill won; not an error, no WARN), or a write
  `Err` (WARN) all skip that row and continue. The embed loop **never** fails the
  caller's upsert — an insight/seed write must not be blocked by a flaky embedder.

`entity_embedding_text` is imported from `crate::memory::entity_reembed`
(re-exported as `crate::memory::entity_embedding_text`) — the single source of
truth, so backfilled and on-insert text agree by construction.

### Layer 3 — wire into the upsert chokepoint

`batch_upsert::upsert_entities_and_relations(pool, merged, embedder)` gains a
`&dyn Embedder` param. The embed loop runs **after the entity upsert map is built
and `entity_ids` computed, before the relations phase** — entities are already
committed at that point, so newly-created rows get embedded even if the relations
phase later errors. The delegating
`gliner_relex::upsert_entities_and_relations` widens identically.

### Layer 4 — extractor owns the embedder

`GlinerRelexExtractor` gains an `embedder: Arc<dyn Embedder>` field (mirrors how
it already owns `pool`); `new(client, pool, embedder)`; `extract` passes
`&*self.embedder` into the upsert. The `NoOpEntityExtractor` path is unaffected —
it never upserts, so it never embeds.

### Layer 5 — wiring

- `main.rs`: build the `RouterEmbedder` Arc **before** the entity-extractor block
  and pass a clone into `GlinerRelexExtractor::new` (the same Arc still moves into
  `spawn_scheduler`). Production new entities now embed on insert.
- Three existing test sites (`entity_extraction_e2e`, `memory_entity_link_e2e`)
  construct the extractor directly → pass `Arc::new(NoOpEmbedder::new())` so their
  behaviour is unchanged (they assert upsert/linking, not embedding).

## Testing

- **Unit (pure):** `select_new_entities` — all-new / all-conflict / mixed / empty.
- **e2e (live PG 18):** new file or an addition to `entity_reembed_e2e` /
  `entity_extraction_e2e`: drive an extractor with a deterministic embedder, upsert
  a new entity, assert (a) its `embedding IS NOT NULL`, (b) it surfaces via
  `entity_similarity_search`, (c) a second upsert of the *same* entity (conflict
  hit) does **not** re-embed / does not change the vector, (d) degrade-and-warn
  leaves the row NULL when the embedder declines.

## Verification

`cargo clippy --workspace --all-targets -D warnings` clean; the touched unit +
e2e suites green on macOS PG 18. Pure-Rust, no migration, no OS-gated code → DGX
not required (carry the standing Linux baseline).
