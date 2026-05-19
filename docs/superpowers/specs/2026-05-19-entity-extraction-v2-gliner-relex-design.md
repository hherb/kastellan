# Entity Extraction v2 (GLiNER-Relex consumer) — design

**Date:** 2026-05-19
**Status:** Design, ready for plan.
**Branch (proposed):** `feat/entity-extraction-v2`
**Supersedes:** [`docs/superpowers/specs/2026-05-18-entity-extraction-graph-lane-design.md`](2026-05-18-entity-extraction-graph-lane-design.md) (v1 — `HybridEntityExtractor` with deterministic substring + LLM fallback; was design-only, no code shipped). v2 keeps the same EXTERNAL contract (`EntityExtractor` trait, `EntitySeeds`, `RecallBuilder` widening, `formulate_plan` flow, `plan.formulate` payload bump) but replaces the production impl with a single-pass GLiNER-Relex worker call.

**Pre-reqs (all shipped on `main`):**

- PR #41 (memory graph lane + `memory_entities` join table, 2026-05-12).
- PR #29 (`Router::embed`, 2026-05-12).
- PR #79 (recall-lane wiring, 2026-05-17).
- PR #82 (L1 promotion writer, 2026-05-18) — establishes the `plan.formulate` payload-bump precedent (Slice E added `l1_insight`); v2 adds Slice F's 3 keys.
- PR #83 (worker-lifecycle slices 1 + 2, 2026-05-18) — `IdleTimeoutLifecycle`, the warm-keep that makes GLiNER-Relex's 1.3 GB resident model economically viable.
- PR #88 (GLiNER-Relex Slice 2 — Rust manifest + e2e, 2026-05-18) — the Python worker + Rust manifest entry + 4 integration tests pinning the wire shape. v2's `Client` wraps the dispatch chokepoint to this worker.

**Companion docs:**

- [`docs/superpowers/specs/2026-05-18-gliner-relex-feasibility-study.md`](2026-05-18-gliner-relex-feasibility-study.md) — establishes the licensing chain (Apache 2.0 on weights + code via `knowledgator/gliner-relex-multi-v1.0`), the cross-platform posture, and the recommendation to prototype before deciding. v2 is the post-prototype commit.
- [`docs/superpowers/specs/2026-05-18-gliner-relex-spike-notes.md`](2026-05-18-gliner-relex-spike-notes.md) — POC corrections that landed into Slice 1 + 2 (method is `inference()`, triple keys are `head`/`tail`, CUDA mem-probe needed, relation_threshold ≥ 0.5 for noise suppression).
- [`docs/superpowers/specs/2026-05-18-gliner-relex-worker-design.md`](2026-05-18-gliner-relex-worker-design.md) — the worker's own design spec.

## Why now

`core::memory::recall` ships three lanes — semantic, lexical, graph — fused via Reciprocal Rank Fusion. Semantic + lexical are wired end-to-end in production (PR #79). The graph lane is **a complete no-op** because:

- `PgRecallBuilder::build` calls `RecallParams::new(text, embedding)` — never `with_seeds`.
- `seed_entity_ids` is therefore always `None`.
- `recall::recall` short-circuits the graph lane when seeds are empty.

The graph lane needs entity IDs from the `entities` table to seed `memory_entities` traversal. Until a query-time extractor resolves entity references and plumbs them into `with_seeds`, the lane is dead.

v1 proposed a `HybridEntityExtractor` (deterministic substring-match primary + LLM fallback) that required a curated `entities` vocab table. That curation burden is the v1 design's weakest point — a maintenance tax that compounds with corpus growth.

GLiNER-Relex's joint zero-shot NER+RE encoder (`knowledgator/gliner-relex-multi-v1.0`, Apache 2.0 on both code and weights, ~1.3 GB resident, CPU p50 ~157 ms, AGPL-compatible) collapses the v1 hybrid's two layers into one fast path with no vocab curation. The worker landed via PR #88 in idle-timeout warm-keep posture. This slice consumes it.

## Scope

**In scope (this slice):**

- New module [`core::entity_extraction`](../../../core/src/entity_extraction/) — trait, types, NoOpEntityExtractor, StaticEntityExtractor (test).
- New module [`core::entity_extraction::gliner_relex`](../../../core/src/entity_extraction/gliner_relex.rs) — production `GlinerRelexExtractor` impl.
- New typed `Client` inside [`core::workers::gliner_relex`](../../../core/src/workers/gliner_relex.rs) — wraps `tool_host::dispatch` for the `extract` method; handles crash classification + RPC-code translation.
- New `RecallBuilder::build_with_seeds(text, &[i64])` required method; default-impl shim on `build(text)`. `PgRecallBuilder::build_with_seeds` plumbs seeds into `RecallParams::with_seeds`.
- `db::memories::graph_search` gains `include_quarantined: bool` param. Production passes `false`; future operator-CLI passes `true` for review.
- New migration [`db/migrations/0015_entity_kinds_and_quarantine.sql`](../../../db/migrations/0015_entity_kinds_and_quarantine.sql) — `entity_kinds` lookup table seeded with default taxonomy; `entities.kind` FK to it (ON DELETE SET DEFAULT → 'undefined'); `entities.quarantine BOOLEAN NOT NULL DEFAULT TRUE`; `entities.name_norm TEXT NOT NULL` (Rust-side NFC+lower+whitespace-collapse, dedup key); partial index on unquarantined rows; replaces `(kind, name)` uniqueness with `(kind, name_norm)`.
- New `db::entity_kinds` module — `list_kinds(pool) -> Vec<String>` with 60s TTL cache.
- `RouterAgent` constructor widened to take `Arc<dyn EntityExtractor>` as its 5th argument; `formulate_plan` runs extraction BEFORE recall; degrades to empty seeds with `tracing::warn!` on failure.
- `FormulationMeta` widened with `graph_seed_entity_ids: Vec<i64>` + `graph_seed_count: u32` + `graph_seed_source: SeedSource`.
- `build_plan_formulate_payload` gains 3 new keys — pure-additive Slice F bump 21/22 → **24/25**.
- New `scheduler::audit::ACTION_EXTRACT_ENTITIES = "extract_entities"` const + `build_extract_entities_payload(...)` helper.
- `core/src/main.rs` constructs the Client + Extractor + threads them into `RouterAgent::new`.
- Unit + integration + audit-pin updates.

**Out of scope (filed as follow-ups; full list in §11):**

- Operator maintenance UI / CLI (`hhagent-cli entities review`).
- Memory-write-time `memory_entities` auto-linker.
- `entities.embedding` population.
- Relation-label vocabulary (v2 ships `relation_labels = vec![]`).
- Per-task entity-seed cache.
- Cross-platform macOS validation.
- Per-extract entity-count bulk-operation safety cap.

## Architecture

```
core/src/entity_extraction/
├── mod.rs            — EntityExtractor trait, EntitySeeds, SeedSource,
│                       EntityExtractionError, NoOpEntityExtractor,
│                       StaticEntityExtractor (test), normalize_entity_name
└── gliner_relex.rs   — GlinerRelexExtractor (production impl),
                        chunk_text, merge_chunks,
                        upsert_entities_and_relations,
                        emit_extract_entities_audit

core/src/workers/gliner_relex.rs  — UNCHANGED PUBLIC SURFACE
                                  + NEW: Client { lifecycle, pool, entry, tool_name }
                                          .extract(req) -> Result<ExtractResponse, ClientError>
                                  + NEW: ClientError enum

db/src/entity_kinds.rs            — list_kinds(pool) with 60s TTL cache
db/src/memories.rs                — graph_search gains include_quarantined param
db/migrations/0015_entity_kinds_and_quarantine.sql
```

**Single `Arc<dyn WorkerLifecycleManager>` shared between dispatcher and Client.** The lifecycle Arc is created once at daemon startup, threaded into both `ToolHostStepDispatcher` and the extractor's `Client`. The same warm slot serves both the extractor's calls and (any future) `PlannedStep`-routed shell-style invocations of `gliner-relex`.

**`ToolEntry` cloned into the Client.** Both registry and Client hold an independent `ToolEntry` for the worker. They serve different consumers (registry: dispatch-by-tool-name from `PlannedStep`; Client: direct typed call from extractor). The duplication is intentional — neither side needs to know about the other.

## Schema migration

[`db/migrations/0015_entity_kinds_and_quarantine.sql`](../../../db/migrations/0015_entity_kinds_and_quarantine.sql):

```sql
-- 0015_entity_kinds_and_quarantine.sql
--
-- Pre-reqs: 0001 (entities/relations baseline).
-- Post-state: entity_kinds lookup is the source of truth for which
-- `kind` values exist; entities.quarantine controls graph-lane visibility;
-- entities.name_norm is the dedup key.

BEGIN;

-- (1) Lookup table for valid entity kinds.
CREATE TABLE entity_kinds (
    kind        TEXT        PRIMARY KEY,
    description TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- (2) Seed taxonomy.
--
--     `undefined` is the FK fallback for ON DELETE SET DEFAULT and
--     must never be removed by operator action — the maintenance UI
--     should refuse to delete it.
--
INSERT INTO entity_kinds (kind, description) VALUES
    ('undefined',     'Fallback kind when the original was removed (DO NOT DELETE)'),
    ('person',        'A specific named individual'),
    ('patient',       'A clinical-context individual receiving care'),
    ('doctor',        'A medical practitioner'),
    ('nurse',         'A nursing practitioner'),
    ('organization',  'A named institution or organisation'),
    ('place',         'A geographic or physical location'),
    ('address',       'A postal or street address'),
    ('phone number',  'A telephone number'),
    ('identifier',    'A reference identifier (case number, patient id, ticket id, etc.)'),
    ('drug',          'A medication, pharmaceutical agent, or substance'),
    ('treatment',     'A procedure, intervention, or therapy'),
    ('disease',       'A diagnosis, disorder, or medical condition'),
    ('infection',     'A specific infectious disease or pathogen'),
    ('symptom',       'A clinical sign or complaint'),
    ('system',        'A software system, service, or technical component'),
    ('file',          'A file, document, or path'),
    ('object',        'A physical or virtual object (device, vehicle, artefact)'),
    ('concept',       'An abstract concept, topic, or idea'),
    ('date',          'A calendar date or time reference');

-- (3) Backfill any pre-existing entities.kind values.
INSERT INTO entity_kinds (kind)
SELECT DISTINCT kind FROM entities
ON CONFLICT (kind) DO NOTHING;

-- (4) Default + FK from entities.kind.
ALTER TABLE entities ALTER COLUMN kind SET DEFAULT 'undefined';

ALTER TABLE entities
    ADD CONSTRAINT entities_kind_fk
    FOREIGN KEY (kind) REFERENCES entity_kinds(kind)
    ON UPDATE CASCADE
    ON DELETE SET DEFAULT;

-- (5) Quarantine flag.
ALTER TABLE entities
    ADD COLUMN quarantine BOOLEAN NOT NULL DEFAULT TRUE;

-- (6) Normalized name column for case/whitespace-insensitive dedup.
--     Backfill via SQL is best-effort for ASCII; the Rust normalize
--     is the source of truth going forward. `entities` is empty in
--     production today so the backfill is a no-op in practice.
ALTER TABLE entities ADD COLUMN name_norm TEXT;
UPDATE entities SET name_norm =
    lower(regexp_replace(trim(name), '\s+', ' ', 'g'));
ALTER TABLE entities ALTER COLUMN name_norm SET NOT NULL;

ALTER TABLE entities DROP CONSTRAINT entities_kind_name_key;
CREATE UNIQUE INDEX entities_kind_name_norm_idx
    ON entities (kind, name_norm);

-- (7) Partial index for the production hot path.
CREATE INDEX entities_unquarantined_idx
    ON entities (kind, name)
    WHERE quarantine = FALSE;

-- (8) GRANT shape. Runtime role needs SELECT on entity_kinds for the
--     extractor's startup label-list resolution. INSERT on entity_kinds
--     is operator-only by GRANT default — adding a kind is a deliberate
--     act, not something the agent or extractor does.
GRANT SELECT ON entity_kinds TO hhagent_runtime;

COMMIT;
```

### Migration invariants

- `entity_kinds.kind = 'undefined'` is load-bearing: it's the FK ON DELETE SET DEFAULT fallback. Maintenance UI/CLI must refuse to delete it.
- `entities.name_norm` is populated by Rust-side `normalize_entity_name`. The SQL backfill in step (6) uses Postgres `lower()` + `regexp_replace`, which won't match the Rust normalize byte-for-byte on Unicode-heavy strings (no NFC). For the empty `entities` table in production today this is fine; the Rust path becomes source of truth going forward.
- `entities.quarantine DEFAULT TRUE` is load-bearing for the operator-curation contract: every entity born by extraction is invisible to graph search until operator action promotes it.
- `relations` table is unchanged. The pre-existing `ON DELETE CASCADE` on both `src_id` and `dst_id` FKs gives the user-stated "preserve relations until entities are deleted" behaviour for free. Relations between two quarantined entities are persisted at storage but invisible to `graph_search` (filtered out by the entity JOIN); operator un-quarantining both endpoints surfaces the existing relation row instantly with no re-extraction.

## Normalization

Rust-side canonicalization on every upsert:

```rust
/// Canonical form for entity-name dedup. Done on the Rust side so the
/// normalization is the same on every host and PostgreSQL doesn't need
/// a locale-sensitive `lower()` call.
///
/// Pipeline:
///   1. Unicode NFC composition (`café` == `cafe\u{0301}`)
///   2. ASCII/Unicode lowercase (`Smith` == `SMITH` == `smith`)
///   3. Whitespace-run collapse to a single space + edge trim
///
/// NOT done: punctuation stripping (would conflate `U.S.` and `US`,
/// `Dr.` and `Dr`). If observation phase shows a need for a
/// punctuation-stripping variant, lift to a `NormalizationPolicy`
/// trait — out of scope for v2.
pub(crate) fn normalize_entity_name(name: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    name.nfc()
        .collect::<String>()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
```

New dependency: `unicode-normalization` (Apache-2.0/MIT, AGPL-compatible, ~80 KiB compiled). Added to `core/Cargo.toml`.

Relations: predicate (`relations.kind`) is lowercased + trimmed before INSERT. The labels we pass to GLiNER are controlled by us, so the output `relation` field is already canonical in practice — the lowercase is defensive.

## Core types

```rust
// core::entity_extraction::mod

#[async_trait::async_trait]
pub trait EntityExtractor: Send + Sync {
    async fn extract(
        &self,
        query_text: &str,
    ) -> Result<EntitySeeds, EntityExtractionError>;
}

pub struct EntitySeeds {
    pub ids: Vec<i64>,
    pub source: SeedSource,
    /// Model version used (e.g. "multi-v1.0"). Populated on
    /// non-degraded extractions; goes into the audit row only.
    pub model_version: Option<String>,
}

/// v2 collapses v1's three-variant enum (Deterministic/Llm/None) to
/// two — the only production source is the worker.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeedSource {
    GlinerRelex,
    None,
}

#[derive(Debug, thiserror::Error)]
pub enum EntityExtractionError {
    #[error("db error: {0}")]
    Db(#[from] hhagent_db::DbError),
    #[error("client error: {0}")]
    Client(String),
}

/// Used when gliner-relex isn't configured. Single startup WARN is
/// the only operator signal; returns empty seeds + no audit row.
pub struct NoOpEntityExtractor;

#[async_trait::async_trait]
impl EntityExtractor for NoOpEntityExtractor {
    async fn extract(&self, _: &str)
        -> Result<EntitySeeds, EntityExtractionError>
    {
        Ok(EntitySeeds {
            ids: Vec::new(),
            source: SeedSource::None,
            model_version: None,
        })
    }
}

/// Test impl — returns operator-scripted seeds.
pub struct StaticEntityExtractor { /* ... */ }
```

```rust
// core::workers::gliner_relex (additions only)

pub struct Client {
    lifecycle: Arc<dyn WorkerLifecycleManager>,
    pool: PgPool,
    entry: ToolEntry,
    tool_name: &'static str,    // "gliner-relex"
}

impl Client {
    pub fn new(
        lifecycle: Arc<dyn WorkerLifecycleManager>,
        pool: PgPool,
        entry: ToolEntry,
    ) -> Self { /* ... */ }

    pub async fn extract(&self, req: ExtractRequest)
        -> Result<ExtractResponse, ClientError>
    { /* ... — see Data flow §6 */ }
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("encode error: {0}")] EncodeError(String),
    #[error("worker spawn failed: {0}")] WorkerSpawnFailed(String),
    #[error("worker dead mid-call: {0}")] WorkerDead(String),
    #[error("rpc error code={code}: {message}")]
    RpcError { code: i32, message: String },
    #[error("decode error: {0}")] DecodeError(String),
}
```

```rust
// core::entity_extraction::gliner_relex

pub struct GlinerRelexExtractor {
    client: Client,
    pool: PgPool,
    kinds_cache: Arc<RwLock<KindsCache>>,
    relation_labels: Vec<String>,     // empty in v2
}

pub(crate) const CHUNK_SIZE_BYTES: usize = 7500;
pub(crate) const OVERLAP_BYTES: usize = 500;
pub(crate) const DEFAULT_THRESHOLD: f32 = 0.5;
pub(crate) const DEFAULT_RELATION_THRESHOLD: f32 = 0.5;

impl GlinerRelexExtractor {
    pub fn new(client: Client, pool: PgPool) -> Self { /* ... */ }
    pub fn with_relation_labels(mut self, labels: Vec<String>) -> Self { /* ... */ }
}

#[async_trait::async_trait]
impl EntityExtractor for GlinerRelexExtractor {
    async fn extract(&self, query_text: &str)
        -> Result<EntitySeeds, EntityExtractionError>
    { /* ... — see Data flow §6 */ }
}
```

## Data flow (one extractor.extract call)

```
RouterAgent::formulate_plan
  │
  ├─ 1. extractor.extract(&ctx.instruction).await
  │    │
  │    ├─ chunks = chunk_text(text, CHUNK_SIZE_BYTES, OVERLAP_BYTES)
  │    │     // UTF-8 char-safe split with overlap; under-cap → 1 chunk
  │    │
  │    ├─ labels = kinds_cache.list_kinds(&self.pool).await?
  │    │     // 60s TTL cache; SELECT kind FROM entity_kinds
  │    │
  │    ├─ for chunk in &chunks:                 // sequential — same warm worker
  │    │     req = ExtractRequest {
  │    │         text: chunk.text.clone(),
  │    │         entity_labels: labels.clone(),
  │    │         relation_labels: self.relation_labels.clone(),  // [] in v2
  │    │         threshold: Some(DEFAULT_THRESHOLD),             // 0.5
  │    │         relation_threshold: Some(DEFAULT_RELATION_THRESHOLD),
  │    │         max_entities: None,
  │    │     };
  │    │     match self.client.extract(req).await:
  │    │       Ok(resp) → push (chunk.byte_offset, resp)
  │    │       Err(e)   → tracing::warn!(error=%e, "client.extract failed; degrading chunk");
  │    │                  continue  (one chunk's failure doesn't kill the whole extract)
  │    │
  │    ├─ if chunk_responses.is_empty():
  │    │     return Ok(EntitySeeds { ids: vec![], source: None, model_version: None });
  │    │
  │    ├─ merged = merge_chunks(chunk_responses)
  │    │     // dedup entities by (kind, normalize_entity_name(text)) — first-wins
  │    │     // dedup triples by (head_norm, tail_norm, relation_norm)
  │    │     // entity offsets re-anchored to original-text byte position
  │    │
  │    ├─ (entity_ids, n_relations_inserted) =
  │    │     upsert_entities_and_relations(&self.pool, &merged).await?
  │    │   // For each entity (in batch):
  │    │   //   INSERT INTO entities (kind, name, name_norm, quarantine)
  │    │   //     VALUES ($1, $2, $3, TRUE)
  │    │   //     ON CONFLICT (kind, name_norm) DO NOTHING
  │    │   //     RETURNING id;
  │    │   //   Then SELECT id FROM entities WHERE (kind, name_norm) = ANY (...)
  │    │   //   for any rows the upsert didn't RETURN (existing rows).
  │    │   // For each triple:
  │    │   //   Resolve head_id, tail_id from the upsert results.
  │    │   //   INSERT INTO relations (src_id, dst_id, kind, attrs)
  │    │   //     SELECT $1, $2, $3, '{}'::jsonb
  │    │   //     WHERE NOT EXISTS (
  │    │   //       SELECT 1 FROM relations WHERE src_id=$1 AND dst_id=$2 AND kind=$3
  │    │   //     );
  │    │   //   (application-layer idempotency — schema allows multi-edges
  │    │   //    intentionally, but the extractor doesn't add them.)
  │    │
  │    ├─ emit_extract_entities_audit(...)
  │    │   // actor='extractor:gliner-relex' action='extract_entities'
  │    │   // payload (8 keys, BTreeSet-pinned):
  │    │   //   {n_chars_in, n_chunks, n_entities_out, n_triples_out,
  │    │   //    n_entities_upserted_new, n_relations_inserted,
  │    │   //    model_version, latency_ms_total}
  │    │   // Best-effort INSERT — DB failure WARNs but doesn't propagate.
  │    │
  │    └─ return EntitySeeds { ids: entity_ids, source: GlinerRelex,
  │                             model_version: Some("multi-v1.0") }
  │
  ├─ 2. recalled = recall_builder.build_with_seeds(&ctx.instruction, &seeds.ids).await
  │     // RecallParams::with_seeds when non-empty; falls through to
  │     // RecallParams::new for empty. graph_search internally passes
  │     // include_quarantined=false, so newly-extracted (quarantined)
  │     // entities don't yet contribute to graph results.
  │
  └─ 3. (Prompt assembly + LLM call unchanged.)
```

### Client.extract details

```rust
pub async fn extract(&self, req: ExtractRequest)
    -> Result<ExtractResponse, ClientError>
{
    let req_value = serde_json::to_value(&req)
        .map_err(|e| ClientError::EncodeError(e.to_string()))?;

    let mut handle = self.lifecycle
        .acquire(self.tool_name, &self.entry)
        .await
        .map_err(|e| ClientError::WorkerSpawnFailed(e.to_string()))?;

    let result = tool_host::dispatch(
        &self.pool,
        handle.worker_mut(),
        self.tool_name,
        "extract",
        req_value,
    ).await;

    // Crash classification — the same chokepoint the step dispatcher uses.
    if crate::worker_lifecycle::idle_timeout::dispatch_indicates_worker_dead(&result) {
        handle.report_crash();
    }

    match result {
        Ok(v) => serde_json::from_value::<ExtractResponse>(v)
            .map_err(|e| ClientError::DecodeError(e.to_string())),
        Err(ToolHostError::Protocol(ClientErrorProtocol::Rpc { code, message, .. })) =>
            Err(ClientError::RpcError { code, message }),
        Err(e) => Err(ClientError::WorkerDead(e.to_string())),
    }
}
```

The `Client::extract` is a pure wrapper around the chokepoint — it adds no audit row of its own (the dispatch row is automatic), and it doesn't decide what counts as "fatal" beyond delegating to the established `dispatch_indicates_worker_dead` classifier. Crash classification happens before the error is mapped, so `report_crash` runs once for every call where the worker is unusable.

### Daemon startup wiring (`core/src/main.rs`)

```rust
// Lifecycle Arc — created once, shared between dispatcher and Client.
let lifecycle: Arc<dyn WorkerLifecycleManager> =
    Arc::new(CompositeLifecycle::new(sandbox.clone()));

// Build tool registry (existing flow).
let mut registry = build_tool_registry(&pool).await?;

// Construct the extractor — same `lifecycle` Arc, same `ToolEntry` shape
// as the one registered for dispatch. The entry is built once via
// `build_gliner_relex_entry()` (existing helper from PR #88) and used
// in both places.
let extractor: Arc<dyn EntityExtractor> = match build_gliner_relex_entry() {
    Some(entry) => {
        // Insert into registry (so PlannedStep-based callers still work).
        registry.insert(&entry);
        // Construct Client + Extractor.
        let client = workers::gliner_relex::Client::new(
            lifecycle.clone(),
            pool.clone(),
            entry,
        );
        Arc::new(GlinerRelexExtractor::new(client, pool.clone()))
    }
    None => {
        tracing::warn!(
            "gliner-relex worker not configured \
             (HHAGENT_GLINER_RELEX_ENABLE=0 or preconditions failed); \
             using NoOpEntityExtractor — graph lane will return empty results"
        );
        Arc::new(NoOpEntityExtractor::new())
    }
};

// RouterAgent constructor widened — 5th arg.
let router_agent = RouterAgent::new(
    router.clone(),
    recall_builder.clone(),
    prompt_builder.clone(),
    /* … */,
    extractor.clone(),
);
```

## `RecallBuilder` widening

```rust
#[async_trait::async_trait]
pub trait RecallBuilder: Send + Sync {
    async fn build_with_seeds(
        &self,
        query_text: &str,
        seeds: &[i64],
    ) -> Result<RecalledContext, RecallError>;

    /// Default-impl shim. Existing call sites keep compiling; production
    /// always goes through build_with_seeds.
    async fn build(&self, query_text: &str)
        -> Result<RecalledContext, RecallError>
    {
        self.build_with_seeds(query_text, &[]).await
    }
}
```

`PgRecallBuilder::build_with_seeds`:

- `seeds.is_empty()` → use `RecallParams::new(text, embedding)` (graph lane off — preserves today's behaviour exactly).
- `seeds.is_non_empty()` → use `RecallParams::with_seeds(text, embedding, seeds)` (graph lane active).
- The graph lane inside calls `db::memories::graph_search(pool, seeds, GRAPH_FANOUT_CAP_PER_SEED, /* include_quarantined */ false)`.

`StaticRecallBuilder::build_with_seeds` is the new required test impl. Test fixtures need updating to construct via this method.

## `graph_search` widening

```rust
pub async fn graph_search(
    pool: &PgPool,
    seed_entity_ids: &[i64],
    fanout_cap_per_seed: i64,
    include_quarantined: bool,    // NEW
) -> Result<Vec<MemoryHit>, DbError> {
    // SQL JOINs entities for the quarantine filter:
    //   SELECT me.memory_id, COUNT(*) AS hit_count
    //   FROM memory_entities me
    //   JOIN entities e ON me.entity_id = e.id
    //   WHERE me.entity_id = ANY($1)
    //     AND (NOT $2 OR e.quarantine = FALSE)
    //   GROUP BY me.memory_id
    //   ORDER BY hit_count DESC, me.memory_id ASC
    //   LIMIT $3;
    //
    // Note: $2 is the negated `include_quarantined` flag — when
    // include_quarantined is TRUE, the predicate short-circuits.
    // When FALSE (production), only unquarantined entities contribute.
}
```

Existing callers (`core::memory::recall::recall`) gain a hardcoded `false` for `include_quarantined`. The flag exists for the future operator-CLI / maintenance-UI consumer.

## `RouterAgent::formulate_plan`

```rust
async fn formulate_plan(&self, ctx: &TaskContext)
    -> Result<(Plan, FormulationMeta), AgentError>
{
    // 1. Entity extraction.
    let seeds = match self.entity_extractor.extract(&ctx.instruction).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e,
                "entity extraction failed; continuing with empty seeds");
            EntitySeeds { ids: Vec::new(), source: SeedSource::None, model_version: None }
        }
    };

    // 2. Per-iteration recall, now seeded.
    let recalled = match self.recall_builder
        .build_with_seeds(&ctx.instruction, &seeds.ids).await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "recall failed; continuing with empty recall context");
            RecalledContext::empty()
        }
    };

    // 3. Prompt assembly (fail-closed — unchanged).
    let assembled = self.prompt_builder
        .build_with_recalled(&base, &recalled).await
        .map_err(AgentError::PromptAssembly)?;

    // 4. LLM call (unchanged).
    // …

    // 5. FormulationMeta carries the 3 new keys.
    let meta = FormulationMeta {
        // … existing fields …
        graph_seed_entity_ids: seeds.ids.clone(),
        graph_seed_count: seeds.ids.len() as u32,
        graph_seed_source: seeds.source,
    };
    Ok((plan, meta))
}
```

Two degrade paths (extraction, recall); one fail-closed step (prompt assembly). Matches the v1 spec discipline.

## Audit-row contract

| Actor | Action | Payload keys | Cardinality |
|---|---|---|---|
| `tool:gliner-relex` | `extract` | `{req, result, ms}` — automatic via `tool_host::dispatch` | One row PER CHUNK per extractor.extract call (1 for under-cap inputs; N for chunked inputs). |
| `extractor:gliner-relex` | `extract_entities` | `{n_chars_in, n_chunks, n_entities_out, n_triples_out, n_entities_upserted_new, n_relations_inserted, model_version, latency_ms_total}` (8 keys, BTreeSet-pinned) | One row PER extractor.extract call. Suppressed on degrade-to-empty (no chunks succeeded). |
| `agent` | `plan.formulate` | 21/22 → **24/25** keys (pure-additive, Slice F) | Unchanged cardinality (one row per formulate_plan call). |

**Wire-stable serde tags** for `SeedSource`: `"gliner_relex"` and `"none"` (snake_case). JSONB queries on observation captures filter via `WHERE payload->>'graph_seed_source' = 'gliner_relex'` to find every plan iteration where extraction succeeded.

**Why two rows per extract (vs one).** The dispatch row carries the full GLiNER response — operator can replay any individual call. The extractor-summary row is the compact JSONB-queryable signal for SLO dashboards (`SELECT AVG((payload->>'latency_ms_total')::int) FROM audit_log WHERE actor='extractor:gliner-relex'`). Storage cost is negligible (one extra small row per extract).

## Error taxonomy and degrade paths

| Failure | Origin | Effect on extraction | Effect on daemon |
|---|---|---|---|
| `HHAGENT_GLINER_RELEX_ENABLE=0` (default) or weights missing | startup | NoOpEntityExtractor returns empty seeds; no extraction audit row | Daemon starts; one WARN line at startup. |
| `Client::extract` fails on first chunk | runtime | Loop continues to next chunk; if all chunks fail, extractor returns empty seeds with WARN | Daemon stays up; recall degrades to semantic + lexical only. |
| Worker crash mid-chunk (Io / EarlyExit / Decode / IdMismatch) | runtime | `handle.report_crash()` runs (next acquire cold-spawns); current chunk's extraction degrades to empty | Daemon stays up; worker auto-recovers via lifecycle. |
| `INVALID_INPUT` (-32001) RPC error | runtime | Bug in extractor (sent bad data — should never happen given chunking + label normalization). WARN; chunk degrades. | Daemon stays up; surfaces in `extract_entities` audit row's `n_entities_out=0`. |
| `INFERENCE_FAILED` (-32003) RPC error | runtime | Transient model error. WARN; chunk degrades. | Daemon stays up. |
| `MODEL_LOAD_FAILED` (-32002) / `UNSUPPORTED_DEVICE` (-32604) | runtime | Worker exits during startup; subsequent `Client::extract` calls fail via `WorkerSpawnFailed`. Each call WARNs; extraction degrades. | Daemon stays up; operator alert via WARN log. |
| `upsert_entities_and_relations` DB error | runtime | Propagated as `EntityExtractionError::Db`; `formulate_plan` catches and degrades. | Daemon stays up. |
| `emit_extract_entities_audit` DB error | runtime | WARN only — does not propagate. | Daemon stays up. |
| `entity_kinds` cache refresh fails | runtime | Propagated as `EntityExtractionError::Db`; `formulate_plan` catches and degrades. | Daemon stays up. |

## Test budget

Workspace 786 → ~825-835 (+39-49).

| Tier | Count | What's pinned |
|---|---|---|
| `entity_extraction::tests` (unit) | ~8 | `SeedSource` snake_case serde + 2-variant exhaustiveness; `NoOpEntityExtractor` returns `SeedSource::None` + empty ids + `model_version = None`; `EntityExtractionError` Display/Debug pins; `normalize_entity_name` (case, NFC, whitespace collapse, trim, punctuation NOT stripped) |
| `entity_extraction::gliner_relex::tests` (unit) | ~10 | `chunk_text` (under-cap = single chunk; exactly-cap; over-cap with overlap; UTF-8 boundary safety pins); `merge_chunks` (dedup by `(kind, name_norm)` for entities, `(head_norm, tail_norm, relation_norm)` for triples; offset re-anchoring across chunk boundaries); `upsert_entities_and_relations` pure-helper portions; `emit_extract_entities_audit` payload shape (8-key BTreeSet pin) |
| `workers::gliner_relex::client::tests` (unit) | ~8 | `Client::extract` error classification across all five outcome buckets (encode/decode/spawn-failed/dead/rpc); `ClientError` Display/Debug pins; trait-mock-based crash-then-recovery (next call cold-spawns); `dispatch_indicates_worker_dead` integration via the mock |
| `db::entity_kinds::tests` (unit) | ~5 | `list_kinds` SQL shape pin; cache TTL pin; `entity_kinds` PRIMARY KEY shape |
| `db::memories::tests` (unit) | +2 | `graph_search` SQL build with `include_quarantined` flag toggled |
| `db/tests/postgres_e2e.rs` (integration) | +5 | `migration_0015_seeds_entity_kinds_and_adds_quarantine` (full seed list present, FK live, partial + unique indexes built); `entities_upsert_dedup_by_name_norm` (Smith/SMITH/smith → one row; original case preserved); `kind_delete_sets_default_to_undefined` (ON DELETE SET DEFAULT exercised); `relation_persists_when_endpoints_quarantined` (insert + verify row exists; `graph_search(include_quarantined=false)` filters; `=true` surfaces); `entities_kind_fk_blocks_unknown_kind` |
| `core/tests/entity_extraction_e2e.rs` (integration, new) | ~6 | Real-model end-to-end against live `multi-v1.0` weights (skip-as-pass without venv+weights+bwrap+PG): happy-path extract returns non-empty seeds matching upserted ids; degrade-on-worker-disabled (NoOpEntityExtractor + WARN line); quarantined-by-default (all extracted entities ship with `quarantine = TRUE`); chunking pin (input > 8192 bytes → multiple chunks, merged result contains entities from both halves); two-row audit shape verified in DB; idempotent re-extraction of same text → no new entity rows, no new relation rows |
| `core/tests/scheduler_inner_loop_e2e.rs` (in-place) | (in-place) | `plan.formulate` payload carries `graph_seed_entity_ids` / `graph_seed_count` / `graph_seed_source` (= `"gliner_relex"` or `"none"`) |
| `core/tests/cli_ask_e2e.rs` (in-place) | (in-place) | Audit multiset unchanged (cli_ask_e2e uses `NoOpEntityExtractor`); `graph_seed_source = "none"` per iteration |

Skip-as-pass posture for real-model tests follows the established `gliner_relex_e2e.rs` pattern: skip cleanly when venv / weights / PG / bwrap absent; run for real when all present (DGX Spark + operator-prepared host).

## Scope-out — follow-up surfaces

- **Operator maintenance UI / CLI for quarantine review.** `hhagent-cli entities review` lists quarantined entities, supports `unquarantine` / `delete` / `merge` actions. User-stated as "yet to be designed" — separate slice.
- **Memory-write-time `memory_entities` auto-linker.** v2 ships READ-side wiring only. Without a write-side hook (in L0 seed / L1 promote / future writers) that calls the same extractor and inserts `memory_entities` rows, the graph lane still returns zero hits in production. Follow-up slice.
- **`entities.embedding` population.** Column stays NULL. Embedding-similarity entity matching is a separate slice (would let recall surface graph seeds via cosine similarity to memory embeddings, no GLiNER call needed).
- **Relation-label vocabulary.** v2 ships `relation_labels = vec![]` (entities-only mode). GLiNER pays the relation-inference cost regardless. Future slice: a `relation_kinds` lookup table (symmetric to `entity_kinds`) + plumbing through `relation_labels` parameter + triple-upsert via the same `upsert_entities_and_relations` helper. Migration would mirror `0015`.
- **Per-task entity-seed cache.** Each plan iteration extracts from the same `ctx.instruction`. A small `RwLock<HashMap<task_id, EntitySeeds>>` would amortise the cost (~471 ms saved per task at 3 iterations). Acceptable to skip for v2; revisit if observation phase shows it hurts.
- **macOS posture.** v2 extractor compiles on macOS but the worker manifest skips registration without a configured venv. The macOS MPS spike completed 2026-05-18 (Apple M3 Max; see ROADMAP entry) and answered all three open questions: `model.to("mps")` works, `PYTORCH_ENABLE_MPS_FALLBACK=1` not required, output byte-equivalent to CPU. **Crucial latency-inversion finding:** MPS wins on short input (28 ms vs CPU 42 ms) but **loses ~5× on a realistic 600-char paragraph** (432 ms MPS vs 82 ms CPU) because the candidate-span batch shape scales with `text_tokens × n_entity_labels × n_relation_labels` and the per-kernel-launch overhead stops amortising. **Implication for v2 macOS:** default `auto` on darwin should resolve to `cpu`, not `mps`. The follow-up macOS slice (Python `mps` branch + Rust manifest cross-platform variant) is the prerequisite for first-class macOS support; lives on its own ROADMAP line.
- **Configurable normalize function.** `normalize_entity_name` is a const-default behaviour. If observation phase shows a punctuation-stripping or stop-word variant is needed, lift to a `NormalizationPolicy` trait. YAGNI for v2.
- **Quarantine bulk-operation safety cap.** No `entities` row count limit on `extractor.extract` — a pathological input could try to upsert thousands of distinct entities. Mitigation: GLiNER's `MAX_ENTITY_LABELS = 64` and the chunk size bound the upper limit, but a per-extract cap (e.g., 256 entities × 8 chunks = 2048 upserts max) could be added defensively. Filed for observation phase.

## Risk surface

- **Empty `entity_kinds` table → extractor returns no entities.** Migration `0015` seeds 20 default kinds, so the cold-start state is non-empty. If an operator deletes all kinds (or removes 'undefined'), the extractor degrades to empty seeds + WARN. Mitigation: maintenance UI must enforce 'undefined' invariant.
- **`entity_kinds` cache staleness.** 60s TTL means operator additions take up to a minute to propagate. Acceptable for human-paced operator cadence. Mitigation if needed: explicit `invalidate()` API on the cache, called from a future operator-CLI write path.
- **Per-chunk worker dispatch is sequential.** A query producing 4 chunks pays ~628 ms wall-clock at 157 ms p50 per chunk. Acceptable for plan-iteration-paced calls (≤ 3 per task). Future optimisation: parallelism within an extractor.extract is bounded by the lifecycle manager's per-tool serialisation (the warm worker can only handle one request at a time); a multi-worker future state would unlock this.
- **Quarantine pollution.** Every extracted entity lands as quarantined; the table grows monotonically until operator review. Acceptable observability data; not a bug. Future bulk-prune slice if the table gets large enough that the partial index doesn't fully amortise it.
- **Normalize collisions on rare inputs.** `normalize_entity_name` produces the same output for `"Dr Smith"` and `"DR SMITH"` — correct. But `"smith"` (no title) ALSO normalizes to `"smith"` distinct from `"dr smith"` — these are operator-resolvable via merge in the future maintenance UI. v2 does NOT auto-merge.
- **Backfill of pre-existing entities.** Step (6) of the migration runs a SQL backfill of `name_norm` that won't match Rust normalize byte-for-byte on Unicode-heavy inputs. The `entities` table is empty in production today so this is a no-op in practice; a fresh-install scenario doesn't hit it. If a future re-install onto an existing corpus needs perfect match, a one-shot Rust-side rewrite script is the fix.
- **`relations` table multi-edges.** Schema allows multi-edges intentionally (per the 0001 comment); v2's application-layer dedup ensures the extractor doesn't add duplicates. A future memory-write-time linker that wants multi-edges (one per source memory) can ignore the WHERE NOT EXISTS pattern.

## Open questions for the implementer

None blocking. Design commits on:

- Storage: `entity_kinds` lookup + `entities.quarantine` (DEFAULT TRUE) + `entities.name_norm` (Rust-side NFC+lower+whitespace-collapse, dedup key) + FK from `entities.kind` to `entity_kinds.kind` with ON DELETE SET DEFAULT 'undefined'.
- Client lives in `core::workers::gliner_relex`; same `Arc<dyn WorkerLifecycleManager>` shared with the step dispatcher; `ToolEntry` cloned into the Client at startup.
- `SeedSource { GlinerRelex, None }` — collapsed from v1's three-variant enum.
- Sliding-window chunking with `CHUNK_SIZE_BYTES = 7500`, `OVERLAP_BYTES = 500`.
- Default thresholds 0.5 for both entities and relations (per spike correction #3).
- `relation_labels = vec![]` in v2 (entities-only mode); triples-capture follow-up slice picks the vocabulary.
- Two audit rows per extract (`tool:gliner-relex/extract` × N chunks + `extractor:gliner-relex/extract_entities` × 1).
- Degrade-and-warn on every failure mode; `NoOpEntityExtractor` when gliner-relex isn't configured.

If any of these turn out wrong during implementation, file inline and update the spec in the same fixup commit.

## Self-review checklist (done before commit)

- [x] No placeholders / TBD / TODO in body text.
- [x] Module structure cross-checked against v1 spec + current tree (`core::entity_extraction` and `core::workers::gliner_relex` are real paths).
- [x] Audit-row payload key counts cross-checked against the L1 slice's 21/22 + 3 = 24/25 bump (Slice F naming preserved).
- [x] `EntitySeeds` field set is bounded and serializable.
- [x] Graph-lane data flow (extract → upsert → dedup → with_seeds → recall → graph_search filter) is fully traced.
- [x] Quarantine semantics: extracted entities born quarantined; relations preserved at storage; graph_search filters via entity JOIN; operator un-quarantining surfaces existing relations with no re-extraction.
- [x] Failure modes are all degrade-and-warn (no fail-closed paths added to `formulate_plan`).
- [x] Scope is one session (estimated ~16 tasks per the L1 / Slice 2 precedent's sizing).
- [x] All deferred items have explicit follow-up surfaces.
- [x] Normalization rationale documented (NFC + lower + whitespace; punctuation NOT stripped; reasoning given).
- [x] Migration invariants documented (load-bearing 'undefined' kind; SQL-vs-Rust normalize divergence on Unicode-heavy strings — acceptable for empty table).
- [x] Cross-references back to the relevant predecessors (PR #41 graph lane, PR #29 embed, PR #79 recall builder, PR #82 L1 promote precedent, PR #83 worker lifecycle, PR #88 GLiNER-Relex worker).
