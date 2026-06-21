# Plan: forward entity embed-on-insert

Spec: `docs/superpowers/specs/2026-06-21-entity-forward-embed-on-insert-design.md`
TDD throughout. Each task is one red→green→refactor cycle.

## Task 1 — pure `select_new_entities` (batch_upsert.rs)

- **Test (red):** in `batch_upsert/tests.rs`, build a `Vec<DedupedEntity>` + a
  hand-made `HashMap<(String,String),(i64,bool)>` and assert `select_new_entities`
  returns only the `inserted==true` rows as `(id, kind, name)`. Cases: all-new,
  all-conflict (empty result), mixed, empty input.
- **Impl (green):** add the pure `pub(crate) fn select_new_entities`.

## Task 2 — `embed_new_entities` loop + wire into upsert

- **Test (red):** core unit test (in `batch_upsert/tests.rs` or a small
  `#[tokio::test]`) is awkward without DB; cover the loop's behaviour in the e2e
  (Task 5). Here, just compile-pin the new `upsert_entities_and_relations`
  signature `(pool, merged, &dyn Embedder)`.
- **Impl (green):** add async `embed_new_entities(pool, &dyn Embedder, &[(i64,&str,&str)])`
  (degrade-and-warn). Call it inside `upsert_entities_and_relations` after the
  entity map + `entity_ids`, before the relations phase. Import
  `crate::memory::entity_embedding_text` + `kastellan_db::entity_embedding::set_entity_embedding`.

## Task 3 — widen the delegate + thread through the extractor

- `gliner_relex::upsert_entities_and_relations(pool, merged, embedder)` delegates.
- `GlinerRelexExtractor` gains `embedder: Arc<dyn Embedder>`; `new(client, pool, embedder)`;
  `extract` passes `&*self.embedder`.
- Fix the 3 direct-construction test sites to pass `Arc::new(NoOpEmbedder::new())`.

## Task 4 — main.rs wiring

- Build `RouterEmbedder` Arc before the entity-extractor block; pass `embedder.clone()`
  into `GlinerRelexExtractor::new`; keep moving `embedder` into `spawn_scheduler`.

## Task 5 — e2e (live PG 18)

- New `core/tests/entity_forward_embed_e2e.rs` (or extend `entity_reembed_e2e`):
  deterministic embedder + a `GlinerRelexExtractor` (or a direct
  `upsert_entities_and_relations` call). Assert:
  1. new entity → `embedding IS NOT NULL` after upsert;
  2. it surfaces via `entity_similarity_search`;
  3. conflict-hit re-upsert does not change the stored vector (no re-embed);
  4. embedder-declines (NoOp) → row stays NULL, upsert still succeeds.

## Task 6 — verify + docs + PR

- `cargo clippy --workspace --all-targets -D warnings`; touched unit + e2e suites green.
- Update HANDOVER.md header + Next TODO; ROADMAP if it tracks the entity arc.
- Commit (stage specific files), push, open PR linking the follow-up.
