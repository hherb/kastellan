# Recall-Lane Wiring Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** First production consumer of `embed_query` (Option O, 2026-05-12) and `recall(SEMANTIC | LEXICAL)` (PR #41, 2026-05-13). Threads retrieved memories into the assembled system prompt as a new `<recalled>` block slotted between L1 and base; adds three audit-row keys (`recalled_memory_ids`, `recall_count`, `recall_query_sha256`) for drift detection.

**Architecture:** New `core::recall_assembly` module ships a `RecalledContext { ids, bodies, query_sha256 }` value type and an async `RecallBuilder` trait parallel to the existing `SystemPromptBuilder`. Production impl `PgRecallBuilder` composes `embed_query` + `recall`; test impl `StaticRecallBuilder` returns a fixed context. The existing `assemble_system_prompt` widens from 3-arg (`l0, l1, base`) to 4-arg (`l0, l1, recalled, base`); `RecalledContext::empty()` reproduces the v1 byte-output. `RouterAgent` gains a second `Arc<dyn RecallBuilder>` constructor argument; `formulate_plan` runs recall, **degrades-and-warns on failure** (recall is enrichment, not policy — distinct from the fail-closed `PromptAssembly` posture), then assembles. `FormulationMeta` widens by 3 fields; the audit-row payload grows by 3 keys (17/18 → 20/21).

**Tech Stack:** Rust (workspace at `/home/hherb/src/kastellan`), `sqlx` for Postgres, `async-trait`, `thiserror`, `sha2`, `tracing`, `tokio`. Branch: `feat/recall-lane-wiring` (already created at `76a342b`).

**Spec:** [docs/superpowers/specs/2026-05-17-recall-lane-wiring-design.md](../specs/2026-05-17-recall-lane-wiring-design.md)

**Pre-reqs (all shipped on `main`):** PR #29 (`Router::embed`) · PR #41 (`recall` graph lane + `RecallParams`) · PR #54 (`RecallParams::with_seeds` + `RecallModes::SEMANTIC_AND_LEXICAL`) · PR #69 (L1 storage) · PR #74 (L0 seed loader) · PR #75 (prompt assembler L0+L1).

**Baseline tests on `main` at `2f339c3`:** 652 passed / 0 failed / 4 ignored / 0 SKIP / 0 warnings (Linux). Target after this plan: **~664** (+12).

---

## Project conventions (read once)

- **Shell setup before any cargo invocation:** `source "$HOME/.cargo/env"`. Cargo isn't on PATH for non-interactive shells.
- **Commit message style:** Conventional commits (`feat(scope):`, `test(scope):`, `docs(scope):`, `fix(scope):`, `chore(scope):`). Finish with the Co-Authored-By trailer per the project's existing `git log` style.
- **TDD discipline (CLAUDE.md rule #2):** Write the test first, run it, see it fail (RED), implement the minimum to make it pass (GREEN), run again to confirm, commit. The RED step pins that the test actually exercises the change — don't skip it even when the implementation is obvious.
- **All tests must pass before each commit (CLAUDE.md rule #6):** `cargo test --workspace` clean before every `git commit`. No exceptions without explicit operator approval.
- **File-size soft cap (CLAUDE.md rule #4):** 500 LOC. The new files in this plan are sized to stay well under. `inner_loop.rs` (currently 991 LOC, pre-existing breach flagged in HANDOVER) gains ~5 LOC for the three new payload keys; not worsened materially. `scheduler/agent.rs` is 176 LOC and grows to ~210 — still well within.
- **Junior-readable docs (CLAUDE.md rule #3):** Every `pub` item gets a `///` doc comment explaining *why* (not just *what* — see [CLAUDE.md](../../../CLAUDE.md) for the rule).

---

## File Structure

NEW files (3):

| Path | Purpose | Target LOC |
| ---- | ------- | ---------- |
| `core/src/recall_assembly/mod.rs` | Trait + error + `RecalledContext` value type + re-exports | ~180 |
| `core/src/recall_assembly/pg_builder.rs` | `PgRecallBuilder` + `StaticRecallBuilder` + `cap_and_split` helper + unit tests | ~280 |
| `core/tests/recall_assembly_e2e.rs` | DB+mock-LLM integration test against per-test PG cluster + per-test embedding TCP listener | ~250 |

Modified files (8):

| Path | Change |
| ---- | ------ |
| `core/src/lib.rs` | Add `pub mod recall_assembly;` near the existing `pub mod prompt_assembly;` line |
| `core/src/prompt_assembly/assemble.rs` | `assemble_system_prompt` gains `&RecalledContext` parameter; new `<recalled>` block rendered between L1 and base; +2 unit tests |
| `core/src/prompt_assembly/mod.rs` | `AssembledPrompt` gains `recalled_count: usize` field; `SystemPromptBuilder` gains `build_with_recalled(base, &RecalledContext)` method with default impl that delegates from `build` |
| `core/src/prompt_assembly/pg_builder.rs` | `PgSystemPromptBuilder::build_with_recalled` impl (calls the widened assembler); `StaticSystemPromptBuilder` build_with_recalled impl (delegates); the `build` thin shim delegates to `build_with_recalled(base, &RecalledContext::empty())` |
| `core/src/scheduler/agent.rs` | `RouterAgent::new` gains a 4th `recall_builder: Arc<dyn RecallBuilder>` arg; `formulate_plan` calls `recall_builder.build(ctx.instruction)` with degrade-and-warn on Err; `FormulationMeta` gains 3 fields; new `AgentError` variants are not needed (recall errors are swallowed inside `formulate_plan`) |
| `core/src/scheduler/inner_loop.rs` | `build_plan_formulate_payload` emits 3 new keys; existing pin tests for 17/18 keys renamed + bumped to 20/21; the inline `ScriptedFormulator` test fixture (if any) updates accordingly |
| `core/src/main.rs` | Construct `PgRecallBuilder::new(pool.clone(), router.clone())` and pass into `RouterAgent::new` as the 4th argument |
| `core/tests/router_agent_mock_e2e.rs` | Update 3 `RouterAgent::new` call sites to pass `Arc::new(StaticRecallBuilder::empty())` as 4th arg |
| `core/tests/scheduler_inner_loop_e2e.rs` | `ScriptedFormulator::formulate_plan` returns `FormulationMeta` with the 3 new recall fields populated (default to empty/zero); happy-path payload assertions gain 3 presence-and-shape checks |
| `core/tests/scheduler_lanes_e2e.rs` | If it constructs a `RouterAgent`, add the 4th arg (sweep with grep) |
| `core/tests/cli_ask_e2e.rs` | If it asserts the `plan.formulate` payload shape, extend the assertions; otherwise no change (the multiset count stays the same — same row count, more keys per row) |

---

## Task 1 — `recall_assembly` module skeleton + `RecalledContext` value type

**Files:**
- Create: `core/src/recall_assembly/mod.rs`
- Create: `core/src/recall_assembly/pg_builder.rs` (stub only — body lands in Task 4/5)
- Modify: `core/src/lib.rs` (add `pub mod recall_assembly;`)

### Step 1.1 — Add the module declaration in `lib.rs`

- [ ] **Modify `core/src/lib.rs`:** find the existing `pub mod prompt_assembly;` line:

```sh
grep -n "^pub mod" /home/hherb/src/kastellan/core/src/lib.rs
```

Insert `pub mod recall_assembly;` immediately after `pub mod prompt_assembly;` (alphabetical-ish: `prompt_assembly` < `recall_assembly`).

### Step 1.2 — Create `core/src/recall_assembly/mod.rs` with the value type, error, and trait skeleton

- [ ] **Create the file** with this exact content:

```rust
//! `recall_assembly` — runs a per-query retrieval and packages the
//! result for prompt assembly.
//!
//! ## Role in the system
//!
//! Sibling of [`crate::prompt_assembly`]. Both modules run inside
//! `RouterAgent::formulate_plan` before each LLM call:
//!
//! 1. [`RecallBuilder::build`] (this module) — embeds the task
//!    instruction, fans out to `recall(SEMANTIC | LEXICAL)`, and
//!    returns the ranked rows plus a SHA-256 of the query text.
//! 2. [`crate::prompt_assembly::SystemPromptBuilder::build_with_recalled`]
//!    consumes the [`RecalledContext`] and threads it into the
//!    assembled `<l0>/<l1>/<recalled>/<base>` system message.
//!
//! Recall is **enrichment, not policy**: failure here degrades to an
//! empty context with a `tracing::warn!`, and the agent still plans
//! against the L0/L1/base prompt. This is asymmetric to
//! [`crate::prompt_assembly::PromptAssemblyError`], which is
//! fail-closed (a missing L0 rule must never silently reach the
//! model).
//!
//! ## Module layout
//!
//! * [`pg_builder::PgRecallBuilder`] — production impl. Holds a
//!   [`sqlx::PgPool`] and an [`kastellan_llm_router::Router`]; composes
//!   [`crate::memory::embed_query`] + [`crate::memory::recall`].
//! * [`pg_builder::StaticRecallBuilder`] — test impl. Returns a fixed
//!   [`RecalledContext`] regardless of the query string.
//!
//! ## Why a trait instead of a free function
//!
//! Mirrors the [`crate::prompt_assembly::SystemPromptBuilder`] precedent:
//! tests swap in [`pg_builder::StaticRecallBuilder`]; production wires
//! [`pg_builder::PgRecallBuilder`] through `RouterAgent::new`. A future
//! "history-aware" recall (one that includes prior plan iterations in
//! the query text) is a new type implementing the same trait, not a
//! rewrite of the call site.

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::memory::MemoryError;
use kastellan_db::DbError;

pub mod pg_builder;

pub use pg_builder::{PgRecallBuilder, StaticRecallBuilder};

/// Errors returned by [`RecallBuilder::build`].
///
/// Note: the caller in `RouterAgent::formulate_plan` is expected to
/// **swallow** these (treat as [`RecalledContext::empty()`] and emit a
/// `tracing::warn!`). The enum exists so impls can distinguish embed
/// failures from DB failures in logs / tests, not so the agent can
/// retry.
#[derive(Debug, Error)]
pub enum RecallError {
    /// The embedding call (`Router::embed`) failed; see the wrapped
    /// [`MemoryError`] for the specific cause (transport, dim
    /// mismatch, count mismatch).
    #[error("embed_query failed: {0}")]
    EmbedQuery(#[from] MemoryError),
    /// One of the recall lanes (semantic, lexical) returned a DB
    /// error. Wraps [`DbError`] from `core::memory::recall`.
    #[error("recall lane failed: {0}")]
    DbLane(#[from] DbError),
}

/// Output of a [`RecallBuilder::build`] call. By construction
/// `bodies.len() == ids.len()`; both vectors are in fused-rank order
/// (semantic + lexical, fused via RRF; see [`crate::memory::recall`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecalledContext {
    /// Memory ids in fused order, capped at the byte cap (see
    /// [`L_RECALL_CAP_BYTES`]). Written to the `recalled_memory_ids`
    /// audit-row key.
    pub ids: Vec<i64>,
    /// Bodies in the same order as [`Self::ids`]. Cumulative byte
    /// length ≤ [`L_RECALL_CAP_BYTES`]; rows that would breach the
    /// cap are dropped with `tracing::warn!`.
    pub bodies: Vec<String>,
    /// Hex SHA-256 of the query text (the task instruction). Lets
    /// observation phase detect paraphrase-vs-drift across captures.
    /// Always 64 hex chars (SHA-256 of any input, including empty).
    pub query_sha256: String,
}

impl RecalledContext {
    /// The empty/degraded-recall sentinel.
    ///
    /// `query_sha256` is the SHA-256 of the empty byte string so the
    /// field is always 64 hex chars (consumers can pin the length
    /// without a special case for "no recall ran").
    pub fn empty() -> Self {
        let mut h = Sha256::new();
        h.update(b"");
        Self {
            ids: Vec::new(),
            bodies: Vec::new(),
            query_sha256: format!("{:x}", h.finalize()),
        }
    }

    /// True iff zero rows were recalled (the failure-degraded state
    /// also satisfies this).
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
}

/// Hard cap on the cumulative bytes of recalled bodies. Mirrors
/// [`crate::memory::layers::L1_DEFAULT_CAP_BYTES`] (4 KiB). A single
/// row whose body exceeds this cap is dropped entirely with
/// `tracing::warn!` carrying the dropped `memory_id`.
pub const L_RECALL_CAP_BYTES: usize = 4096;

/// Async seam between `RouterAgent` and the embed+recall composition.
///
/// Production: [`PgRecallBuilder`] (runs `embed_query` + `recall`).
/// Tests: [`StaticRecallBuilder`] (fixed context, no I/O).
///
/// **Degrade-and-warn contract:** callers (specifically
/// `RouterAgent::formulate_plan`) are expected to swallow `Err`
/// returns and substitute `RecalledContext::empty()`. The async
/// signature mirrors [`crate::prompt_assembly::SystemPromptBuilder`]
/// so the agent can keep both calls structurally similar.
#[async_trait]
pub trait RecallBuilder: Send + Sync {
    /// Build a [`RecalledContext`] for the given query text.
    async fn build(&self, query: &str) -> Result<RecalledContext, RecallError>;
}

/// Compute the hex SHA-256 of a byte slice. Used by [`PgRecallBuilder`]
/// to populate [`RecalledContext::query_sha256`] and by
/// [`StaticRecallBuilder::with`] in tests.
///
/// Pure helper, no I/O.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_context_is_empty_and_has_64_char_sha256() {
        let c = RecalledContext::empty();
        assert!(c.is_empty());
        assert!(c.ids.is_empty());
        assert!(c.bodies.is_empty());
        // SHA-256 of empty byte string is well-known.
        assert_eq!(
            c.query_sha256,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "query_sha256 of empty input must equal the canonical SHA-256 empty digest"
        );
        assert_eq!(c.query_sha256.len(), 64, "query_sha256 must always be 64 hex chars");
    }

    #[test]
    fn sha256_hex_matches_known_answer_test_for_abc() {
        // NIST FIPS 180-2 test vector for "abc".
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        );
    }
}
```

### Step 1.3 — Create `core/src/recall_assembly/pg_builder.rs` with a stub `StaticRecallBuilder`

- [ ] **Create the file** with this exact content (the production `PgRecallBuilder` body lands in Task 5; this stub gets the trait impl compiling so the other module files don't break the workspace build):

```rust
//! Production + test implementations of [`super::RecallBuilder`].
//!
//! * [`PgRecallBuilder`] — composes [`crate::memory::embed_query`] +
//!   [`crate::memory::recall`] against a [`sqlx::PgPool`] and a shared
//!   [`kastellan_llm_router::Router`].
//! * [`StaticRecallBuilder`] — returns a fixed [`super::RecalledContext`]
//!   regardless of the query string. Always `pub` (not `cfg(test)`)
//!   so cross-crate integration tests in `core/tests/*.rs` can use it.

use async_trait::async_trait;

use super::{sha256_hex, RecallBuilder, RecalledContext, RecallError};

/// Production builder. Body lands in Task 5; the constructor + struct
/// are declared here so the trait impl compiles.
pub struct PgRecallBuilder {
    // Fields land in Task 5 with the body. Keep the struct private
    // to-be-revealed; only `new` is public surface today.
    _placeholder: (),
}

impl PgRecallBuilder {
    /// **Task 5 will replace this** with a real constructor taking
    /// `(PgPool, Arc<Router>)`. Stubbed today so module shape compiles.
    pub fn new() -> Self {
        Self { _placeholder: () }
    }
}

impl Default for PgRecallBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RecallBuilder for PgRecallBuilder {
    async fn build(&self, _query: &str) -> Result<RecalledContext, RecallError> {
        // Task 5 replaces this body. Today: empty context so the
        // module compiles and degrade-and-warn callers behave sanely
        // if the stub is reached (it should not be — `main.rs` wires
        // the real impl in Task 8, which lands together with the
        // Task 5 body).
        Ok(RecalledContext::empty())
    }
}

/// Test-only fixed-context builder.
pub struct StaticRecallBuilder {
    fixed: RecalledContext,
}

impl StaticRecallBuilder {
    /// Empty-context builder. Most tests use this — recall is "off"
    /// and the assembled prompt has no `<recalled>` block.
    pub fn empty() -> Self {
        Self {
            fixed: RecalledContext::empty(),
        }
    }

    /// Construct with an explicit (ids, bodies, query) triple. The
    /// `query_sha256` field is computed automatically so the test
    /// caller doesn't have to hand-hash. Panics if `ids.len() != bodies.len()`.
    pub fn with(ids: Vec<i64>, bodies: Vec<String>, query: &str) -> Self {
        assert_eq!(
            ids.len(),
            bodies.len(),
            "StaticRecallBuilder::with: ids.len() must equal bodies.len()",
        );
        Self {
            fixed: RecalledContext {
                ids,
                bodies,
                query_sha256: sha256_hex(query.as_bytes()),
            },
        }
    }
}

#[async_trait]
impl RecallBuilder for StaticRecallBuilder {
    async fn build(&self, _query: &str) -> Result<RecalledContext, RecallError> {
        Ok(self.fixed.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn static_builder_empty_returns_empty_context() {
        let b = StaticRecallBuilder::empty();
        let c = b.build("anything").await.expect("static build never fails");
        assert!(c.is_empty());
        assert_eq!(c.query_sha256.len(), 64);
    }

    #[tokio::test]
    async fn static_builder_with_returns_fixed_context_ignoring_query_arg() {
        let b = StaticRecallBuilder::with(
            vec![1, 2, 3],
            vec!["a".into(), "b".into(), "c".into()],
            "operator query text",
        );
        let c1 = b.build("ignored").await.expect("static build never fails");
        let c2 = b.build("also ignored").await.expect("static build never fails");
        assert_eq!(c1.ids, vec![1, 2, 3]);
        assert_eq!(c1.bodies, vec!["a", "b", "c"]);
        assert_eq!(c2.ids, vec![1, 2, 3], "second call must return identical context");
        // SHA-256 of "operator query text" — locked so a future
        // refactor changing the hash input (e.g. trimming the query)
        // trips this test immediately.
        let mut h = sha2::Sha256::new();
        use sha2::Digest;
        h.update(b"operator query text");
        let expected = format!("{:x}", h.finalize());
        assert_eq!(c1.query_sha256, expected);
    }

    #[test]
    #[should_panic(expected = "ids.len() must equal bodies.len()")]
    fn static_builder_with_panics_on_length_mismatch() {
        let _ = StaticRecallBuilder::with(vec![1, 2], vec!["only one".into()], "q");
    }
}
```

### Step 1.4 — Compile + run the new module's tests

- [ ] **Run:**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib recall_assembly
```

**Expected:** 5 tests pass (2 in `mod.rs::tests` + 3 in `pg_builder::tests`). Total workspace count rises 652 → 657 transiently — Task 5 will replace the stub `PgRecallBuilder`, but the test counts are stable from this point forward.

### Step 1.5 — Commit Task 1

- [ ] **Run:**

```sh
source "$HOME/.cargo/env"
cargo test --workspace 2>&1 | grep "^test result:" | awk '{p+=$4; f+=$6; i+=$8} END {print "passed:", p, "failed:", f, "ignored:", i}'
```

**Expected:** `passed: 657 failed: 0 ignored: 4`.

- [ ] **Commit:**

```sh
git add core/src/lib.rs core/src/recall_assembly/
git commit -m "$(cat <<'EOF'
feat(core,recall_assembly): module skeleton + RecalledContext + RecallBuilder trait

Stub for the recall-lane wiring slice. Ships the pure value type,
error enum, async trait, and the StaticRecallBuilder test helper.
PgRecallBuilder is a stub returning the empty context — its real body
lands together with the main.rs wire-in in Task 5/8 once the assembler
widening is in place.

+5 unit tests (workspace 652 → 657).

Per spec: docs/superpowers/specs/2026-05-17-recall-lane-wiring-design.md

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2 — Widen `assemble_system_prompt` to take a `RecalledContext`

**Files:**
- Modify: `core/src/prompt_assembly/assemble.rs` (signature + `<recalled>` rendering + 2 new unit tests; existing `empty_l0_and_l1_emits_base_block_only` test gains an `&RecalledContext::empty()` arg)
- Modify: `core/src/prompt_assembly/pg_builder.rs` (existing `PgSystemPromptBuilder::build` body passes `&RecalledContext::empty()` to the widened assembler — preserves the v1 output for the build thin shim until Task 3 introduces `build_with_recalled`)

### Step 2.1 — Write the failing test for the widened signature

- [ ] **Modify `core/src/prompt_assembly/assemble.rs`:** in the existing `#[cfg(test)] mod tests` block, REPLACE the existing `empty_l0_and_l1_emits_base_block_only` test with this updated version (the existing test passes `assemble_system_prompt(&[], &[], "BASE BODY")` with 3 args; the widened function takes 4 so the existing test stops compiling — replace it as the RED step):

```rust
    #[test]
    fn empty_l0_l1_recalled_emits_base_block_only() {
        let out = assemble_system_prompt(
            &[],
            &[],
            &crate::recall_assembly::RecalledContext::empty(),
            "BASE BODY",
        );
        assert_eq!(
            out,
            "<base>\nBASE BODY\n</base>\n",
            "no L0/L1/recalled → base block alone; got:\n{out}"
        );
    }

    #[test]
    fn empty_recalled_omits_recalled_section() {
        // Same input as the L0+L1 happy-path tests below — proves the
        // empty `RecalledContext` produces byte-identical output to the
        // v1 assembler (regression pin for the migration).
        let l0 = vec![mem(1, "L0 RULE ONE", MemoryLayer::Meta)];
        let l1 = vec![mem(2, "L1 INSIGHT ONE", MemoryLayer::Index)];
        let out = assemble_system_prompt(
            &l0,
            &l1,
            &crate::recall_assembly::RecalledContext::empty(),
            "BASE BODY",
        );
        assert!(!out.contains("<recalled>"),
                "empty recalled context must not emit a <recalled> tag; got:\n{out}");
        assert!(out.contains("<l0_meta_rules>"), "L0 section still required");
        assert!(out.contains("<l1_insights>"), "L1 section still required");
    }

    #[test]
    fn renders_recalled_block_between_l1_and_base() {
        let l0 = vec![mem(1, "L0 RULE", MemoryLayer::Meta)];
        let l1 = vec![mem(2, "L1 INSIGHT", MemoryLayer::Index)];
        let recalled = crate::recall_assembly::RecalledContext {
            ids: vec![100, 101],
            bodies: vec!["RECALL ONE".into(), "RECALL TWO".into()],
            query_sha256: "f".repeat(64),
        };
        let out = assemble_system_prompt(&l0, &l1, &recalled, "BASE");

        // Positional ordering pin.
        let l0_end = out.find("</l0_meta_rules>").expect("L0 end tag");
        let l1_start = out.find("<l1_insights>").expect("L1 start tag");
        let l1_end = out.find("</l1_insights>").expect("L1 end tag");
        let recalled_start = out.find("<recalled>").expect("recalled start tag");
        let recalled_end = out.find("</recalled>").expect("recalled end tag");
        let base_start = out.find("<base>").expect("base start tag");

        assert!(l0_end < l1_start, "L0 must come before L1; out:\n{out}");
        assert!(l1_end < recalled_start, "L1 must come before recalled; out:\n{out}");
        assert!(recalled_end < base_start, "recalled must come before base; out:\n{out}");

        // Body rendering pin: one bullet per row.
        assert!(out.contains("<recalled>\n- RECALL ONE\n- RECALL TWO\n</recalled>"),
                "recalled rows must render `- {{body}}` newest-first; got:\n{out}");
    }

    #[test]
    fn recalled_block_passes_xml_chars_in_body_verbatim() {
        // Threat-model note: bodies are not operator-curated (any process
        // with INSERT on `memories` writes them), but Phase 1's posture
        // is to trust the model's tokeniser. Pin the pass-through so a
        // future "escape `<`" patch is a deliberate decision, not a
        // silent regression.
        let recalled = crate::recall_assembly::RecalledContext {
            ids: vec![1],
            bodies: vec!["body with <closing> tag".into()],
            query_sha256: "0".repeat(64),
        };
        let out = assemble_system_prompt(&[], &[], &recalled, "BASE");
        assert!(out.contains("- body with <closing> tag\n"),
                "body must pass through verbatim; got:\n{out}");
    }
```

### Step 2.2 — Run the failing tests to confirm they don't compile

- [ ] **Run:**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib prompt_assembly::assemble::tests 2>&1 | tail -30
```

**Expected:** compile error — `assemble_system_prompt` takes 3 args but the new tests pass 4. This is the RED step proving the test exercises the signature change.

### Step 2.3 — Widen the function signature and add the `<recalled>` rendering

- [ ] **Modify `core/src/prompt_assembly/assemble.rs`:**

Add the import at the top (near the existing `use kastellan_db::memories::Memory;`):

```rust
use crate::recall_assembly::RecalledContext;
```

Replace the `assemble_system_prompt` function body in full with the widened version:

```rust
/// Render the supplied memory slices, recall context, and base prompt
/// into a single LLM-ready system message.
///
/// See the module-level docstring for the framing rules. The
/// `recalled` argument follows L1 and precedes `base`; an empty
/// [`RecalledContext`] omits the `<recalled>` tag entirely so the
/// output is byte-identical to the v1 (no-recall) assembler.
pub fn assemble_system_prompt(
    l0: &[Memory],
    l1: &[Memory],
    recalled: &RecalledContext,
    base: &str,
) -> String {
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

    if !recalled.bodies.is_empty() {
        out.push_str("<recalled>\n");
        for body in &recalled.bodies {
            out.push_str("- ");
            out.push_str(body);
            out.push('\n');
        }
        out.push_str("</recalled>\n\n");
    }

    out.push_str("<base>\n");
    out.push_str(base);
    if !base.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("</base>\n");

    out
}
```

Also update the module-level docstring (top of file) to mention the recalled section. Replace the existing framing block (around lines 5–18) with:

```rust
//! Output framing (always L0 → L1 → recalled → base in this order):
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
//! <recalled>
//! - {body of recall row #1 (RRF-ranked-first)}
//! - {body of recall row #2}
//! </recalled>
//!
//! <base>
//! {agent_planner.md verbatim}
//! </base>
//! ```
```

And add a new rule entry between the existing rules 4 and 5 (relabel as 5 and 6):

```rust
//! 5. The `<recalled>` block is omitted when the
//!    [`crate::recall_assembly::RecalledContext`] is empty (the
//!    failure-degraded state). Recall is enrichment, not policy —
//!    this asymmetry is deliberate.
//! 6. Deterministic: same `(l0, l1, recalled, base)` produces the
//!    same bytes.
```

### Step 2.4 — Update `pg_builder.rs::PgSystemPromptBuilder::build` to call the widened assembler

- [ ] **Modify `core/src/prompt_assembly/pg_builder.rs`:** in `PgSystemPromptBuilder::build`, the line `let system_prompt = assemble_system_prompt(&l0, &l1, base);` becomes:

```rust
        let system_prompt = assemble_system_prompt(
            &l0,
            &l1,
            &crate::recall_assembly::RecalledContext::empty(),
            base,
        );
```

(The fully-realised `build_with_recalled` method lands in Task 3.)

### Step 2.5 — Run the tests to confirm GREEN

- [ ] **Run:**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib prompt_assembly
```

**Expected:** all `prompt_assembly` tests pass — existing 10 + 4 new from Step 2.1 = 14. The widened signature compiles cleanly.

- [ ] **Run the full workspace:**

```sh
source "$HOME/.cargo/env"
cargo test --workspace 2>&1 | grep "^test result:" | awk '{p+=$4; f+=$6; i+=$8} END {print "passed:", p, "failed:", f, "ignored:", i}'
```

**Expected:** `passed: 661 failed: 0 ignored: 4` (652 baseline + 5 from Task 1 + 4 from Task 2).

### Step 2.6 — Commit Task 2

- [ ] **Commit:**

```sh
git add core/src/prompt_assembly/
git commit -m "$(cat <<'EOF'
feat(core,prompt_assembly): widen assemble_system_prompt to take RecalledContext

assemble_system_prompt signature grows from 3-arg (l0, l1, base) to
4-arg (l0, l1, recalled, base). Empty RecalledContext reproduces the
v1 byte-output, so PgSystemPromptBuilder::build passes
&RecalledContext::empty() and the assembled prompt is byte-identical
to today for the no-recall path.

The <recalled> block is rendered between L1 and base when non-empty.
Bodies pass through verbatim (matches L0/L1 rendering posture). Empty
context omits the tag entirely.

+4 unit tests (workspace 657 → 661).

Per spec: docs/superpowers/specs/2026-05-17-recall-lane-wiring-design.md

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3 — `SystemPromptBuilder::build_with_recalled` + `AssembledPrompt::recalled_count`

**Files:**
- Modify: `core/src/prompt_assembly/mod.rs` (add `recalled_count` field to `AssembledPrompt`; add `build_with_recalled` trait method with a default impl that delegates from `build`)
- Modify: `core/src/prompt_assembly/pg_builder.rs` (implement `build_with_recalled` for both `PgSystemPromptBuilder` and `StaticSystemPromptBuilder`; `build` becomes a thin shim delegating with `&RecalledContext::empty()`)

### Step 3.1 — Write the failing test for `build_with_recalled` on `StaticSystemPromptBuilder`

- [ ] **Modify `core/src/prompt_assembly/pg_builder.rs`:** in the existing `#[cfg(test)] mod tests` block at the bottom, ADD this new test:

```rust
    #[tokio::test]
    async fn static_builder_build_with_recalled_passes_recalled_count_through() {
        use crate::recall_assembly::RecalledContext;
        let b = StaticSystemPromptBuilder::new("FIXED");
        let recalled = RecalledContext {
            ids: vec![1, 2],
            bodies: vec!["body one".into(), "body two".into()],
            query_sha256: "a".repeat(64),
        };
        let r = b.build_with_recalled("base", &recalled).await.unwrap();
        // StaticSystemPromptBuilder ignores base + recalled in the
        // assembled string (it's fixed), but the recalled_count field
        // must report the supplied recalled.ids.len() so RouterAgent
        // can write the audit row with the right number.
        assert_eq!(r.system_prompt, "FIXED");
        assert_eq!(r.l0_count, 0);
        assert_eq!(r.l1_count, 0);
        assert_eq!(r.recalled_count, 2, "recalled_count must reflect the supplied context");
    }
```

### Step 3.2 — Run to confirm RED

- [ ] **Run:**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib prompt_assembly::pg_builder::tests::static_builder_build_with_recalled_passes_recalled_count_through 2>&1 | tail -20
```

**Expected:** compile error — `build_with_recalled` doesn't exist on the trait; `AssembledPrompt` has no `recalled_count` field.

### Step 3.3 — Add `recalled_count` to `AssembledPrompt` and the new trait method

- [ ] **Modify `core/src/prompt_assembly/mod.rs`:** in the `AssembledPrompt` struct (around line 64), add the new field at the bottom:

```rust
    /// Number of recalled-memory rows that fed into the assembly.
    /// `0` for callers that don't run recall (e.g. tests using
    /// `StaticSystemPromptBuilder::empty()` without calling
    /// `build_with_recalled`). RouterAgent writes this into the
    /// `recall_count` audit-row key.
    pub recalled_count: usize,
```

Update the existing `SystemPromptBuilder` trait to add the new method with a default impl:

```rust
#[async_trait]
pub trait SystemPromptBuilder: Send + Sync {
    /// Assemble a system prompt by combining the loaded L0/L1 rows
    /// with the supplied `base`. Equivalent to
    /// [`Self::build_with_recalled`] with an empty
    /// [`crate::recall_assembly::RecalledContext`].
    ///
    /// Retained as a convenience for call sites that pre-date the
    /// recall-lane wiring slice (mostly tests).
    async fn build(&self, base: &str) -> Result<AssembledPrompt, PromptAssemblyError> {
        self.build_with_recalled(base, &crate::recall_assembly::RecalledContext::empty()).await
    }

    /// Assemble a system prompt by combining the loaded L0/L1 rows,
    /// the supplied `recalled` context, and `base`.
    ///
    /// Production use site: `RouterAgent::formulate_plan` calls
    /// `RecallBuilder::build(query)` first, then passes the result here.
    async fn build_with_recalled(
        &self,
        base: &str,
        recalled: &crate::recall_assembly::RecalledContext,
    ) -> Result<AssembledPrompt, PromptAssemblyError>;
}
```

### Step 3.4 — Implement `build_with_recalled` for `PgSystemPromptBuilder`

- [ ] **Modify `core/src/prompt_assembly/pg_builder.rs`:** replace the existing `impl SystemPromptBuilder for PgSystemPromptBuilder` block in full:

```rust
#[async_trait]
impl SystemPromptBuilder for PgSystemPromptBuilder {
    async fn build_with_recalled(
        &self,
        base: &str,
        recalled: &crate::recall_assembly::RecalledContext,
    ) -> Result<AssembledPrompt, PromptAssemblyError> {
        // TODO(token-cap, issue #78): all three loaders (L0, L1,
        // recalled) are uncapped at the I/O layer beyond their
        // internal per-layer caps. Safe today because both L1 and the
        // recalled-bodies cap are bounded; the deferred "global token
        // cap with priority drop" follow-up will plumb a budget
        // through here. See https://github.com/hherb/kastellan/issues/78.
        let l0 = load_l0_active_default(&self.pool).await?;
        let l1 = load_l1_default(&self.pool).await?;
        let system_prompt = assemble_system_prompt(&l0, &l1, recalled, base);
        Ok(AssembledPrompt {
            system_prompt,
            l0_count: l0.len(),
            l1_count: l1.len(),
            recalled_count: recalled.ids.len(),
        })
    }
}
```

(The default impl on the trait covers `build`, so no separate `fn build` body is needed on `PgSystemPromptBuilder`.)

### Step 3.5 — Implement `build_with_recalled` for `StaticSystemPromptBuilder`

- [ ] **Modify `core/src/prompt_assembly/pg_builder.rs`:** replace the existing `impl SystemPromptBuilder for StaticSystemPromptBuilder` block in full:

```rust
#[async_trait]
impl SystemPromptBuilder for StaticSystemPromptBuilder {
    async fn build_with_recalled(
        &self,
        _base: &str,
        recalled: &crate::recall_assembly::RecalledContext,
    ) -> Result<AssembledPrompt, PromptAssemblyError> {
        Ok(AssembledPrompt {
            system_prompt: self.fixed.clone(),
            l0_count: 0,
            l1_count: 0,
            recalled_count: recalled.ids.len(),
        })
    }
}
```

### Step 3.6 — Update the existing pg_builder tests that read `AssembledPrompt`

The existing tests `static_builder_returns_fixed_string_ignoring_base` and `static_builder_empty_constructor_yields_empty_string` build `AssembledPrompt` via `.build(...)` and only assert `l0_count`/`l1_count`. The new `recalled_count` field is `0` for both because the default trait impl passes `RecalledContext::empty()`.

- [ ] **Modify `core/src/prompt_assembly/pg_builder.rs`:** in `static_builder_returns_fixed_string_ignoring_base`, add `assert_eq!(r1.recalled_count, 0);` and `assert_eq!(r2.recalled_count, 0);` near the existing `l0_count`/`l1_count` assertions. Same pattern in `static_builder_empty_constructor_yields_empty_string`.

### Step 3.7 — Update the existing `prompt_assembly_e2e.rs` integration test asserting `result`

- [ ] **Modify `core/tests/prompt_assembly_e2e.rs`:** in `pg_builder_build_against_seeded_db`, add this assertion after the existing `l1_count` check (~line 86):

```rust
        assert_eq!(result.recalled_count, 0,
                   "build() with no recall context defaults to recalled_count = 0; got: {result:?}");
```

Same in `pg_builder_build_with_empty_db_returns_base_only` after the existing assertions (~line 146).

### Step 3.8 — Run + commit Task 3

- [ ] **Run:**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib prompt_assembly
cargo test -p kastellan-core --test prompt_assembly_e2e 2>&1 | tail -10
```

**Expected:** all prompt_assembly tests pass.

- [ ] **Run the full workspace:**

```sh
source "$HOME/.cargo/env"
cargo test --workspace 2>&1 | grep "^test result:" | awk '{p+=$4; f+=$6; i+=$8} END {print "passed:", p, "failed:", f, "ignored:", i}'
```

**Expected:** `passed: 662 failed: 0 ignored: 4` (+1 from the new `static_builder_build_with_recalled_passes_recalled_count_through` test).

- [ ] **Commit:**

```sh
git add core/src/prompt_assembly/ core/tests/prompt_assembly_e2e.rs
git commit -m "$(cat <<'EOF'
feat(core,prompt_assembly): SystemPromptBuilder.build_with_recalled + recalled_count

SystemPromptBuilder gains a new trait method build_with_recalled(base,
&RecalledContext) returning AssembledPrompt with a new recalled_count
field. The existing build(base) method becomes a thin default impl
that delegates with RecalledContext::empty() — preserves all existing
call sites byte-for-byte while adding the recall-aware seam.

AssembledPrompt.recalled_count carries the count of recalled rows
straight from the source-of-truth (the supplied RecalledContext.ids
length) so it can't drift from what the assembler actually rendered.
RouterAgent uses this to populate the recall_count audit-row key.

+1 unit test (workspace 661 → 662).

Per spec: docs/superpowers/specs/2026-05-17-recall-lane-wiring-design.md

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4 — `cap_and_split` pure helper + unit tests

**Files:**
- Modify: `core/src/recall_assembly/pg_builder.rs` (add `cap_and_split` private helper + 3 unit tests)

### Step 4.1 — Write the failing tests for `cap_and_split`

- [ ] **Modify `core/src/recall_assembly/pg_builder.rs`:** in the existing `#[cfg(test)] mod tests` block, ADD these new tests:

```rust
    use kastellan_db::memories::{Memory, MemoryLayer};
    use time::OffsetDateTime;

    fn mem(id: i64, body: &str) -> Memory {
        Memory {
            id,
            body: body.to_string(),
            metadata: serde_json::json!({}),
            layer: MemoryLayer::Stable,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn cap_and_split_empty_input_returns_empty_vectors() {
        let (ids, bodies) = super::cap_and_split(vec![], 4096);
        assert!(ids.is_empty());
        assert!(bodies.is_empty());
    }

    #[test]
    fn cap_and_split_below_cap_keeps_all_rows() {
        let rows = vec![mem(1, "aaa"), mem(2, "bb"), mem(3, "c")];
        let (ids, bodies) = super::cap_and_split(rows, 100);
        assert_eq!(ids, vec![1, 2, 3]);
        assert_eq!(bodies, vec!["aaa", "bb", "c"]);
    }

    #[test]
    fn cap_and_split_drops_oversize_first_row_returns_empty() {
        // Single row 10 bytes, cap 5 bytes → row is dropped entirely.
        let rows = vec![mem(7, "0123456789")];
        let (ids, bodies) = super::cap_and_split(rows, 5);
        assert!(ids.is_empty(), "oversize-first-row must be dropped");
        assert!(bodies.is_empty());
    }

    #[test]
    fn cap_and_split_stops_at_cap_keeping_rows_that_fit() {
        // Row 1 = 4 bytes, row 2 = 4 bytes, cap = 5. Only row 1 fits
        // (after row 1: 4 used, room for 1 byte; row 2 needs 4 more
        // and would exceed cap → dropped).
        let rows = vec![mem(1, "aaaa"), mem(2, "bbbb"), mem(3, "c")];
        let (ids, bodies) = super::cap_and_split(rows, 5);
        // Row 1 fits (4 ≤ 5); row 2 would push to 8 > 5 → dropped.
        // Row 3 is 1 byte, would push to 5 ≤ 5 → fits — but the
        // function stops at the first dropped row (cumulative greedy).
        assert_eq!(ids, vec![1], "only the first row fits under the cap");
        assert_eq!(bodies, vec!["aaaa"]);
    }
```

### Step 4.2 — Run to confirm RED

- [ ] **Run:**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib recall_assembly::pg_builder::tests::cap_and_split 2>&1 | tail -15
```

**Expected:** compile error — `cap_and_split` doesn't exist.

### Step 4.3 — Implement `cap_and_split` as a private helper

- [ ] **Modify `core/src/recall_assembly/pg_builder.rs`:** ABOVE the existing `pub struct PgRecallBuilder` line, add the helper:

```rust
use kastellan_db::memories::Memory;

use super::L_RECALL_CAP_BYTES;

/// Greedy newest-first cap: walk `rows` in order, push as long as
/// cumulative body bytes stay ≤ `cap_bytes`. The first row that
/// would push cumulative bytes over the cap is dropped (with a
/// `tracing::warn!`) and the walk stops — matches the L1 loader's
/// `saturating_add` break idiom in `core::memory::layers::load_l1`.
///
/// Pure helper, no I/O. Doesn't drop later rows that might
/// individually fit — that would risk reorder vs. the RRF-fused
/// order coming out of `recall`. Operators see the dropped id in
/// logs and can either retire the oversized memory or raise the cap.
pub(crate) fn cap_and_split(rows: Vec<Memory>, cap_bytes: usize) -> (Vec<i64>, Vec<String>) {
    let mut ids = Vec::with_capacity(rows.len());
    let mut bodies = Vec::with_capacity(rows.len());
    let mut used: usize = 0;

    for row in rows {
        let next = used.saturating_add(row.body.len());
        if next > cap_bytes {
            tracing::warn!(
                target: "kastellan::recall_assembly",
                memory_id = row.id,
                row_bytes = row.body.len(),
                used_bytes = used,
                cap_bytes,
                "recall row exceeds cap; dropping this and any remaining recall rows",
            );
            break;
        }
        used = next;
        ids.push(row.id);
        bodies.push(row.body);
    }

    (ids, bodies)
}
```

### Step 4.4 — Run + commit Task 4

- [ ] **Run:**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib recall_assembly
```

**Expected:** all `recall_assembly` tests pass (5 from Task 1 + 4 from Task 4 = 9 in this module's tests).

- [ ] **Run the full workspace:**

```sh
source "$HOME/.cargo/env"
cargo test --workspace 2>&1 | grep "^test result:" | awk '{p+=$4; f+=$6; i+=$8} END {print "passed:", p, "failed:", f, "ignored:", i}'
```

**Expected:** `passed: 666 failed: 0 ignored: 4` (662 + 4 new).

- [ ] **Commit:**

```sh
git add core/src/recall_assembly/
git commit -m "$(cat <<'EOF'
feat(core,recall_assembly): cap_and_split pure helper for byte-capped recall

Pure helper that walks RRF-fused recall rows newest-first and stops
at the first row whose body would push cumulative bytes over the
L_RECALL_CAP_BYTES (4 KiB) ceiling. Drops the offending row with a
tracing::warn! carrying memory_id + sizes — matches the L1 loader's
saturating_add break idiom in core::memory::layers::load_l1.

Stopping at the first oversized row (rather than skipping it and
trying later rows) preserves the RRF-fused order that recall returned;
otherwise the rendered <recalled> block could end up out-of-order with
respect to the lane scores.

+4 unit tests (workspace 662 → 666).

Per spec: docs/superpowers/specs/2026-05-17-recall-lane-wiring-design.md

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5 — Real `PgRecallBuilder` body (composes embed_query + recall)

**Files:**
- Modify: `core/src/recall_assembly/pg_builder.rs` (replace the Task-1 stub with the production body)

### Step 5.1 — Write the failing test in a new e2e file

- [ ] **Create `core/tests/recall_assembly_e2e.rs`:**

```rust
//! End-to-end smoke for [`kastellan_core::recall_assembly::PgRecallBuilder`].
//!
//! Each scenario brings up its own per-test Postgres cluster + a
//! hand-rolled `tokio::net::TcpListener` mock for `/embeddings` (same
//! pattern as `embedding_recall_e2e.rs`).
//!
//! Skips silently with `[SKIP]` lines on hosts without Postgres or a
//! reachable supervisor; `cargo test -- --nocapture` to see them.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::sync::Arc;

use kastellan_core::memory::EMBEDDING_DIM;
use kastellan_core::recall_assembly::{PgRecallBuilder, RecallBuilder};
use kastellan_db::memories::insert_memory;
use kastellan_llm_router::{RouterConfig, Router};
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, text_to_embedding,
    unique_suffix,
};

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime")
}

/// Spawn a `tokio::net::TcpListener` that responds to one
/// `/embeddings` POST with a fixed [`EMBEDDING_DIM`]-element vector.
///
/// Returns `(socket_addr_string, JoinHandle)`. The handle drops the
/// listener at end-of-test; the bound port is auto-allocated so
/// concurrent runs don't collide.
async fn spawn_mock_embedding_listener(vec: Vec<f32>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr").to_string();
    let url = format!("http://{addr}");

    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { break };
            let body = serde_json::json!({
                "object": "list",
                "data": [{"object": "embedding", "index": 0, "embedding": vec}],
                "model": "test-embedding-model",
                "usage": {"prompt_tokens": 1, "total_tokens": 1},
            });
            let payload = body.to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                payload.len(),
                payload,
            );
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf).await;
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });

    (url, handle)
}

#[test]
fn pg_recall_builder_round_trips_against_seeded_pool_and_mock_embedding() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "rae-d",
        "rae-l",
        &format!("kastellan-supervisor-test-pg-rae-{suffix}"),
    );

    rt().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "recall-assembly-e2e"}),
        )
        .await
        .expect("probe");

        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        // Seed 3 memories with deterministic embeddings. The query
        // embedding (computed below from the same `text_to_embedding`
        // helper) matches the second memory's text, so semantic+lexical
        // fusion should rank it first.
        let texts = ["alpha bravo charlie", "delta echo foxtrot", "golf hotel india"];
        let mut seeded_ids: Vec<i64> = Vec::new();
        for t in texts {
            let emb = text_to_embedding(t);
            let id = insert_memory(&pool, t, &serde_json::json!({}), Some(&emb))
                .await
                .expect("insert memory");
            seeded_ids.push(id);
        }

        // Query embedding = the second seeded memory's embedding (so
        // it will rank top-1 in the semantic lane); lexical lane will
        // also hit because the query string carries the exact body words.
        let query = "delta echo foxtrot";
        let query_emb = text_to_embedding(query);

        // Start mock embedding listener that returns the same vector.
        let (mock_url, _handle) = spawn_mock_embedding_listener(query_emb.clone()).await;

        // Build a Router pointed at the mock.
        let router_cfg = RouterConfig {
            local_url: mock_url.clone(),
            local_model: "test-model".into(),
            frontier_url: "http://0.0.0.0".into(),
            frontier_model: "frontier-model".into(),
            embedding_url: mock_url,
            embedding_model: "test-embedding-model".into(),
            timeout_ms: 5000,
        };
        let router = Arc::new(Router::new(router_cfg).expect("Router::new"));

        let builder = PgRecallBuilder::new(pool.clone(), router);
        let recalled = builder.build(query).await.expect("recall builder");

        // The seeded second memory should be top-1 in fused order.
        assert!(!recalled.ids.is_empty(), "recall must return at least one row");
        assert_eq!(recalled.ids[0], seeded_ids[1],
                   "seeded memory matching query must rank #1 (got ids={:?}, expected top-1={})",
                   recalled.ids, seeded_ids[1]);
        assert_eq!(recalled.bodies[0], "delta echo foxtrot");
        assert_eq!(recalled.query_sha256.len(), 64,
                   "query_sha256 must be 64 hex chars");

        pool.close().await;
    });
}
```

### Step 5.2 — Run to confirm RED (compile error: PgRecallBuilder::new takes no args today)

- [ ] **Run:**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --test recall_assembly_e2e --no-run 2>&1 | tail -15
```

**Expected:** compile error on `PgRecallBuilder::new(pool.clone(), router)` — the Task-1 stub takes no args.

### Step 5.3 — Replace the `PgRecallBuilder` stub with the production body

- [ ] **Modify `core/src/recall_assembly/pg_builder.rs`:**

Add the imports near the top (under the existing `use kastellan_db::memories::Memory;` line that Task 4 added):

```rust
use std::sync::Arc;
use sqlx::PgPool;
use kastellan_llm_router::Router;

use crate::memory::{embed_query, recall, RecallParams, RecallModes};
```

Replace the existing stub `pub struct PgRecallBuilder { _placeholder: () }` and its `impl PgRecallBuilder` + `impl Default` + `impl RecallBuilder` blocks with:

```rust
/// Production builder. Composes [`embed_query`] + [`recall`] over a
/// shared [`PgPool`] and [`Router`]; caps the rendered bodies via
/// [`cap_and_split`].
///
/// Holds `PgPool` by value (cheap to clone via sqlx's internal `Arc`
/// — matches the [`crate::prompt_assembly::PgSystemPromptBuilder`]
/// convention) and `Router` behind an `Arc` (the same `Arc<Router>`
/// already constructed in `main.rs`).
pub struct PgRecallBuilder {
    pool: PgPool,
    router: Arc<Router>,
}

impl PgRecallBuilder {
    /// Construct a builder pinned to the supplied pool and router.
    pub fn new(pool: PgPool, router: Arc<Router>) -> Self {
        Self { pool, router }
    }
}

#[async_trait]
impl RecallBuilder for PgRecallBuilder {
    async fn build(&self, query: &str) -> Result<RecalledContext, RecallError> {
        let query_sha256 = sha256_hex(query.as_bytes());

        // Step 1 — turn the query text into an embedding (writes the
        // actor='llm:router' action='embed' audit row internally).
        let emb = embed_query(&self.pool, &self.router, query).await?;

        // Step 2 — fan out semantic + lexical lanes. Graph lane stays
        // off because we have no entity seeds at this slice — that's a
        // separate "entity extraction" follow-up. The empty seeds vec
        // + SEMANTIC_AND_LEXICAL modes is the cleanest call shape
        // (matches RecallParams::new's defaults).
        let mut params = RecallParams::new(query, &emb);
        params.modes = RecallModes::SEMANTIC_AND_LEXICAL;
        let rows = recall(&self.pool, &params).await?;

        // Step 3 — byte-cap into the final RecalledContext.
        let (ids, bodies) = cap_and_split(rows, L_RECALL_CAP_BYTES);
        Ok(RecalledContext {
            ids,
            bodies,
            query_sha256,
        })
    }
}
```

### Step 5.4 — Run the new e2e test

- [ ] **Run:**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --test recall_assembly_e2e pg_recall_builder_round_trips_against_seeded_pool_and_mock_embedding -- --nocapture 2>&1 | tail -20
```

**Expected:** test passes (or skips silently with `[SKIP]` on a host without Postgres). On the DGX Spark with PG available, expect ~2 s runtime.

### Step 5.5 — Update the `kastellan_core::memory` re-export to include `EMBEDDING_DIM`

The e2e test imports `kastellan_core::memory::EMBEDDING_DIM`. If `cargo test --test recall_assembly_e2e --no-run` reports a missing-symbol error here:

- [ ] **Modify `core/src/memory/mod.rs`:** in the `pub use` re-exports near the bottom, add `EMBEDDING_DIM`:

```rust
pub use embed::{embed_query, MemoryError};
pub use kastellan_db::memories::EMBEDDING_DIM;
pub use recall::{
    recall, reciprocal_rank_fusion, RecallModes, RecallParams, GRAPH_FANOUT_CAP_PER_SEED,
    RRF_K_CONSTANT,
};
```

(Skip this step if the e2e test compiles without it — `EMBEDDING_DIM` may already be reachable transitively.)

### Step 5.6 — Run the full workspace + commit Task 5

- [ ] **Run:**

```sh
source "$HOME/.cargo/env"
cargo test --workspace 2>&1 | grep "^test result:" | awk '{p+=$4; f+=$6; i+=$8} END {print "passed:", p, "failed:", f, "ignored:", i}'
```

**Expected:** `passed: 667 failed: 0 ignored: 4` (666 + 1 new e2e on Linux with PG, or 666 if skipped).

- [ ] **Commit:**

```sh
git add core/src/recall_assembly/pg_builder.rs core/src/memory/mod.rs core/tests/recall_assembly_e2e.rs
git commit -m "$(cat <<'EOF'
feat(core,recall_assembly): PgRecallBuilder production body + e2e round-trip

Replaces the Task-1 stub with the real composition: embed_query (which
writes the actor='llm:router' action='embed' audit row) followed by
recall with RecallModes::SEMANTIC_AND_LEXICAL, capped via cap_and_split
at L_RECALL_CAP_BYTES (4 KiB). Graph lane stays off this slice — that
requires entity extraction from ctx.instruction, a separate follow-up.

New cross-platform integration test recall_assembly_e2e exercises the
full path against a per-test PG cluster + hand-rolled TCP mock for the
embedding endpoint (same pattern as embedding_recall_e2e.rs). Seeds 3
memories with deterministic embeddings; asserts the matching memory
ranks #1 in fused order; checks the query_sha256 is 64 hex chars.

+1 integration test (workspace 666 → 667).

Per spec: docs/superpowers/specs/2026-05-17-recall-lane-wiring-design.md

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6 — `RouterAgent` wire-in + `FormulationMeta` widening

**Files:**
- Modify: `core/src/scheduler/agent.rs` (constructor gains a 4th arg; `formulate_plan` runs recall with degrade-and-warn; `FormulationMeta` gains 3 fields)

### Step 6.1 — Update `FormulationMeta` first (the type change cascades)

- [ ] **Modify `core/src/scheduler/agent.rs`:** add the three new fields to `FormulationMeta` (around line 64, just before the closing brace):

```rust
    /// Memory ids the recall lane surfaced for this iteration's
    /// instruction (RRF-fused order, capped at `L_RECALL_CAP_BYTES`).
    /// Empty when recall returned nothing or degraded due to error.
    /// Written verbatim to the `recalled_memory_ids` audit-row key.
    pub recalled_memory_ids: Vec<i64>,
    /// `recalled_memory_ids.len() as u32`. Redundant but cheap to
    /// query — observation-phase SQL avoids `jsonb_array_length` for
    /// the common "did recall fire at all?" question.
    pub recall_count: u32,
    /// Hex SHA-256 of the query text (the task instruction). Lets
    /// observation phase detect when paraphrased prompts produce the
    /// same recalled-id set vs. genuine drift.
    pub recall_query_sha256: String,
```

### Step 6.2 — Update `RouterAgent` struct + `new` to carry the recall builder

- [ ] **Modify `core/src/scheduler/agent.rs`:** replace the `RouterAgent` struct + `impl RouterAgent`:

```rust
/// Production adapter: calls the real `Router::send`.
pub struct RouterAgent {
    router: std::sync::Arc<Router>,
    prompts: std::sync::Arc<PromptCache>,
    prompt_builder: std::sync::Arc<dyn crate::prompt_assembly::SystemPromptBuilder>,
    recall_builder: std::sync::Arc<dyn crate::recall_assembly::RecallBuilder>,
}

impl RouterAgent {
    pub fn new(
        router: std::sync::Arc<Router>,
        prompts: std::sync::Arc<PromptCache>,
        prompt_builder: std::sync::Arc<dyn crate::prompt_assembly::SystemPromptBuilder>,
        recall_builder: std::sync::Arc<dyn crate::recall_assembly::RecallBuilder>,
    ) -> Self {
        Self { router, prompts, prompt_builder, recall_builder }
    }
}
```

### Step 6.3 — Wire recall into `formulate_plan` with degrade-and-warn

- [ ] **Modify `core/src/scheduler/agent.rs`:** replace the `formulate_plan` body in full:

```rust
#[async_trait]
impl PlanFormulator for RouterAgent {
    async fn formulate_plan(
        &self,
        ctx: &TaskContext,
    ) -> Result<(Plan, FormulationMeta), AgentError> {
        let entry = self.prompts.get("agent_planner")
            .ok_or(AgentError::PromptMissing)?;

        let base = entry.content.clone();

        // Per-iteration recall. Asymmetric posture vs the prompt
        // assembler below: recall failure DEGRADES (we still want the
        // model to plan with L0/L1/base even if retrieval is broken),
        // while prompt-assembly failure is FAIL-CLOSED (a degraded
        // safety prompt would have the agent flying blind on operator
        // rules). See spec §"Failure-mode matrix".
        let recalled = match self.recall_builder.build(&ctx.instruction).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    target: "kastellan::scheduler::agent",
                    error = %e,
                    "recall failed; continuing with empty recall context",
                );
                crate::recall_assembly::RecalledContext::empty()
            }
        };

        let assembled = self.prompt_builder
            .build_with_recalled(&base, &recalled)
            .await
            .map_err(AgentError::PromptAssembly)?;
        let assembled_prompt_sha256 = {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(assembled.system_prompt.as_bytes());
            format!("{:x}", h.finalize())
        };

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

        let start = std::time::Instant::now();
        let resp = self.router.send(&req).await?;
        let latency_ms = start.elapsed().as_millis() as u64;

        let raw = resp.choices.first()
            .map(|c| c.message.content.clone())
            .unwrap_or_default();

        let plan: Plan = parse_plan_lenient(&raw).map_err(|e| AgentError::Decode {
            detail: e.to_string(),
            raw: raw.clone(),
        })?;

        // recall_count is `usize` → `u32` via `as`; the cap_and_split
        // helper bounds the row count to L_RECALL_CAP_BYTES/min-body
        // size = at most ~4096 rows in the pathological 1-byte case,
        // so a u32 has 6 orders of magnitude of headroom.
        let recall_count = recalled.ids.len() as u32;

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
            recalled_memory_ids: recalled.ids,
            recall_count,
            recall_query_sha256: recalled.query_sha256,
        };
        Ok((plan, meta))
    }
}
```

### Step 6.4 — Run the workspace to find the cascading constructor breakage

- [ ] **Run:**

```sh
source "$HOME/.cargo/env"
cargo build --workspace 2>&1 | tail -40
```

**Expected:** compile errors at every existing `RouterAgent::new` call site (3-arg call now needs 4) and at every `FormulationMeta { ... }` literal (3 new required fields). The next tasks fix these one file at a time.

### Step 6.5 — Do NOT commit yet — the build is broken until Task 7/8/9 land

- [ ] No commit; proceed to Task 7. (The compile errors are the RED state for the cascading changes.)

---

## Task 7 — Update `main.rs` to construct `PgRecallBuilder` and pass it in

**Files:**
- Modify: `core/src/main.rs` (one block around line 119 — the existing `RouterAgent::new` site)

### Step 7.1 — Add the 4th constructor argument

- [ ] **Modify `core/src/main.rs`:** replace the existing `RouterAgent::new` construction (lines 119–124) with:

```rust
    let formulator: Arc<dyn kastellan_core::scheduler::agent::PlanFormulator> =
        Arc::new(kastellan_core::scheduler::agent::RouterAgent::new(
            router.clone(),
            prompts.clone(),
            Arc::new(kastellan_core::prompt_assembly::PgSystemPromptBuilder::new(pool.clone())),
            Arc::new(kastellan_core::recall_assembly::PgRecallBuilder::new(
                pool.clone(),
                router.clone(),
            )),
        ));
```

### Step 7.2 — Run `cargo build` for the daemon to confirm it compiles

- [ ] **Run:**

```sh
source "$HOME/.cargo/env"
cargo build --bin kastellan 2>&1 | tail -10
```

**Expected:** `kastellan` daemon builds cleanly. Workspace tests still fail to compile (call sites in `tests/*.rs` still broken — fixed in Task 8).

---

## Task 8 — Update `RouterAgent::new` and `FormulationMeta { ... }` literal call sites in tests

**Files:**
- Modify: `core/tests/router_agent_mock_e2e.rs` (3 call sites; lines 274, 330, 374 per spec — verify with grep)
- Modify: `core/tests/scheduler_inner_loop_e2e.rs` (`ScriptedFormulator::formulate_plan` body around line 316)
- Modify: `core/tests/scheduler_lanes_e2e.rs` (sweep — may or may not construct `RouterAgent`)
- Modify: `core/tests/cli_ask_e2e.rs` (sweep — same)

### Step 8.1 — Sweep to find all call sites that still need updating

- [ ] **Run:**

```sh
grep -rn "RouterAgent::new\|FormulationMeta {" /home/hherb/src/kastellan/core/tests/ /home/hherb/src/kastellan/core/src/ 2>/dev/null | grep -v "target/"
```

Expect: 3 sites in `router_agent_mock_e2e.rs`, ≥1 in `scheduler_inner_loop_e2e.rs`, and possibly more depending on what the inner-loop unit-test fixtures use. (`main.rs` and the prod `scheduler/agent.rs` body are already fixed by Tasks 6+7.)

### Step 8.2 — Fix `router_agent_mock_e2e.rs` (3 sites)

- [ ] **Modify `core/tests/router_agent_mock_e2e.rs`:** at the top, add the import:

```rust
use kastellan_core::recall_assembly::StaticRecallBuilder;
```

Then at each of the 3 `RouterAgent::new(...)` call sites (use grep to find them), add the 4th argument `Arc::new(StaticRecallBuilder::empty())`:

```rust
    let agent = RouterAgent::new(
        router,
        prompts,
        Arc::new(StaticSystemPromptBuilder::new(PLANNER_PROMPT_CONTENT)),
        Arc::new(StaticRecallBuilder::empty()),
    );
```

Same change at all 3 sites (the third uses `StaticSystemPromptBuilder::empty()` for the prompt builder — pattern stays identical).

### Step 8.3 — Fix `scheduler_inner_loop_e2e.rs` `ScriptedFormulator`

- [ ] **Modify `core/tests/scheduler_inner_loop_e2e.rs`:** find the `ScriptedFormulator::formulate_plan` body (around line 304) and look for the `FormulationMeta { ... }` literal (around line 316). Add the three new fields to the literal:

```rust
            FormulationMeta {
                prompt_name: "agent_planner".into(),
                prompt_sha256: "scripted".into(),
                llm_model: "scripted-stub".into(),
                llm_backend: "local".into(),
                latency_ms: 0,
                retry_count: 0,
                assembled_prompt_sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
                l0_count: 0,
                l1_count: 0,
                recalled_memory_ids: Vec::new(),
                recall_count: 0,
                recall_query_sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
            },
```

(SHA-256 of empty string is `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`; matches the `RecalledContext::empty()` shape.)

### Step 8.4 — Sweep `scheduler_lanes_e2e.rs` + `cli_ask_e2e.rs`

- [ ] **For each file** the Step 8.1 grep flagged, apply the same `RouterAgent::new` 4th-arg or `FormulationMeta { ... }` 3-field update. If they don't construct `RouterAgent` directly, no change needed.

### Step 8.5 — Build + run the workspace to confirm green

- [ ] **Run:**

```sh
source "$HOME/.cargo/env"
cargo test --workspace 2>&1 | grep "^test result:" | awk '{p+=$4; f+=$6; i+=$8} END {print "passed:", p, "failed:", f, "ignored:", i}'
```

**Expected:** `passed: 667 failed: 0 ignored: 4` — the existing tests still pass and there are no new tests yet. If anything fails, fix it before moving on.

### Step 8.6 — Commit Tasks 6 + 7 + 8 together

The three tasks form one logical change ("wire RecallBuilder through to the agent and update all call sites"). Commit as one:

- [ ] **Commit:**

```sh
git add core/src/scheduler/agent.rs core/src/main.rs core/tests/router_agent_mock_e2e.rs core/tests/scheduler_inner_loop_e2e.rs core/tests/scheduler_lanes_e2e.rs core/tests/cli_ask_e2e.rs
git commit -m "$(cat <<'EOF'
feat(core,scheduler,agent): wire RecallBuilder through RouterAgent + degrade-and-warn

RouterAgent::new gains a 4th Arc<dyn RecallBuilder> argument. Inside
formulate_plan, recall runs BEFORE the prompt assembler:

  let recalled = match self.recall_builder.build(&ctx.instruction).await {
      Ok(c) => c,
      Err(e) => { tracing::warn!(...); RecalledContext::empty() }
  };

Recall errors DEGRADE silently — the agent still gets the L0/L1/base
prompt and the model still plans. This is asymmetric to the
fail-closed PromptAssembly posture: a missing L0 rule would have the
agent flying blind on operator constraints; a missing recall row is
just enrichment that didn't fire.

FormulationMeta widens by 3 fields (recalled_memory_ids,
recall_count, recall_query_sha256) so the inner loop can write them
into the plan.formulate audit row in Task 9.

main.rs constructs PgRecallBuilder::new(pool.clone(), router.clone())
and passes the Arc into RouterAgent::new. All test call sites
(router_agent_mock_e2e, ScriptedFormulator in scheduler_inner_loop_e2e,
sweeps in scheduler_lanes_e2e + cli_ask_e2e) updated to pass
Arc::new(StaticRecallBuilder::empty()) as 4th arg / populate the new
fields with empty/zero defaults.

Net test count unchanged (workspace 667, all existing tests still
pass). The audit-row payload extension lands in Task 9.

Per spec: docs/superpowers/specs/2026-05-17-recall-lane-wiring-design.md

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9 — `build_plan_formulate_payload` emits 3 new keys + bumps the pin tests

**Files:**
- Modify: `core/src/scheduler/inner_loop.rs` (writer + 4 inline pin tests at the bottom of the file)

### Step 9.1 — Write the failing pin tests in `inner_loop.rs::tests`

- [ ] **Modify `core/src/scheduler/inner_loop.rs`:** look at the existing pin tests around lines 830 (`build_plan_formulate_payload_pins_seventeen_keys_for_default_source`) and 907 (`..._cli_inferred_source_has_18_keys_with_signals`). REPLACE these tests in full:

```rust
    #[test]
    fn build_plan_formulate_payload_pins_twenty_keys_for_default_source() {
        // Slice D (2026-05-17, recall-lane wiring) bumps the
        // default-source key count from 17 to 20 by adding
        // recalled_memory_ids, recall_count, recall_query_sha256.
        let plan = make_text_plan();
        let meta = FormulationMeta {
            prompt_name: "agent_planner".into(),
            prompt_sha256: "p1".into(),
            llm_model: "lm".into(),
            llm_backend: "local".into(),
            latency_ms: 1,
            retry_count: 0,
            assembled_prompt_sha256: "ax".into(),
            l0_count: 0,
            l1_count: 0,
            recalled_memory_ids: vec![100, 200],
            recall_count: 2,
            recall_query_sha256: "f".repeat(64),
        };
        let payload = build_plan_formulate_payload(
            1, 1, DataClass::Public,
            ClassificationFloorSource::Default,
            &[],
            &plan,
            &meta,
        );
        let obj = payload.as_object().expect("payload object");
        let mut keys: Vec<&str> = obj.keys().map(|s| s.as_str()).collect();
        keys.sort();
        let expected: std::collections::BTreeSet<&str> = [
            "task_id", "plan_count", "prompt_name", "prompt_sha256",
            "llm_model", "llm_backend", "latency_ms", "retry_count",
            "plan_step_count", "decision_kind", "refused",
            "plan", "classification_floor", "classification_floor_source",
            "system_prompt_sha256", "l0_count", "l1_count",
            "recalled_memory_ids", "recall_count", "recall_query_sha256",
        ].into_iter().collect();
        let got: std::collections::BTreeSet<&str> = keys.into_iter().collect();
        assert_eq!(got, expected,
            "default-source payload must carry exactly 20 keys; diff:\n\
             missing = {:?}\nextra = {:?}",
            expected.difference(&got).collect::<Vec<_>>(),
            got.difference(&expected).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn build_plan_formulate_payload_cli_inferred_source_has_21_keys_with_signals() {
        let plan = make_text_plan();
        let meta = FormulationMeta {
            prompt_name: "agent_planner".into(),
            prompt_sha256: "p1".into(),
            llm_model: "lm".into(),
            llm_backend: "local".into(),
            latency_ms: 1,
            retry_count: 0,
            assembled_prompt_sha256: "ax".into(),
            l0_count: 0,
            l1_count: 0,
            recalled_memory_ids: vec![1],
            recall_count: 1,
            recall_query_sha256: "9".repeat(64),
        };
        let payload = build_plan_formulate_payload(
            1, 1, DataClass::ClinicalConfidential,
            ClassificationFloorSource::CliInferred,
            &["patient".to_string()],
            &plan,
            &meta,
        );
        let obj = payload.as_object().expect("payload object");
        assert_eq!(obj.len(), 21,
            "cli_inferred source with signals must carry 21 keys (20 default + signals); got {} keys: {:?}",
            obj.len(), obj.keys().collect::<Vec<_>>(),
        );
        assert_eq!(payload["classification_floor_signals"], serde_json::json!(["patient"]));
    }

    #[test]
    fn build_plan_formulate_payload_recall_keys_round_trip_through_meta() {
        let plan = make_text_plan();
        let meta = FormulationMeta {
            prompt_name: "agent_planner".into(),
            prompt_sha256: "p1".into(),
            llm_model: "lm".into(),
            llm_backend: "local".into(),
            latency_ms: 1,
            retry_count: 0,
            assembled_prompt_sha256: "ax".into(),
            l0_count: 0,
            l1_count: 0,
            recalled_memory_ids: vec![42, 99, 7],
            recall_count: 3,
            recall_query_sha256: "deadbeef".repeat(8), // 64 hex chars
        };
        let payload = build_plan_formulate_payload(
            1, 1, DataClass::Public,
            ClassificationFloorSource::Default,
            &[],
            &plan,
            &meta,
        );
        assert_eq!(payload["recalled_memory_ids"], serde_json::json!([42, 99, 7]),
                   "recalled_memory_ids must round-trip from meta.recalled_memory_ids");
        assert_eq!(payload["recall_count"], 3u64,
                   "recall_count must round-trip from meta.recall_count");
        assert_eq!(payload["recall_query_sha256"], serde_json::json!("deadbeef".repeat(8)),
                   "recall_query_sha256 must round-trip from meta.recall_query_sha256");
    }

    #[test]
    fn build_plan_formulate_payload_recall_query_sha256_is_64_hex_chars_in_empty_default() {
        // Defensive format pin: when recall degraded (or no rows
        // returned), the sha256 of the empty string still satisfies
        // the 64-hex-char contract. Observation phase SQL can pin the
        // format without a special case.
        let plan = make_text_plan();
        let meta = FormulationMeta {
            prompt_name: "agent_planner".into(),
            prompt_sha256: "p1".into(),
            llm_model: "lm".into(),
            llm_backend: "local".into(),
            latency_ms: 1,
            retry_count: 0,
            assembled_prompt_sha256: "ax".into(),
            l0_count: 0,
            l1_count: 0,
            recalled_memory_ids: Vec::new(),
            recall_count: 0,
            recall_query_sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
        };
        let payload = build_plan_formulate_payload(
            1, 1, DataClass::Public,
            ClassificationFloorSource::Default,
            &[],
            &plan,
            &meta,
        );
        let sha = payload["recall_query_sha256"].as_str().expect("string");
        assert_eq!(sha.len(), 64, "recall_query_sha256 must always be 64 chars; got {sha}");
        assert!(sha.chars().all(|c| c.is_ascii_hexdigit()),
                "recall_query_sha256 must be hex; got {sha}");
    }
```

You will also need to find the two existing tests `build_plan_formulate_payload_default_source_omits_signals_key` and `build_plan_formulate_payload_agent_raised_source_omits_signals` around lines 881 and 939 and add the 3 new fields to their `FormulationMeta` literals:

```rust
            recalled_memory_ids: Vec::new(),
            recall_count: 0,
            recall_query_sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
```

And find the existing test `build_plan_formulate_payload_carries_full_plan_and_classification_floor` around line 757 — same 3-field addition to its `FormulationMeta` literal.

### Step 9.2 — Run to confirm RED

- [ ] **Run:**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib scheduler::inner_loop::tests 2>&1 | tail -20
```

**Expected:** the 4 new tests fail (payload missing the new keys); the existing 17/18-key tests also fail (they're gone — renamed to 20/21).

### Step 9.3 — Add the 3 new payload keys to `build_plan_formulate_payload`

- [ ] **Modify `core/src/scheduler/inner_loop.rs`:** find `build_plan_formulate_payload` (around line 436) and add 3 new `obj.insert(...)` calls between the existing `obj.insert("l1_count".into(), ...)` line and the `if classification_floor_source == ClassificationFloorSource::CliInferred ...` block:

```rust
    obj.insert("l1_count".into(), serde_json::json!(meta.l1_count));
    // Slice D (recall-lane wiring, 2026-05-17): the recall lane's
    // contribution to this iteration. recalled_memory_ids is the
    // RRF-fused id list capped by L_RECALL_CAP_BYTES; recall_count is
    // a cheap-to-query duplicate of its length; recall_query_sha256 is
    // a stable hash of the query text the agent embedded so the
    // observation phase can detect paraphrase vs. genuine drift.
    obj.insert(
        "recalled_memory_ids".into(),
        serde_json::json!(meta.recalled_memory_ids),
    );
    obj.insert("recall_count".into(), serde_json::json!(meta.recall_count));
    obj.insert(
        "recall_query_sha256".into(),
        serde_json::json!(meta.recall_query_sha256),
    );
```

Also update the doc comment on `build_plan_formulate_payload` (around line 433) to mention Slice D:

```rust
/// Slice D (2026-05-17) added `recalled_memory_ids`, `recall_count`,
/// and `recall_query_sha256` so the observation phase can audit which
/// memories the recall lane surfaced and detect drift across captures.
pub(crate) fn build_plan_formulate_payload(
```

### Step 9.4 — Run to confirm GREEN + commit

- [ ] **Run:**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib scheduler::inner_loop::tests
cargo test --workspace 2>&1 | grep "^test result:" | awk '{p+=$4; f+=$6; i+=$8} END {print "passed:", p, "failed:", f, "ignored:", i}'
```

**Expected:** `passed: 669 failed: 0 ignored: 4` (667 + 2 net new pin tests; the renamed 17→20 and 18→21 tests are renames, not additions, so they net to +2 not +4 from this task).

Wait — re-count: the original file had `build_plan_formulate_payload_pins_seventeen_keys_for_default_source` (1) + `build_plan_formulate_payload_cli_inferred_source_has_18_keys_with_signals` (1) = 2. We replaced these with 4 new tests (the 20-key, 21-key, round-trip, format-pin). Net +2.

- [ ] **Commit:**

```sh
git add core/src/scheduler/inner_loop.rs
git commit -m "$(cat <<'EOF'
feat(core,scheduler,inner_loop): plan.formulate audit row carries 3 recall keys

build_plan_formulate_payload emits three new keys for every
plan.formulate row:

  - recalled_memory_ids: [i64] (RRF-fused ids from recall, capped)
  - recall_count: u32 (cheap-to-query duplicate of the array length)
  - recall_query_sha256: String (SHA-256 of the embedded query text)

Default-source payload key count grows 17 → 20; cli_inferred source
(with signals) grows 18 → 21. Pure-additive — existing JSONB
consumers (replay harness, observation captures) keep working
unchanged.

Existing 17-key and 18-key pin tests renamed and bumped to assert the
new shape; +2 new pin tests cover the value round-trip (recall ids
flow from meta.recalled_memory_ids verbatim) and the SHA-256 format
(always 64 hex chars, even for the empty/degraded case).

+2 unit tests net (workspace 667 → 669).

Per spec: docs/superpowers/specs/2026-05-17-recall-lane-wiring-design.md

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 10 — Mid-tier audit-key gate in `scheduler_inner_loop_e2e`

**Files:**
- Modify: `core/tests/scheduler_inner_loop_e2e.rs` (extend the existing happy-path assertions around line 477 — where l0_count/l1_count are already pinned — to also pin the 3 new recall keys)

### Step 10.1 — Find the existing happy-path assertion block

- [ ] **Run:**

```sh
grep -n 'l0_count\|l1_count\|recalled_memory_ids\|recall_count\|recall_query_sha256' /home/hherb/src/kastellan/core/tests/scheduler_inner_loop_e2e.rs
```

The Slice-C happy-path assertions live around line 477 (where `l0_count`/`l1_count` are pinned as numeric u64).

### Step 10.2 — Add the 3 new key assertions next to the existing 2

- [ ] **Modify `core/tests/scheduler_inner_loop_e2e.rs`:** in the happy-path test, find the block:

```rust
    assert!(payload.get("l0_count").and_then(|v| v.as_u64()).is_some(),
        "plan.formulate must carry numeric l0_count; got {payload:?}");
    assert!(payload.get("l1_count").and_then(|v| v.as_u64()).is_some(),
        "plan.formulate must carry numeric l1_count; got {payload:?}");
```

INSERT these three checks immediately after (same indentation):

```rust
    assert!(payload.get("recall_count").and_then(|v| v.as_u64()).is_some(),
        "plan.formulate must carry numeric recall_count; got {payload:?}");
    assert!(payload.get("recalled_memory_ids").and_then(|v| v.as_array()).is_some(),
        "plan.formulate must carry array recalled_memory_ids; got {payload:?}");
    let sha = payload.get("recall_query_sha256")
        .and_then(|v| v.as_str())
        .expect(&format!("plan.formulate must carry string recall_query_sha256; got {payload:?}"));
    assert_eq!(sha.len(), 64, "recall_query_sha256 must be 64 hex chars; got {sha}");
    // Cross-key consistency: count must equal the ids array length.
    let n = payload["recall_count"].as_u64().unwrap();
    let ids_len = payload["recalled_memory_ids"].as_array().unwrap().len() as u64;
    assert_eq!(n, ids_len,
        "recall_count must equal recalled_memory_ids.len(); got {n} vs {ids_len}");
```

### Step 10.3 — Run + commit Task 10

- [ ] **Run:**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --test scheduler_inner_loop_e2e -- --nocapture 2>&1 | tail -10
cargo test --workspace 2>&1 | grep "^test result:" | awk '{p+=$4; f+=$6; i+=$8} END {print "passed:", p, "failed:", f, "ignored:", i}'
```

**Expected:** existing tests still pass; no new `#[test]` functions, just extended in-place assertions. Workspace count stays `passed: 669`.

- [ ] **Commit:**

```sh
git add core/tests/scheduler_inner_loop_e2e.rs
git commit -m "$(cat <<'EOF'
test(core,scheduler): mid-tier audit-key gate for the 3 recall keys

scheduler_inner_loop_e2e happy-path now asserts presence + shape of
the 3 new plan.formulate keys (recalled_memory_ids array,
recall_count numeric, recall_query_sha256 64-char hex string) and
their cross-key consistency (recall_count == recalled_memory_ids.len()).

cli_ask_e2e covers the same path end-to-end against the real
production stack, but it requires sandbox + worker + LLM mock. This
lightweight gate runs whenever Postgres is reachable so a future
regression in build_plan_formulate_payload's emission is caught at
the mid tier rather than slipping past until cli_ask_e2e fires.

No new #[test] functions; in-place assertion expansion.

Per spec: docs/superpowers/specs/2026-05-17-recall-lane-wiring-design.md

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 11 — Extend `prompt_assembly_e2e.rs` to exercise `build_with_recalled` with a static context

**Files:**
- Modify: `core/tests/prompt_assembly_e2e.rs` (add 1 new test using `PgSystemPromptBuilder` + an explicit `RecalledContext` to exercise the full assembled-prompt rendering against a real PG cluster)

### Step 11.1 — Write the failing test

- [ ] **Modify `core/tests/prompt_assembly_e2e.rs`:** at the bottom of the file, ADD this new test:

```rust
#[test]
fn pg_builder_with_recalled_renders_block_against_seeded_db() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "par-d",
        "par-l",
        &format!("kastellan-supervisor-test-pg-par-{suffix}"),
    );

    rt().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "prompt-assembly-with-recalled"}),
        )
        .await
        .expect("probe");

        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        // Empty DB → no L0/L1 sections; recalled context supplied
        // directly so we exercise the <recalled> rendering without
        // going through the real recall lane.
        let recalled = kastellan_core::recall_assembly::RecalledContext {
            ids: vec![10, 20],
            bodies: vec!["RECALL ALPHA".into(), "RECALL BETA".into()],
            query_sha256: "a".repeat(64),
        };

        let builder = PgSystemPromptBuilder::new(pool.clone());
        let result = builder.build_with_recalled("BASE BODY", &recalled)
            .await
            .expect("build_with_recalled");

        assert_eq!(result.l0_count, 0);
        assert_eq!(result.l1_count, 0);
        assert_eq!(result.recalled_count, 2);
        let s = &result.system_prompt;
        assert!(s.contains("<recalled>\n- RECALL ALPHA\n- RECALL BETA\n</recalled>"),
                "recalled block missing/wrong shape; got:\n{s}");
        assert!(s.contains("<base>\nBASE BODY\n</base>\n"),
                "base section missing; got:\n{s}");

        // Empty-recalled fallback: build() (the legacy 1-arg shim) must
        // produce identical output to build_with_recalled(base, &empty).
        let r_via_legacy = builder.build("BASE BODY").await.expect("legacy build");
        let r_via_explicit_empty = builder
            .build_with_recalled(
                "BASE BODY",
                &kastellan_core::recall_assembly::RecalledContext::empty(),
            )
            .await
            .expect("explicit empty build");
        assert_eq!(r_via_legacy.system_prompt, r_via_explicit_empty.system_prompt,
                   "legacy build() must produce byte-identical output to build_with_recalled(base, &empty)");

        pool.close().await;
    });
}
```

### Step 11.2 — Run + commit Task 11

- [ ] **Run:**

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --test prompt_assembly_e2e -- --nocapture 2>&1 | tail -10
cargo test --workspace 2>&1 | grep "^test result:" | awk '{p+=$4; f+=$6; i+=$8} END {print "passed:", p, "failed:", f, "ignored:", i}'
```

**Expected:** `passed: 670 failed: 0 ignored: 4` (669 + 1 new e2e test).

- [ ] **Commit:**

```sh
git add core/tests/prompt_assembly_e2e.rs
git commit -m "$(cat <<'EOF'
test(core,prompt_assembly): e2e pin for build_with_recalled rendering + legacy parity

New integration test against a real PG cluster:
- Constructs a non-empty RecalledContext (2 rows) and asserts the
  assembled prompt contains the <recalled> block in the correct shape.
- Asserts the legacy single-arg build() produces byte-identical output
  to build_with_recalled(base, &RecalledContext::empty()) — pins the
  thin-shim default impl, so a future refactor that breaks the
  delegation is caught immediately.

+1 integration test (workspace 669 → 670).

Per spec: docs/superpowers/specs/2026-05-17-recall-lane-wiring-design.md

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 12 — HANDOVER + ROADMAP update + final verification

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md` (add "Recently completed" entry; bump test count; tick the spec's open follow-ups)
- Modify: `docs/devel/ROADMAP.md` (tick the recall-lane wiring item; cross-link to the spec/plan)

### Step 12.1 — Verify the final workspace test count

- [ ] **Run:**

```sh
source "$HOME/.cargo/env"
cargo test --workspace 2>&1 | grep "^test result:" | awk '{p+=$4; f+=$6; i+=$8} END {print "passed:", p, "failed:", f, "ignored:", i}'
```

**Expected:** `passed: 670 failed: 0 ignored: 4` (652 baseline + 18 across the slice).

Wait — re-count the actual deltas:

| Task | Tests added | Tests removed | Net |
| ---- | ----------- | ------------- | --- |
| 1 | +5 (2 mod + 3 pg_builder) | 0 | +5 |
| 2 | +4 (assemble) | 0 | +4 |
| 3 | +1 (static_builder_build_with_recalled_passes_recalled_count_through) | 0 | +1 |
| 4 | +4 (cap_and_split unit) | 0 | +4 |
| 5 | +1 (recall_assembly_e2e) | 0 | +1 |
| 6+7+8 | 0 | 0 | 0 |
| 9 | +4 new (20-key, 21-key, round-trip, format-pin) | -2 (17-key, 18-key) | +2 |
| 10 | 0 (in-place expansion) | 0 | 0 |
| 11 | +1 | 0 | +1 |
| **Total** | | | **+18** |

So the final count should be **670** (652 + 18). The plan's headline claim was +12; this is +18. The extra 6 are: the cap_and_split sub-tests (+3 over what the spec listed because the spec hand-waved them as "+4 unit on RecalledContext / SHA-256 / cap"; the actual decomposition is 2 mod-level + 3 static-builder + 4 cap_and_split = 9, whereas the spec's flat "+4" was an underestimate). Update HANDOVER's test-count claim to the actual final number.

### Step 12.2 — Update `docs/devel/handovers/HANDOVER.md`

- [ ] **Modify `docs/devel/handovers/HANDOVER.md`:** at the top, bump the header:

```markdown
**Last updated:** 2026-05-17 (recall-lane wiring — shipped on branch `feat/recall-lane-wiring`; new `core::recall_assembly` module + `RecallBuilder` trait + `PgRecallBuilder`/`StaticRecallBuilder` impls + widened `assemble_system_prompt` 3-arg → 4-arg + 3 new `plan.formulate` audit-row keys; +18 tests; workspace 652 → **670**).
**Last commit (branch HEAD):** `<HASH>` (use `git rev-parse --short HEAD` to fill in).
**Session-end verification:** `cargo test --workspace` on branch HEAD: **670 passed, 0 failed, 4 ignored, 0 [SKIP] lines, 0 warnings** on Linux.
```

Add a "Recently completed" entry immediately after the existing Slice-C entry (the prompt-assembler one), structured the same way (section + key design choices + audit-row contract table + non-goals + file-touch list).

The "Next TODO" section's "Open follow-up surfaces" line "Recall-lane wiring — next natural slice" should be moved/struck-through and replaced with the next natural follow-up (most likely: **entity extraction + graph lane wiring** or **L1 promotion writer**).

### Step 12.3 — Update `docs/devel/ROADMAP.md`

- [ ] **Modify `docs/devel/ROADMAP.md`:** find the existing Phase 1 entries near the prompt-assembler bullet (around line 113) and add immediately after:

```markdown
- [x] **Recall-lane wiring** — landed 2026-05-17 on branch `feat/recall-lane-wiring`. New `core::recall_assembly` module ships pure `RecalledContext { ids, bodies, query_sha256 }` value type + async `RecallBuilder` trait (parallel to `SystemPromptBuilder`) + prod `PgRecallBuilder` (composes `embed_query` + `recall(SEMANTIC | LEXICAL)`) + test `StaticRecallBuilder`. `assemble_system_prompt` widens to 4-arg (`l0, l1, recalled, base`); `RecalledContext::empty()` reproduces v1 byte-output. `RouterAgent::formulate_plan` runs recall before assembly with **degrade-and-warn** posture (recall is enrichment, not policy — distinct from the fail-closed `PromptAssembly` posture). 3 new `agent/plan.formulate` audit-row keys: `recalled_memory_ids`, `recall_count`, `recall_query_sha256` (pure-additive; payload 17/18 → 20/21 keys). +18 tests (workspace 652 → **670**). Spec at `docs/superpowers/specs/2026-05-17-recall-lane-wiring-design.md`; plan at `docs/superpowers/plans/2026-05-17-recall-lane-wiring.md`. Closes the HANDOVER "Next concrete engineering pickup #1" (recall lane wiring); unblocks future entity-extraction + graph-lane wiring and prompt-cap priority-drop ([issue #78](https://github.com/hherb/kastellan/issues/78)).
```

### Step 12.4 — Final commit

- [ ] **Run:**

```sh
source "$HOME/.cargo/env"
cargo test --workspace 2>&1 | grep "^test result:" | awk '{p+=$4; f+=$6; i+=$8} END {print "passed:", p, "failed:", f, "ignored:", i}'
```

**Expected:** `passed: 670 failed: 0 ignored: 4`. Sanity-check.

- [ ] **Commit:**

```sh
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md docs/superpowers/plans/2026-05-17-recall-lane-wiring.md
git commit -m "$(cat <<'EOF'
docs(handover,roadmap): recall-lane wiring shipped

Workspace 652 → 670 tests (+18) on branch feat/recall-lane-wiring.
Per-task breakdown in the new "Recently completed" entry in HANDOVER.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Step 12.5 — Open the PR (operator-driven)

- [ ] After all 12 tasks land green, the operator opens the PR via the project's normal workflow (no `gh pr create` from this plan — that's an operator decision per the user's preference recorded in earlier sessions).

---

## Self-review checklist (run before handing off)

**1. Spec coverage:** every numbered section of the spec mapped to a task:

| Spec section | Task |
| ------------ | ---- |
| `RecalledContext` value type | 1 |
| `RecallError` enum | 1 |
| `RecallBuilder` trait + `StaticRecallBuilder` impl | 1, 4 |
| `assemble_system_prompt` 4-arg widening + `<recalled>` block rendering | 2 |
| `SystemPromptBuilder::build_with_recalled` + `AssembledPrompt::recalled_count` | 3 |
| `cap_and_split` byte-cap helper | 4 |
| `PgRecallBuilder` production body | 5 |
| `RouterAgent` constructor + `formulate_plan` wiring + degrade-and-warn | 6 |
| `main.rs` wire-in | 7 |
| Test call-site cascade (RouterAgent::new + FormulationMeta literals) | 8 |
| `build_plan_formulate_payload` 3 new keys + 20/21-key pin tests | 9 |
| `scheduler_inner_loop_e2e` mid-tier audit-key gate | 10 |
| `prompt_assembly_e2e` build_with_recalled rendering + legacy parity | 11 |
| HANDOVER + ROADMAP update | 12 |

**2. Placeholder scan:** none.

**3. Type consistency:** `RecallBuilder::build` returns `Result<RecalledContext, RecallError>` consistently; `assemble_system_prompt(&[Memory], &[Memory], &RecalledContext, &str)` consistent; `AssembledPrompt { system_prompt, l0_count, l1_count, recalled_count }` consistent; `FormulationMeta` field names match: `recalled_memory_ids: Vec<i64>`, `recall_count: u32`, `recall_query_sha256: String` — both in the struct definition (Task 6) and in the audit-payload writer (Task 9).

**4. Cross-task dependencies:** Task 6 breaks the build; Task 7 fixes the daemon; Task 8 fixes the tests; commit happens at end of Task 8 (Step 8.6). Tasks 9, 10, 11 each add tests and commit independently. Task 12 wraps with docs.
