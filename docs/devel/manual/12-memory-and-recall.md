# 12 — Memory and recall

The agent's long-term memory lives in Postgres (the `memories` table plus
the entity/relation graph) and is read and written only by the core
process — workers never link to `db`. This chapter covers the five memory
layers, how each one reaches the planner's prompt, the four recall lanes
and how they are fused, the entity graph that feeds two of them, and the
two short-term mechanisms that are *not* memory layers but are often
confused with them (the handoff cache and conversational continuity).

The per-task working memory of notebook mode (notebook and scrapbook) is
part of the planner, not of this store — see [chapter 15](./15-planner.md).

---

## Five layers, five doors

Every `memories` row has a `layer` (`kastellan_db::memories::MemoryLayer`,
a SMALLINT with a CHECK constraint):

| Layer | Name | Written by | Reaches the prompt through | In recall? |
|---|---|---|---|---|
| L0 `Meta` | Meta-rules / hard constraints | Operator TOML seed only | `<l0_meta_rules>` — newest version of each rule, every prompt | **no** |
| L1 `Index` | Insight index | Operator CLI; agent-raised on task completion | `<l1_insights>` — newest 32 rows / 4 KiB, every prompt | yes |
| L2 `Stable` | Stable facts | Default layer of `insert_memory`; no automatic writer today | recall only | yes |
| L3 `Skill` | Skills (tool-call templates, Python snippets) | Agent crystallisation, then operator approval | `<skills>` — approved skills only | **no** |
| L4 `Digest` | Session digests | **Nothing yet** — reserved | recall only (no dedicated block yet) | yes |

The design rule: **a layer with its own gated door into the prompt is kept
out of recall**, so that door stays the only one. Recall must not be a way
around `<l0_meta_rules>`' newest-version load (L0 is append-only, so recall
could return a superseded rule) or around `<skills>`' trust gate (recall
would let an `untrusted` skill in). The rule is the exhaustive
`MemoryLayer::is_recallable` in `db/src/memories/recall_layers.rs`; every
lane's SQL applies it before its `LIMIT`, and `recall()` re-checks after
hydration, logging and (in debug builds) panicking if a lane ever lets a
row through. Adding a layer is a compile error there until someone decides
this question for it.

### L0 — meta-rules

Hand-curated rules in `seeds/memory/l0_meta_rules.toml` (override with
`KASTELLAN_L0_RULES_FILE`). At boot `memory::l0_seed` hashes each rule and
appends a row only when `(l0_rule_id, body_sha256)` is new, so editing a
rule writes a new version and keeps the old one for audit. A malformed file
stops the daemon. `insert_memory_at_layer` **rejects** L0 with
`DbError::PolicyViolation`; the only writer is the deliberately named
`seed_meta_memory`, so a grep finds every L0 write site.

### L1 — insight index

Small routing pointers loaded into every prompt regardless of the query
(`memory::layers::load_l1`, capped at `L1_DEFAULT_CAP_ROWS` = 32 and
`L1_DEFAULT_CAP_BYTES` = 4 KiB). Two writers share one validation + SHA-256
dedup path (`memory::l1_promote`):

- the operator, via `kastellan-cli memory l1 add|list|remove`;
- the agent, via a terminal plan's `l1_insight` field, consumed when the
  task completes.

Agent-raised rows are embedded and entity-linked on insert (both
degrade-and-warn: a failed embed stores a NULL embedding, never blocks the
write). Operator rows are not; `kastellan-cli memory l1 reembed` backfills
NULL embeddings. L1 stays recallable on purpose, so insights older than the
32-row block are still reachable.

### L3 — skills

Parameterised JSON-RPC tool-call templates (`l3_*` modules) or verbatim
agent-authored Python (`l3py_*`, run in `python-exec`), crystallised from a
terminal plan's `l3_skill` / `python_skill` candidate — only when the task
actually dispatched at least one tool step. Every crystallised skill lands
`untrusted`. The trust lifecycle is operator-driven:

```
untrusted ──memory l3 approve──► user_approved ──memory l3 pin──► pinned
```

`memory l3 revoke` returns either approved state to `untrusted`.

- `l3_surface` renders `user_approved` and `pinned` skills into `<skills>`
  (name, description, parameter manifest — not the template).
- The agent can **invoke** only `pinned` skills, via a plan's
  `invoke_skill`; the invocation is expanded into ordinary steps *before*
  CASSANDRA review, so the expanded steps are reviewed like any others.
- Every invocation (agent or operator, `kastellan-cli memory l3 run`)
  re-validates the stored template against the live tool registry,
  re-scans for `secret://` refs and re-hashes before dispatch.

`kastellan-cli memory l3 list|show|approve|pin|revoke|remove|run` is the
operator surface. Skills are never operator-authored, so there is no `add`.

### L2 and L4 — what is not filled yet

L2 is the default layer of the low-level `insert_memory` and the layer
pre-migration rows were backfilled to, but nothing in the running agent
writes L2 automatically today. L4 is declared, accepted by the DB and
written by nothing. Both are on the roadmap: tiered session digests that
fill L4 with an episodic `<history>` block
([#629](https://github.com/hherb/kastellan/issues/629)), and a
context-reset snapshot writer. Until then cross-task continuity comes from
L1 and from conversational continuity (below).

---

## How memory reaches the planner

Every notebook turn, `RouterAgent::send_turn`
(`core/src/scheduler/agent.rs`) does three things:

1. **Entity extraction** on the task instruction (gliner-relex, if
   enabled) → seed entity ids for the graph lane. Failure degrades to no
   seeds.
2. **Recall** (`recall_assembly::PgRecallBuilder`): embed the instruction
   with `memory::embed_query`, run `recall()` with all four lanes when
   there are seeds, otherwise semantic + lexical + entity-similarity.
   **Recall is enrichment, not policy:** any failure degrades to an empty
   `<recalled>` block with a warning.
3. **Prompt assembly** (`prompt_assembly::PgSystemPromptBuilder`): load L0,
   L1 and surfaced skills and render the system message. **This is
   fail-closed:** a failed L0/L1 load fails the planning turn, because a
   prompt missing its meta-rules is worse than no plan.

The assembled order is `<base>`, `<handoff>`, `<tools>`, `<l0_meta_rules>`,
`<l1_insights>`, `<skills>`, `<recalled>` — most stable first, so
consecutive prompts share the longest byte prefix and the backend's prompt
cache can skip re-reading it (see [chapter 13](./13-llm-router.md)).
L0 and skill bodies are operator-gated and rendered verbatim; **L1 and
`<recalled>` bodies are escaped** with `escape_untrusted_body`, because the
agent can write them. The `plan.formulate` audit row records the per-layer
counts, the recall count and the graph seed ids.

---

## Four recall lanes

`core::memory::recall::recall()` runs the lanes selected in
`RecallParams.modes` (`RecallModes { semantic, lexical, graph, entity }`).
Each lane lives in `kastellan_db` and returns a ranked list of memory ids;
`recall()` fuses them and hydrates the top `k` in one round trip
(`db::memories::fetch_by_ids`, no N+1).

```rust
let emb = memory::embed_query(&pool, &router, query).await?;
let rows = recall(&pool, &RecallParams::new(query, &emb)).await?;            // semantic + lexical + entity
let rows = recall(&pool, &RecallParams::with_seeds(query, &emb, &seeds)).await?; // all four
```

1. **Semantic** — pgvector cosine over the 256-dim `memories.embedding`
   (`EMBEDDING_DIM`; the embedding model's output is Matryoshka-truncated
   to 256 and unit-normalised). Best when meaning matches but words do not.
2. **Lexical** — `tsvector` + `ts_rank`. Best for rare tokens the
   embedding has no signal for (CVE ids, version numbers, proper nouns).
3. **Graph** — the seed entities plus their 1-hop neighbours
   (`kastellan_db::graph::Graph::neighbors`, capped by
   `GRAPH_FANOUT_CAP_PER_SEED` = 32 so a hub cannot dominate), then the
   memories linked to the most of them.
4. **Entity similarity** — nearest entities to the *query embedding*
   (`entities.embedding`, fan-out `ENTITY_SIMILARITY_FANOUT` = 64), then
   their linked memories. Needs no seeds, so it still finds related
   memories when extraction recognised nothing.

The graph and entity lanes exclude **quarantined** entities (see below)
on the agent's path; only operator CLIs see them.

### RRF fusion

```
score(d) = Σ over lanes  1 / (k + rank_lane(d))        k = RRF_K_CONSTANT = 60
```

`reciprocal_rank_fusion` is a pure function, tested without a database.
It needs no score calibration between lanes (cosine distance and `ts_rank`
are not comparable) and no training.

### What `recall()` does not do

- Call the LLM. The query embedding is precomputed by `embed_query`,
  which goes through `llm-router` and writes the
  `actor='llm:router' action='embed'` audit row. Recall itself is
  read-only and writes no audit row.
- Return L0 or L3 rows — ever.

---

## The entity graph

`core/src/entity_extraction/` owns the `EntityExtractor` trait. The
production impl, `GlinerRelexExtractor`, dispatches to the **gliner-relex**
worker (Python, sandboxed like every worker, joint NER + relation
extraction) through the normal chokepoint. It is **opt-in**
(`KASTELLAN_GLINER_RELEX_ENABLE=1` plus the model weights); without it the
daemon uses `NoOpEntityExtractor` and the graph lane stays empty.

Extraction runs in two places: on the task instruction (to seed the graph
lane) and on newly written L0 rows and agent-raised L1 rows
(`memory::entity_link`, which upserts
entities and relations via `entity_extraction::batch_upsert` and links them
to the memory). Names are deduplicated through
`kastellan_db::normalize_entity_name`. New entities are embedded on insert
for the entity-similarity lane; `kastellan-cli entities reembed` backfills.

**Quarantine by default.** Every newly upserted entity is quarantined and
does not influence the agent's recall until an operator reviews it:

```sh
kastellan-cli entities list [--state quarantined|approved|any] [--kind K] ...
kastellan-cli entities show <id>
kastellan-cli entities approve <id>...      # or: reject, merge --keep <id> --drop <id>,...
kastellan-cli relations show <entity-id> [--depth N]   # quarantined endpoints tagged [Q]
```

The entity-kind and relation-label vocabularies are operator-curated
(`kastellan-cli entities kinds …`, `relations kinds …`); the runtime DB role
has no write grant on them, so the agent cannot widen its own vocabulary.

---

## Short-term context (not memory layers)

**Handoff cache** (`core/src/handoff.rs`). A tool result larger than
`DEFAULT_RESULT_BYTE_CAP` (64 KiB) is stashed in an in-memory, per-task,
content-addressed cache and replaced with a placeholder carrying a small
head of it; the planner pulls slices back with the reserved `handoff` /
`fetch` built-in, each slice re-screened for injection. Entries are purged
when the task ends. Nothing is persisted.

**Conversational continuity** (`core/src/scheduler/conversation/`). A
finishing channel task leaves a `tasks.turn_record` (the tool *calls* it
made, not their results, plus the most sensitive data class it touched);
the next task in the same conversation sees a few recent turns as data and
inherits their classification floor. It lives on the task row, not in
`memories`, and is never recalled. Details are in
[chapter 16](./16-channels.md#conversational-continuity).

---

## Module layout

```
core/src/memory/
  mod.rs               Public re-exports
  recall.rs, recall/   recall(), reciprocal_rank_fusion, RecallParams,
                       RecallModes, RRF_K_CONSTANT, lane fan-out caps
  embed.rs             embed_query(), MemoryError
  embedder.rs          Embedder seam (RouterEmbedder / NoOpEmbedder)
  layers.rs            L1 loader + caps
  l0_seed.rs           L0 TOML seed loader + load_l0_active
  l1_promote.rs        L1 writer (operator + agent-raised)
  entity_link.rs       Link a new memory to extracted entities
  l1_reembed.rs, entity_reembed.rs, reembed.rs   NULL-embedding backfills
  l3_crystallise / l3_approval / l3_surface / l3_invoke    Templated skills
  l3py_crystallise / l3py_approval / l3py_invoke          Python skills

core/src/recall_assembly/   Per-turn recall → RecalledContext
core/src/prompt_assembly/   System-prompt assembly (pure assembler + PG builder)
core/src/entity_extraction/ EntityExtractor, gliner-relex client, batch_upsert
core/src/handoff.rs         Oversized-result cache
core/src/scheduler/conversation/   turn_record, view, inherited floor

db/src/memories/            Lanes (search.rs), writes, layer loads, recall_layers.rs
db/src/graph.rs, entity_embedding.rs, entities.rs   The entity graph
```

Files are split to stay near the soft 500-LOC cap (see
[chapter 9](./09-rust-patterns.md)). Keeping `recall()` free of LLM calls
means tests seed deterministic embeddings without a router mock.

---

## Adding a new lane

1. Decide how the lane is keyed (a `RecallParams` field) and add a
   `RecallModes` flag.
2. Implement the query in `kastellan_db` returning `Vec<i64>` of memory ids
   in rank order — do not hydrate bodies. **Apply the layer gate in the
   SQL** (`layer = ANY(…)` bound from `recallable_layer_codes()`) before the
   `LIMIT`, and exclude quarantined entities if the lane touches the graph.
   The post-hydration re-check will catch you if you forget, loudly.
3. Wire it into `recall()`.
4. Tests: the lane alone against seeded fixtures, the lane fused with one
   other lane asserting RRF order, and a case in
   `core/tests/memory_recall_layer_gate_e2e.rs` showing it returns no L0/L3
   row.

Do **not** add a lane that talks to anything outside Postgres. An external
index or model belongs behind the sandbox boundary in a worker.

---

## Common questions

- **Why four lanes?** Each misses a different class of query: semantic
  misses rare tokens, lexical misses paraphrase, graph misses memories with
  no extracted entity, entity similarity misses memories with no linked
  entity at all. RRF recovers the union without picking a winner.
- **Why RRF and not a learned reranker?** No hyperparameters, no training,
  a robust baseline. A reranker can be a post-fusion stage once there is
  evidence it earns its keep (a recall-evaluation harness is proposed on
  the roadmap; its golden set must not expect L0/L3 hits).
- **Why is L1 both in every prompt and in recall?** The block holds only
  the newest 32 rows; recall keeps older insights reachable. L1 has no
  trust gate to bypass, so the second door is harmless.
- **Why are L1 and recalled bodies escaped but L0 and skills not?** The
  agent can write L1 (and anything recall returns could have come from it);
  L0 and approved skills passed an operator.
