# Entity extraction + graph-lane wiring (read-side) — design

**Date:** 2026-05-18
**Status:** Design, ready for plan.
**Branch (proposed):** `feat/entity-extraction-graph-lane`
**Scope class:** Read-side infrastructure. The slice ships the extractor + recall plumbing; the graph lane stays a no-op in production until follow-up slices populate `entities` (vocab seeder) + `memory_entities` (memory-write-time linker). The infrastructure is testable in isolation and the audit-row contract is observable from day one.
**Pre-reqs (all shipped):**
- PR #41 (memory graph lane + `memory_entities` join table, 2026-05-12) — `Graph` trait, `PgGraph`, `RecallParams::with_seeds`, `RecallModes::graph`, `GRAPH_FANOUT_CAP_PER_SEED = 32`.
- PR #29 (`Router::embed`, Option O, 2026-05-12) — used by `embed_query`; the LLM-fallback path also uses `Router::send`.
- PR #79 (recall-lane wiring, 2026-05-17) — `RecallBuilder` trait + `PgRecallBuilder` (the surface this slice widens).
- `feat/l1-promotion-writer` (this session, awaiting PR + merge) — establishes the audit-row payload-bump precedent (Slice E added `l1_insight` in `build_plan_formulate_payload`); this slice adds Slice F.

## Why now

`core::memory::recall` ships three lanes — semantic, lexical, and graph — fused via Reciprocal Rank Fusion. The semantic and lexical lanes are wired end-to-end in production (PR #79). The graph lane is **a complete no-op**:

- `PgRecallBuilder::build` calls `RecallParams::new(text, embedding)` — never `with_seeds`.
- `seed_entity_ids` is therefore always `None` in production.
- `recall::recall` short-circuits the graph lane with a `tracing::warn!("graph lane requested but seed_entity_ids is empty or None; skipping")` when seeds are None or empty.
- No production code path passes seeds.

Until a query-time extractor resolves entity references and plumbs them into `with_seeds`, the graph lane is dead. This slice closes the **read side** of that wiring. The write side (auto-linking memories to entities at insert time) and the operator side (vocab seeding) are separate slices — both prerequisites for the graph lane to surface non-empty results, but each independently scoped.

The slice's value-add to a fresh-from-checkout daemon today is **zero observable behaviour change** (the graph lane still returns zero because `entities` is empty). The slice's value-add to the system one slice later (when vocab seeding lands) is **automatic graph-lane firing on every query that mentions a known entity**. The infrastructure ships now so the write-side and operator-side slices can ship without re-touching `formulate_plan`.

## Scope

In scope (this slice):

- New module [`core/src/entity_extraction/`](../../../core/src/entity_extraction/) — extractor trait, hybrid impl, telemetry types, mock impl.
- New async `EntityExtractor` trait + `HybridEntityExtractor` production impl (deterministic primary + LLM fallback) + `StaticEntityExtractor` test impl.
- New `EntitySeeds { ids, source, llm_input_sha256 }` value type returned by the extractor.
- New `SeedSource { Deterministic, Llm, None }` enum + telemetry on the extractor's verdict path.
- New `EntityExtractionError` enum (`Db(DbError)`, `Llm(RouterError)`, `Parse(String)`).
- `RecallBuilder` trait widening: `build_with_seeds(text, &[i64])` is the new required method; `build(text)` becomes a thin default-impl shim → `build_with_seeds(text, &[])`. The recall-lane-wiring slice (PR #79) established this default-impl pattern.
- `PgRecallBuilder::build_with_seeds` plumbs seeds into `RecallParams::with_seeds(text, embedding, seeds)` when non-empty; falls through to the existing `RecallParams::new` path when seeds is empty.
- `RouterAgent` constructor widened to take `Arc<dyn EntityExtractor>` as its 5th argument. `formulate_plan` runs extraction BEFORE recall; failure degrades to empty seeds with `tracing::warn!`.
- `core::scheduler::audit` gains `ACTION_EXTRACT_ENTITIES = "extract_entities"` const + a pure helper `build_extract_entities_payload(model, n_chars_in, n_entities_out, backend, latency_ms) -> Value`.
- `build_plan_formulate_payload` gains 3 new keys: `graph_seed_entity_ids` (array), `graph_seed_count` (numeric), `graph_seed_source` (snake-case string tag). **Audit-row bump: 21/22 → 24/25 keys, pure-additive (Slice F).**
- `FormulationMeta` widened with the same 3 fields so the inner-loop test fixtures still construct cleanly.
- `core/src/main.rs` constructs `HybridEntityExtractor::new(pool.clone(), router.clone())` and passes into `RouterAgent::new`.
- Unit tests + DB integration tests + mid-tier audit-pin updates + `cli_ask_e2e` cascade fix for the new LLM-fallback dial count.

Out of scope (filed as follow-ups, listed at the end of this doc):

- **Entity vocabulary seeder.** No TOML / CLI for populating `entities`. Operator works via direct SQL (or waits for the seeder slice). Mirrors the L0 seed loader shape exactly.
- **Memory-write-time entity linking.** No hook into L1 promote / L0 seed / future writers to populate `memory_entities`.
- **`entities.embedding` population.** Column stays NULL. The embedding-similarity extractor (Option C from brainstorming) is a separate slice.
- **Operator CLI for entities.** No `kastellan-cli entities {add, link, list}`. Future slice.
- **Explicit cache invalidation API.** Cache TTL is 60s; explicit invalidation is deferred until the operator CLI lands and has a write event to invalidate on.
- **Per-task agent-raised entity hint channel.** Agent cannot self-declare entities in its Plan. No `Plan.entities_referenced: Vec<EntityRef>` field. Could be a future slice if observation phase shows the system-side extractor under-recalls and the agent has signal the system doesn't.

## Shape decision: why a dedicated `entity_extraction` module

The pure-helper + async-writer + audit shape mirrors `l0_seed`, `recall_assembly`, and `l1_promote`. Cross-reading those four modules at session-end should reveal a single design idiom: "module X has a curated input source (TOML or table), idempotent operations, with a typed audit row per write or extraction." Folding the extractor onto `RecallBuilder` (single trait) is rejected for two reasons:

1. `RecallBuilder` is concerned with **reading** memories. The extractor is concerned with **resolving entity names to ids**, which is a pre-step. The two have different failure surfaces (recall = DB / embed; extractor = DB / NER / LLM) and different test surfaces (recall pins RRF + lane fusion; extractor pins matching + LLM-fallback gating).
2. The extractor's output is consumed by the recall plumbing AS A PARAMETER. `RecallParams::with_seeds` takes `&[i64]`. The extractor returns `Vec<i64>`. Coupling the trait gives the recall builder a second job (it would need to know the entity-extraction policy); separation gives the orchestrator (`RouterAgent::formulate_plan`) the choice.

`core::entity_extraction` is the right home.

## Module shape

```
core/src/entity_extraction/
├── mod.rs         — public surface: EntityExtractor trait,
│                     EntitySeeds / SeedSource / EntityExtractionError types,
│                     StaticEntityExtractor (test impl),
│                     module-level docs + threat-model cross-ref
├── deterministic.rs — DeterministicNameMatcher (cached map + substring match)
├── llm.rs           — LLM-fallback path (meta-prompt, parsing, gating heuristic)
└── hybrid.rs        — HybridEntityExtractor (production impl composing the two)
```

Estimate per file: `mod.rs` ~150 LOC, `deterministic.rs` ~150 LOC, `llm.rs` ~200 LOC, `hybrid.rs` ~150 LOC + inline tests. All comfortably under the 500-LOC soft cap.

## Core types

```rust
/// Telemetry: which extraction path produced the seeds returned to
/// the recall lane. Mirrors `ClassificationFloorSource` (issue #71)
/// — a closed enum with snake_case serde tags so JSONB queries
/// (`payload->>'graph_seed_source'`) stay precise.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeedSource {
    /// Deterministic substring match hit. Cheap path; no LLM call.
    Deterministic,
    /// LLM fallback fired (deterministic produced zero hits + the
    /// gating heuristic was satisfied + the LLM returned a non-empty
    /// parsed result + at least one tuple resolved via Graph::get_entity).
    Llm,
    /// Neither path produced ids. The recall lane proceeds with
    /// semantic + lexical only.
    None,
}

pub struct EntitySeeds {
    pub ids: Vec<i64>,
    pub source: SeedSource,
    /// SHA-256 of the LLM input text iff source == Llm. None
    /// otherwise. Goes into the agent/plan.formulate audit row only
    /// when the LLM path fired, for cross-restart drift detection
    /// of identical-input rerunnability.
    pub llm_input_sha256: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum EntityExtractionError {
    #[error("entity extraction db error: {0}")]
    Db(#[from] DbError),
    #[error("entity extraction LLM error: {0}")]
    Llm(#[from] RouterError),
    #[error("entity extraction parse error: {0}")]
    Parse(String),
}

#[async_trait::async_trait]
pub trait EntityExtractor: Send + Sync {
    async fn extract(
        &self,
        query_text: &str,
    ) -> Result<EntitySeeds, EntityExtractionError>;
}
```

Note: `async-trait` is added to `core/Cargo.toml` if not already present. (Check first — the existing `RecallBuilder` trait uses the async-fn-in-trait stable feature; we can use the same.)

## DeterministicNameMatcher behaviour

```rust
pub struct DeterministicNameMatcher {
    pool: PgPool,
    /// `(name_lower → id)` cache. Refreshed on a 60s TTL via the
    /// `RwLock`'s write path. Read path is hot (every plan iteration).
    cache: Arc<RwLock<DeterministicCache>>,
}

struct DeterministicCache {
    map: HashMap<String, i64>,
    refreshed_at: Instant,
}

impl DeterministicNameMatcher {
    /// Cache TTL. 60s is a balance: short enough that operator edits
    /// of the entities table propagate without explicit invalidation,
    /// long enough that the common case (zero edits between plan
    /// iterations) doesn't refresh on every call.
    const CACHE_TTL: Duration = Duration::from_secs(60);

    async fn match_in_query(&self, query_lower: &str) -> Result<Vec<i64>, DbError> {
        let cache = self.cache_or_refresh().await?;
        let mut hits = Vec::new();
        for (name_lower, id) in cache.map.iter() {
            if query_lower.contains(name_lower.as_str()) {
                hits.push(*id);
            }
        }
        hits.sort_unstable();
        hits.dedup();
        Ok(hits)
    }

    async fn cache_or_refresh(&self) -> Result<Arc<DeterministicCache>, DbError> {
        // Read-lock check first; only acquire the write lock when expired.
        // ... details elided ...
    }
}
```

The cache is empty in v1 (because `entities` is empty), so every query hits the "refresh empty map; iterate empty map; return empty Vec" cold path. Once the vocab seeder lands, the cache populates and the deterministic path starts producing hits.

## HybridEntityExtractor behaviour

```rust
pub struct HybridEntityExtractor {
    deterministic: DeterministicNameMatcher,
    llm: LlmEntityExtractor,  // composes Router + meta-prompt + parser + Graph
    pool: PgPool,
}

impl EntityExtractor for HybridEntityExtractor {
    async fn extract(&self, query_text: &str) -> Result<EntitySeeds, EntityExtractionError> {
        let lower = query_text.to_lowercase();
        let det_hits = self.deterministic.match_in_query(&lower).await?;
        if !det_hits.is_empty() {
            return Ok(EntitySeeds {
                ids: det_hits,
                source: SeedSource::Deterministic,
                llm_input_sha256: None,
            });
        }

        // Gating heuristic: skip LLM for trivial queries.
        if !should_invoke_llm(query_text) {
            return Ok(EntitySeeds {
                ids: Vec::new(),
                source: SeedSource::None,
                llm_input_sha256: None,
            });
        }

        // LLM fallback path.
        match self.llm.extract(query_text, &self.pool).await {
            Ok((llm_ids, sha256)) if !llm_ids.is_empty() => Ok(EntitySeeds {
                ids: llm_ids,
                source: SeedSource::Llm,
                llm_input_sha256: Some(sha256),
            }),
            Ok((_empty, _)) => Ok(EntitySeeds {
                ids: Vec::new(),
                source: SeedSource::None,
                llm_input_sha256: None,
            }),
            Err(e) => {
                tracing::warn!(error = %e, "LLM entity extraction failed; degrading");
                Ok(EntitySeeds {
                    ids: Vec::new(),
                    source: SeedSource::None,
                    llm_input_sha256: None,
                })
            }
        }
    }
}
```

The LLM-fallback path's errors (HTTP, Parse, over-cap) are caught INSIDE `HybridEntityExtractor::extract` and converted to `Ok(EntitySeeds::empty_with_source_none())` with a `tracing::warn!` carrying the underlying error. The LLM path never causes the trait to return `Err`.

The DETERMINISTIC path's `DbError` does propagate out of the trait as `EntityExtractionError::Db`. `formulate_plan` then catches all `EntityExtractionError` variants with one `match` arm and degrades to empty seeds + `tracing::warn!`. So from the agent's perspective there's one degrade path; from the extractor's internals, the LLM and DB error paths converge at different points (LLM inside the trait, DB at the trait boundary). Choosing this split keeps the trait's error surface narrow (one variant = DB) while letting the LLM path do its own logging with model + latency context.

## Gating heuristic for LLM fallback

```rust
fn should_invoke_llm(query_text: &str) -> bool {
    // Skip very short queries — likely commands like "list /tmp"
    // that don't reference entities.
    let token_count = query_text.split_whitespace().count();
    if token_count < 4 {
        return false;
    }
    // Skip queries with no capitalized words — proper-noun candidates
    // are typically capitalized. False negatives on entity names that
    // are all-lowercase are accepted (the deterministic path catches
    // those).
    query_text
        .split_whitespace()
        .any(|w| w.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false))
}
```

Token count + capitalization. Cheap, no LLM. Operator-tunable later if observation phase shows the heuristic mis-gates.

## LLM-fallback meta-prompt + parser

```rust
const LLM_EXTRACT_SYSTEM_PROMPT: &str = "\
You extract entity references from a user query for a knowledge graph lookup.

Return a JSON array of {\"kind\": string, \"name\": string} objects.
- `kind` is a short lowercase category (e.g. \"person\", \"file\", \"system\").
- `name` is the surface form as it appears or as canonically spelled.
- Return at most 16 tuples.
- Return [] if the query has no entity references.
- Return ONLY the JSON array. No prose, no markdown, no code fences.";

pub const LLM_EXTRACT_MAX_TUPLES: usize = 16;

struct LlmEntityExtractor {
    router: Arc<Router>,
    model: String,  // configurable; defaults to local-model
}

impl LlmEntityExtractor {
    async fn extract(
        &self,
        query_text: &str,
        pool: &PgPool,
    ) -> Result<(Vec<i64>, String), EntityExtractionError> {
        let req = ChatRequest {
            model: self.model.clone(),
            messages: vec![
                ChatMessage::system(LLM_EXTRACT_SYSTEM_PROMPT.into()),
                ChatMessage::user(query_text.into()),
            ],
            max_tokens: Some(512),
            temperature: Some(0.0),
        };

        let started = Instant::now();
        let resp = self.router.send(&req).await?;
        let latency_ms = started.elapsed().as_millis() as i64;
        let body = resp.choices.first()
            .map(|c| c.message.content.as_str())
            .unwrap_or("");

        // Reuse parse_plan_lenient's first-`[`-wins discipline,
        // adapted for arrays. Concretely: locate the first `[` in the
        // response and stream-parse one complete JSON array. If the
        // parse fails, emit a Parse error (degraded by HybridEntityExtractor).
        let tuples: Vec<EntityRef> = parse_entity_refs_lenient(body)
            .map_err(|e| EntityExtractionError::Parse(format!("{e}")))?;

        if tuples.len() > LLM_EXTRACT_MAX_TUPLES {
            return Err(EntityExtractionError::Parse(format!(
                "LLM returned {} tuples; cap is {}",
                tuples.len(), LLM_EXTRACT_MAX_TUPLES
            )));
        }

        let n_entities_out = tuples.len();
        let graph = PgGraph::new(pool);
        let mut ids = Vec::with_capacity(n_entities_out);
        for t in &tuples {
            if let Some(entity) = graph.get_entity(&t.kind, &t.name).await? {
                ids.push(entity.id);
            }
        }
        ids.sort_unstable();
        ids.dedup();

        // Emit the actor='llm:router' action='extract_entities' audit row.
        let payload = build_extract_entities_payload(
            &self.model,
            query_text.len() as i64,
            n_entities_out as i64,
            backend_tag,  // from router.pick_backend
            latency_ms,
        );
        // Best-effort insert; WARN on failure.
        if let Err(e) = kastellan_db::audit::insert(
            pool, "llm:router", ACTION_EXTRACT_ENTITIES, payload,
        ).await {
            tracing::warn!(error = %e, "extract_entities audit insert failed");
        }

        let sha256 = compute_sha256(query_text);
        Ok((ids, sha256))
    }
}

#[derive(Deserialize, Debug)]
struct EntityRef {
    kind: String,
    name: String,
}
```

`parse_entity_refs_lenient` reuses the lenient-parsing discipline from [`core::scheduler::plan_parser::parse_plan_lenient`](../../../core/src/scheduler/plan_parser.rs) (the gemma4 markdown-fence handling lesson from 2026-05-14). Implementation mirrors that file's pattern: strict-`serde_json::from_str` first; on failure, locate the first `[` (instead of first `{`) and stream-parse one complete JSON array from there; on lenient-path failure, re-emit the strict-path error. The decode error is the unrecoverable failure mode; the extractor degrades on it. `backend_tag` comes from `self.router.pick_backend()`'s response — the existing `Backend::as_tag()` already returns the snake-case string the audit row needs.

## `RecallBuilder` widening

```rust
#[async_trait::async_trait]
pub trait RecallBuilder: Send + Sync {
    async fn build_with_seeds(
        &self,
        query_text: &str,
        seeds: &[i64],
    ) -> Result<RecalledContext, RecallError>;

    async fn build(&self, query_text: &str) -> Result<RecalledContext, RecallError> {
        self.build_with_seeds(query_text, &[]).await
    }
}
```

The default-impl shim for `build` keeps the recall-lane wiring slice's contract intact — callers that don't need seeds still work unchanged. `PgRecallBuilder::build_with_seeds` is the sole required impl method. When `seeds.is_empty()`, the implementation uses `RecallParams::new(text, embedding)` — preserving today's behaviour exactly. When `seeds.is_non_empty()`, the implementation uses `RecallParams::with_seeds(text, embedding, seeds)` — activating the graph lane.

Wire-shape impact:
- `core::recall_assembly::pg_builder.rs::PgRecallBuilder` gains the new method; the old `build` method body is removed (the default impl handles it).
- `StaticRecallBuilder::build_with_seeds` is the new required impl. Test fixtures need updating.
- `RouterAgent::formulate_plan` calls `self.recall_builder.build_with_seeds(&ctx.instruction, &seeds.ids).await` instead of `build(...)`.

## `RouterAgent::formulate_plan` flow

```rust
async fn formulate_plan(
    &self,
    ctx: &TaskContext,
) -> Result<(Plan, FormulationMeta), AgentError> {
    // 1. Entity extraction (NEW). Degrade-and-warn on failure.
    let seeds = match self.entity_extractor.extract(&ctx.instruction).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "entity extraction failed; continuing with empty seeds");
            EntitySeeds {
                ids: Vec::new(),
                source: SeedSource::None,
                llm_input_sha256: None,
            }
        }
    };

    // 2. Per-iteration recall, NOW seeded. Failure still degrades.
    let recalled = match self.recall_builder
        .build_with_seeds(&ctx.instruction, &seeds.ids).await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "recall failed; continuing with empty recall context");
            RecalledContext::empty()
        }
    };

    // 3. Prompt assembly (unchanged; fail-closed).
    let assembled = self.prompt_builder
        .build_with_recalled(&base, &recalled).await
        .map_err(AgentError::PromptAssembly)?;

    // 4. LLM call (unchanged).
    let req = ChatRequest { /* ... */ };
    // ...

    // 5. FormulationMeta now carries 3 new fields.
    let meta = FormulationMeta {
        // ... existing fields ...
        graph_seed_entity_ids: seeds.ids.clone(),
        graph_seed_count: seeds.ids.len() as u32,
        graph_seed_source: seeds.source,
    };
    Ok((plan, meta))
}
```

The extraction step happens BEFORE recall (sequential — recall needs the seeds). The two failures are both degrade-and-warn; the only fail-closed step is prompt assembly.

## Audit-row contract

**New row** (fires only when LLM fallback runs):

| Actor | Action | Payload keys | When |
|---|---|---|---|
| `llm:router` | `extract_entities` | `{model, n_chars_in, n_entities_out, backend, latency_ms}` (5 keys) | LLM fallback path. NOT emitted on deterministic-only success. |

The deterministic-only success path produces NO audit row (the only signal is the `plan.formulate` payload's `graph_seed_source = "deterministic"`). This matches the recall-lane wiring's choice not to audit individual lane outcomes.

**Bumped row:**

| Actor | Action | Payload keys (before → after) | Pure-additive? |
|---|---|---|---|
| `agent` | `plan.formulate` | 21/22 → **24/25** keys (gains `graph_seed_entity_ids`, `graph_seed_count`, `graph_seed_source`) | Yes |

Where `graph_seed_source` is one of `"deterministic"`, `"llm"`, or `"none"` (snake_case serialization of `SeedSource`).

## Test budget

Estimate: **+25-30 tests**, workspace 721 → ~746-751.

| Tier | Count | What's pinned |
|---|---|---|
| Pure unit (`entity_extraction::tests`) | ~12 | `SeedSource` serde round-trip; `DeterministicNameMatcher` substring match (case-insensitive, trailing/leading, multi-name dedup); `should_invoke_llm` gating (token count, capitalized-word detection); `LLM_EXTRACT_MAX_TUPLES` cap enforcement on parsed responses; `parse_entity_refs_lenient` (JSON array; first-`[`-wins; markdown fence; trailing prose; empty; malformed) |
| Async unit (`HybridEntityExtractor`) | ~6 | Deterministic happy path; LLM fallback fires on zero deterministic + non-trivial query; LLM fallback skipped on trivial query; LLM HTTP error degrades; LLM parse error degrades; resolved-id deduplication |
| DB integration (`entity_extraction_e2e.rs`) | ~5 | Seed entities in `entities`; query containing entity name → ids resolved via deterministic path; query with no entity → empty; LLM mock fallback resolves via `Graph::get_entity`; LLM mock returns over-cap → Parse error degrade |
| Audit pin (`scheduler_inner_loop_e2e.rs`) | (in-place) | `plan.formulate` payload carries the 3 new keys; `graph_seed_source = "none"` when extractor returns empty |
| Full-stack pin (`cli_ask_e2e.rs`) | (in-place) | `extract_entities` audit row NOT emitted on the test paths (deterministic-only; LLM never fires) AND `plan.formulate` payload still carries the new keys |

## What this slice deliberately does NOT do (filed as follow-up surfaces)

- **Entity vocabulary seeder.** No TOML loader for `entities`. Mirror the L0 seed loader shape (`seeds/entities/vocab.toml` + `core::entity_seed` module + audit row at startup) when this slice lands.
- **Memory-write-time entity linking.** No hook into L1 promote / L0 seed / future writers to write `memory_entities` rows. Even with seeded entities, the graph lane returns zero hits until this lands.
- **`entities.embedding` population.** Column stays NULL. The embedding-similarity extractor (faster than LLM, robust to paraphrase) is a separate slice once embeddings are seeded.
- **Operator CLI for entities.** No `kastellan-cli entities {add, link, list}`. Operator works via direct SQL or waits for the CLI slice.
- **Explicit cache invalidation.** 60s TTL only. Explicit `invalidate()` is a 5-line addition once the operator CLI exists (called from `cli_audit::entities_add_and_audit`).
- **Per-task agent-raised entity hint channel.** Agent cannot supply `Plan.entities_referenced`. The agent extracts at plan time; the system extracts at query time. If observation phase shows the system extractor under-recalls and the agent has signal the system doesn't, a future hybrid is a separate slice.
- **Per-extractor LLM model override.** The hybrid uses `router.config().local_model` by default. No `KASTELLAN_LLM_ENTITY_MODEL` env var; could be added if the local-model is too slow for the gating heuristic + observation phase shows latency pressure.

## Risk surface

- **Deterministic cache invalidation lag.** Operator inserts a new entity via direct SQL; deterministic match takes up to 60s to pick it up. Acceptable for v1 because operator-edit cadence is human-paced. Mitigation if needed: TTL knob + explicit `invalidate()`.
- **LLM fallback latency variance.** Adds ~200-500ms when the gating heuristic passes and the deterministic path returns zero. Hot path stays cheap; cold path pays the cost. Observable via the `extract_entities` audit row's `latency_ms` field.
- **LLM cost / token usage.** Every cold-path query consumes LLM tokens. Mitigation: the gating heuristic gates trivial queries; observation phase can tighten the heuristic if cost is an issue.
- **Pre-flight cache load on cold start.** First query after daemon start refreshes the cache (DB roundtrip). Acceptable — adds ~5ms to the first plan iteration. No `await` race conditions in the refresh path (RwLock write).
- **Empty `entities` table → 100% LLM fallback firing rate today.** Until the vocab seeder ships, every plan iteration whose gating heuristic passes runs the LLM extractor. This is OBSERVABLE (the audit row carries `latency_ms`) and FIXABLE (operator seeds the vocab). No production damage; just observability noise. Acceptable for read-side-only v1.
- **`Graph::get_entity` per-tuple round-trip.** LLM returns up to 16 tuples; each requires a separate `SELECT FROM entities WHERE kind = ? AND name = ?` query. Cap is 16, so worst case 16 round-trips. Mitigation if needed: batch via `WHERE (kind, name) IN (VALUES ...)`.

## Open questions for the implementer

None blocking. The design above commits on:
- Module structure (4 sub-files under `core::entity_extraction`).
- Hybrid extractor (deterministic primary + LLM fallback).
- 60s TTL on the deterministic cache (no operator-CLI invalidation in this slice).
- Gating heuristic: ≥ 4 tokens AND ≥ 1 capitalized word.
- Audit-row action `"extract_entities"` (snake_case, matches existing audit-action naming).
- `plan.formulate` payload bump 21/22 → 24/25 keys.
- `RecallBuilder` default-impl shim for `build(text)` → `build_with_seeds(text, &[])`.

If any of these turn out wrong during implementation, file the correction inline and update the plan + spec in the same fixup commit.

## Self-review checklist (done before commit)

- [x] No placeholders / TBD / TODO in body text.
- [x] Module structure cross-checked against the `recall_assembly` precedent.
- [x] Audit-row payload key counts cross-checked against the L1 slice's 21/22 + 3 = 24/25.
- [x] `EntitySeeds` field set is bounded and serializable.
- [x] Graph-lane data flow (extract → resolve → with_seeds → recall) is fully traced.
- [x] Failure modes are all degrade-and-warn (no fail-closed paths added to `formulate_plan`).
- [x] Scope is one session (estimated 14 tasks per the L1 precedent's sizing).
- [x] All deferred items have explicit follow-up surfaces.
