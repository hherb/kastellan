# Prompt Assembler (L0 + L1 + base) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** First real consumer of `load_l0_active_default` + `load_l1_default`: assemble the agent's system message from L0 (meta-rules) + L1 (insights) + the existing `agent_planner.md` base, wired through `RouterAgent::formulate_plan`. Add `system_prompt_sha256` + `l0_count` + `l1_count` to the `agent/plan.formulate` audit row.

**Architecture:** New `core::prompt_assembly` module ships a pure `assemble_system_prompt(l0, l1, base) -> String` function and a `SystemPromptBuilder` async trait parallel to the existing `PlanFormulator`. Production impl `PgSystemPromptBuilder` holds a `PgPool` (cheap-clonable; sqlx wraps its connections in an internal `Arc`) and orchestrates the two loaders; test-only `StaticSystemPromptBuilder` returns a fixed string. `RouterAgent` gains an `Arc<dyn SystemPromptBuilder>` field. The trait returns `AssembledPrompt { system_prompt, l0_count, l1_count }` so counts reach the audit row without re-parsing the string.

**Tech Stack:** Rust (workspace at `/home/hherb/src/hhagent`), `sqlx` for Postgres, `async-trait`, `thiserror`, `sha2`, `serde_json`, `tokio`. Branch: `feat/prompt-assembler-l0-l1` (already created at `7062e5e`).

**Spec:** [docs/superpowers/specs/2026-05-16-prompt-assembler-design.md](../specs/2026-05-16-prompt-assembler-design.md)

**Pre-reqs (both shipped on main):** PR #69 (L1 storage) + PR #74 (L0 seed loader).

**Baseline tests on `main` at `3cd6364`:** 638 passed / 0 failed / 0 SKIP / 0 warnings. Target after this plan: **~651** (+13).

---

## Project conventions (read once)

- **Shell setup before any cargo invocation:** `source "$HOME/.cargo/env"`. Cargo isn't on PATH for non-interactive shells.
- **Commit message style:** Conventional commits (`feat(scope):`, `test(scope):`, `docs(scope):`, `fix(scope):`, `chore(scope):`). Always finish with the Co-Authored-By trailer for Claude-assisted commits — see the project's existing `git log` for the exact line.
- **TDD discipline (CLAUDE.md rule #2):** Write the test first, run it, see it fail (RED), implement the minimum to make it pass (GREEN), run again to confirm, commit. Don't skip RED — it pins that the test actually exercises the change.
- **All tests must pass before each commit (CLAUDE.md rule #6):** `cargo test --workspace` clean before every `git commit`. No exceptions without explicit operator approval.
- **File-size soft cap (CLAUDE.md rule #4):** 500 LOC. New files in this plan are sized to stay under. `inner_loop.rs` is already over and we're adding ~40 LOC — flagged in the spec as a future split, not blocking this slice.
- **Junior-readable docs (CLAUDE.md rule #3):** Every `pub` item gets a `///` doc comment explaining WHY (not just WHAT — see [CLAUDE.md](../../../CLAUDE.md) for the rule).

---

## File Structure

NEW files (4):

| Path | Purpose | Target LOC |
| ---- | ------- | ---------- |
| `core/src/prompt_assembly/mod.rs` | Trait + error + `AssembledPrompt` struct + re-exports | ~70 |
| `core/src/prompt_assembly/assemble.rs` | Pure `assemble_system_prompt` + inline unit tests | ~250 |
| `core/src/prompt_assembly/pg_builder.rs` | `PgSystemPromptBuilder` + `StaticSystemPromptBuilder` + unit tests | ~180 |
| `core/tests/prompt_assembly_e2e.rs` | DB integration tests against per-test PG cluster | ~140 |

Modified files (7):

| Path | Change |
| ---- | ------ |
| `core/src/lib.rs` | Add `pub mod prompt_assembly;` |
| `core/src/scheduler/agent.rs` | `RouterAgent::new` gains `prompt_builder` arg; `FormulationMeta` widened by 3 fields; new `AgentError::PromptAssembly` variant; `formulate_plan` calls the builder |
| `core/src/scheduler/inner_loop.rs` | `build_plan_formulate_payload` emits 3 new keys; existing pin tests renamed + assertions widened |
| `core/src/main.rs` | Construct `PgSystemPromptBuilder` and pass into `RouterAgent::new` |
| `core/tests/router_agent_mock_e2e.rs` | Update 3 `RouterAgent::new` call sites (lines 271, 325, 369) to pass the static builder |
| `core/tests/scheduler_inner_loop_e2e.rs` | `ScriptedFormulator::formulate_plan` returns `FormulationMeta` with the 3 new fields populated |
| `core/tests/cli_ask_e2e.rs` | Payload assertion at line ~631 gains 3-key check |

---

## Task 1 — Pure assembler module and `assemble_system_prompt`

**Files:**
- Create: `core/src/prompt_assembly/mod.rs`
- Create: `core/src/prompt_assembly/assemble.rs`
- Modify: `core/src/lib.rs` (add module declaration)

### Step 1.1 — Add the module declaration so RED tests compile

- [ ] **Modify `core/src/lib.rs`:** find the existing `pub mod memory;` line and add `pub mod prompt_assembly;` near it (alphabetical order if the surrounding modules are alphabetized; otherwise just before `pub mod scheduler;`).

Find the existing line:

```sh
grep -n "^pub mod" /home/hherb/src/hhagent/core/src/lib.rs
```

Add this new line in alphabetical position:

```rust
pub mod prompt_assembly;
```

### Step 1.2 — Create `core/src/prompt_assembly/mod.rs` with the minimal module skeleton

- [ ] **Create the file** with this exact content (no trait/error yet — those land in Task 2):

```rust
//! `prompt_assembly` — build the LLM system message from L0 meta-rules,
//! L1 insights, and the existing `agent_planner.md` base.
//!
//! ## Role in the system
//!
//! `RouterAgent::formulate_plan` ([crate::scheduler::agent]) previously
//! sent the bare base prompt as the system message. Now it sends an
//! assembled prompt that frames the L0 layer (hard agent constraints)
//! and L1 layer (insight routing pointers) ahead of the base. The
//! model sees safety + operational context every plan iteration, with
//! a fresh load on each call so operator-edited rules take effect
//! without a daemon restart.
//!
//! ## Module layout
//!
//! * [`assemble::assemble_system_prompt`] — pure: takes `&[Memory]`
//!   slices and a base `&str`, returns the assembled `String`. Empty
//!   layers are omitted entirely (no tag emitted).
//! * [`pg_builder::PgSystemPromptBuilder`] — async impl of
//!   [`SystemPromptBuilder`] that holds a [`PgPool`] and calls
//!   the two loaders before invoking the pure assembler.
//! * [`pg_builder::StaticSystemPromptBuilder`] — test-only impl that
//!   returns a fixed string with `(l0_count, l1_count) = (0, 0)`.
//!
//! ## Why a trait instead of a free function
//!
//! Parallel to the existing [`PlanFormulator`](crate::scheduler::agent::PlanFormulator)
//! seam. Tests swap in the static impl; production wires the PG impl
//! through `RouterAgent::new`. A future recall-aware impl is a new
//! type implementing the same trait, not a rewrite.

pub mod assemble;
pub mod pg_builder;

pub use assemble::assemble_system_prompt;
```

### Step 1.3 — Create the failing `assemble.rs` with the function signature and one test

- [ ] **Create `core/src/prompt_assembly/assemble.rs`:**

```rust
//! Pure prompt assembler. No I/O, no async, no errors.
//!
//! Output framing (always L0 → L1 → base in this order):
//!
//! ```text
//! <l0_meta_rules>
//! - {body of newest L0 row per l0_rule_id}
//! - {next L0 row body}
//! </l0_meta_rules>
//!
//! <l1_insights>
//! - {body of L1 row, newest-first}
//! </l1_insights>
//!
//! <base>
//! {agent_planner.md verbatim}
//! </base>
//! ```
//!
//! Rules:
//!
//! 1. Empty layers omit their entire tag block — no `<l1_insights>`
//!    when L1 has zero rows. The `<base>` block is always present.
//! 2. One blank line between sections.
//! 3. Each row renders as `- {body}` (one row per line).
//! 4. Bodies pass through verbatim (no HTML-style escaping of `<` `>`).
//!    Operators curate L0/L1 content; trust posture matches the rest
//!    of the memory store.
//! 5. Deterministic: same `(l0, l1, base)` produces the same bytes.

use hhagent_db::memories::Memory;

/// Render the supplied memory slices and base prompt into a single
/// LLM-ready system message.
///
/// See the module-level docstring for the framing rules.
pub fn assemble_system_prompt(l0: &[Memory], l1: &[Memory], base: &str) -> String {
    let mut out = String::new();

    if !l0.is_empty() {
        out.push_str("<l0_meta_rules>\n");
        for row in l0 {
            out.push_str("- ");
            out.push_str(&row.body);
            out.push('\n');
        }
        out.push_str("</l0_meta_rules>\n\n");
    }

    if !l1.is_empty() {
        out.push_str("<l1_insights>\n");
        for row in l1 {
            out.push_str("- ");
            out.push_str(&row.body);
            out.push('\n');
        }
        out.push_str("</l1_insights>\n\n");
    }

    out.push_str("<base>\n");
    out.push_str(base);
    if !base.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("</base>\n");

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use hhagent_db::memories::{Memory, MemoryLayer};
    use time::OffsetDateTime;

    /// Construct a minimal `Memory` for tests. `id` is set to a stable
    /// 1-based index so test failures are debuggable; `created_at` is
    /// pinned to the Unix epoch so the value is deterministic.
    fn mem(id: i64, body: &str, layer: MemoryLayer) -> Memory {
        Memory {
            id,
            body: body.to_string(),
            metadata: serde_json::json!({}),
            layer,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn empty_l0_and_l1_emits_base_block_only() {
        let out = assemble_system_prompt(&[], &[], "BASE BODY");
        assert_eq!(
            out,
            "<base>\nBASE BODY\n</base>\n",
            "no L0/L1 → base block alone; got:\n{out}"
        );
    }
}
```

### Step 1.4 — Run the first test to confirm GREEN

- [ ] **Run:**

```sh
source "$HOME/.cargo/env"
cargo test -p hhagent-core --lib prompt_assembly::assemble::tests::empty_l0_and_l1_emits_base_block_only
```

**Expected:** test passes. If the test fails, read the diff in the panic output and fix the function until the assertion holds.

### Step 1.5 — Add the remaining 8 unit tests (RED → GREEN per chunk)

The function above already implements the full contract, so the remaining tests are GREEN-against-existing-code from the start. Add them to the same `tests` module in `core/src/prompt_assembly/assemble.rs`:

- [ ] **Append these tests** below the existing one (inside `mod tests`):

```rust
    #[test]
    fn l0_only_skips_l1_section() {
        let l0 = vec![mem(1, "rule one", MemoryLayer::Meta)];
        let out = assemble_system_prompt(&l0, &[], "BASE");
        assert!(out.starts_with("<l0_meta_rules>\n"), "L0 section first; got:\n{out}");
        assert!(!out.contains("<l1_insights>"), "L1 must be skipped when empty; got:\n{out}");
        assert!(out.contains("<base>\nBASE\n</base>\n"), "base must be present; got:\n{out}");
    }

    #[test]
    fn l1_only_skips_l0_section() {
        let l1 = vec![mem(1, "insight one", MemoryLayer::Index)];
        let out = assemble_system_prompt(&[], &l1, "BASE");
        assert!(!out.contains("<l0_meta_rules>"), "L0 must be skipped when empty; got:\n{out}");
        assert!(out.contains("<l1_insights>\n- insight one\n</l1_insights>"),
                "L1 section present; got:\n{out}");
    }

    #[test]
    fn both_layers_assembled_in_order_with_blank_separators() {
        let l0 = vec![mem(1, "rule one", MemoryLayer::Meta)];
        let l1 = vec![mem(2, "insight one", MemoryLayer::Index)];
        let out = assemble_system_prompt(&l0, &l1, "BASE");
        let expected = concat!(
            "<l0_meta_rules>\n",
            "- rule one\n",
            "</l0_meta_rules>\n",
            "\n",
            "<l1_insights>\n",
            "- insight one\n",
            "</l1_insights>\n",
            "\n",
            "<base>\n",
            "BASE\n",
            "</base>\n",
        );
        assert_eq!(out, expected, "full shape pin");
    }

    #[test]
    fn every_row_renders_with_bullet_prefix() {
        let l0 = vec![
            mem(1, "first", MemoryLayer::Meta),
            mem(2, "second", MemoryLayer::Meta),
            mem(3, "third", MemoryLayer::Meta),
        ];
        let out = assemble_system_prompt(&l0, &[], "BASE");
        for needle in ["- first\n", "- second\n", "- third\n"] {
            assert!(out.contains(needle), "missing {needle:?} in {out}");
        }
    }

    #[test]
    fn multi_line_body_renders_verbatim_without_re_bulleting() {
        // A body with an internal newline is rendered as-is. The contract
        // is "bullet on the first line; continuation lines pass through"
        // — a future refactor that tries to indent continuation lines
        // would break this test deliberately.
        let l0 = vec![mem(1, "line one\nline two", MemoryLayer::Meta)];
        let out = assemble_system_prompt(&l0, &[], "BASE");
        assert!(out.contains("- line one\nline two\n"),
                "multi-line body must pass through verbatim; got:\n{out}");
    }

    #[test]
    fn body_with_xml_chars_is_not_escaped() {
        // Operator-curated content. < and > pass through. A future
        // refactor that adds HTML escaping would break this test
        // deliberately so the team can re-evaluate the trust posture.
        let l0 = vec![mem(1, "guard <secret> and </tag>", MemoryLayer::Meta)];
        let out = assemble_system_prompt(&l0, &[], "BASE");
        assert!(out.contains("- guard <secret> and </tag>\n"),
                "XML chars must pass through; got:\n{out}");
    }

    #[test]
    fn output_is_deterministic_for_same_inputs() {
        let l0 = vec![mem(1, "rule one", MemoryLayer::Meta)];
        let l1 = vec![mem(2, "insight", MemoryLayer::Index)];
        let a = assemble_system_prompt(&l0, &l1, "BASE");
        let b = assemble_system_prompt(&l0, &l1, "BASE");
        assert_eq!(a, b, "same inputs must yield same bytes");
    }

    #[test]
    fn row_order_matches_input_order() {
        // The assembler does not re-sort. Callers are responsible for
        // input ordering (loaders return newest-first today).
        let l0 = vec![
            mem(3, "third-newest", MemoryLayer::Meta),
            mem(2, "second-newest", MemoryLayer::Meta),
            mem(1, "oldest", MemoryLayer::Meta),
        ];
        let out = assemble_system_prompt(&l0, &[], "BASE");
        let idx_a = out.find("- third-newest").expect("first row present");
        let idx_b = out.find("- second-newest").expect("second row present");
        let idx_c = out.find("- oldest").expect("third row present");
        assert!(idx_a < idx_b && idx_b < idx_c,
                "rows must appear in input order; offsets {idx_a}/{idx_b}/{idx_c}");
    }

    #[test]
    fn base_without_trailing_newline_is_normalized() {
        // If the caller passes a base prompt without a terminating
        // newline, the assembler inserts one before `</base>\n` so the
        // closing tag always sits on its own line. This keeps the
        // output shape stable regardless of how the prompt file ends.
        let out_no_nl = assemble_system_prompt(&[], &[], "no trailing nl");
        let out_with_nl = assemble_system_prompt(&[], &[], "with trailing nl\n");
        assert!(out_no_nl.ends_with("with trailing nl\n</base>\n")
                || out_no_nl.ends_with("no trailing nl\n</base>\n"),
                "closing tag must follow a newline; got {out_no_nl:?}");
        assert!(out_with_nl.ends_with("</base>\n"),
                "closing tag must follow a newline; got {out_with_nl:?}");
    }
```

### Step 1.6 — Run the full module's tests

- [ ] **Run:**

```sh
source "$HOME/.cargo/env"
cargo test -p hhagent-core --lib prompt_assembly::assemble::tests
```

**Expected:** 9 tests pass, 0 failed. If any fails, the function in step 1.3 doesn't yet match the contract — re-read the failing assertion and adjust the function (not the test).

### Step 1.7 — Run the full workspace to confirm nothing regressed

- [ ] **Run:**

```sh
source "$HOME/.cargo/env"
cargo test --workspace 2>&1 | tail -5
```

**Expected:** `test result: ok. NNN passed; 0 failed`. Baseline is 638, so NNN should be 647 (638 + 9).

### Step 1.8 — Commit

- [ ] **Commit:**

```sh
git add core/src/lib.rs core/src/prompt_assembly/
git commit -m "$(cat <<'EOF'
feat(core,prompt_assembly): pure assemble_system_prompt + 9 unit tests

New core::prompt_assembly module ships the pure assembler that builds
the agent system message from L0 + L1 + base. Empty layers omit their
tag block entirely; rows render as `- {body}` bullets; output is
deterministic. No I/O, no async, no error path — pure String
construction.

This is the building block for the SystemPromptBuilder trait (Task 2)
and the PgSystemPromptBuilder prod impl (Task 3).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2 — `SystemPromptBuilder` trait, `AssembledPrompt`, `PromptAssemblyError`, `StaticSystemPromptBuilder`

**Files:**
- Modify: `core/src/prompt_assembly/mod.rs` (add trait + error + result struct + re-exports)
- Create: `core/src/prompt_assembly/pg_builder.rs` (static helper only this task; PG impl in Task 3)

### Step 2.1 — Write the failing `StaticSystemPromptBuilder` tests

- [ ] **Create `core/src/prompt_assembly/pg_builder.rs`** with this exact content:

```rust
//! Production + test implementations of [`SystemPromptBuilder`].
//!
//! * [`PgSystemPromptBuilder`] — async DB-backed builder used by
//!   `RouterAgent` in production.
//! * [`StaticSystemPromptBuilder`] — fixed-string builder for tests
//!   that don't care about the assembled shape. Always reports
//!   `(l0_count, l1_count) = (0, 0)` — tests that need non-zero
//!   counts use the prod builder against a per-test PG cluster.

use async_trait::async_trait;
use sqlx::PgPool;

use crate::memory::l0_seed::load_l0_active_default;
use crate::memory::layers::load_l1_default;

use super::{
    assemble::assemble_system_prompt, AssembledPrompt, PromptAssemblyError, SystemPromptBuilder,
};

/// Production builder: loads L0 + L1 from Postgres on every call.
///
/// Each `build` invocation re-runs both loaders so operator edits to
/// the seed file (after restart) and DB-level changes take effect on
/// the next plan iteration. The cost is two small SELECTs; cheap
/// relative to the LLM call that follows.
///
/// Holds [`PgPool`] by value (not `Arc<PgPool>`) to match the
/// codebase convention — `sqlx::PgPool` already wraps its connection
/// pool in an internal `Arc`, so cloning is cheap and ordinary
/// `pool.clone()` at call sites is the established idiom (see e.g.
/// `core::scheduler::tool_dispatch::ToolHostStepDispatcher::new`).
pub struct PgSystemPromptBuilder {
    pool: PgPool,
}

impl PgSystemPromptBuilder {
    /// Construct a builder pinned to the supplied pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
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

/// Test-only fixed-string builder.
///
/// Always returns the same `system_prompt` regardless of the `base`
/// argument. Both counts are `0` (tests requiring real counts use
/// [`PgSystemPromptBuilder`] against a per-test PG cluster). `pub`
/// (not `cfg(test)`) so cross-crate integration tests in
/// `core/tests/*.rs` can use it without a separate dev-dep export.
pub struct StaticSystemPromptBuilder {
    fixed: String,
}

impl StaticSystemPromptBuilder {
    /// Empty-string builder. Most tests use this — the assembled
    /// prompt is empty and the model never sees L0/L1 framing.
    pub fn empty() -> Self {
        Self { fixed: String::new() }
    }

    /// Fixed-string builder. Used by the one test (in this module)
    /// that needs to assert a specific output flowed through.
    pub fn new(fixed: impl Into<String>) -> Self {
        Self { fixed: fixed.into() }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn static_builder_returns_fixed_string_ignoring_base() {
        let b = StaticSystemPromptBuilder::new("FIXED-OUTPUT");
        // The base is ignored — same return regardless of input.
        let r1 = b.build("base one").await.expect("static build never fails");
        let r2 = b.build("base two").await.expect("static build never fails");
        assert_eq!(r1.system_prompt, "FIXED-OUTPUT");
        assert_eq!(r2.system_prompt, "FIXED-OUTPUT");
        assert_eq!(r1.l0_count, 0, "static builder always reports 0 l0 rows");
        assert_eq!(r1.l1_count, 0, "static builder always reports 0 l1 rows");
    }

    #[tokio::test]
    async fn static_builder_empty_constructor_yields_empty_string() {
        let b = StaticSystemPromptBuilder::empty();
        let r = b.build("ignored").await.expect("static build never fails");
        assert_eq!(r.system_prompt, "", "empty constructor yields empty system_prompt");
        assert_eq!(r.l0_count, 0);
        assert_eq!(r.l1_count, 0);
    }
}
```

### Step 2.2 — Update `core/src/prompt_assembly/mod.rs` to declare the trait + types

- [ ] **Replace** the contents of `core/src/prompt_assembly/mod.rs` with:

```rust
//! `prompt_assembly` — build the LLM system message from L0 meta-rules,
//! L1 insights, and the existing `agent_planner.md` base.
//!
//! ## Role in the system
//!
//! `RouterAgent::formulate_plan` ([crate::scheduler::agent]) previously
//! sent the bare base prompt as the system message. Now it sends an
//! assembled prompt that frames the L0 layer (hard agent constraints)
//! and L1 layer (insight routing pointers) ahead of the base. The
//! model sees safety + operational context every plan iteration, with
//! a fresh load on each call so operator-edited rules take effect
//! without a daemon restart.
//!
//! ## Module layout
//!
//! * [`assemble::assemble_system_prompt`] — pure: takes `&[Memory]`
//!   slices and a base `&str`, returns the assembled `String`. Empty
//!   layers are omitted entirely (no tag emitted).
//! * [`pg_builder::PgSystemPromptBuilder`] — async impl of
//!   [`SystemPromptBuilder`] that holds a [`PgPool`] and calls
//!   the two loaders before invoking the pure assembler.
//! * [`pg_builder::StaticSystemPromptBuilder`] — test impl that
//!   returns a fixed string with `(l0_count, l1_count) = (0, 0)`.
//!
//! ## Why a trait instead of a free function
//!
//! Parallel to the existing
//! [`PlanFormulator`](crate::scheduler::agent::PlanFormulator) seam.
//! Tests swap in the static impl; production wires the PG impl
//! through `RouterAgent::new`. A future recall-aware impl is a new
//! type implementing the same trait, not a rewrite.

use async_trait::async_trait;
use hhagent_db::DbError;
use thiserror::Error;

pub mod assemble;
pub mod pg_builder;

pub use assemble::assemble_system_prompt;
pub use pg_builder::{PgSystemPromptBuilder, StaticSystemPromptBuilder};

/// Error returned by [`SystemPromptBuilder::build`] when the underlying
/// memory loaders fail.
///
/// The variant exists primarily so callers (specifically
/// [`crate::scheduler::agent::RouterAgent::formulate_plan`]) can
/// fail-closed on memory-load errors. Running with a degraded prompt
/// (missing L0 → missing constitutional posture) is more dangerous than
/// failing the plan iteration and letting the scheduler retry.
#[derive(Debug, Error)]
pub enum PromptAssemblyError {
    /// One of the layer loaders returned an error from `db::memories`.
    #[error("memory load failed: {0}")]
    Memory(#[from] DbError),
}

/// Result of a [`SystemPromptBuilder::build`] call.
///
/// Carries the assembled `system_prompt` plus the per-layer row counts.
/// The counts come straight from the loader output at the moment of
/// assembly — they cannot drift away from what the model actually saw.
/// `RouterAgent` writes them into the `plan.formulate` audit row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssembledPrompt {
    /// The full system message text the model will see.
    pub system_prompt: String,
    /// Number of L0 (meta-rule) rows that fed into the assembly.
    pub l0_count: usize,
    /// Number of L1 (insight-index) rows that fed into the assembly.
    pub l1_count: usize,
}

/// Async seam between `RouterAgent` and the L0/L1 loaders.
///
/// Production: [`PgSystemPromptBuilder`] (runs the DB loaders).
/// Tests: [`StaticSystemPromptBuilder`] (fixed string + zero counts).
///
/// **Fail-closed contract:** any error from the underlying memory
/// loaders propagates as [`PromptAssemblyError`]. The caller must
/// surface it (don't fall back to base-only — see
/// [`PromptAssemblyError`] docstring for why).
#[async_trait]
pub trait SystemPromptBuilder: Send + Sync {
    /// Assemble a system prompt by combining the loaded L0/L1 rows
    /// with the supplied `base`.
    async fn build(&self, base: &str) -> Result<AssembledPrompt, PromptAssemblyError>;
}
```

### Step 2.3 — Run the new tests; expect RED then GREEN

The trait and `AssembledPrompt` now exist, but `PgSystemPromptBuilder::build` references symbols (`load_l0_active_default`, `load_l1_default`) that already exist on `main` — so the file should compile. The static-builder tests should pass immediately because the impl was written alongside them in step 2.1.

- [ ] **Run:**

```sh
source "$HOME/.cargo/env"
cargo test -p hhagent-core --lib prompt_assembly::pg_builder::tests
```

**Expected:** 2 tests pass.

### Step 2.4 — Workspace regression check

- [ ] **Run:**

```sh
source "$HOME/.cargo/env"
cargo test --workspace 2>&1 | tail -5
```

**Expected:** `test result: ok. 649 passed; 0 failed` (638 baseline + 9 assemble + 2 pg_builder = 649).

### Step 2.5 — Commit

- [ ] **Commit:**

```sh
git add core/src/prompt_assembly/mod.rs core/src/prompt_assembly/pg_builder.rs
git commit -m "$(cat <<'EOF'
feat(core,prompt_assembly): SystemPromptBuilder trait + impls

New trait SystemPromptBuilder + AssembledPrompt result struct +
PromptAssemblyError. PgSystemPromptBuilder is the prod impl
(load_l0_active_default + load_l1_default + assemble_system_prompt).
StaticSystemPromptBuilder is the test seam (fixed string, zero
counts). Parallel to PlanFormulator's trait + RouterAgent pattern.

+2 unit tests pin the static helper's contract; the prod impl is
exercised by Task 3's DB integration tests.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3 — `PgSystemPromptBuilder` DB integration tests

**Files:**
- Create: `core/tests/prompt_assembly_e2e.rs`

### Step 3.1 — Create the integration-test file with the two scenarios

- [ ] **Create `core/tests/prompt_assembly_e2e.rs`:**

```rust
//! End-to-end smoke for [`hhagent_core::prompt_assembly::PgSystemPromptBuilder`].
//!
//! Each scenario brings up its own per-test Postgres cluster (same
//! recipe as `memory_l0_seed_e2e.rs` and `memory_layers_e2e.rs`) so
//! seeded rows cannot drift between scenarios.
//!
//! Skips silently with `[SKIP]` lines on hosts without Postgres or a
//! reachable supervisor; `cargo test -- --nocapture` to see them.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use hhagent_core::prompt_assembly::{PgSystemPromptBuilder, SystemPromptBuilder};
use hhagent_db::memories::{insert_memory_at_layer, seed_meta_memory, MemoryLayer};
use hhagent_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix,
};

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime")
}

#[test]
fn pg_builder_build_against_seeded_db() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "pas-d",
        "pas-l",
        &format!("hhagent-supervisor-test-pg-pa-seeded-{suffix}"),
    );

    rt().block_on(async {
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "prompt-assembly-seeded"}),
        )
        .await
        .expect("probe");

        let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        // Seed 2 L0 rules — each metadata carries an `l0_rule_id` key
        // so `load_active_l0` (which filters on the key) returns them.
        for (rule_id, body) in [("never_rm_rf", "L0 RULE ONE"), ("refusal_terminal", "L0 RULE TWO")] {
            let meta = serde_json::json!({
                "l0_rule_id": rule_id,
                "body_sha256": format!("sha-{rule_id}"),
                "source_path": "test",
                "tags": ["test"],
            });
            seed_meta_memory(&pool, body, &meta, None)
                .await
                .expect("seed L0");
        }

        // Seed 1 L1 row using the non-policy-restricted writer.
        insert_memory_at_layer(
            &pool,
            "L1 INSIGHT ONE",
            &serde_json::json!({}),
            None,
            MemoryLayer::Index,
        )
        .await
        .expect("insert L1");

        let builder = PgSystemPromptBuilder::new(pool.clone());
        let result = builder.build("BASE BODY").await.expect("build");

        assert_eq!(result.l0_count, 2, "two L0 rows seeded: {result:?}");
        assert_eq!(result.l1_count, 1, "one L1 row seeded: {result:?}");
        let s = &result.system_prompt;
        assert!(s.starts_with("<l0_meta_rules>\n"),
                "L0 section first; got:\n{s}");
        assert!(s.contains("- L0 RULE ONE\n"), "L0 rule one missing in:\n{s}");
        assert!(s.contains("- L0 RULE TWO\n"), "L0 rule two missing in:\n{s}");
        assert!(s.contains("<l1_insights>\n- L1 INSIGHT ONE\n</l1_insights>"),
                "L1 section missing/wrong shape; got:\n{s}");
        assert!(s.contains("<base>\nBASE BODY\n</base>\n"),
                "base section missing; got:\n{s}");
    });
}

#[test]
fn pg_builder_build_with_empty_db_returns_base_only() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "pae-d",
        "pae-l",
        &format!("hhagent-supervisor-test-pg-pa-empty-{suffix}"),
    );

    rt().block_on(async {
        hhagent_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "prompt-assembly-empty"}),
        )
        .await
        .expect("probe");

        let pool = hhagent_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        let builder = PgSystemPromptBuilder::new(pool.clone());
        let result = builder.build("BASE BODY").await.expect("build");

        assert_eq!(result.l0_count, 0, "no rows seeded: {result:?}");
        assert_eq!(result.l1_count, 0, "no rows seeded: {result:?}");
        assert_eq!(
            result.system_prompt, "<base>\nBASE BODY\n</base>\n",
            "empty-DB build must return just the <base> block"
        );
    });
}
```

### Step 3.2 — Run the new tests

- [ ] **Run:**

```sh
source "$HOME/.cargo/env"
cargo test -p hhagent-core --test prompt_assembly_e2e -- --nocapture
```

**Expected on a host with Postgres available:** 2 tests pass. On a host without Postgres: 2 `[SKIP]` lines and the tests still report `ok` (skip-as-pass).

### Step 3.3 — Workspace regression check

- [ ] **Run:**

```sh
source "$HOME/.cargo/env"
cargo test --workspace 2>&1 | tail -5
```

**Expected:** `test result: ok. 651 passed; 0 failed` (649 + 2 integration).

### Step 3.4 — Commit

- [ ] **Commit:**

```sh
git add core/tests/prompt_assembly_e2e.rs
git commit -m "$(cat <<'EOF'
test(core,prompt_assembly): PgSystemPromptBuilder DB integration tests

Two scenarios against per-test PG clusters: (1) seeded DB with 2 L0
rules + 1 L1 row → assembled prompt carries all three sections with
correct counts; (2) empty DB → assembled prompt is just the <base>
block with counts (0, 0).

Skip-as-pass on hosts without Postgres or a reachable supervisor.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4 — Widen `FormulationMeta`, wire `RouterAgent::new` to the builder

**Files:**
- Modify: `core/src/scheduler/agent.rs`
- Modify: `core/tests/router_agent_mock_e2e.rs` (3 constructor call sites)
- Modify: `core/tests/scheduler_inner_loop_e2e.rs` (`ScriptedFormulator` impl)

### Step 4.1 — Inspect the current `FormulationMeta` and `RouterAgent` shapes

- [ ] **Skim `core/src/scheduler/agent.rs:22-65`** (the `AgentError`, `FormulationMeta`, `RouterAgent` definitions) and `core/src/scheduler/agent.rs:67-121` (the `formulate_plan` body) so the surgery is targeted, not exploratory.

### Step 4.2 — Widen `AgentError` with the `PromptAssembly` variant

- [ ] **Modify `core/src/scheduler/agent.rs`:** find the `AgentError` enum (around line 22) and add the new variant. The full enum becomes:

```rust
#[derive(Debug, Error)]
pub enum AgentError {
    #[error("router: {0}")]
    Router(#[from] RouterError),
    #[error("plan decode failed: {detail}")]
    Decode { detail: String, raw: String },
    #[error("agent prompt 'agent_planner' not found in cache")]
    PromptMissing,
    /// L0/L1 load failed under the [`SystemPromptBuilder`]; the scheduler's
    /// retry policy decides whether to retry or fail permanently.
    #[error("prompt assembly: {0}")]
    PromptAssembly(#[from] crate::prompt_assembly::PromptAssemblyError),
}
```

### Step 4.3 — Widen `FormulationMeta` with 3 new fields

- [ ] **Modify `core/src/scheduler/agent.rs`:** find the `FormulationMeta` struct and add 3 fields at the bottom. The full struct becomes:

```rust
/// Returned alongside the decoded `Plan`. The inner loop writes
/// these fields into the `plan.formulate` audit-log row payload.
#[derive(Clone, Debug)]
pub struct FormulationMeta {
    pub prompt_name: String,
    pub prompt_sha256: String,
    pub llm_model: String,
    pub llm_backend: String,
    pub latency_ms: u64,
    pub retry_count: u32,
    /// SHA-256 (hex) of the *assembled* system prompt the model
    /// actually saw — distinct from `prompt_sha256`, which is the
    /// base agent_planner.md hash only.
    pub assembled_prompt_sha256: String,
    /// Number of L0 rows the assembler folded in. Operator triage:
    /// 0 here on a clinical task means the L0 seeder didn't run.
    pub l0_count: usize,
    /// Number of L1 rows the assembler folded in. Stays 0 in
    /// production until an L1 promotion writer lands.
    pub l1_count: usize,
}
```

### Step 4.4 — Add `prompt_builder` field and constructor argument to `RouterAgent`

- [ ] **Modify `core/src/scheduler/agent.rs`:** the struct and constructor become:

```rust
/// Production adapter: calls the real `Router::send`.
pub struct RouterAgent {
    router: std::sync::Arc<Router>,
    prompts: std::sync::Arc<PromptCache>,
    prompt_builder: std::sync::Arc<dyn crate::prompt_assembly::SystemPromptBuilder>,
}

impl RouterAgent {
    pub fn new(
        router: std::sync::Arc<Router>,
        prompts: std::sync::Arc<PromptCache>,
        prompt_builder: std::sync::Arc<dyn crate::prompt_assembly::SystemPromptBuilder>,
    ) -> Self {
        Self { router, prompts, prompt_builder }
    }
}
```

### Step 4.5 — Wire the builder into `formulate_plan`

- [ ] **Modify `core/src/scheduler/agent.rs::RouterAgent::formulate_plan`:** locate the line that builds `ChatMessage::system(entry.content.clone())` (currently around line 86) and the `FormulationMeta` construction (currently around line 111). Replace the relevant region so it reads:

```rust
    async fn formulate_plan(
        &self,
        ctx: &TaskContext,
    ) -> Result<(Plan, FormulationMeta), AgentError> {
        let entry = self.prompts.get("agent_planner")
            .ok_or(AgentError::PromptMissing)?;

        let base = entry.content.clone();
        // Assemble L0 + L1 + base BEFORE dialing the LLM so a
        // memory-load error short-circuits the same way as a missing
        // prompt — never run the model with a degraded safety prompt.
        let assembled = self.prompt_builder.build(&base).await
            .map_err(AgentError::PromptAssembly)?;
        let assembled_prompt_sha256 = {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(assembled.system_prompt.as_bytes());
            format!("{:x}", h.finalize())
        };

        let user_msg = serialise_context_for_agent(ctx);

        // Clone the model name before constructing the request so we can
        // reference it later for FormulationMeta without fighting the borrow
        // checker (req is moved into send's &req borrow).
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

        let start = std::time::Instant::now();
        let resp = self.router.send(&req).await?;
        let latency_ms = start.elapsed().as_millis() as u64;

        // ChatMessage.content is String (not Option<String>); take the first
        // choice's message content directly.
        let raw = resp.choices.first()
            .map(|c| c.message.content.clone())
            .unwrap_or_default();

        // Tolerant of markdown-fenced JSON (```json … ```) and short
        // model preambles before the JSON body. See
        // `super::plan_parser::parse_plan_lenient` for the contract.
        let plan: Plan = parse_plan_lenient(&raw).map_err(|e| AgentError::Decode {
            detail: e.to_string(),
            raw: raw.clone(),
        })?;

        let meta = FormulationMeta {
            prompt_name: "agent_planner".into(),
            prompt_sha256: entry.sha256.clone(),
            llm_model: local_model,
            llm_backend: "local".to_string(),
            latency_ms,
            retry_count: 0,
            assembled_prompt_sha256,
            l0_count: assembled.l0_count,
            l1_count: assembled.l1_count,
        };
        Ok((plan, meta))
    }
```

### Step 4.6 — Fix the 3 `RouterAgent::new` call sites in `router_agent_mock_e2e.rs`

The struct now needs a third argument. The mock tests don't care about the assembled prompt content — they assert the wire shape. Pass `Arc::new(StaticSystemPromptBuilder::empty())`.

- [ ] **Modify `core/tests/router_agent_mock_e2e.rs`:** at the top of the file, add the import:

```rust
use hhagent_core::prompt_assembly::StaticSystemPromptBuilder;
```

(If a `use` block is already present, slot the line in alphabetically.)

- [ ] **Modify the three call sites at lines 271, 325, 369** — change each occurrence of:

```rust
let agent = RouterAgent::new(router, prompts);
```

to:

```rust
let agent = RouterAgent::new(router, prompts, Arc::new(StaticSystemPromptBuilder::empty()));
```

If `Arc` is not already imported in this file, add `use std::sync::Arc;` to the top.

### Step 4.7 — Update `ScriptedFormulator::formulate_plan` to fill the 3 new `FormulationMeta` fields

- [ ] **Modify `core/tests/scheduler_inner_loop_e2e.rs`:** find the `FormulationMeta` literal at line 316 (inside `ScriptedFormulator::formulate_plan`). Replace it with:

```rust
            FormulationMeta {
                prompt_name: "agent_planner".into(),
                prompt_sha256: "test".into(),
                llm_model: "test-model".into(),
                llm_backend: "local".into(),
                latency_ms: 1,
                retry_count: 0,
                assembled_prompt_sha256: "test-assembled-sha".into(),
                l0_count: 0,
                l1_count: 0,
            },
```

The exact values don't matter for behaviour — they just need to be present so the struct literal compiles.

### Step 4.8 — Run the full workspace

- [ ] **Run:**

```sh
source "$HOME/.cargo/env"
cargo test --workspace 2>&1 | tail -5
```

**Expected:** `test result: ok. 651 passed; 0 failed`. The number is unchanged from Task 3 because Task 4 doesn't add tests — it updates existing call sites to compile against the wider signatures. **If you see compile errors** about missing `l0_count` / `l1_count` / `assembled_prompt_sha256` in other test files, search for the offending `FormulationMeta {` literal and update it the same way as step 4.7.

To find any remaining call sites:

```sh
grep -rn "FormulationMeta {" /home/hherb/src/hhagent/core/src /home/hherb/src/hhagent/core/tests
```

Every occurrence must initialize the new fields.

### Step 4.9 — Commit

- [ ] **Commit:**

```sh
git add core/src/scheduler/agent.rs core/tests/router_agent_mock_e2e.rs core/tests/scheduler_inner_loop_e2e.rs
git commit -m "$(cat <<'EOF'
feat(core,scheduler): RouterAgent wires through SystemPromptBuilder

RouterAgent::new gains an Arc<dyn SystemPromptBuilder> argument;
formulate_plan calls builder.build(&base) before constructing the
ChatRequest, so the model sees an L0+L1+base assembled prompt
instead of the bare agent_planner.md. New AgentError::PromptAssembly
variant carries memory-load failures.

FormulationMeta widened by 3 fields (assembled_prompt_sha256,
l0_count, l1_count) — populated by the builder's AssembledPrompt
result, ready for the audit row widening in Task 5.

router_agent_mock_e2e.rs and scheduler_inner_loop_e2e.rs constructor
call sites updated to pass the new field; behaviour byte-stable for
those tests (StaticSystemPromptBuilder::empty() yields no L0/L1
content).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5 — Widen `plan.formulate` audit-row payload with 3 new keys

**Files:**
- Modify: `core/src/scheduler/inner_loop.rs` (the `build_plan_formulate_payload` function + its existing pin tests)
- Modify: `core/tests/cli_ask_e2e.rs` (the payload assertion loop at line ~640)

### Step 5.1 — Add the 3 new key emissions to `build_plan_formulate_payload`

- [ ] **Modify `core/src/scheduler/inner_loop.rs:432-504`:** in `build_plan_formulate_payload`, append three `obj.insert` calls right before the existing `if classification_floor_source == ClassificationFloorSource::CliInferred` block (i.e. between line 485's `obj.insert("classification_floor_source"...` and the `if` at line 495). The new emissions:

```rust
    // Slice C (prompt-assembler, 2026-05-16): drift detection for
    // L0/L1 across daemon restarts and operator edits. `prompt_sha256`
    // above is the BASE prompt only; `system_prompt_sha256` here is
    // the assembled prompt the model actually saw.
    obj.insert(
        "system_prompt_sha256".into(),
        serde_json::json!(meta.assembled_prompt_sha256),
    );
    obj.insert("l0_count".into(), serde_json::json!(meta.l0_count));
    obj.insert("l1_count".into(), serde_json::json!(meta.l1_count));
```

### Step 5.2 — Rename `pins_fourteen_keys_for_default_source` → `_seventeen_keys_` and update the expected set

- [ ] **Modify `core/src/scheduler/inner_loop.rs:799-842`:** rename the test function and update its `expected` set. The full test becomes:

```rust
    #[test]
    fn build_plan_formulate_payload_pins_seventeen_keys_for_default_source() {
        // Pin the total key count so a future additive change to the
        // wire shape becomes a deliberate, reviewable edit instead of
        // an accidental drift. Default source: 17 keys (no signals).
        let plan = Plan {
            context: "".into(),
            decision: "task_complete".into(),
            rationale: "".into(),
            steps: vec![],
            result: Some(serde_json::json!({"kind": "text", "body": "ok"})),
            data_ceiling: DataClass::Public,
            refused: None,
            floor_request: None,
        };
        let meta = FormulationMeta {
            prompt_name: "agent_planner".into(),
            prompt_sha256: "x".into(),
            llm_model: "m".into(),
            llm_backend: "local".into(),
            latency_ms: 0,
            retry_count: 0,
            assembled_prompt_sha256: "ax".into(),
            l0_count: 0,
            l1_count: 0,
        };
        let payload = build_plan_formulate_payload(
            1, 0, DataClass::Public,
            ClassificationFloorSource::Default, &[],
            &plan, &meta,
        );
        let keys: std::collections::BTreeSet<&str> = payload
            .as_object()
            .expect("payload is a JSON object")
            .keys()
            .map(|s| s.as_str())
            .collect();
        let expected: std::collections::BTreeSet<&str> = [
            "task_id", "plan_count", "prompt_name", "prompt_sha256",
            "llm_model", "llm_backend", "latency_ms", "retry_count",
            "plan_step_count", "decision_kind", "refused",
            // Slice A additions:
            "plan", "classification_floor",
            // Slice B (automatic floor inference):
            "classification_floor_source",
            // Slice C (prompt assembler, this commit):
            "system_prompt_sha256", "l0_count", "l1_count",
        ].into_iter().collect();
        assert_eq!(keys, expected, "payload key set drifted; update the pin deliberately");
    }
```

### Step 5.3 — Update `default_source_omits_signals_key` to assert 17 keys

- [ ] **Modify `core/src/scheduler/inner_loop.rs:844-866`:** the test body needs the 3 new fields in its `FormulationMeta` literal AND the `obj.len()` assertion bumped from 14 to 17. The full test becomes:

```rust
    #[test]
    fn build_plan_formulate_payload_default_source_omits_signals_key() {
        let plan = Plan {
            context: "".into(), decision: "task_complete".into(), rationale: "".into(),
            steps: vec![], result: Some(serde_json::json!({"kind":"text","body":"ok"})),
            data_ceiling: DataClass::Public, refused: None, floor_request: None,
        };
        let meta = FormulationMeta {
            prompt_name: "p".into(), prompt_sha256: "h".into(),
            llm_model: "m".into(), llm_backend: "local".into(),
            latency_ms: 1, retry_count: 0,
            assembled_prompt_sha256: "ah".into(),
            l0_count: 0, l1_count: 0,
        };
        let payload = build_plan_formulate_payload(
            1, 1, DataClass::Public, ClassificationFloorSource::Default, &[], &plan, &meta,
        );
        let obj = payload.as_object().expect("payload is an object");
        assert_eq!(obj.len(), 17,
            "default-source payload should have 17 keys; got {} keys: {:?}",
            obj.len(), obj.keys().collect::<Vec<_>>());
        assert_eq!(obj["classification_floor_source"], serde_json::Value::String("default".into()));
        assert!(obj.get("classification_floor_signals").is_none(),
            "signals key must be ABSENT when source is not cli_inferred");
    }
```

### Step 5.4 — Update `cli_inferred_source_has_15_keys_with_signals` to assert 18 keys + rename

- [ ] **Modify `core/src/scheduler/inner_loop.rs:868-896`:** rename function and bump the assertion. The full test becomes:

```rust
    #[test]
    fn build_plan_formulate_payload_cli_inferred_source_has_18_keys_with_signals() {
        let plan = Plan {
            context: "".into(), decision: "task_complete".into(), rationale: "".into(),
            steps: vec![], result: Some(serde_json::json!({"kind":"text","body":"ok"})),
            data_ceiling: DataClass::ClinicalConfidential, refused: None, floor_request: None,
        };
        let meta = FormulationMeta {
            prompt_name: "p".into(), prompt_sha256: "h".into(),
            llm_model: "m".into(), llm_backend: "local".into(),
            latency_ms: 1, retry_count: 0,
            assembled_prompt_sha256: "ah".into(),
            l0_count: 0, l1_count: 0,
        };
        let signals = vec!["patient".to_string(), "pathology".to_string()];
        let payload = build_plan_formulate_payload(
            1, 1, DataClass::ClinicalConfidential,
            ClassificationFloorSource::CliInferred, &signals,
            &plan, &meta,
        );
        let obj = payload.as_object().expect("payload is an object");
        assert_eq!(obj.len(), 18,
            "cli_inferred payload should have 18 keys (default 17 + signals); got {} keys: {:?}",
            obj.len(), obj.keys().collect::<Vec<_>>());
        assert_eq!(obj["classification_floor_source"], serde_json::Value::String("cli_inferred".into()));
        let arr = obj["classification_floor_signals"].as_array()
            .expect("signals key is an array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0], serde_json::Value::String("patient".into()));
        assert_eq!(arr[1], serde_json::Value::String("pathology".into()));
    }
```

### Step 5.5 — Update `agent_raised_source_omits_signals` to assert 17 keys

- [ ] **Modify `core/src/scheduler/inner_loop.rs:898-924`:** the assertion bump. The full test becomes:

```rust
    #[test]
    fn build_plan_formulate_payload_agent_raised_source_omits_signals() {
        // After an agent raise, signals are cleared — they only explain the
        // original CLI inference, not the elevated floor.
        let plan = Plan {
            context: "".into(), decision: "task_complete".into(), rationale: "".into(),
            steps: vec![], result: None,
            data_ceiling: DataClass::ClinicalConfidential, refused: None,
            floor_request: Some(DataClass::ClinicalConfidential),
        };
        let meta = FormulationMeta {
            prompt_name: "p".into(), prompt_sha256: "h".into(),
            llm_model: "m".into(), llm_backend: "local".into(),
            latency_ms: 1, retry_count: 0,
            assembled_prompt_sha256: "ah".into(),
            l0_count: 0, l1_count: 0,
        };
        let payload = build_plan_formulate_payload(
            1, 1, DataClass::ClinicalConfidential,
            ClassificationFloorSource::AgentRaised,
            &[],  // empty: signals are cleared on raise
            &plan, &meta,
        );
        let obj = payload.as_object().expect("payload is an object");
        assert_eq!(obj.len(), 17,
            "agent_raised should have 17 keys (no signals); got: {:?}", obj.keys().collect::<Vec<_>>());
        assert_eq!(obj["classification_floor_source"], serde_json::Value::String("agent_raised".into()));
        assert!(obj.get("classification_floor_signals").is_none());
    }
```

### Step 5.6 — Update `carries_full_plan_and_classification_floor` to include the new meta fields

- [ ] **Modify `core/src/scheduler/inner_loop.rs:742-796`:** the test's `meta` literal needs the 3 new fields. Inside the test, the literal becomes:

```rust
        let meta = FormulationMeta {
            prompt_name: "agent_planner".into(),
            prompt_sha256: "deadbeef".into(),
            llm_model: "gemma4:26b".into(),
            llm_backend: "local".into(),
            latency_ms: 42,
            retry_count: 0,
            assembled_prompt_sha256: "abcdef".into(),
            l0_count: 0,
            l1_count: 0,
        };
```

The body of the test (assertions on `plan_back`, `classification_floor`, etc.) stays unchanged.

### Step 5.7 — Update `cli_ask_e2e.rs` payload-loop assertion to check the 3 new keys

- [ ] **Modify `core/tests/cli_ask_e2e.rs:631-650`:** find the `for (i, row) in plan_rows.iter().enumerate()` loop. Inside the loop, after the existing `classification_floor_source` and `classification_floor_signals` assertions, add three more:

```rust
            // Slice C (prompt assembler, 2026-05-16): the three new
            // keys are present on every plan.formulate row. The exact
            // SHA varies across runs because the assembled prompt
            // includes the L0 starter rules; just assert presence + shape.
            assert!(p.get("system_prompt_sha256")
                .and_then(|v| v.as_str())
                .map(|s| s.len() == 64)
                .unwrap_or(false),
                "plan.formulate row {i} must carry system_prompt_sha256 as a 64-char hex string; got {p:?}");
            assert!(p.get("l0_count").and_then(|v| v.as_u64()).is_some(),
                "plan.formulate row {i} must carry numeric l0_count; got {p:?}");
            assert!(p.get("l1_count").and_then(|v| v.as_u64()).is_some(),
                "plan.formulate row {i} must carry numeric l1_count; got {p:?}");
```

### Step 5.8 — Run the modified-only tests first

- [ ] **Run:**

```sh
source "$HOME/.cargo/env"
cargo test -p hhagent-core --lib scheduler::inner_loop::tests::build_plan_formulate
```

**Expected:** all `build_plan_formulate_*` tests pass (including the renamed ones).

### Step 5.9 — Workspace regression check

- [ ] **Run:**

```sh
source "$HOME/.cargo/env"
cargo test --workspace 2>&1 | tail -5
```

**Expected:** `test result: ok. 651 passed; 0 failed`. Count is unchanged from Task 3 because Task 5 only modifies / renames existing tests. **If `cli_ask_e2e` skips** because Postgres isn't reachable, that's expected; you'll re-verify in Task 6 after wiring main.rs.

### Step 5.10 — Commit

- [ ] **Commit:**

```sh
git add core/src/scheduler/inner_loop.rs core/tests/cli_ask_e2e.rs
git commit -m "$(cat <<'EOF'
feat(core,audit): plan.formulate carries L0/L1 drift signal

build_plan_formulate_payload emits 3 new keys: system_prompt_sha256,
l0_count, l1_count. Lets an operator detect L0/L1 drift across daemon
restarts (and trivially triage "L0 seeder didn't run" by spotting an
unexpected 0 in l0_count).

Existing 14/15-key pin tests renamed and bumped to 17/18 keys.
cli_ask_e2e happy-path payload loop asserts presence + shape of the
new keys on every plan.formulate row.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6 — `main.rs` wire-in: construct `PgSystemPromptBuilder` and pass to `RouterAgent::new`

**Files:**
- Modify: `core/src/main.rs`

### Step 6.1 — Update the `RouterAgent::new` call site to pass the builder

- [ ] **Modify `core/src/main.rs:114-118`:** locate the `let formulator: Arc<dyn ...> = Arc::new(...RouterAgent::new(...))` block. Update it to construct + pass the `PgSystemPromptBuilder`:

```rust
    let prompt_builder: std::sync::Arc<
        dyn hhagent_core::prompt_assembly::SystemPromptBuilder,
    > = std::sync::Arc::new(
        hhagent_core::prompt_assembly::PgSystemPromptBuilder::new(pool.clone()),
    );

    let formulator: Arc<dyn hhagent_core::scheduler::agent::PlanFormulator> =
        Arc::new(hhagent_core::scheduler::agent::RouterAgent::new(
            router.clone(),
            prompts.clone(),
            prompt_builder,
        ));
```

Note: the existing `pool` binding in `main.rs` is `sqlx::PgPool` (the type returned by `connect_runtime_pool`). `pool.clone()` is the idiomatic call — `PgPool` wraps its connections in an internal `Arc`, so the clone is cheap. The other downstream call sites (`ToolHostStepDispatcher::new(pool.clone(), ...)` at line 140, and `spawn_scheduler(pool.clone(), ...)` at line 147) use the same pattern.

### Step 6.2 — Build to confirm main.rs compiles

- [ ] **Run:**

```sh
source "$HOME/.cargo/env"
cargo build -p hhagent-core --bins 2>&1 | tail -10
```

**Expected:** clean build (or warning-free).

### Step 6.3 — Run the full workspace including `cli_ask_e2e` (which exercises the bin)

- [ ] **Run:**

```sh
source "$HOME/.cargo/env"
cargo test --workspace 2>&1 | tail -5
```

**Expected:** `test result: ok. 651 passed; 0 failed`. The `cli_ask_e2e` happy-path now exercises the full chain with the L0 starter rules loaded — and the payload loop from step 5.7 confirms every `plan.formulate` row carries the 3 new keys with the right shape.

If `cli_ask_e2e` is the test that fails, the most likely cause is that the `system_prompt_sha256` length isn't 64 chars (a stale `assembled_prompt_sha256` from a `StaticSystemPromptBuilder` somewhere). Re-check that the binary is built with the changes from Task 4 (`cargo clean -p hhagent-core && cargo build -p hhagent-core --bins`) if in doubt.

### Step 6.4 — Manual smoke (optional but recommended)

- [ ] **Optional verification** (requires the local LLM running, per `~/.claude/projects/-home-hherb-src-hhagent/memory/user_local_inference_setup.md`):

```sh
source "$HOME/.cargo/env"
./target/debug/hhagent-cli ask "echo marker"
```

Expected: exits 0, output contains `marker`. If you want to see the new audit keys, in another terminal:

```sh
./target/debug/hhagent-cli audit tail --since 1m
```

You should see two `agent/plan.formulate` rows whose payload contains `"l0_count": 2, "l1_count": 0, "system_prompt_sha256": "<64-char-hex>"`.

### Step 6.5 — Commit

- [ ] **Commit:**

```sh
git add core/src/main.rs
git commit -m "$(cat <<'EOF'
feat(core,main): wire PgSystemPromptBuilder into RouterAgent

main.rs constructs the prod prompt builder against the runtime pool
right before instantiating RouterAgent, so the daemon's plan path
now sees the assembled L0+L1+base system prompt on every iteration.

cli_ask_e2e happy-path exercises the full chain end-to-end against
the per-test PG cluster + starter L0 rules and confirms every
plan.formulate audit row carries the 3 new keys.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7 — HANDOVER + ROADMAP update

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`
- Modify: `docs/devel/ROADMAP.md`

### Step 7.1 — Update `HANDOVER.md` header + insert a new "Recently completed" entry

- [ ] **At the top of `docs/devel/handovers/HANDOVER.md`:** update the `Last updated:` and `Last commit:` lines. Replace the existing two header lines (lines 7–8) with:

```markdown
**Last updated:** 2026-05-16 (prompt assembler — shipped on branch `feat/prompt-assembler-l0-l1`; new `core::prompt_assembly` module + `SystemPromptBuilder` trait + `PgSystemPromptBuilder`/`StaticSystemPromptBuilder` impls + 3 new `plan.formulate` audit-row keys; +13 tests; workspace 638 → **651**).
**Last commit (branch HEAD):** _(filled by the operator at PR open time — run `git rev-parse HEAD` to confirm)_. 8 commits on branch off `main` at `3cd6364`: spec (`7062e5e`) + plan → Task 1 → Task 2 → Task 3 → Task 4 → Task 5 → Task 6 → Task 7 docs (this commit).
```

### Step 7.2 — Add a "Recently completed (this session, 2026-05-16 — prompt assembler)" section

- [ ] **In `docs/devel/handovers/HANDOVER.md`:** find the existing `## Recently completed (this session, 2026-05-16 — L0 seed data loader, branch \`feat/l0-seed-loader\`)` section. Immediately above that heading, insert a new section:

```markdown
## Recently completed (this session, 2026-05-16 — prompt assembler L0 + L1 wiring, branch `feat/prompt-assembler-l0-l1`)

Branch: `feat/prompt-assembler-l0-l1` (off `main` at `3cd6364`). Spec: [`docs/superpowers/specs/2026-05-16-prompt-assembler-design.md`](../../superpowers/specs/2026-05-16-prompt-assembler-design.md). Plan: [`docs/superpowers/plans/2026-05-16-prompt-assembler.md`](../../superpowers/plans/2026-05-16-prompt-assembler.md). First real consumer of `load_l0_active_default` + `load_l1_default` (shipped by PR #69 + PR #74).

**Shape (4 NEW + 5 modified):**

- **NEW `core/src/prompt_assembly/`** (3 files, ~500 LOC total). Public surface: pure `assemble_system_prompt(l0, l1, base) -> String`, async `SystemPromptBuilder` trait returning `AssembledPrompt { system_prompt, l0_count, l1_count }`, prod `PgSystemPromptBuilder` (PgPool-backed), test `StaticSystemPromptBuilder`.
- **NEW `core/tests/prompt_assembly_e2e.rs`** — 2 DB integration scenarios: seeded DB (2 L0 + 1 L1) → expected shape with correct counts; empty DB → `<base>` block only with `(0, 0)` counts.
- **`core/src/scheduler/agent.rs`** — `RouterAgent::new` gains an `Arc<dyn SystemPromptBuilder>` argument; `FormulationMeta` widened by 3 fields (`assembled_prompt_sha256`, `l0_count`, `l1_count`); new `AgentError::PromptAssembly` variant; `formulate_plan` calls the builder before constructing the `ChatRequest`.
- **`core/src/scheduler/inner_loop.rs`** — `build_plan_formulate_payload` emits 3 new keys (`system_prompt_sha256`, `l0_count`, `l1_count`); existing 14/15-key pin tests renamed and bumped to 17/18 keys.
- **`core/src/main.rs`** — constructs `PgSystemPromptBuilder` against the runtime pool and passes into `RouterAgent::new`.
- **`core/tests/router_agent_mock_e2e.rs`** + **`core/tests/scheduler_inner_loop_e2e.rs`** + **`core/tests/cli_ask_e2e.rs`** — constructor and payload-assertion updates.

**Audit-row contract (the headline):**

| When | actor | action | payload keys (before → after) |
| ---- | ----- | ------ | ----------------------------- |
| Agent emits plan (default source) | agent | `plan.formulate` | 14 → **17** keys |
| Agent emits plan (cli_inferred source) | agent | `plan.formulate` | 15 → **18** keys |
| Agent emits plan (operator source) | agent | `plan.formulate` | 14 → **17** keys |
| Agent emits plan (agent_raised source) | agent | `plan.formulate` | 14 → **17** keys |

Pure-additive; existing JSONB consumers (replay harness, observation captures) keep working unchanged.

**Test count delta:** **638 → 651** (+13: 9 unit in `assemble.rs` + 2 unit in `pg_builder.rs` + 2 DB integration in `prompt_assembly_e2e.rs`). Zero failures, zero warnings, zero `[SKIP]` lines on Linux.

**What this slice deliberately does NOT do** (matches the spec's non-goals):

- **No recall lane wiring.** Semantic/lexical/graph search stays unwired. Next natural slice.
- **No global token cap with priority drop.** Both L0 and L1 already enforce per-loader caps; no over-budget condition exists today.
- **No L3 / L4 writers.** Empty layers stay empty.
- **No prompt assembly for reviewer chain.** CG / DP are deterministic Rust today.
- **No prompt caching across iterations.** Two small DB queries per plan iteration; cheap relative to the LLM call.
- **No metadata in row rendering.** `l0_rule_id` stays out of the prompt body; still in audit + source TOML.

**Open follow-up surfaces (not blocking):**

- **Recall-lane wiring** — next natural slice. Needs query embedding + (separately) entity extraction for graph seeds.
- **L1 promotion writer** — until this lands, L1 stays empty in production (`l1_count = 0` on every audit row).
- **`inner_loop.rs` split** — the +40 LOC nudges the file further over the 500-LOC soft cap. Natural split: lift `build_plan_formulate_payload` + the audit writers into `core/src/scheduler/inner_loop_audit.rs`.
- **Replay-harness refresh** — pre-Slice-C captures don't carry the 3 new keys. Re-capture turns them into harness inputs that exercise drift detection.

**Files touched (4 NEW + 5 modified + 2 docs + 1 plan + 1 spec):**

- NEW `core/src/prompt_assembly/mod.rs` + `assemble.rs` + `pg_builder.rs`.
- NEW `core/tests/prompt_assembly_e2e.rs`.
- NEW `docs/superpowers/specs/2026-05-16-prompt-assembler-design.md`.
- NEW `docs/superpowers/plans/2026-05-16-prompt-assembler.md`.
- `core/src/lib.rs` — `pub mod prompt_assembly;`.
- `core/src/scheduler/agent.rs` — `RouterAgent` widening + `FormulationMeta` widening + new error variant.
- `core/src/scheduler/inner_loop.rs` — `build_plan_formulate_payload` +3 keys + pin-test renames.
- `core/src/main.rs` — `PgSystemPromptBuilder` wire-in.
- `core/tests/router_agent_mock_e2e.rs` + `scheduler_inner_loop_e2e.rs` + `cli_ask_e2e.rs` — constructor and payload-assertion updates.
- `docs/devel/handovers/HANDOVER.md` + `docs/devel/ROADMAP.md` — this update.

---
```

### Step 7.3 — Update `ROADMAP.md` to mark prompt assembler shipped

- [ ] **Locate the L0 seed loader entry** in `docs/devel/ROADMAP.md` (around line 112 per the earlier grep). After the L0 entry, append a new `[x]` line for the prompt assembler:

```markdown
- [x] **Prompt assembler (L0 + L1 + base)** — landed 2026-05-16 on branch `feat/prompt-assembler-l0-l1`. New `core::prompt_assembly` module ships pure `assemble_system_prompt(l0, l1, base) -> String` + async `SystemPromptBuilder` trait (parallel to `PlanFormulator`) + prod `PgSystemPromptBuilder` (PgPool-backed) + test `StaticSystemPromptBuilder`. `RouterAgent::formulate_plan` wires through the trait so every plan iteration sees an L0 + L1 + base assembled prompt instead of the bare `agent_planner.md`. `agent/plan.formulate` audit row widened by 3 keys (`system_prompt_sha256`, `l0_count`, `l1_count`) for cross-restart L0/L1 drift detection. +13 tests (638 → 651). Spec at `docs/superpowers/specs/2026-05-16-prompt-assembler-design.md`; plan at `docs/superpowers/plans/2026-05-16-prompt-assembler.md`. Closes the HANDOVER "Next concrete engineering pickup #3" (`llm_router::build_system_prompt`). Unblocks recall-lane wiring as the next natural slice.
```

### Step 7.4 — Run the workspace one more time for sanity

- [ ] **Run:**

```sh
source "$HOME/.cargo/env"
cargo test --workspace 2>&1 | tail -5
```

**Expected:** `test result: ok. 651 passed; 0 failed`.

### Step 7.5 — Commit

- [ ] **Commit:**

```sh
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md docs/superpowers/plans/2026-05-16-prompt-assembler.md
git commit -m "$(cat <<'EOF'
docs(handover,roadmap): prompt assembler L0+L1 wiring shipped

HANDOVER and ROADMAP record the prompt-assembler slice on branch
feat/prompt-assembler-l0-l1. Plan file at
docs/superpowers/plans/2026-05-16-prompt-assembler.md.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Self-review (run before opening the PR)

After all 7 tasks land:

1. **Workspace clean:** `cargo test --workspace` → 651 passed, 0 failed, 0 SKIP, 0 warnings.
2. **No leftover TODOs:** `grep -rn "TODO\|FIXME\|XXX" core/src/prompt_assembly/ core/tests/prompt_assembly_e2e.rs` should be empty.
3. **File-size soft cap:** `wc -l core/src/prompt_assembly/*.rs core/tests/prompt_assembly_e2e.rs` — each file under 500 LOC.
4. **Branch lineage clean:** `git log --oneline main..HEAD` → 8 commits, one per task plus the spec commit.

If all four pass, the branch is ready to open as a PR.
