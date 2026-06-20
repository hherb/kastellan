# 12 — Memory and recall

The agent's memory store lives in Postgres and is queried only by the
core process (workers never link to `db`). This chapter explains the two
memory tiers, the three retrieval lanes, and how fusion produces a single
ranked result list.

---

## Two tiers: L0 and L1

```
incoming event  ───►  L0 (raw observations)  ───►  L1 (promoted memories)
                       memory::l0_seed              memory::l1_promote
                                                    └─ embeddings, tsvector,
                                                       entity_links
```

- **L0 — raw observations.** Every channel message, every tool result the
  scheduler decides to remember, lands in L0 first. L0 is append-only
  and preserves the original wording.
- **L1 — promoted memories.** A separate pass (`memory::l1_promote`)
  distills L0 rows into longer-lived L1 memories. Promotion attaches:
  - a 256-dim embedding (via `llm-router`; the model's native output is
    Matryoshka-truncated to 256 — see `db::memories::truncate_to_embedding_dim`),
  - a `tsvector` for lexical search,
  - links into the entity/relation graph
    (`memory::entity_link`).

Recall queries always hit L1. L0 is forensic — it answers "what exactly
came in?" rather than "what does the agent know?".

There is also an **L3 — skills** layer: parameterised JSON-RPC tool-call
templates (or verbatim agent-authored Python) crystallised from successful
trajectories and promoted through a trust lifecycle (untrusted →
user-approved → pinned) before they can be recalled and re-invoked. L3 is a
distinct surface (`l3_*` / `l3py_*` modules + the `memory l3` CLI), separate
from the L0/L1 recall lanes below.

The layer types and layer-aware helpers live in `core/src/memory/layers.rs`;
the seeding and promotion logic in `l0_seed.rs` and `l1_promote.rs`.

---

## Three retrieval lanes

`core::memory::recall::recall()` runs the lanes you ask for, each
returning a ranked list of L1 memory ids.

### 1. Semantic — pgvector ANN

```rust
let emb = memory::embed_query(&pool, &router, query).await?;
recall(&pool, &RecallParams {
    query_embedding: Some(&emb),
    modes: RecallModes::SEMANTIC,
    ..Default::default()
}).await?
```

- Vector is produced by `memory::embed_query`, which routes through
  `llm-router::embeddings` and writes the first
  `actor='llm:router' action='embed'` audit row.
- Postgres-side: pgvector ANN over the 256-dim `embedding` column on
  L1 memories.

### 2. Lexical — `tsvector` + `ts_rank`

```rust
recall(&pool, &RecallParams {
    query_text: Some(query),
    modes: RecallModes::LEXICAL,
    ..Default::default()
}).await?
```

- No embedding needed. Best when the query carries a rare token the
  embedding model has no special signal for (CVE ids, version numbers,
  proper nouns).

### 3. Graph — entity neighbour walk

```rust
recall(&pool, &RecallParams {
    seed_entities: Some(&entity_ids),
    modes: RecallModes::GRAPH,
    ..Default::default()
}).await?
```

- 1-hop walk over the `entities`/`relations` tables, starting from the
  entities mentioned in the query (extracted upstream by the entity
  extraction pipeline). Fan-out is capped per seed
  (`GRAPH_FANOUT_CAP_PER_SEED`) so a hub entity doesn't dominate the list.

---

## RRF fusion

When multiple lanes are requested, their ranked lists are combined via
**Reciprocal Rank Fusion**:

```
score(d) = Σ over lanes  1 / (k + rank_lane(d))
```

`k = RRF_K_CONSTANT` (defined in `recall.rs`). RRF is a pure function
(`reciprocal_rank_fusion`) and is tested independently of the database.

After fusion, the top-k ids are hydrated in one round-trip via
`db::memories::fetch_by_ids` — no N+1 query.

---

## What `recall()` does and does not do

**Does:**
- Run each requested lane against L1 only.
- Apply the RRF fusion when more than one lane runs.
- Hydrate the top-k memory bodies in one query.

**Does not:**
- Call the LLM router. That happens upstream in `embed_query` — recall
  takes a precomputed embedding.
- Touch L0. Recall is for promoted memories.
- Write any audit row. The audit row for embedding generation is
  written by `embed_query`; recall itself is read-only.

---

## Module layout

```
core/src/memory/
  mod.rs           Public re-exports
  recall.rs        recall(), reciprocal_rank_fusion, RecallParams,
                   RecallModes, RRF_K_CONSTANT, GRAPH_FANOUT_CAP_PER_SEED
  embed.rs         embed_query(), MemoryError
  layers.rs        Layer enum + layer-aware accessors
  l0_seed.rs       Seed L0 rows from new observations
  l1_promote.rs    Promote L0 rows into L1 memories
  entity_link.rs   Helpers that link L1 memories to entities
  l3_crystallise / l3_approval / l3_invoke / l3_surface
                   Templated L3 skill lifecycle
  l3py_crystallise / l3py_approval / l3py_invoke
                   Agent-authored Python L3 skill lifecycle
```

The files are split because each one is at or near the soft 500-LOC cap
in `CLAUDE.md`. Keeping `recall.rs` pure-data (no LLM calls) means tests
can seed deterministic embeddings without a router mock.

---

## Adding a new lane

1. Define how the lane is keyed (`RecallParams` field).
2. Implement the query in `recall.rs`. Return `Vec<i64>` of memory ids in
   rank order — do not hydrate bodies.
3. Add a `RecallModes` flag and wire it into the dispatcher inside
   `recall()`.
4. Cover with two tests:
   - lane in isolation, against seeded fixtures,
   - lane fused with one other lane, asserting the RRF order.

Do **not** add a lane that talks to anything outside Postgres. New
external dependencies (e.g. an external vector index) belong behind the
sandbox boundary in a worker.

---

## Common questions

- **Why not just one lane?** Each lane misses a different class of
  query. Semantic alone misses rare tokens; lexical alone misses
  paraphrases; graph alone misses memories with no entity mention. RRF
  recovers the union without picking a king.
- **Why RRF and not a learned reranker?** RRF has no hyperparameters
  and no training. It is the right baseline. A learned reranker can
  slot in as a post-fusion stage when there's evidence it earns its
  complexity.
- **Where does graph data come from?** The entity extraction pipeline
  (`core/src/entity_extraction/`) calls the GLiNER/ReLeX worker, then
  upserts entities and relations via
  `entity_extraction::batch_upsert`. L1 memories are linked to
  entities by `memory::entity_link` during promotion.
