# Cross-project study — **cognee** and **cognee-rs** (topoteretes)

**Date:** 2026-09-28
**Status:** Investigation / design input (no code change in this note)
**Question:** [`topoteretes/cognee`](https://github.com/topoteretes/cognee) is a
widely used "AI memory" engine: documents go in, a knowledge graph plus vector
index comes out, and an agent queries it. [`topoteretes/cognee-rs`](https://github.com/topoteretes/cognee-rs)
is its Rust port. Could either become a dependency of kastellan's memory
subsystem? If not, which of their ideas should we take?

> Sources, shallow-cloned and read directly:
> - `topoteretes/cognee` @ `c4cd8ceb` (2026-09-27), version **1.6.1**, **Apache-2.0**.
>   About 31 k stars, active since 2023-08.
> - `topoteretes/cognee-rs` @ `219bc84e` (2026-09-28), workspace version **0.2.0**,
>   **MIT OR Apache-2.0**. 258 commits since 2026-06-25, 52 stars, 31 crates.
>
> Files read:
> - `cognee/api/v1/{cognify,recall,improve}`
> - `cognee/modules/{search,improve,truth_subspace,ontology,chunking,users}`
> - `cognee/tasks/{graph,provenance,temporal_graph}`
> - `cognee/infrastructure/{engine/models/DataPoint.py,llm/structured_output_framework}`
> - `cognee/shared/{utils,graph_model_utils}.py`
> - `cognee/eval_framework/beam/REPORT.md`
> - cognee-rs `Cargo.toml`, `crates/{search,cognify,llm,embedding,telemetry,http-server}`
>
> Every file:line cited below was re-checked against those clones. Two claims
> were **not** re-verified from primary sources: the Neo4j server licence, and the
> details of CVE-2026-58473, which come from the OSV/OpenCVE listing.

---

## 0. Verdict in one paragraph

**Don't adopt either as a dependency. There are four or five ideas worth taking.**
The licence is not the problem: both are permissive and AGPL-compatible. The
problem is that cognee is a *second memory system* with its own stores, its own
LLM egress, its own auth and its own telemetry. Kastellan's invariants say memory
access is **core-only**, LLM calls go **only through `llm-router`**, storage is
**Postgres-only** (#9 and #10 closed as won't-fix), and Python runs **only inside a
sandboxed, stdio-JSON-RPC worker**. cognee cannot satisfy any of those without
being gutted to the point where nothing of it is left. Its security defaults also
point the wrong way for us: raw Cypher execution is on by default, telemetry is on
by default, and it `exec()`s generated code (§3).

What *does* transfer is a set of data-model and pipeline ideas. cognee has
shipped them, and our graph lane lacks them today (§4):
- per-edge provenance;
- supersession of outdated facts instead of deletion;
- non-destructive contradiction edges;
- schema-ordered structured output;
- a retrieval-eval harness.

---

## 1. What cognee is

**Pipeline.** An "ECL" (extract, cognify, load) pipeline. The default tasks, from
`cognee/api/v1/cognify/cognify.py:get_default_tasks`:
1. `classify_documents`
2. `extract_chunks_from_documents`, with token-sized chunks
3. `extract_graph_and_summarize`: LLM structured-output extraction and summarisation, run concurrently
4. `add_data_points`: writes to the graph store and to the vector store

**Opt-in tasks** add provenance, contradiction detection, temporal resolution and a
"temporal cognify" event graph.

**API.** The public surface has been reframed as a memory API:
`remember` = `add` → `cognify` → `improve`, plus `recall`, `improve` and `forget`.
`improve` is a nine-stage post-processor (`cognee/modules/improve/registry.py:DEFAULT_STAGES`)
that turns session Q&A, agent traces and user feedback back into graph content and
ranking weights.

**Data model.** `DataPoint` is a Pydantic base class:
- fields that hold other DataPoints become edges;
- `metadata.index_fields` get embedded;
- `identity_fields` give a deterministic `uuid5` id, so re-mentions merge.

**Stores.** Default is fully embedded, with no Postgres:
- graph: **Ladybug**, an MIT fork of the archived Kuzu;
- vectors: **LanceDB**;
- relational: **SQLite**.

pgvector is supported. **The Postgres *graph* adapter is explicitly a demo.**
`README.md:270` says "the production-ready version is available as a licensed
product", and the local GLiNER extractor is labelled the same way. This is
open-core: the code in the repository is all Apache-2.0, but some production
features are withheld.

**Search.** 20 `SearchType`s. The default is `HYBRID_COMPLETION`, and the list
includes `CYPHER` and `NATURAL_LANGUAGE` (LLM-written Cypher, executed).

**LLM layer.** Goes through litellm. The default structured-output path is
`litellm_native`: pass the Pydantic schema as `response_format` where the backend
supports it, otherwise use JSON-object mode with the schema in the prompt and
re-prompt on validation failure. With no API key, a "keyless" mode uses GLiNER for
extraction and fastembed (`bge-small-en-v1.5`) for embeddings.

**cognee-rs** is a genuine native port, not a client. It is on-device first
(pitched as 350 ms boot and 260 ms search), with bindings for JS, Python, C, Java
and Swift.
- **Defaults:** SQLite + Ladybug (`lbug`, bundled C++, needs cmake) + LanceDB
  (`=0.29`, needs protoc).
- **Feature-gated alternatives:** `pgvector` / `pggraph`.
- **LLMs:** only through an external OpenAI-compatible, Anthropic or Bedrock
  endpoint. There is no in-process model, and ONNX embeddings download from
  HuggingFace at runtime.
- **Toolchain:** Rust **1.91.1**, edition 2024. Our workspace MSRV is **1.78**.

---

## 2. Why neither can be a dependency

| Integration shape | Verdict | Why |
| --- | --- | --- |
| **cognee (Python) in-process** | ❌ Impossible | Python runs only inside sandboxed workers; no PyO3 (`CLAUDE.md` hard constraints). |
| **cognee as a sandboxed worker that owns memory** | ❌ Reject | Its value lives in its own stores and its own LLM calls. A worker holding the agent's memory in its own Ladybug/LanceDB files, calling the LLM through litellm, breaks "memory access is core-only" and "LLM calls go through `llm-router`". It would also split recall across two systems that CASSANDRA, the audit log and the quarantine CLI cannot see into. |
| **cognee as a stateless, `Net::Deny` extraction worker** (the gliner-relex shape) | ❌ No gain | Its extraction is either (a) an LLM call, which a worker may not make, or (b) keyless GLiNER, which we already run in `workers/gliner-relex` with an operator-managed vocabulary, quarantine-by-default entities and seccomp. What it would add is a large dependency footprint (dlt, pandas, lancedb, onnxruntime, litellm, fastapi…) and a runtime `pip install` of torch on the GLiNER path (`tasks/graph/gliner_demo/install.py`). |
| **cognee-rs as a library inside `core`** | ❌ Reject | Its default stores are embedded C++ engines outside Postgres, which reopens #9 and #10. The Postgres graph path is feature-gated and young. Its LLM client would be a second model egress beside `llm-router`. It needs a toolchain 13 minor versions above our MSRV. It changes API frequently at 0.2.0. And it would put a whole second memory engine into the core's trusted computing base. |
| **cognee-rs as a sandboxed Rust worker** | ❌ No gain | Same objections as the Python worker: no LLM, no Postgres, so nothing of it is left. |

**Licensing is not a blocker.** No CDDL, BUSL or SSPL dependency is on either
default path. Two things to watch: litellm's `enterprise/` directory has its own
licence, and FalkorDB (SSPL) exists only as an out-of-tree community adapter. We
would not lift code anyway (§4 is shapes only), but we could with attribution.

---

## 3. Security posture: what we would be importing

These are why "just try it in a worker" is not a cheap experiment. They are also
worth reading as a checklist of mistakes to avoid.

**Raw Cypher execution is on by default in both.**
- `ALLOW_CYPHER_QUERY` defaults to `"true"`:
  - Python: `cognee/modules/search/methods/get_search_type_retriever_instance.py:410`
  - Rust: `crates/search/src/retrievers/cypher_nl_retrievers.rs:85`
- `CYPHER` passes the query text verbatim to `graph_engine.query()`, with no
  read-only check. `NATURAL_LANGUAGE` executes Cypher that the LLM wrote, with
  retries.
- By design this bypasses the `InternalDataPoint` filter (see that class's
  docstring).

**The port lost a safety property the original had fixed.**
- Python's `recall` router makes Cypher non-routable on purpose, and says so
  (`cognee/api/v1/recall/query_router.py:41-46`):
  > *"auto-routing would let `{"query": "MATCH (n) DETACH DELETE n"}` mutate the
  > graph for anyone who can read it."*
- cognee-rs's `crates/search/src/query_router.rs:163-178` still carries the
  pre-fix rule `(^MATCH\s|^RETURN\s|^CREATE\s|^MERGE\s|--\(|\)--)` →
  `SearchType::Cypher` at weight 10, its highest priority. It cites the Python
  pattern as its source.
- **This is the drift-between-copies failure `CLAUDE.md` describes for our bwrap
  argv producers** (#661, #669): a "parity" port that copied the shape and lost the
  guard. From source this looks exploitable, but it was not exercised at runtime.

**Telemetry is on by default in both.**
- `cognee/shared/utils.py:21` POSTs to `https://test.prometh.ai`, from about 89
  call sites.
- The payload carries:
  - an anonymous id and a persistent id (`~/.cognee/.persistent_id`), both stable
    across reinstalls;
  - user and tenant ids;
  - a PBKDF2 hash of `LLM_API_KEY` with a *public default salt*.
- Opt-out is `TELEMETRY_DISABLED` set to any non-empty value (line 400), or
  `ENV=test|dev`.
- cognee-rs makes `telemetry` a default Cargo feature ("decision 1", in
  `crates/lib/Cargo.toml:70`).
- For a vendor-neutral, self-hosted, single-user system this is disqualifying as a
  default, and dangerous to depend on remembering to switch off.

**Code execution from input.**
- `cognee/shared/graph_model_utils.py:79` calls `exec()` on Python source
  generated from a JSON schema that arrives over HTTP. External `$ref`s are
  blocked; how much of the generated source an attacker controls was not assessed.
- `cognee/__init__.py:27` runs `load_dotenv(override=True)`, so a `.env` in the
  working directory overrides the process environment.

**Server auth.**
- **CVE-2026-58473** (CVSS 9.1): a self-registered user could overwrite the global
  LLM config. Fixed in 1.5.0 per the listing, and exploited in the wild from
  2026-07.
- cognee-rs's open-source HTTP server has `require_authentication` **off** by
  default. Real auth resolvers are left to "closed cloud builds".

**No prompt-injection defence on ingestion.** Ingested text flows straight into
extraction prompts. Our injection catalogue, `escape_untrusted_body`, and
quarantine-by-default entities have no counterpart.

**What cognee does well here, and we already match:**
- **Per-dataset database isolation.** With `ENABLE_BACKEND_ACCESS_CONTROL`, each
  user+dataset gets its own graph and vector database. That bounds blast radius,
  which is the same instinct as our one-sandbox-per-worker rule.
- **Text-to-SQL is off by default**, and when enabled it is SELECT-only and
  rollback-only.
- **The SSRF guard** on its web scraper.

---

## 4. What to borrow

Each item names the gap in kastellan that it fills, verified in our tree on
2026-09-28.

### 4.1 Per-observation provenance on relations *(highest value)*

**Our gap.**
- `core/src/entity_extraction/batch_upsert.rs:456` and `:491` insert every
  relation with `attrs = '{}'::jsonb`.
- The `WHERE NOT EXISTS (src_id, dst_id, kind)` dedup drops every later
  observation of the same edge.

So an edge in `relations` records neither *which text* asserted it, nor the
extractor's *score*, nor *model version*, nor *how many times* it has been seen.
Three consequences:
- Deleting the memory that caused an edge leaves the edge behind: no retraction.
- An edge seen once from a channel message ranks the same as one seen twenty times
  from operator-authored L1: no source weighting.
- The planned classification column on memory (`docs/cassandra_design_plan.md`
  line 366) has nothing to propagate through the graph lane.

**cognee's shape.** `cognee/tasks/provenance/record_provenance.py` is an opt-in,
append-only ledger of document → chunk → entity → relationship lineage. It is
keyed per dataset, written in one transaction per batch, and it **can never break
ingestion**: it swallows its own errors and returns its input unchanged.

**Kastellan version (proposal).** A `relation_observations` table:
`(relation_id FK, memory_id FK NULL, task_id NULL, source, score REAL, model_version, observed_at)`.
- Written in the same `batch_upsert` transaction.
- The dedup keeps the *edge* unique but always appends the *observation*.
- The graph lane can then rank by observation count and source.
- Edges whose last observation was cascaded away can be pruned or quarantined.
- The same table is where classification would ride.
- It is insert-only for the runtime role, like `deleted_memories`.

### 4.2 Supersession instead of deletion, driven by an operator-owned cardinality flag

**cognee's shape.** `cognee/modules/graph/utils/temporal_conflict_resolver.py`:
- When a *functional* (single-valued) relation has more than one target for the
  same source, the most recent assertion wins and the older edges are **tagged
  superseded, not deleted**, so history and provenance survive.
- Recency is decided deterministically.
- Their own admission is the interesting part:
  > *"there is no cardinality metadata to tell them apart"*

  so the caller must name the functional relations per call.

**Our advantage.** We *have* the place for that metadata. `relation_kinds` is an
operator-managed vocabulary (`0017`), SELECT-only to the runtime role.

**Kastellan version (proposal).**
- A `functional BOOLEAN NOT NULL DEFAULT FALSE` column on `relation_kinds`.
- `superseded_at` / `superseded_by` on `relations`.
- The graph lane skips superseded edges by default.

The clinical seeds show why this matters. For "prescribed" and "diagnosed with",
*current* versus *historical* is the whole point, and today the graph cannot tell
them apart. This sits well with 4.1, since the recency key would come from
`relation_observations.observed_at`. It is an incremental step toward the
valid-time modelling we don't have. The full bitemporal treatment in cognee's
`temporal_cognify` is not worth it yet.

### 4.3 Contradictions as non-destructive edges routed to the operator

**cognee's shape.** `cognee/tasks/graph/detect_contradictions.py` is opt-in and
LLM-driven. For the entities an ingestion touched, it gathers the 1-hop facts, asks
the model which pairs contradict, and writes a `contradicts` edge carrying both
facts, the reason and a confidence. It never rewrites or deletes. It works across
ingestions only because entity ids are deterministic, which our
`(kind, name_norm)` unique key already guarantees.

**Kastellan version.**
- Lower priority than 4.1 and 4.2, because it costs an LLM call per ingestion
  through `llm-router`.
- A contradiction should land in the **operator quarantine-review CLI** (the
  `entities approve/reject` surface), not be auto-resolved.
- An agent that resolves its own memory contradictions is exactly the kind of
  memory write adversary #6 in `docs/threat-model.md` is about.

### 4.4 Structured output, and schema key order is load-bearing

**Our gap.**
- `llm-router` has no `response_format` / `json_schema` / grammar support: a grep
  of `llm-router` and `core/src` finds none.
- Plans are parsed from free text by `parse_plan_lenient`.

The planner-reliability arc (#505/#508/#560/#677) is largely a history of paying
for that.

**cognee's shape.** `litellm_native` (§1) is the right fallback ladder:
1. native `response_format` where the backend supports it;
2. otherwise JSON mode with the schema in the prompt;
3. then re-prompt with the validation error.

vLLM, SGLang, llama.cpp and Ollama all accept some form of schema-constrained
decoding.

**cognee-rs's measurement.** This is worth more than the code (workspace
`Cargo.toml:144-153` and `:170-183`):
- **Key order in the JSON schema changed the runaway-generation rate from 19% to
  0%** (Fisher exact p = 0.0002, n = 126).
- With alphabetical keys, a free-text `description` came before `target_node_id`,
  and the model ran away inside the id.
- They enable `serde_json/preserve_order` **workspace-wide**, because a per-crate
  opt-in "worked" silently through feature unification until the one crate that
  needed it didn't have it.

We do not enable `preserve_order` anywhere today. That is harmless while we send
no schemas, and becomes a trap the day we do. **When structured output lands, take
both lessons:** declaration-ordered schemas, and a pinned test that the wire schema
lists properties in declaration order.

### 4.5 A retrieval-eval harness for `memory::recall`

**Our gap.** There is no retrieval evaluation anywhere in the tree: no golden set,
no MRR or nDCG.

We run four lanes fused with RRF (k = 60, scores discarded), with no recency
weighting and no re-ranker. Every proposal above, and any future GraphRAG-style
idea, is a hypothesis until recall has a number.

**cognee's shape.** `cognee/eval_framework/benchmark_adapters/` holds HotpotQA,
MuSiQue, 2Wiki and BEAM adapters, scored with EM/F1 and an LLM judge. Read their
own caveats before trusting their numbers:
- the HotpotQA suite is archived at 24 questions;
- BEAM 10M is in-sample (`beam/REPORT.md`).

**Kastellan version.**
- A small operator-authored golden set of (query, expected memory ids), scored for
  recall@k and MRR per lane *and* fused.
- It becomes one more profile in `scripts/run-e2e-gate.sh`, with a floor.
- This is the memory-side sibling of the planner A/B battery already on the
  ROADMAP.

### 4.6 Smaller shapes, noted but not proposed now

- **Content-addressed chunk ids for incremental re-extraction.** In
  `cognee/modules/chunking/chunk_id.py`, a chunk id is `uuid5(doc : sha256(text) : occurrence)`,
  so an edited document re-extracts only the chunks that changed. Relevant when
  document ingestion or an L2 writer arrives; our `body_sha256` is already halfway
  there.
- **Triplet embeddings.** Embed the text `"src rel dst"` so an edge can be reached
  by semantic search. Cheap in pgvector. It would give the graph lane an entry
  point that does not depend on NER seeds, which matters because the graph lane
  returns nothing until entities are approved out of quarantine.
- **Epoch-as-commit-point writes.** In `cognee/modules/truth_subspace/build.py`,
  derived coordinates are written at epoch N+1 first and the epoch pointer last,
  so a failed rebuild gives *"degraded reranking, never wrong reranking"*. It is a
  good general pattern for any derived index we rebuild, such as a future ANN index
  or L4 digests.
- **Watermarked, idempotent distillation stages.** In `cognee/modules/improve/stages.py`,
  each stage keeps a persist watermark and returns `already_completed` /
  `no_new_entries` rather than re-running. The same shape is needed for the L4
  digest writer (#629) and the reset snapshot writer.
- **Feedback weights.** A read-time ranking knob moved by scored answers. **Not
  proposed:** feedback arriving over a channel is a memory write path from a
  less-trusted source (adversary #6). If it is ever built, it should be
  operator-only.

### 4.7 Explicitly not borrowed

- **The whole "AI memory as a service" shape**: multi-tenant ACLs, an HTTP API,
  cloud sync. We are single-user and core-only.
- **Agent-queryable raw graph queries** (`CYPHER`, `NATURAL_LANGUAGE`). Our graph
  sits behind the `db::graph::Graph` trait chokepoint with fixed, parameterised
  recursive CTEs. It stays that way.
- **Ontology import from OWL/RDF** (`RDFLibOntologyResolver`, fuzzy matched at
  cutoff 0.8). Our vocabulary is already *strict*, which is cognee's opt-in
  `ONTOLOGY_MODE=strict`, and operator-curated. Fuzzy-matching new labels onto it
  would weaken that. Worth revisiting only for seeding `entity_kinds` from a
  published clinical ontology, and then as an operator import command, not a
  runtime resolver.
- **`DataPoint` identity fields.** Convergent with our `(kind, name_norm)` key;
  there is nothing to take.

---

## 5. Two findings about our own tree, surfaced by this study

1. **The semantic and lexical recall lanes have no `layer` or trust filter.**
   `db/src/memories/search.rs` has `semantic_search` (`WHERE embedding IS NOT NULL`)
   and `lexical_search` (`WHERE m.tsv @@ query`). Two consequences:
   - `core/src/memory/l3_surface.rs:17-20` promises that an `untrusted` skill
     "never reaches the planner". It does, through `<recalled>`: the lexical lane
     needs no embedding, so any L3 row whose words match the query qualifies.
   - L0 rows can appear twice.

   The bodies are escaped and screened there, so this is not an injection hole. It
   is a trust gate that one path honours and another does not.
   **Worth an issue:** either filter `layer`/trust in the lane SQL, or correct the
   module doc and say why `<recalled>` may see unapproved skills.
   *(Filed and fixed as #785: every lane now filters `layer` in SQL — L1/L2/L4
   only — and `recall()` re-checks after hydration.)*

2. **`CLAUDE.md` describes "three-lane memory"; the code has four lanes**
   (semantic, lexical, graph, entity-similarity) plus the L0–L4 layers. The
   description is stale.
