# L1 embedding population — design

**Issue:** [#323](https://github.com/hherb/kastellan/issues/323) — "Populate L1 embeddings through `truncate_to_embedding_dim` (semantic recall lane is empty until then)"
**Follow-up from:** PR #322 (`EMBEDDING_DIM` 1024 → 256, embeddinggemma Matryoshka truncation)
**Date:** 2026-06-20

## Problem

PR #322 made the **query** path correct: `core::memory::embed_query` Matryoshka-truncates the
model's native 768-d output to 256 via `db::memories::truncate_to_embedding_dim` before the dim
gate, and migration 0019 narrowed the embedding columns to `vector(256)`.

But **no write path populates an embedding for any layer** — every writer passes `None`
(`l0_seed` → `seed_meta_memory(…, None)`, `l1_promote` → `insert_memory_at_layer(…, None, Index)`,
`l3_crystallise`/`l3py_crystallise`, and there is no L2 writer in `core` at all). Since
`db::memories::semantic_search` selects `FROM memories WHERE embedding IS NOT NULL` (layer-agnostic),
the semantic recall lane returns **0 rows** end-to-end. `core::recall_assembly::pg_builder` embeds the
query and runs `recall`, but the semantic lane has nothing to match against — recall effectively runs
lexical + graph only.

## Decisions (locked in brainstorming)

1. **Layer scope: L1 only.** Wire `l1_promote` to embed-and-store so L1 insights become both
   in-prompt (sequential, unchanged) **and** semantically retrievable. This supersedes the old
   `l1_promote.rs:209-213` doc note ("embedding not populated… L1 is loaded by sequential scan");
   that note is rewritten by this change.
2. **Injection: an `Embedder` seam, lazy, degrade-and-warn.** `promote_l1` gains a `&dyn Embedder`
   param (mirroring the existing `&dyn EntityExtractor` it already carries). It is called **only after
   the dedup EXISTS-check passes**, so a duplicate body never triggers an embed. On embed failure the
   row is inserted with a NULL embedding plus a WARN — graceful degradation, exactly like the
   entity-linker beside it; an insight write is never blocked by a flaky local embedder.
3. **Agent path embeds; operator path is NoOp; backfill deferred.** Only the agent-raised scheduler
   path injects a real `Router`-backed embedder. The operator CLI `l1 add` path injects a `NoOpEmbedder`
   (symmetric with its existing `NoOpEntityExtractor`; no `Router` needed in the CLI). Existing
   NULL-embedding rows + operator rows are handled later by a **tracked follow-up** batch-(re)embed
   subcommand (issue #323 item 2). This change is the forward write path only.

## Architecture

### New seam — `Embedder` (`core/src/memory/embedder.rs`, new file)

Mirrors `core::entity_extraction::EntityExtractor` one-for-one (`#[async_trait]`, `Send + Sync`,
a real impl + a `NoOp` impl).

```rust
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Produce a stored-contract embedding (EMBEDDING_DIM-length, unit-norm)
    /// for `text`. `None` means "store no embedding" — either intentional
    /// (NoOp) or a soft-failed embed (the impl logs the WARN). Callers store
    /// NULL on None.
    async fn embed_for_storage(&self, text: &str) -> Option<Vec<f32>>;
}
```

- **`RouterEmbedder { pool: PgPool, router: Arc<Router> }`** — delegates to the existing
  `core::memory::embed::embed_query`, which already runs `truncate_to_embedding_dim` (satisfying the
  issue's "route through the same chokepoint" requirement) **and** writes the
  `actor='llm:router' action='embed'` audit row. `Ok(v) → Some(v)`; `Err(e) → tracing::warn! + None`.
  The degrade-and-warn lives in the impl, keeping `promote_l1` trivial.
- **`NoOpEmbedder`** — always returns `None`. Used by the operator CLI path.

**Why `Option`, not `Result`:** `promote_l1` does not need to distinguish "intentional skip" from
"failure" — both store NULL. The WARN distinction is preserved inside `RouterEmbedder`. A `Result`
return would force every caller to re-decide the degrade posture.

### `promote_l1` (`core/src/memory/l1_promote.rs`)

Gains `embedder: &dyn Embedder`, called lazily after the dedup miss:

```rust
pub async fn promote_l1(
    pool: &PgPool,
    extractor: &dyn EntityExtractor,
    embedder: &dyn Embedder,
    body: &str,
    source: L1Source,
) -> Result<L1WriteOutcome, L1Error> {
    let trimmed = validate_l1_body(body)?;
    // ... EXISTS-check; on hit → return SkippedDuplicate (embedder NOT called) ...
    let embedding = embedder.embed_for_storage(trimmed).await;       // Option<Vec<f32>>
    let new_id = insert_memory_at_layer(
        pool, trimmed, &metadata, embedding.as_deref(), MemoryLayer::Index,
    ).await?;
    // ... existing entity auto-link (unchanged) ...
}
```

The vector from `embed_query` is already `EMBEDDING_DIM`-length unit-norm, so
`insert_memory_at_layer`'s `check_embedding_dim` passes. **No change to the `db` crate.**

### Threading the real embedder to the agent-raised path

Same shape `entity_extractor` already uses through the scheduler:

```
spawn_scheduler(…, entity_extractor, embedder: Arc<dyn Embedder>)
  → lane_loop → drain_lane → write_l1_promoted_row(…, embedder: &dyn Embedder) → promote_l1
```

- `core/src/main.rs:325`: build `Arc::new(RouterEmbedder { pool: pool.clone(), router: router.clone() })`
  (both already in scope) and pass it into `spawn_scheduler`.
- The two scheduler fns already carry `#[allow(clippy::too_many_arguments)]`; add the one param
  alongside `entity_extractor` rather than introducing a dependency-bundle struct (consistent with
  the existing in-file note that deliberately resisted bundling).

### Operator path (`core/src/cli_audit.rs::l1_add_and_audit`)

Passes `&NoOpEmbedder`. Operator-added rows stay embedding-free, handled later by the deferred
batch-(re)embed follow-up.

## Data flow

```
agent task completes
  → runner::drain_lane (Outcome::Completed, has insight)
  → write_l1_promoted_row(pool, &*entity_extractor, &*embedder, task_id, insight)
  → promote_l1:
       validate → dedup EXISTS-check
         hit  → SkippedDuplicate            (no embed, no insert)
         miss → embedder.embed_for_storage  (RouterEmbedder → embed_query → 256-d unit vec + audit row)
              → insert_memory_at_layer(…, Some(vec), Index)
              → entity auto-link (unchanged)
  → semantic_search now returns the row (lane no longer empty)
```

## Error handling

- **Embed failure (RouterEmbedder):** `warn!` + `None` → row inserted with NULL embedding. Lexical +
  metadata lanes still find it; semantic lane skips it (`WHERE embedding IS NOT NULL`). Write succeeds.
- **Dedup hit:** embedder never called (laziness — pinned by test).
- **DB / validation errors:** unchanged — surface as `L1Error::{Db,Validation}`; the agent-raised
  caller already swallows them at WARN (observability aid, not a correctness signal).
- **Audit-row insert failure (inside `embed_query`):** already best-effort (logged, embedding preserved).

## Testing (TDD)

### Unit (`core/src/memory/embedder.rs`, no PG)
- `NoOpEmbedder::embed_for_storage` returns `None`.
- `Embedder` is object-safe — `&dyn Embedder` compile-pin (mirrors existing trait-pin tests).

### E2E (`core/tests/memory_l1_promote_e2e.rs`, PG-required)
A `FakeEmbedder` test helper (counts calls, returns a fixed 256-d unit vector; a variant returns `None`).

1. **Embed-on-insert:** `promote_l1` with `FakeEmbedder→Some(256-vec)` → inserted row has a non-NULL
   embedding, and `semantic_search` for that vector returns the row (proves the lane is populated
   end-to-end).
2. **Lazy on dedup-skip:** insert a body, insert the same body again → `FakeEmbedder` call-count stays
   **1** (skip path never embeds). Pins the laziness.
3. **Degrade-and-warn:** `FakeEmbedder→None` → row still inserts with NULL embedding; `semantic_search`
   skips it, lexical still finds it.
4. Existing `promote_l1` e2e tests get the new `&NoOpEmbedder` arg (mechanical).

### Verification gate
`cargo test -p kastellan-core` (affected suites run live-PG on the dev Mac) +
`cargo clippy --workspace --all-targets -D warnings`. Pure-Rust, no OS-gated code → DGX not required;
the existing native-Linux baseline carries forward.

## Out of scope (deferred)

- **Backfill / `kastellan-cli memory l1 reembed`** of existing NULL-embedding rows → new tracked issue
  (closes #323 item 2 later). Today there are zero embedded rows anyway (0019 discarded any; the live
  DGX never stored one since 768 ≠ 1024).
- **Operator CLI path embedding** — stays NoOp.
- L2 (Stable) / L3 (Skill) embedding population — separate features.

## Files touched

| File | Change |
| ---- | ------ |
| `core/src/memory/embedder.rs` | **new** — `Embedder` trait + `RouterEmbedder` + `NoOpEmbedder` + unit tests |
| `core/src/memory/mod.rs` | re-export the new module's public items |
| `core/src/memory/l1_promote.rs` | `promote_l1` gains `embedder: &dyn Embedder`; lazy embed after dedup; rewrite the stale embedding doc note |
| `core/src/scheduler/runner.rs` | thread `Arc<dyn Embedder>` through `spawn_scheduler`/`lane_loop`/`drain_lane`/`write_l1_promoted_row`; update the signature-pin test |
| `core/src/main.rs` | build `RouterEmbedder`, pass into `spawn_scheduler` |
| `core/src/cli_audit.rs` | `l1_add_and_audit` passes `&NoOpEmbedder` |
| `core/tests/memory_l1_promote_e2e.rs` | `FakeEmbedder` helper + 3 new tests + mechanical arg updates |
