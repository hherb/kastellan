# `build_system_prompt` — prompt assembler that pins L0 + L1 in the agent's system message

**Date:** 2026-05-16
**Status:** Design, ready for plan.
**Branch (proposed):** `feat/prompt-assembler-l0-l1`
**Pre-reqs (both shipped):** PR #69 (L1 storage primitive) + PR #74 (L0 seed loader).

## Why now

`RouterAgent::formulate_plan` ([core/src/scheduler/agent.rs:86](../../../core/src/scheduler/agent.rs#L86)) currently builds a two-message `ChatRequest` whose system message is the bare contents of `prompts/agent_planner.md`. The L0 meta-rules loader (PR #74) and the L1 insight-index loader (PR #69) both ship today as storage primitives with no consumer. This slice ships the first real consumer: the agent's system prompt now concatenates L0 + L1 + base before each plan iteration.

Until this slice lands, an operator-edited L0 rule has no effect on agent behaviour — the rule sits in the `memories` table at `layer = 0` but never reaches the model.

## Scope

In scope (this slice):

- New pure function `assemble_system_prompt(l0, l1, base) -> String` in `core::prompt_assembly`.
- New async `SystemPromptBuilder` trait + production `PgSystemPromptBuilder` impl + test-only `StaticSystemPromptBuilder` impl.
- `RouterAgent` constructor + `formulate_plan` wired through the trait.
- `agent/plan.formulate` audit-row payload gains 3 new keys: `system_prompt_sha256`, `l0_count`, `l1_count`.
- `FormulationMeta` widened with the same 3 fields.
- `main.rs` constructs `PgSystemPromptBuilder` and passes it into `RouterAgent::new`.

Out of scope (filed as follow-ups):

- **Recall lane** — semantic / lexical / graph search results stay unwired. Adds latency (query embedding) and complexity (entity extraction for the graph lane). Separate slice.
- **Global token cap with priority-drop logic** (L4 → L2 → L3 → L1 → L0 from the HANDOVER's headline spec). Both L0 and L1 already enforce per-loader caps; no over-budget condition exists today. Lands with recall.
- **L3 (skills) or L4 (session-digest) writers.** Empty layers.
- **Reviewer-chain prompt assembly.** `ConstitutionalGuard` / `DeterministicPolicy` are deterministic Rust checks today — no LLM call, no prompt.
- **Prompt caching across iterations.** Each plan iteration re-loads + re-assembles; fresh L0/L1 state.

## Architecture

```
RouterAgent::formulate_plan
        │
        ├──► self.prompt_builder.build(base)  ─►  PgSystemPromptBuilder
        │                                              │
        │                                              ├─► load_l0_active_default(pool)
        │                                              ├─► load_l1_default(pool)
        │                                              └─► assemble_system_prompt(l0, l1, base)
        │
        └──► Router::send(ChatRequest{ system=assembled, user=ctx_json })
```

The pure assembler is the single concatenation/framing site. The trait is the seam for tests (swap in `StaticSystemPromptBuilder`) and the future recall-aware builder (separate impl, same trait).

## Module layout

```
core/src/
├── prompt_assembly/
│   ├── mod.rs          (re-exports; SystemPromptBuilder trait + error)
│   ├── assemble.rs     (pure assemble_system_prompt + inline tests)
│   └── pg_builder.rs   (PgSystemPromptBuilder prod impl + StaticSystemPromptBuilder test helper)
```

Top-level under `core/src/`, not under `scheduler/`. Future reviewer chains and channel-bus agents may want assembled prompts too; the placement reflects that.

## Assembled-prompt shape

```text
<l0_meta_rules>
- {body of newest-distinct l0_rule_id, in DESC(created_at) order}
- {body of next L0 row}
</l0_meta_rules>

<l1_insights>
- {body of L1 row #1 (newest-first)}
- {body of L1 row #2}
</l1_insights>

<base>
{contents of prompts/agent_planner.md verbatim}
</base>
```

Rules:

1. **Order:** L0 → L1 → base, always.
2. **Empty sections skipped.** If `l0.is_empty()`, no `<l0_meta_rules>` tag emitted. Same for L1. The `<base>` section is always emitted.
3. **Inter-section separator:** one blank line between sections.
4. **Row rendering:** `- ` prefix, body verbatim, one row per line. Symmetric across L0 and L1.
5. **No body escaping.** Bodies are operator-curated and pass through `seed_meta_memory` validation (UTF-8, ≤ 1024 bytes). No HTML-style escaping of `<` / `>` inside bodies; an `<` in a body is the operator's choice. Tests pin this.
6. **No metadata rendering.** `l0_rule_id` stays out of the prompt; it's in the audit log + source TOML.
7. **Deterministic.** Same inputs → same output byte-for-byte. Pinned by test.

### Worked example (today's production state)

With the starter `seeds/memory/l0_meta_rules.toml` (2 rules) and L1 empty:

```text
<l0_meta_rules>
- Never run rm -rf or any other recursive delete without explicit operator confirmation.
- A refusal is terminal: once the agent refuses, no further plan steps run in this task.
</l0_meta_rules>

<base>
# Agent Planner
You are the agent...
</base>
```

(Two newlines between `</l0_meta_rules>` and `<base>` — one to close the section, one blank separator.)

## Public surface

### Pure function

```rust
// core/src/prompt_assembly/assemble.rs

use hhagent_db::memories::Memory;

/// Build the system message by concatenating L0 + L1 + base under
/// XML-style section tags. Empty layers omit their tags entirely.
///
/// The output is deterministic: same `(l0, l1, base)` inputs produce
/// the same byte sequence on every call. No SHA computation, no I/O.
pub fn assemble_system_prompt(l0: &[Memory], l1: &[Memory], base: &str) -> String;
```

### Trait + error + result struct

```rust
// core/src/prompt_assembly/mod.rs

use async_trait::async_trait;
use hhagent_db::DbError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PromptAssemblyError {
    #[error("memory load failed: {0}")]
    Memory(#[from] DbError),
}

/// Result of an assembly call. Carries the assembled string plus the
/// per-layer counts. The counts come straight from the loader output
/// at the time of assembly — they cannot drift from what the model
/// actually saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssembledPrompt {
    pub system_prompt: String,
    pub l0_count: usize,
    pub l1_count: usize,
}

#[async_trait]
pub trait SystemPromptBuilder: Send + Sync {
    /// Assemble a system prompt from the supplied base. Fail-closed:
    /// any error from the underlying memory loaders propagates so the
    /// scheduler can fail the plan iteration rather than run with a
    /// degraded prompt.
    async fn build(&self, base: &str) -> Result<AssembledPrompt, PromptAssemblyError>;
}
```

### Production impl

```rust
// core/src/prompt_assembly/pg_builder.rs

use std::sync::Arc;
use async_trait::async_trait;
use sqlx::PgPool;

use crate::memory::l0_seed::load_l0_active_default;
use crate::memory::layers::load_l1_default;
use super::{assemble::assemble_system_prompt, PromptAssemblyError, SystemPromptBuilder};

pub struct PgSystemPromptBuilder {
    pool: Arc<PgPool>,
}

impl PgSystemPromptBuilder {
    pub fn new(pool: Arc<PgPool>) -> Self { Self { pool } }
}

#[async_trait]
impl SystemPromptBuilder for PgSystemPromptBuilder {
    async fn build(&self, base: &str) -> Result<AssembledPrompt, PromptAssemblyError> {
        let l0 = load_l0_active_default(&self.pool).await?;
        let l1 = load_l1_default(&self.pool).await?;
        let system_prompt = assemble_system_prompt(&l0, &l1, base);
        Ok(AssembledPrompt {
            system_prompt,
            l0_count: l0.len(),
            l1_count: l1.len(),
        })
    }
}
```

### Test helper

```rust
// core/src/prompt_assembly/pg_builder.rs (same file, behind cfg(test) or pub-for-tests)

/// Fixed-output builder for tests that don't care about the assembled
/// shape (existing inner-loop / agent tests).
pub struct StaticSystemPromptBuilder {
    fixed: String,
}

impl StaticSystemPromptBuilder {
    pub fn empty() -> Self { Self { fixed: String::new() } }
    pub fn new(fixed: impl Into<String>) -> Self { Self { fixed: fixed.into() } }
}

#[async_trait]
impl SystemPromptBuilder for StaticSystemPromptBuilder {
    async fn build(&self, _base: &str) -> Result<AssembledPrompt, PromptAssemblyError> {
        Ok(AssembledPrompt {
            system_prompt: self.fixed.clone(),
            l0_count: 0,
            l1_count: 0,
        })
    }
}
```

`StaticSystemPromptBuilder` is `pub` (not `cfg(test)`) so cross-crate test files (`core/tests/*.rs`) can use it without a separate dev-dep export. The static helper always reports `(l0_count, l1_count) = (0, 0)` — tests that need non-zero counts can either use the prod `PgSystemPromptBuilder` against a per-test PG cluster, or supply a different fixture builder.

## RouterAgent wire-in

```rust
// core/src/scheduler/agent.rs

pub struct RouterAgent {
    router: Arc<Router>,
    prompts: Arc<PromptCache>,
    prompt_builder: Arc<dyn SystemPromptBuilder>,  // NEW
}

impl RouterAgent {
    pub fn new(
        router: Arc<Router>,
        prompts: Arc<PromptCache>,
        prompt_builder: Arc<dyn SystemPromptBuilder>,  // NEW
    ) -> Self { Self { router, prompts, prompt_builder } }
}

#[async_trait]
impl PlanFormulator for RouterAgent {
    async fn formulate_plan(&self, ctx: &TaskContext) -> Result<(Plan, FormulationMeta), AgentError> {
        let entry = self.prompts.get("agent_planner").ok_or(AgentError::PromptMissing)?;
        let base = entry.content.clone();

        // NEW: assemble L0 + L1 + base before sending.
        let assembled = self.prompt_builder.build(&base).await
            .map_err(AgentError::PromptAssembly)?;
        let system_prompt_sha256 = sha256_hex(assembled.system_prompt.as_bytes());

        let user_msg = serialise_context_for_agent(ctx);
        let local_model = self.router.config().local_model.clone();
        let req = ChatRequest {
            model: local_model.clone(),
            messages: vec![
                ChatMessage::system(assembled.system_prompt),
                ChatMessage::user(user_msg),
            ],
            max_tokens: None,
            temperature: Some(0.0),
        };

        let start = Instant::now();
        let resp = self.router.send(&req).await?;
        let latency_ms = start.elapsed().as_millis() as u64;
        // ... existing parse logic ...

        let meta = FormulationMeta {
            // ... existing fields ...
            assembled_prompt_sha256: system_prompt_sha256,  // NEW
            l0_count: assembled.l0_count,                   // NEW
            l1_count: assembled.l1_count,                   // NEW
        };
        Ok((plan, meta))
    }
}
```

### How the counts reach the audit row

The counts come from `AssembledPrompt.l0_count` / `l1_count`, which the `PgSystemPromptBuilder` populates directly from `l0.len()` / `l1.len()` at load time. No string re-parsing, no regex over the assembled prompt. A future refactor of the bullet prefix or section delimiter cannot drift the counts away from what was actually loaded.

The SHA is computed in `RouterAgent` (not in the builder) because the builder doesn't need to know it's being audited — keeps the trait single-purpose. Pure `sha256_hex(bytes)` helper imported from `sha2`.

### New `AgentError` variant

```rust
#[derive(Debug, Error)]
pub enum AgentError {
    #[error("router: {0}")]
    Router(#[from] RouterError),
    #[error("plan decode failed: {detail}")]
    Decode { detail: String, raw: String },
    #[error("agent prompt 'agent_planner' not found in cache")]
    PromptMissing,
    #[error("prompt assembly: {0}")]                 // NEW
    PromptAssembly(#[from] PromptAssemblyError),     // NEW
}
```

## Audit-row contract

`agent/plan.formulate` payload gains three keys:

| Key | Type | Source |
| --- | ---- | ------ |
| `system_prompt_sha256` | string (hex, 64 chars) | SHA-256 of the assembled system prompt the model actually saw |
| `l0_count` | integer | number of L0 rows in the assembled prompt |
| `l1_count` | integer | number of L1 rows in the assembled prompt |

`prompt_sha256` keeps its current semantics (base agent_planner.md prompt only). Operators can detect base-prompt drift independently of L0/L1 drift.

### Payload key count after this slice

Per the recent automatic-floor-inference work, `plan.formulate` is currently 14 / 15 keys depending on source. After this slice:

| Source | Keys before | Keys after |
| ------ | ----------- | ---------- |
| `default` | 14 | **17** |
| `cli_inferred` | 15 | **18** |
| `operator` | 14 | **17** |
| `agent_raised` | 14 | **17** |

Shape pins in `inner_loop::tests` (currently `pins_fourteen_keys_for_default_source` etc.) get renamed/updated to the new counts.

### Audit-row JSON sketch

```json
{
  "task_id": 1234,
  "lane": "fast",
  "iteration": 1,
  "prompt_name": "agent_planner",
  "prompt_sha256": "abc...64chars",
  "llm_model": "qwen3.6:35b-a3b",
  "llm_backend": "local",
  "latency_ms": 412,
  "retry_count": 0,
  "decision_kind": "plan",
  "classification_floor": "Public",
  "classification_floor_source": "default",
  "plan": { ... },
  "system_prompt_sha256": "def...64chars",
  "l0_count": 2,
  "l1_count": 0
}
```

## Error handling

Fail-closed throughout. Any path that prevents the assembler from running propagates as an error:

- `load_l0_active_default` returns `DbError` → wrapped as `PromptAssemblyError::Memory` → wrapped as `AgentError::PromptAssembly` → surfaced by `formulate_plan` to the scheduler's retry policy.
- `load_l1_default` same path.
- Assembler itself cannot fail (pure function returning `String`).

The scheduler's existing retry policy (transient retry, decode permanent) treats `AgentError::PromptAssembly` as a transient error if the underlying `DbError` is transient (connection-shaped); decode-shaped DB errors would be permanent. Today the existing inner-loop treats every `AgentError` other than `Decode` as transient, so this slice's wiring inherits that posture.

No silent fallback to "base prompt only" — running with degraded safety context is more dangerous than failing the iteration. The L0 layer is load-bearing for the agent's constitutional posture.

## Testing strategy

### Unit tests (in `assemble.rs`)

- `assembles_empty_l0_empty_l1_to_base_only` — base passes through verbatim, no tags emitted at all except `<base>`.
- `assembles_l0_only_skips_l1_section` — pinned: no `<l1_insights>` in output.
- `assembles_l1_only_skips_l0_section` — pinned: no `<l0_meta_rules>` in output.
- `assembles_both_layers_with_separator` — both sections present, blank line separator between them and before `<base>`.
- `row_rendering_uses_bullet_prefix` — every body line prefixed `- `.
- `multi_line_body_renders_verbatim` — a body containing `\n` is not re-bulleted; preserves shape.
- `body_with_xml_chars_is_not_escaped` — `<` / `>` in a body pass through; documents the trust posture.
- `output_is_deterministic_for_same_inputs` — two calls with the same `(l0, l1, base)` produce identical strings.
- `row_order_matches_input_order` — assembler does not re-sort.

### Unit tests (in `pg_builder.rs`)

- `static_builder_returns_fixed_string_ignoring_base` — test helper contract; counts are `(0, 0)`.
- `static_builder_empty_constructor_returns_empty` — `StaticSystemPromptBuilder::empty()`.

### DB integration tests (new `core/tests/prompt_assembly_e2e.rs`)

- `pg_builder_build_against_seeded_db` — per-test PG cluster, seed 2 L0 rows + 1 L1 row, call `build("base")`, assert: `system_prompt` starts with `<l0_meta_rules>`, contains `<l1_insights>`, contains `<base>\nbase\n</base>`, `l0_count == 2`, `l1_count == 1`.
- `pg_builder_build_with_empty_db_returns_base_only` — fresh PG, no seeds, `build("base")` returns the `<base>` block only with `l0_count == 0`, `l1_count == 0`.

### E2E test updates (existing files)

- `core/tests/scheduler_inner_loop_e2e.rs` — existing scenarios pass `Arc::new(StaticSystemPromptBuilder::empty())` so they remain byte-stable. One new assertion block extends the existing happy-path test to verify the 3 new `plan.formulate` keys are present in the audit payload.
- `core/tests/cli_ask_e2e.rs` happy-path — payload multiset assertion gains the 3 new keys.
- `core/tests/router_agent_mock_e2e.rs` — constructor update only (pass `StaticSystemPromptBuilder::empty()`).

### Test count delta

Target: **+13 tests** (638 → ~651). 9 unit in `assemble.rs` + 2 unit in `pg_builder.rs` + 2 DB integration in `prompt_assembly_e2e.rs`. Existing `inner_loop::tests` shape-pin tests (`pins_fourteen_keys_for_default_source` etc.) get renamed in place and don't add to the count.

## Implementation order (TDD)

Per CLAUDE.md rule #2, RED → GREEN per task, with a post-review fixup commit allowed per task.

1. **Spec + plan commits** (this doc + writing-plans output).
2. **Task 1 — pure assembler.** Create `core/src/prompt_assembly/{mod.rs, assemble.rs}` with the pure function and 9 unit tests (RED with `todo!()` body, then GREEN by filling the function).
3. **Task 2 — trait + `AssembledPrompt` + `StaticSystemPromptBuilder`.** Add the `SystemPromptBuilder` trait + `PromptAssemblyError` + `AssembledPrompt` struct to `mod.rs`; add `StaticSystemPromptBuilder` to `pg_builder.rs`. 2 unit tests for the static helper.
4. **Task 3 — `PgSystemPromptBuilder` + DB integration.** Fill the prod impl. New `core/tests/prompt_assembly_e2e.rs` with 2 integration tests against a per-test PG cluster.
5. **Task 4 — `FormulationMeta` widening + `RouterAgent` wire-in.** Add 3 fields to `FormulationMeta`. Add `prompt_builder` field to `RouterAgent::new`. Replace `entry.content.clone()` with `self.prompt_builder.build(&base).await?`. Add new `AgentError::PromptAssembly` variant. Update `router_agent_mock_e2e.rs` to construct with the static builder.
6. **Task 5 — audit-payload widening.** Modify `build_plan_formulate_payload` in `inner_loop.rs` to emit the 3 new keys. Update existing `pins_fourteen_keys_for_default_source` and siblings to the new counts. Update `scheduler_inner_loop_e2e.rs` and `cli_ask_e2e.rs` payload assertions.
7. **Task 6 — `main.rs` wire-in.** Construct `PgSystemPromptBuilder` after `connect_runtime_pool`; pass into `RouterAgent::new`.
8. **Task 7 — docs.** Update HANDOVER + ROADMAP.

## Files touched

NEW (3):

- `core/src/prompt_assembly/mod.rs` — trait + error + `AssembledPrompt` struct + re-exports (~70 LOC).
- `core/src/prompt_assembly/assemble.rs` — pure assembler + inline tests (~250 LOC).
- `core/src/prompt_assembly/pg_builder.rs` — `PgSystemPromptBuilder` + `StaticSystemPromptBuilder` + inline tests (~180 LOC).
- `core/tests/prompt_assembly_e2e.rs` — 2 DB integration tests (~140 LOC).

Modified (7):

- `core/src/lib.rs` — `pub mod prompt_assembly;`.
- `core/src/scheduler/agent.rs` — RouterAgent constructor + formulate_plan; new `AgentError::PromptAssembly`.
- `core/src/scheduler/inner_loop.rs` — `FormulationMeta` widening + `build_plan_formulate_payload` +3 keys + payload-shape test renames.
- `core/src/main.rs` — construct `PgSystemPromptBuilder`; pass to `RouterAgent::new`.
- `core/tests/scheduler_inner_loop_e2e.rs` — constructor update + payload assertion extension.
- `core/tests/cli_ask_e2e.rs` — payload assertion extension.
- `core/tests/router_agent_mock_e2e.rs` — constructor update.
- `docs/devel/handovers/HANDOVER.md` + `docs/devel/ROADMAP.md` — session-end update.

## LOC accounting

| File | Before | After | Delta |
| ---- | ------ | ----- | ----- |
| `core/src/prompt_assembly/mod.rs` | (new) | ~70 | +70 |
| `core/src/prompt_assembly/assemble.rs` | (new) | ~250 | +250 |
| `core/src/prompt_assembly/pg_builder.rs` | (new) | ~180 | +180 |
| `core/tests/prompt_assembly_e2e.rs` | (new) | ~140 | +140 |
| `core/src/scheduler/agent.rs` | 145 | ~190 | +45 |
| `core/src/scheduler/inner_loop.rs` | ~870 | ~910 | +40 |

All three new files stay under the 500-LOC soft cap. `inner_loop.rs`'s pre-existing breach extends by ~40 LOC; flagged for the previously-tracked `inner_loop_audit.rs` split when a second contributor lands.

## What this slice deliberately does NOT do

- **No recall lane.** Stays unwired; follow-up slice.
- **No global token cap with priority drop.** No over-budget condition exists with only L0 + L1 + base.
- **No L3 or L4 writers.** Empty layers stay empty.
- **No prompt assembly for reviewer chain.** CG / DP are deterministic Rust; no LLM call, no prompt today.
- **No prompt caching across iterations.** Each plan iteration re-assembles. Cheap (two small queries); ensures fresh state.
- **No metadata in row rendering.** `l0_rule_id` stays out of the prompt body. Available in audit log + source TOML for ops.
- **No body escaping.** Bodies are operator-curated; `<` / `>` in a body pass through verbatim. Tests pin this so a future refactor can't silently regress.
- **No new audit-row actor or action.** Reuses the existing `agent/plan.formulate` row; just widens the payload.

## Open follow-up surfaces

- **Recall-lane wiring** — next natural slice. Needs query embedding (calls `embed_query`, adds an audit row) + (separately) entity extraction for the graph lane.
- **Global token cap with priority drop** — lands when recall + L3 + L4 create a real over-budget condition.
- **L1 promotion writer** — observation-phase signal: which L2 rows get hit often → promote to L1. Until this lands, L1 stays empty in production.
- **L3 (skills) crystallisation** — task-success signal distils trajectory into a parameterised JSON-RPC tool-call template.
- **L4 session digest** — end-of-session summariser writing one L4 row per finished task.
- **`inner_loop.rs` split** — the +40 LOC from this slice nudges the file further over the 500-LOC soft cap. Natural split: lift `build_plan_formulate_payload` + the audit writers into `core/src/scheduler/inner_loop_audit.rs`.
- **Prompt-caching opportunity** — if observation shows the assembled prompt rarely changes within a task, the assembler could memoise per (task_id, l0_revision, l1_revision). Today's two-DB-query cost is small; defer.
- **Reviewer-chain prompt assembly** — when CG/DP gain LLM-backed variants (Phase 2+), the same trait can serve them with a different builder impl.

## Verification step

`cargo test --workspace` on Linux: 638 → ~651 passed, 0 failed, 0 `[SKIP]` lines, 0 warnings. Manual smoke: `hhagent-cli ask "echo marker"` with the L0 seed file in place; inspect the resulting `plan.formulate` row (`hhagent-cli audit tail`) and confirm `l0_count = 2`, `l1_count = 0`, `system_prompt_sha256 != prompt_sha256`.
