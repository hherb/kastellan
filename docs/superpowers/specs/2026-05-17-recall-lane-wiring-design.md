# `RecallBuilder` — first production consumer of `embed_query`, threads recalled memories into the assembled system prompt

**Date:** 2026-05-17
**Status:** Design, ready for plan.
**Branch (proposed):** `feat/recall-lane-wiring`
**Pre-reqs (all shipped):**
- PR #29 (`Router::embed`, Option O, 2026-05-12) — embedding side of the LLM router.
- PR #41 (memory graph lane + `memory_entities`, 2026-05-13) — `recall(pool, params)` semantic + lexical + graph fan-out.
- PR #54 (issue #17 + #40 closure, 2026-05-14) — `RecallParams::with_seeds` + `RecallModes::SEMANTIC_AND_LEXICAL` const.
- PR #69 (L1 storage primitive, 2026-05-15) — `load_l1_default`.
- PR #74 (L0 seed loader, 2026-05-16) — `load_l0_active_default`.
- PR #75 (prompt assembler L0 + L1, 2026-05-16) — `SystemPromptBuilder` trait + `assemble_system_prompt`.

## Why now

`RouterAgent::formulate_plan` ([core/src/scheduler/agent.rs:86](../../../core/src/scheduler/agent.rs#L86)) currently builds a system prompt from L0 meta-rules + L1 insight index + the bare `prompts/agent_planner.md` base. The semantic, lexical, and graph recall lanes ship today as a storage + retrieval primitive in [core/src/memory/recall.rs](../../../core/src/memory/recall.rs) with **no production consumer** — every recall row sits in `memories` at `layer = 2 (Stable)` but is never seen by the planner.

`Router::embed` (Option O, 2026-05-12) is also a primitive with no production consumer. It's exercised only by the `embedding_recall_e2e` integration test.

This slice ships the first real consumer of both: before each plan iteration, embed the task instruction, fan out to `recall(SEMANTIC | LEXICAL)`, and render the retrieved bodies into a `<recalled>...</recalled>` block slotted between L1 and base in the assembled system prompt.

Until this slice lands, a stable fact written to the `memories` table has **zero effect on agent behaviour** — recall is a complete dead lane in production.

## Scope

In scope (this slice):

- Widen the existing `assemble_system_prompt(l0, l1, base)` pure helper to a 4-arg `assemble_system_prompt(l0, l1, recalled, base)`. Every call site updates; no v1/v2 split (justified below in "Shape decision").
- New async `RecallBuilder` trait + production `PgRecallBuilder` impl + test-only `StaticRecallBuilder` impl. Same shape as the `SystemPromptBuilder` precedent.
- `RecalledContext { ids: Vec<i64>, bodies: Vec<String>, query_sha256: String }` value type — the typed handoff between `RecallBuilder::build` and the assembler.
- New `RecallError` enum (`EmbedQuery(MemoryError)` + `DbLane(DbError)`).
- `RouterAgent` constructor + `formulate_plan` wired through the new trait.
- `agent/plan.formulate` audit-row payload gains 3 new keys: `recalled_memory_ids: [i64]`, `recall_count: u32`, `recall_query_sha256: String`.
- `FormulationMeta` widened with the same 3 fields.
- `main.rs` constructs `PgRecallBuilder` (sharing the existing `Router` + `PgPool`) and passes it into `RouterAgent::new`.

Out of scope (filed as follow-ups):

- **Graph lane.** Needs entity extraction from the task instruction before any `seed_entity_ids` array can be populated. Separate slice; the existing `RecallModes::SEMANTIC_AND_LEXICAL` const is exactly the right default until then.
- **L1 promotion writer.** L1 stays empty in production until a separate slice writes it. Recall reads what's in the `memories` table; whether L0/L1 hydration happens is independent of whether recall runs.
- **Global token cap with priority-drop logic** ([#78](https://github.com/hherb/hhagent/issues/78)). Each loader still enforces its own per-loader cap (L0: 8 KiB / L1: 4 KiB / recall: 4 KiB). When all three would jointly overflow the model's context, the priority-drop logic from the HANDOVER headline spec lands as a separate slice.
- **Recall caching across plan iterations.** Re-runs on every iteration (matches the L0/L1 cadence — the `PgSystemPromptBuilder::build` from PR #75 is already called per-iteration). The instruction doesn't change mid-task, so this looks redundant — but the same is true of L0/L1 today, and adding caching is a cross-cutting decision that should land for all three loaders at once.
- **Reviewer-chain recall.** `ConstitutionalGuard` / `DeterministicPolicy` are deterministic Rust checks today, no LLM call, no prompt.
- **Operator-visible recall metrics.** No `tracing::info!` on every recall, no metrics export. The audit row is the recall trail.

## Architecture

```
RouterAgent::formulate_plan
        │
        ├──► self.recall_builder.build(ctx.instruction)  ─►  PgRecallBuilder
        │                                                       │
        │                                                       ├─► embed_query(pool, router, instruction)
        │                                                       └─► recall(pool, RecallParams::with_seeds(
        │                                                                              instruction, embedding, &[]))
        │
        ├──► self.prompt_builder.build_with_recalled(base, recalled)  ─►  PgSystemPromptBuilder
        │                                                                     │
        │                                                                     ├─► load_l0_active_default(pool)
        │                                                                     ├─► load_l1_default(pool)
        │                                                                     └─► assemble_system_prompt(
        │                                                                              l0, l1, recalled, base)
        │
        └──► Router::send(ChatRequest{ system=assembled, user=ctx_json })
```

The two trait calls are sequential (recall first, then prompt assembly that consumes the recalled context). They are **not** parallel — the recall result feeds the prompt assembler. The cost is one extra `await` boundary per plan iteration.

The `RecallBuilder` trait is the seam for tests (swap in `StaticRecallBuilder`) and for any future "recall-aware-with-history" variant that includes prior plan iterations in the query text.

## Module layout

```
core/src/
├── recall_assembly/
│   ├── mod.rs              (re-exports; RecallBuilder trait + RecalledContext + RecallError)
│   └── pg_builder.rs       (PgRecallBuilder prod impl + StaticRecallBuilder test helper)
├── prompt_assembly/
│   ├── mod.rs              (unchanged public surface; widens SystemPromptBuilder with build_with_recalled)
│   ├── assemble.rs         (assemble_system_prompt widened to take recalled)
│   └── pg_builder.rs       (PgSystemPromptBuilder.build_with_recalled added; .build delegates with empty recalled)
```

`recall_assembly` is a sibling of `prompt_assembly`, not nested. Both are "pre-LLM-call assembly steps" called from `RouterAgent::formulate_plan` — sibling placement keeps the dependency tree flat (`recall_assembly` doesn't depend on `prompt_assembly` or vice versa; they meet in the agent).

## Assembled-prompt shape (with recalled block)

```text
<l0_meta_rules>
- {body of newest-distinct l0_rule_id, in DESC(created_at) order}
- {body of next L0 row}
</l0_meta_rules>

<l1_insights>
- {body of L1 row #1 (newest-first)}
- {body of L1 row #2}
</l1_insights>

<recalled>
- {body of recall row #1 (RRF-ranked-first)}
- {body of recall row #2}
</recalled>

<base>
{contents of prompts/agent_planner.md verbatim}
</base>
```

Rules:

1. **Order:** L0 → L1 → recalled → base, always.
2. **Empty sections skipped.** If `recalled.bodies.is_empty()` (which is also the failure-degraded state), no `<recalled>` tag is emitted. The assembled prompt is then byte-identical to the prompt-assembler v1 output.
3. **Inter-section separator:** one blank line between sections (matches v1).
4. **Row rendering:** `- ` prefix, body verbatim, one row per line (matches v1).
5. **No body escaping.** Memory bodies are not operator-curated (any process with `INSERT` on `memories` can write them), but the threat model is bounded — `<` / `>` in a body still pass through verbatim. The model's tokeniser handles its own framing.
6. **No metadata in body.** `memory_id` stays out of the prompt; it's in the audit log (`recalled_memory_ids`).
7. **Byte cap: 4 KiB after rendering.** The `recalled` block contributes at most `L_RECALL_CAP_BYTES = 4096` bytes to the assembled prompt. Rows are appended newest-first; a row whose addition would breach the cap is dropped (with `tracing::warn!` carrying the dropped `memory_id`). Same idiom as `load_l1`'s saturating_add break.
8. **Deterministic.** Same `(l0, l1, recalled, base)` inputs → same output byte-for-byte.

### Worked example (today's production state)

With the L0 starter rules, L1 empty, and 2 stable-fact memories matching the instruction:

```text
<l0_meta_rules>
- Never run rm -rf or any other recursive delete without explicit operator confirmation.
- A refusal is terminal: once the agent refuses, no further plan steps run in this task.
</l0_meta_rules>

<recalled>
- The operator prefers concise, action-oriented responses.
- Use the `shell-exec` tool for file inspection; never inline shell commands in chat output.
</recalled>

<base>
# Agent Planner
You are the agent...
</base>
```

(L1 empty → its section omitted, parallel to today's recalled-empty case under v1.)

## Public surface

### Value type

```rust
// core/src/recall_assembly/mod.rs

/// Bodies + ids the recall pipeline emitted for a given query.
/// `bodies.len() == ids.len()` by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecalledContext {
    /// Memory ids in fused (RRF) order, capped at `DEFAULT_RECALL_K`.
    pub ids: Vec<i64>,
    /// Bodies in the same order as `ids`, capped at `L_RECALL_CAP_BYTES`
    /// cumulative bytes.
    pub bodies: Vec<String>,
    /// Hex SHA-256 of the query text (`ctx.instruction`). Lets the
    /// observation phase detect when paraphrased prompts produce
    /// identical recalled-id sets vs. genuine drift.
    pub query_sha256: String,
}

impl RecalledContext {
    /// The empty/degraded-recall sentinel. SHA-256 of the empty string
    /// for `query_sha256` so the field is always 64 hex chars.
    pub fn empty() -> Self;
    pub fn is_empty(&self) -> bool { self.ids.is_empty() }
}
```

### Trait + error

```rust
// core/src/recall_assembly/mod.rs

use async_trait::async_trait;
use thiserror::Error;
use hhagent_core::memory::MemoryError;
use hhagent_db::DbError;

#[derive(Debug, Error)]
pub enum RecallError {
    #[error("embed_query failed: {0}")]
    EmbedQuery(#[from] MemoryError),
    #[error("recall lane failed: {0}")]
    DbLane(#[from] DbError),
}

/// Runs a per-query recall and packages the result for prompt assembly.
/// The async signature mirrors `SystemPromptBuilder` so the agent can
/// swap impls and tests can supply deterministic stubs.
#[async_trait]
pub trait RecallBuilder: Send + Sync {
    /// Build a recalled context for the given query text. Implementations
    /// MAY return `Err`, but `RouterAgent::formulate_plan` is expected
    /// to degrade (treat as `RecalledContext::empty()`) and emit a
    /// `tracing::warn!`. Recall is enrichment, not policy.
    async fn build(&self, query: &str) -> Result<RecalledContext, RecallError>;
}
```

### Production impl

```rust
// core/src/recall_assembly/pg_builder.rs

use std::sync::Arc;
use async_trait::async_trait;
use sqlx::PgPool;
use hhagent_llm_router::Router;

use crate::memory::{embed_query, recall, RecallParams, RecallModes, DEFAULT_RECALL_K};
use super::{RecallBuilder, RecalledContext, RecallError};

pub struct PgRecallBuilder {
    pool: Arc<PgPool>,
    router: Arc<Router>,
}

impl PgRecallBuilder {
    pub fn new(pool: Arc<PgPool>, router: Arc<Router>) -> Self {
        Self { pool, router }
    }
}

#[async_trait]
impl RecallBuilder for PgRecallBuilder {
    async fn build(&self, query: &str) -> Result<RecalledContext, RecallError> {
        let query_sha256 = sha256_hex(query.as_bytes());
        let embedding = embed_query(&self.pool, &self.router, query).await?;

        let params = RecallParams::with_seeds(query, &embedding, &[])
            .with_modes(RecallModes::SEMANTIC_AND_LEXICAL)
            .with_k(DEFAULT_RECALL_K);
        let memories = recall(&self.pool, params).await?;

        let (ids, bodies) = cap_and_split(memories, L_RECALL_CAP_BYTES);
        Ok(RecalledContext { ids, bodies, query_sha256 })
    }
}
```

`cap_and_split` is a pure helper in `recall_assembly::pg_builder` (or hoisted to `assemble.rs` if symmetry with `load_l1` is preferred). It walks the recall result newest-first, accumulating `saturating_add(bytes)`, and stops at the first row that would breach `L_RECALL_CAP_BYTES`. Dropped rows are logged via `tracing::warn!` with their `memory_id`.

### Test helper

```rust
// core/src/recall_assembly/pg_builder.rs (same file, pub-for-tests)

/// Fixed-output builder for tests that don't care about lane fan-out.
pub struct StaticRecallBuilder {
    fixed: RecalledContext,
}

impl StaticRecallBuilder {
    pub fn empty() -> Self { Self { fixed: RecalledContext::empty() } }
    pub fn with(ids: Vec<i64>, bodies: Vec<String>, query: &str) -> Self {
        Self { fixed: RecalledContext {
            ids,
            bodies,
            query_sha256: sha256_hex(query.as_bytes()),
        }}
    }
}

#[async_trait]
impl RecallBuilder for StaticRecallBuilder {
    async fn build(&self, _query: &str) -> Result<RecalledContext, RecallError> {
        Ok(self.fixed.clone())
    }
}
```

`StaticRecallBuilder` is `pub` (not `cfg(test)`) so cross-crate integration tests can construct it without re-exporting.

## `assemble_system_prompt` widening

The existing signature:

```rust
pub fn assemble_system_prompt(l0: &[Memory], l1: &[Memory], base: &str) -> String;
```

becomes:

```rust
pub fn assemble_system_prompt(
    l0: &[Memory],
    l1: &[Memory],
    recalled: &RecalledContext,
    base: &str,
) -> String;
```

Every call site updates; the old shape is gone. There is no v1/v2 split — a `RecalledContext::empty()` argument produces output byte-identical to the v1 (pre-recall) state, so the migration is mechanical.

The choice between adding a parameter vs. introducing a new function name was settled by the same logic as the prompt-assembler slice: the recalled block is always part of an "assembled system prompt"; splitting the function would force every caller to know whether recall ran or not. The empty-context degradation handles the no-recall case cleanly.

## `SystemPromptBuilder` widening

The existing trait method:

```rust
async fn build(&self, base: &str) -> Result<AssembledPrompt, PromptAssemblyError>;
```

is supplemented by:

```rust
async fn build_with_recalled(
    &self,
    base: &str,
    recalled: &RecalledContext,
) -> Result<AssembledPrompt, PromptAssemblyError>;
```

`build` is retained as a thin shim that calls `build_with_recalled(base, &RecalledContext::empty())`. This preserves the existing test surface and the `StaticSystemPromptBuilder::empty()` / `::new(content)` constructors. The `AssembledPrompt` struct gains a `recalled_count: usize` field (matches `l0_count` / `l1_count`).

## RouterAgent wire-in

```rust
// core/src/scheduler/agent.rs

pub struct RouterAgent {
    router: Arc<Router>,
    prompts: Arc<PromptCache>,
    prompt_builder: Arc<dyn SystemPromptBuilder>,
    recall_builder: Arc<dyn RecallBuilder>,  // NEW
}

impl RouterAgent {
    pub fn new(
        router: Arc<Router>,
        prompts: Arc<PromptCache>,
        prompt_builder: Arc<dyn SystemPromptBuilder>,
        recall_builder: Arc<dyn RecallBuilder>,  // NEW
    ) -> Self { /* ... */ }
}

#[async_trait]
impl PlanFormulator for RouterAgent {
    async fn formulate_plan(&self, ctx: &TaskContext) -> Result<(Plan, FormulationMeta), AgentError> {
        let entry = self.prompts.get("agent_planner").ok_or(AgentError::PromptMissing)?;
        let base = entry.content.clone();

        // NEW: per-iteration recall. Degrade-and-warn on failure.
        let recalled = match self.recall_builder.build(&ctx.instruction).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "recall failed; continuing with empty recall context");
                RecalledContext::empty()
            }
        };

        let assembled = self.prompt_builder
            .build_with_recalled(&base, &recalled)
            .await
            .map_err(AgentError::PromptAssembly)?;

        // ... existing ChatRequest construction (system = assembled.system_prompt) ...

        let meta = FormulationMeta {
            // ... existing fields ...
            assembled_prompt_sha256: assembled.system_prompt_sha256.clone(),
            l0_count: assembled.l0_count,
            l1_count: assembled.l1_count,
            recalled_memory_ids: recalled.ids.clone(),     // NEW
            recall_count: recalled.ids.len() as u32,       // NEW
            recall_query_sha256: recalled.query_sha256.clone(),  // NEW
        };
        Ok((plan, meta))
    }
}
```

Recall errors are **swallowed by the agent**, not propagated. This is the load-bearing posture choice: recall is enrichment (the agent still works without it); prompt assembly is policy (a degraded prompt would have the agent flying blind on operator rules).

## Audit-row payload (after the slice)

| Source              | Before | After  | New keys                                                                         |
|---------------------|--------|--------|----------------------------------------------------------------------------------|
| `default`           | 17     | **20** | `recalled_memory_ids`, `recall_count`, `recall_query_sha256`                     |
| `cli_inferred`      | 18     | **21** | (same three; `classification_floor_signals` already present, retained)           |
| `operator`          | 17     | **20** | (same three)                                                                     |
| `agent_raised`      | 17     | **20** | (same three)                                                                     |

Pure-additive. Existing JSONB consumers keep working unchanged. The replay harness (Slice B of the rule-iteration harness) inherits the new keys automatically — they round-trip through the same audit-row capture path. Pre-Slice-1 captures will lack the recall keys; the harness's existing "skip on missing keys" branch (`plans_skipped_missing_body` counter) extends naturally to `plans_skipped_missing_recall` if we want a distinct counter, but the simplest path is to fold both into a single "skip on missing field" branch.

## Daemon wire-in (`main.rs`)

```rust
// core/src/main.rs (after the existing prompt_builder construction)

let recall_builder: Arc<dyn RecallBuilder> = Arc::new(PgRecallBuilder::new(
    pool.clone(),
    Arc::new(router.clone()),  // existing router already in scope
));

let agent: Arc<dyn PlanFormulator> = Arc::new(RouterAgent::new(
    router_arc.clone(),
    prompts.clone(),
    prompt_builder,
    recall_builder,  // NEW
));
```

No new env vars, no new config. `PgRecallBuilder` uses the same `PgPool` + `Router` already constructed for everything else.

## Testing posture

### Unit tests

`assemble.rs` (extended):
- `assembles_l0_l1_recalled_base_in_order` — all four sections present.
- `omits_recalled_section_when_empty` — byte-identical to v1 output.
- `respects_byte_cap_dropping_oldest_overflow_row` — feed 5 rows totalling > 4 KiB; assert only the rows that fit appear; assert dropped-row warn fires.
- `does_not_escape_xml_chars_in_body` — body containing `<` survives verbatim.

`recall_assembly::pg_builder` (new):
- `static_builder_returns_fixed_context` — trivial round-trip.
- `static_builder_empty_returns_empty_context` — verifies the empty sentinel shape (`query_sha256 == sha256_hex(b"")`).
- `cap_and_split_drops_oversize_rows_with_warn` — pure helper test.

`build_plan_formulate_payload`:
- `payload_carries_three_new_recall_keys` — pins the 20-key shape (default source).
- `payload_carries_recall_query_sha256_as_64_hex_chars` — defensive format pin.

### Integration tests

`core/tests/prompt_assembly_e2e.rs` (extended):
- `pg_builder_with_recalled_renders_block` — seed 3 memories, pass them via a `StaticRecallBuilder`, assert the assembled prompt contains the `<recalled>` block and the three bodies.
- `pg_builder_with_empty_recalled_omits_block` — passing `RecalledContext::empty()` produces byte-identical output to the v1 builder.

`core/tests/recall_assembly_e2e.rs` (new):
- `pg_recall_builder_round_trips_against_real_pool` — seed 3 memories, build via `PgRecallBuilder`, assert one of them surfaces and the SHA-256 matches.

`core/tests/scheduler_inner_loop_e2e.rs` (extended):
- The existing happy path (`Outcome::Completed`) gains assertions on the 3 new audit-row keys: presence, type-shape, and `recall_count == recalled_memory_ids.len()` consistency.

### Test count delta

Expected: **+12 tests** (652 → 664).

| Tier                                   | Tests |
|----------------------------------------|-------|
| `assemble.rs` (extended)               | +4    |
| `recall_assembly::pg_builder` (new)    | +3    |
| `build_plan_formulate_payload` (extended) | +2 |
| `prompt_assembly_e2e.rs` (extended)    | +2    |
| `recall_assembly_e2e.rs` (new)         | +1    |
| `scheduler_inner_loop_e2e` (extended)  | 0 (in-place assertion expansion) |

### Skip behaviour

Tests that depend on `Router::embed` against a live HTTP endpoint use the existing TCP mock from `embedding_recall_e2e.rs` (no live LLM dial — same pattern as Option O's tests). Integration tests that need both PG and the mock LLM follow the `cli_ask_e2e` precedent (per-test PG cluster + per-test TCP listener for the embedding endpoint).

## Failure-mode matrix

| Failure                            | What happens                                                                                       |
|------------------------------------|----------------------------------------------------------------------------------------------------|
| `Router::embed` returns Err        | `RecallError::EmbedQuery`; agent swallows, `tracing::warn!`, `recall_count = 0`, prompt has no `<recalled>`. |
| `recall()` returns Err             | `RecallError::DbLane`; same degradation as above.                                                  |
| `recall()` returns empty Vec       | `RecalledContext::empty()`; no `<recalled>` block; audit row has `recall_count: 0`, `recalled_memory_ids: []`. |
| `embed_query` dim mismatch         | Surfaces as `MemoryError::EmbeddingDimMismatch` → `RecallError::EmbedQuery` → degrade.            |
| Single recall row > 4 KiB          | Dropped with `tracing::warn!`; remaining rows under the cap appear.                                |
| `SystemPromptBuilder::build_with_recalled` returns Err | Agent fails the plan iteration (existing `PromptAssembly` posture — load-bearing).      |

The asymmetry is deliberate: prompt assembly is policy (L0 rules MUST reach the model), recall is enrichment (it's nice if relevant memories help, but the agent can plan without them).

## Shape decision (recorded, not open)

**`assemble_system_prompt` keeps a single signature** that grows the new `&RecalledContext` parameter; the v1/v2 split is rejected.

Rejected alternative: introducing a sibling `assemble_system_prompt_with_recalled` (4-arg) and leaving the v1 (3-arg) in place. That would force every caller to know whether recall ran — exactly the coupling the assembler exists to avoid. With the chosen 4-arg shape, a `RecalledContext::empty()` argument produces output byte-identical to the v1 (pre-recall) state, so call sites that don't run recall (tests, the `StaticSystemPromptBuilder` shim, the `SystemPromptBuilder::build` thin shim) pass `&RecalledContext::empty()` and get the v1 output for free.

## Migration / rollback

Pure-additive at the schema level (no migration). The audit-row payload grows but every consumer that reads existing keys keeps working. If we needed to roll back, the only state that changes is in `audit_log` rows written between the rollback and the deploy — those rows would carry the three new keys with no consumer, which is harmless.

No prompt change. The `agent_prompts` ledger records the same SHA-256 for `agent_planner.md` as today.

## Non-goals (re-stated for emphasis)

- No graph lane.
- No L1 promotion writer.
- No global token cap.
- No caching across plan iterations.
- No reviewer-chain recall.
- No new env vars.
- No new operator surfaces (CLI flags, config files).

Each is a separate slice. The recall-lane wiring slice is deliberately narrow: it makes recall reachable in production, nothing more.
