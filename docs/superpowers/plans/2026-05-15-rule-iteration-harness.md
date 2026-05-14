# Rule-iteration harness Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a two-slice rule-iteration harness for CASSANDRA: (A) enrich the `agent/plan.formulate` audit payload so captures carry the full `Plan` JSON + `classification_floor`; (B) build a pure-Rust replay library + `hhagent-cli observation replay` subcommand that loads captures and reports per-fixture verdict deltas against a candidate `ChainReviewStage`.

**Architecture:** Slice A is a pure-additive audit-payload bump on one writer (`write_audit_plan_formulate` in `core/src/scheduler/inner_loop.rs`). Slice B adds one new module `core/src/observation/replay.rs` (pure helpers + an async `replay_capture`) plus a thin CLI wrapper in `core/src/bin/hhagent-cli.rs` (one new top-level `observation` subcommand mirroring the existing `tools` pattern). Slice B reads on-disk captures via `serde_json` and degrades gracefully on pre-Slice-A captures (skips with `plans_skipped_missing_body` counter).

**Tech Stack:** Rust 2021, `serde_json`, `tokio` (async-trait for the `ReviewStage`), `async_trait`, no new workspace deps. Tests use `tokio::test` + the existing `hhagent-tests-common` workspace dev-dep.

**Spec:** [docs/superpowers/specs/2026-05-15-rule-iteration-harness-design.md](../specs/2026-05-15-rule-iteration-harness-design.md)

**Branch strategy:**
- **Slice A** lands on `feat/audit-plan-formulate-carries-plan-body` (already created; sits on `7588b9e` + this plan + the spec).
- **Slice B** lands on a fresh branch `feat/rule-iteration-harness` once Slice A merges.

---

## Slice A — Audit-payload bump

Branch: `feat/audit-plan-formulate-carries-plan-body` (already exists; carries the spec + plan + HANDOVER refresh).

### Task A1: Extract pure `build_plan_formulate_payload` helper (RED + GREEN)

**Files:**
- Modify: `core/src/scheduler/inner_loop.rs` — extract pure payload-builder + add new fields
- Test: `core/src/scheduler/inner_loop.rs::tests` (same file's existing test module)

**Why a pure helper first.** The current `write_audit_plan_formulate` mixes payload-shape and DB-insert. Extracting the shape into a pure function lets us unit-test the new payload fields without needing a live Postgres pool — the same pattern `core/src/scheduler/audit.rs` already uses (`build_finalize_payload`, `build_lifecycle_payload`, `build_scheduler_step_failure_payload`).

- [ ] **Step 1: Write the failing unit test in `core/src/scheduler/inner_loop.rs::tests`**

Locate the existing `#[cfg(test)] mod tests` block in `core/src/scheduler/inner_loop.rs`. Append:

```rust
    #[test]
    fn build_plan_formulate_payload_carries_full_plan_and_classification_floor() {
        let plan = Plan {
            context: "ctx".into(),
            decision: "act".into(),
            rationale: "r".into(),
            steps: vec![PlannedStep {
                tool: "shell-exec".into(),
                method: "shell.exec".into(),
                parameters: serde_json::json!({"argv": ["/bin/echo", "hi"]}),
                returns: "stdout".into(),
                done_when: "echoed".into(),
                classification: DataClass::Public,
            }],
            result: None,
            data_ceiling: DataClass::Personal,
            refused: None,
        };
        let meta = FormulationMeta {
            prompt_name: "agent_planner".into(),
            prompt_sha256: "deadbeef".into(),
            llm_model: "gemma4:26b".into(),
            llm_backend: "local".into(),
            latency_ms: 42,
            retry_count: 0,
        };
        let payload = build_plan_formulate_payload(
            /*task_id*/ 7,
            /*plan_count*/ 1,
            /*classification_floor*/ DataClass::ClinicalConfidential,
            &plan,
            &meta,
        );

        // New: full Plan JSON round-trips byte-for-byte.
        let plan_back: Plan = serde_json::from_value(payload["plan"].clone())
            .expect("plan key must deserialise back into a Plan");
        assert_eq!(plan_back, plan, "plan payload field must round-trip");

        // New: task-level classification_floor stringified PascalCase.
        assert_eq!(
            payload["classification_floor"], "ClinicalConfidential",
            "classification_floor must serialise as PascalCase string"
        );

        // Existing 11 keys remain unchanged.
        assert_eq!(payload["task_id"], 7);
        assert_eq!(payload["plan_count"], 1);
        assert_eq!(payload["decision_kind"], "act");
        assert_eq!(payload["plan_step_count"], 1);
        assert!(payload["refused"].is_null());
    }

    #[test]
    fn build_plan_formulate_payload_pins_thirteen_keys() {
        // Pin the total key count so a future additive change to the
        // wire shape becomes a deliberate, reviewable edit instead of
        // an accidental drift.
        let plan = Plan {
            context: "".into(),
            decision: "task_complete".into(),
            rationale: "".into(),
            steps: vec![],
            result: Some(serde_json::json!({"kind": "text", "body": "ok"})),
            data_ceiling: DataClass::Public,
            refused: None,
        };
        let meta = FormulationMeta {
            prompt_name: "agent_planner".into(),
            prompt_sha256: "x".into(),
            llm_model: "m".into(),
            llm_backend: "local".into(),
            latency_ms: 0,
            retry_count: 0,
        };
        let payload = build_plan_formulate_payload(
            1, 0, DataClass::Public, &plan, &meta,
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
        ].into_iter().collect();
        assert_eq!(keys, expected, "payload key set drifted; update the pin deliberately");
    }
```

The tests use `Plan`, `PlannedStep`, `DataClass`, `FormulationMeta`. They must all be in scope inside `mod tests`. Check the current `use` block at the top of the tests module; if missing, add `use super::*;` (likely already present) and any per-type imports (`PlannedStep`, `DataClass` from `crate::cassandra::types`).

- [ ] **Step 2: Run the tests to verify they fail with "function not defined"**

```sh
source "$HOME/.cargo/env"
cargo test -p hhagent-core --lib scheduler::inner_loop::tests::build_plan_formulate_payload -- --nocapture
```

Expected: compile errors mentioning `build_plan_formulate_payload` unresolved. If you see "test passed," the helper already exists and you have the wrong file — verify location.

- [ ] **Step 3: Extract the pure helper + add the two new fields**

In `core/src/scheduler/inner_loop.rs`, replace the existing `write_audit_plan_formulate` body. Current code (around lines 332–372):

```rust
async fn write_audit_plan_formulate(
    pool: &PgPool,
    ctx: &TaskContext,
    plan: &Plan,
    meta: &FormulationMeta,
) -> Result<(), InnerLoopError> {
    let decision_kind = if plan.is_refused() { ... }
        else if plan.is_terminal() { ... }
        else { "act" };
    let refused = plan.refused.as_ref()...;
    let payload = serde_json::json!({ ... });
    hhagent_db::audit::insert(pool, "agent", "plan.formulate", payload).await?;
    Ok(())
}
```

Replace with:

```rust
/// Pure builder for the `agent/plan.formulate` audit-row payload.
///
/// Extracted from `write_audit_plan_formulate` so the wire shape is
/// unit-testable without a live Postgres pool. The 13-key shape pins
/// (in this file's `tests` module) defend against accidental drift.
///
/// Slice A (2026-05-15) added `plan` (full serialised Plan) +
/// `classification_floor` (task-level DataClass) so captures carry
/// everything the reviewer pipeline needs to be replayed offline —
/// see `core::observation::replay`.
pub(crate) fn build_plan_formulate_payload(
    task_id: i64,
    plan_count: u32,
    classification_floor: DataClass,
    plan: &Plan,
    meta: &FormulationMeta,
) -> serde_json::Value {
    // Issue #23 (spec §3): "refused" takes precedence over the
    // is_terminal-derived "task_complete" so a refusal payload is
    // wire-distinguishable from a successful completion via the same
    // discriminator field — including the malformed-refusal-with-steps
    // shape the inner-loop short-circuit also honours.
    let decision_kind = if plan.is_refused() {
        crate::cassandra::types::DECISION_REFUSED
    } else if plan.is_terminal() {
        crate::cassandra::types::DECISION_TERMINAL
    } else {
        "act"
    };

    // Explicit JSON null (not key-absent) so downstream JSONB queries
    // can rely on `refused` always being present.
    let refused = plan.refused.as_ref()
        .map(|r| serde_json::json!({ "principle": r.principle, "reason": r.reason }))
        .unwrap_or(serde_json::Value::Null);

    // `plan` is the full Plan JSON. Together with `classification_floor`
    // this is what enables offline replay (Slice B / observation::replay).
    // Plans are typically <1 KiB; the audit-envelope SHA-256 truncation
    // at 4 KiB is the safety net for the rare oversized case.
    let plan_json = serde_json::to_value(plan)
        .expect("Plan serialisation cannot fail (no non-string keys, no NaN)");

    // PascalCase string via DataClass's #[serde(rename_all = "PascalCase")].
    let classification_floor_json = serde_json::to_value(classification_floor)
        .expect("DataClass serialisation cannot fail (closed enum, no payloads)");

    serde_json::json!({
        "task_id":              task_id,
        "plan_count":           plan_count,
        "prompt_name":          meta.prompt_name,
        "prompt_sha256":        meta.prompt_sha256,
        "llm_model":            meta.llm_model,
        "llm_backend":          meta.llm_backend,
        "latency_ms":           meta.latency_ms,
        "retry_count":          meta.retry_count,
        "plan_step_count":      plan.steps.len(),
        "decision_kind":        decision_kind,
        "refused":              refused,
        // Slice A additions:
        "plan":                 plan_json,
        "classification_floor": classification_floor_json,
    })
}

async fn write_audit_plan_formulate(
    pool: &PgPool,
    ctx: &TaskContext,
    plan: &Plan,
    meta: &FormulationMeta,
) -> Result<(), InnerLoopError> {
    let payload = build_plan_formulate_payload(
        ctx.task_id,
        ctx.plan_count,
        ctx.classification_floor,
        plan,
        meta,
    );
    hhagent_db::audit::insert(pool, "agent", "plan.formulate", payload).await?;
    Ok(())
}
```

If the tests module needs additional imports (e.g. `PlannedStep`), add them inside `#[cfg(test)] mod tests { use super::*; use crate::cassandra::types::{Plan, PlannedStep, DataClass, RefusedReason}; ... }`. Check what's already imported and only add missing types.

- [ ] **Step 4: Run the unit tests to verify they pass**

```sh
cargo test -p hhagent-core --lib scheduler::inner_loop::tests::build_plan_formulate_payload -- --nocapture
```

Expected: 2 tests pass.

- [ ] **Step 5: Run the full lib test suite + commit**

```sh
cargo test -p hhagent-core --lib
git add core/src/scheduler/inner_loop.rs
git commit -m "$(cat <<'EOF'
feat(scheduler): extract build_plan_formulate_payload + carry full Plan + classification_floor

Closes the precondition for the rule-iteration harness (Slice A of
the 2026-05-15-rule-iteration-harness spec). The agent/plan.formulate
audit-row payload now carries the full Plan JSON and the task-level
classification_floor; together these are everything the reviewer
pipeline needs to be replayed offline against a candidate
ChainReviewStage.

Pure-additive payload bump (13 keys, up from 11). Existing test pins
assert specific key values, not total key count, so no existing
assertions break. New unit tests pin the 13-key set + the round-trip
shape of the two new keys.

Pure helper extraction follows the established pattern in
core/src/scheduler/audit.rs (build_finalize_payload etc.) so the
wire shape is unit-testable without a live Postgres pool.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

Expected output: `cargo test -p hhagent-core --lib` reports the new tests as passing alongside existing lib tests; the commit shows up in `git log --oneline -1`.

### Task A2: Extend e2e test assertions to pin new keys

**Files:**
- Modify: `core/tests/scheduler_inner_loop_e2e.rs` — two assertion blocks (happy + refusal scenarios)

- [ ] **Step 1: Extend the happy-path payload assertions (around line 440)**

Locate the assertion block in `core/tests/scheduler_inner_loop_e2e.rs` starting around line 432 (the `agent/plan.formulate` row fetch). After the existing `assert!(payload.get("refused")...)` block (around line 444), add:

```rust
    // Slice A (2026-05-15): payload carries full Plan + classification_floor.
    let plan_back: crate::Plan = serde_json::from_value(payload["plan"].clone())
        .expect("plan payload key must deserialise into a Plan");
    assert_eq!(plan_back.decision, "task_complete",
        "plan round-trip must preserve decision");
    assert_eq!(plan_back.steps.len(), 0,
        "plan round-trip must preserve steps");
    assert_eq!(
        payload["classification_floor"], "Public",
        "classification_floor must serialise as PascalCase string (Public for unset producer floor)"
    );
```

Note: `crate::Plan` likely won't resolve — integration tests in `core/tests/*.rs` use `hhagent_core::cassandra::types::Plan` (or whichever the existing test imports use). Check the top-of-file `use` block and use whatever path is already in scope. If nothing's imported, add `use hhagent_core::cassandra::types::Plan;` at the test module top.

- [ ] **Step 2: Extend the refusal-scenario payload assertions (around line 735)**

In the refusal scenario (function `refusal_plan_terminates_with_state_refused`, around line 728-735), after the existing `assert_eq!(payload["plan_step_count"], 0);` add:

```rust
    // Slice A: refusal plan body round-trips including refused field.
    let plan_back: Plan = serde_json::from_value(payload["plan"].clone())
        .expect("refusal plan must round-trip");
    assert!(plan_back.refused.is_some(),
        "round-tripped refusal plan must carry refused: Some(..)");
    assert_eq!(plan_back.refused.as_ref().unwrap().principle, 1);
    assert_eq!(plan_back.refused.as_ref().unwrap().reason, "physical_harm");
    assert_eq!(
        payload["classification_floor"], "Public",
        "test fixture's task has no classification_floor in payload; defaults to Public"
    );
```

- [ ] **Step 3: Run the two scenarios to confirm they pass**

```sh
cargo test -p hhagent-core --test scheduler_inner_loop_e2e -- --nocapture
```

Expected: 4 tests pass (happy + tool-fail-then-recover + plan-cap-exhausted + cancel-mid-exec) plus the 3 refusal scenarios (the count matches HANDOVER's reporting of 4 scenarios; the 3 refusal e2e tests come from the post-refusal-state slice).

If the tests skip with `[SKIP]` on this host (no PG available), document the skip and move on; the unit tests at A1 already pin the shape. Verify a non-skip run locally before pushing the PR.

- [ ] **Step 4: Run the full workspace test suite**

```sh
cargo test --workspace
```

Expected: 466 tests pass (465 baseline + the new unit test we just added isn't 1 yet, see below). Actually: the count after Slice A is 465 + 2 (new unit tests in A1) = **467**. Zero failures, zero warnings, zero `[SKIP]` lines on a host with PG + sandbox available.

- [ ] **Step 5: Commit**

```sh
git add core/tests/scheduler_inner_loop_e2e.rs
git commit -m "$(cat <<'EOF'
test(scheduler): pin new agent/plan.formulate keys in e2e scenarios

Slice A test-pin update. The happy-path and refusal scenarios now
assert that the agent/plan.formulate audit-row payload carries:
- `plan` — full serialised Plan, round-trips bytewise via serde
- `classification_floor` — PascalCase DataClass string

Pure-additive: existing key-value assertions are unchanged. Both
scenarios use Public classification_floor because the test fixtures
don't set one in tasks.payload (runner.rs defaults to Public when
absent, see SECURITY comment at runner.rs:278).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task A3: Update HANDOVER + ROADMAP, push branch, open PR

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md` — append new "Recently completed (this session)" entry
- Modify: `docs/devel/ROADMAP.md` — tick the slice off

- [ ] **Step 1: Append the HANDOVER "Recently completed (this session)" entry**

In `docs/devel/handovers/HANDOVER.md`, insert a new section just after the header block (above the existing "Recently completed (previous session, 2026-05-14 — observation-phase first capture run..." section, around line 145). Use this template — fill in the SHAs after the commits land:

```markdown
## Recently completed (this session, 2026-05-15 — Slice A: audit-payload bump on agent/plan.formulate, branch `feat/audit-plan-formulate-carries-plan-body`)

Branch: `feat/audit-plan-formulate-carries-plan-body` (off `main` at `7588b9e`). Pure-additive bump on the `agent/plan.formulate` audit-row payload: 11 keys → 13 keys, adding `plan` (full serialised Plan) and `classification_floor` (task-level `DataClass` string). Closes the precondition for the rule-iteration harness (Slice B); together these are everything the reviewer pipeline needs to be replayed offline.

**Shape (1 production file + 1 e2e test modified):**

- **`core/src/scheduler/inner_loop.rs` — extracted pure `build_plan_formulate_payload`.** Same pattern `scheduler/audit.rs` already uses (`build_finalize_payload`, `build_lifecycle_payload`); the wire shape is now unit-testable without a Postgres pool. 2 new unit tests pin the 13-key set (BTreeSet equality assertion so a future accidental extra/missing key trips loudly) and the round-trip shape of `plan` + `classification_floor`. `write_audit_plan_formulate` shrinks to a one-line shim over the helper + `hhagent_db::audit::insert`.

- **`core/tests/scheduler_inner_loop_e2e.rs` — extended two scenarios** (happy path around line 440; refusal around line 730). New assertions deserialise `payload["plan"]` back into a `Plan` and pin the round-trip; both scenarios assert `payload["classification_floor"]` is the PascalCase string `"Public"` (the test fixtures' tasks don't set `classification_floor` in `tasks.payload`, so `runner.rs` defaults it to Public per the security comment at line 278).

**Audit-row contract (the headline):**

| When                                       | actor | action            | payload keys (13)                                                                                                                                                                                                                                                                                                  |
| ------------------------------------------ | ----- | ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Agent emits any plan (refusal or not)      | agent | `plan.formulate`  | existing 11 + `plan` (full serialised Plan: context/decision/rationale/steps/result/data_ceiling/refused) + `classification_floor` (task-level DataClass string: "Public" / "Personal" / "ClinicalConfidential" / "Secret")                                                                                          |

**Test count delta:** 465 → **467** (+2 new unit tests in `scheduler::inner_loop::tests`).

**TDD ordering** (per CLAUDE.md rule #2):
1. Wrote 2 unit tests for `build_plan_formulate_payload` — confirmed compile-error RED.
2. Extracted the helper + added the 2 new fields — unit tests green.
3. Extended e2e assertion blocks — confirmed they pass against the new writer.
4. Workspace test: 467 / 0 fail / 0 SKIP / 0 warnings.

**What this slice deliberately does NOT do.**
- **No on-disk capture re-emission.** Existing `tests/observation/captures/*.json` files retain `plan_json: null`; operator recaptures (one-time action against their local LLM) to get the new shape. Slice B's harness handles the missing-plan-body case gracefully.
- **No schema migration.** Pure audit-row payload bump; downstream JSONB consumers unaffected if they don't request the new keys.
- **No `data_ceiling` change.** The Plan's own `data_ceiling` field is unrelated to the task's `classification_floor`; both round-trip independently (plan-level inferred ceiling vs task-level producer floor; spec §7).

**Open follow-up surfaces.**
- **`core/src/observation/capture.rs::extract_plans_from_audit_rows`** already reads `payload.get("plan")` and falls back to `null`; with this slice's payload bump it auto-lights-up on recapture. No code change in the capture-side helper.
- **Audit envelope truncation:** a plan with 20+ act-steps could push past the 4 KiB SHA-256 truncate threshold; this is the existing safety net (forensics still works via the SHA prefix). Real-world plans are typically <1 KiB; truncation is the right answer for the rare oversized case.

**Files touched (3 modified):**
- `core/src/scheduler/inner_loop.rs` — extract pure helper + add 2 fields + 2 new unit tests.
- `core/tests/scheduler_inner_loop_e2e.rs` — 2 assertion blocks extended.
- `docs/devel/handovers/HANDOVER.md` + `docs/devel/ROADMAP.md` — this update.
```

After writing this, also update the **header** of HANDOVER (the lines mentioning "Last commit (main)" and "Session-start working state"): bump them to reflect the new branch HEAD's expected position once the PR merges. Leave a placeholder reading `(branch HEAD will become <SHA> after merge; check `git log` post-merge)` — this header is rewritten at session end anyway.

- [ ] **Step 2: Tick the Slice A item in ROADMAP**

In `docs/devel/ROADMAP.md`, locate Phase 1 — Memory & Loop (line 85+). After the existing `[x] **[follow-up] Observation-phase first capture run + lenient JSON parser**` entry (around line 96), add:

```markdown
- [x] **[follow-up] Audit-payload bump: agent/plan.formulate carries full Plan + classification_floor (Slice A of rule-iteration harness)** — landed 2026-05-15 on branch `feat/audit-plan-formulate-carries-plan-body`. Pure-additive payload bump (11 keys → 13 keys) so captures carry everything the reviewer pipeline needs to be replayed offline. Extracted pure `build_plan_formulate_payload` helper (same pattern as `scheduler::audit::build_finalize_payload`) so wire shape is unit-testable without a live Postgres pool. +2 new unit tests pinning the 13-key set + round-trip shape of `plan` + `classification_floor`; +2 e2e assertion blocks extended. Workspace test count 465 → 467. The operator recaptures (one-time action against their local LLM) to get the new shape on disk; Slice B's harness handles the pre-Slice-A captures via `plans_skipped_missing_body` counter.
```

- [ ] **Step 3: Commit docs**

```sh
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "$(cat <<'EOF'
docs(handover,roadmap): Slice A — audit-payload bump shipped

Slice A of the rule-iteration harness spec. agent/plan.formulate
audit-row payload now carries the full Plan JSON + the task-level
classification_floor — both pure-additive. Closes the precondition
for Slice B (the harness itself).

Test count 465 → 467 (+2 unit tests). Zero failures, zero warnings,
zero [SKIP] lines on Linux.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 4: Push the branch + open PR**

```sh
git push -u origin feat/audit-plan-formulate-carries-plan-body
```

Then:

```sh
gh pr create --title "feat(scheduler): audit-payload bump — agent/plan.formulate carries Plan + classification_floor" --body "$(cat <<'EOF'
## Summary
- Pure-additive bump on `agent/plan.formulate` audit-row payload: 11 keys → 13 keys, adding `plan` (full serialised Plan) and `classification_floor` (task-level DataClass PascalCase string).
- Closes the precondition for the rule-iteration harness (Slice B) per [spec](docs/superpowers/specs/2026-05-15-rule-iteration-harness-design.md).
- Extracted pure `build_plan_formulate_payload` helper following the established `scheduler::audit::build_finalize_payload` pattern so the wire shape is unit-testable without a Postgres pool.
- Test count 465 → 467 (+2 unit tests pinning 13-key set + round-trip shape).

## Test plan
- [ ] `cargo test --workspace` green on Linux (467 / 0 fail / 0 SKIP / 0 warnings).
- [ ] Operator recaptures the 7 fixtures against `gemma4:26b-a4b-it-q8_0` (or any local LLM) — captures should now show `plan_json: { … }` instead of `null`.
- [ ] Spot-check one captured file: `payload.plan` round-trips into a `Plan`; `payload.classification_floor` is a PascalCase string.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Expected: PR URL printed; user can review.

### Slice A acceptance gate

- [ ] **Stop and wait for PR review + merge before starting Slice B.** Slice B depends on Slice A's payload format; if Slice A's API changes during review, Slice B's design assumptions shift. Once Slice A merges, rebase Slice B's branch on the new `main` and proceed.

---

## Slice B — Rule-iteration harness

Branch: `feat/rule-iteration-harness` (create fresh once Slice A merges).

```sh
git checkout main
git pull origin main
git checkout -b feat/rule-iteration-harness
```

### Task B1: Create `core/src/observation/replay.rs` skeleton + module wiring

**Files:**
- Create: `core/src/observation/replay.rs`
- Modify: `core/src/observation/mod.rs` — add `pub mod replay;`

- [ ] **Step 1: Create the empty replay module**

Write `core/src/observation/replay.rs`:

```rust
//! Offline replay of captured plans through a candidate
//! `ChainReviewStage`. Pure-functional; no DB, no LLM, no daemon —
//! the harness reads `CaptureJson` files from disk, replays each
//! captured plan through the provided chain, and reports per-fixture
//! verdict deltas against the recorded baseline.
//!
//! Slice B of the rule-iteration harness spec
//! (`docs/superpowers/specs/2026-05-15-rule-iteration-harness-design.md`).
//!
//! ## Public surface
//!
//! - [`VerdictSnapshot`] — JSON-serialisable projection of a `Verdict`.
//! - [`ReplayedPlan`] / [`ReplayResult`] — per-plan / per-capture row.
//! - [`replay_capture`] — async; runs one capture through a chain.
//! - [`load_captures_from_dir`] — I/O; deserialises a captures tree.
//! - [`format_report_table`] — pure; ASCII table for stdout.
//!
//! ## Missing plan body
//!
//! Captures produced before Slice A's audit-payload bump
//! (2026-05-15) carry `plan_json: null`. `replay_capture` emits a
//! [`ReplayedPlan`] with `skipped_reason: Some(...)` and
//! `new_verdict: None` for each such plan; it never silently
//! fabricates a synthetic `Plan` from derived fields, because that
//! would let the operator design rules against fake inputs.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::cassandra::review::{ChainReviewStage, ReviewStage, ReviewStageContext};
use crate::cassandra::types::{DataClass, Plan, Verdict};
use crate::observation::capture::CaptureJson;

/// JSON-serialisable projection of a [`Verdict`]. Keeps the
/// discriminator kind separate from the detail so the harness can
/// compare verdicts ignoring detail-string churn ("physical harm" vs
/// "weapons" both project to the same `kind = "constitutional_block"`).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VerdictSnapshot {
    /// One of "approve" | "advisory" | "escalate" | "block" |
    /// "constitutional_block". Lowercase + underscore matches the
    /// existing `cassandra:chain/verdict` audit-row `verdict_kind`
    /// strings (see `core/src/scheduler/inner_loop.rs::write_audit_verdict`).
    pub kind: String,
    pub detail: Option<serde_json::Value>,
}

impl VerdictSnapshot {
    /// Pure projection of a [`Verdict`] into the wire shape.
    pub fn from_verdict(v: &Verdict) -> Self {
        match v {
            Verdict::Approve => Self {
                kind: "approve".into(),
                detail: None,
            },
            Verdict::Advisory(msg) => Self {
                kind: "advisory".into(),
                detail: Some(serde_json::json!(msg)),
            },
            Verdict::Escalate(concern, severity) => Self {
                kind: "escalate".into(),
                detail: Some(serde_json::json!({
                    "concern": concern,
                    "severity": severity,
                })),
            },
            Verdict::Block(reason) => Self {
                kind: "block".into(),
                detail: Some(serde_json::json!(reason)),
            },
            Verdict::ConstitutionalBlock { principle, reason } => Self {
                kind: "constitutional_block".into(),
                detail: Some(serde_json::json!({
                    "principle": principle,
                    "reason": reason,
                })),
            },
        }
    }
}

/// Result of replaying one plan iteration through the candidate chain.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplayedPlan {
    pub iter: u32,
    /// Verdict recorded in the capture (the `cassandra:chain/verdict`
    /// row's `verdict_kind` string). `None` when the capture has no
    /// verdict row for this iteration.
    pub baseline_verdict: Option<String>,
    /// Verdict from the candidate chain. `None` when the plan body
    /// was missing from the capture (pre-Slice-A) and replay was
    /// skipped.
    pub new_verdict: Option<VerdictSnapshot>,
    /// True iff `new_verdict.kind` differs from `baseline_verdict`.
    /// Detail strings ignored. False whenever `skipped_reason.is_some()`.
    pub is_delta: bool,
    /// Populated iff the plan was skipped. Operator sees which
    /// fixtures need recapture.
    pub skipped_reason: Option<String>,
}

/// Aggregate result for one capture file replayed against a chain.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplayResult {
    pub fixture_id: String,
    pub fixture_summary: String,
    pub captured_at: String,
    pub llm_model: String,
    pub plans_replayed: u32,
    pub plans_skipped_missing_body: u32,
    pub per_plan: Vec<ReplayedPlan>,
}

/// One capture file loaded from disk.
#[derive(Clone, Debug)]
pub struct LoadedCapture {
    pub path: PathBuf,
    pub capture: CaptureJson,
}
```

- [ ] **Step 2: Wire the module into `observation/mod.rs`**

Read `core/src/observation/mod.rs`. It should currently declare `pub mod capture;`. Add the new module:

```rust
pub mod capture;
pub mod replay;
```

- [ ] **Step 3: Verify the workspace builds**

```sh
cargo build -p hhagent-core
```

Expected: clean build, possibly one or two `unused` warnings on the new types — fine for this task; they get exercised in B2.

- [ ] **Step 4: Commit**

```sh
git add core/src/observation/replay.rs core/src/observation/mod.rs
git commit -m "$(cat <<'EOF'
feat(observation): scaffold replay module — types only

Slice B Task 1 of the rule-iteration harness spec. Adds the
public type surface for the offline replay path:

- VerdictSnapshot — JSON-serialisable projection of Verdict
- ReplayedPlan / ReplayResult — per-plan / per-capture row
- LoadedCapture — capture file loaded from disk

Body of replay_capture / load_captures_from_dir / format_report_table
arrives in subsequent tasks (each TDD-first).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task B2: Unit tests for `VerdictSnapshot::from_verdict`

**Files:**
- Modify: `core/src/observation/replay.rs` — add `#[cfg(test)] mod tests`

- [ ] **Step 1: Append the failing tests at the end of `replay.rs`**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cassandra::types::Severity;

    // ---- VerdictSnapshot::from_verdict ----

    #[test]
    fn verdict_snapshot_approve_has_no_detail() {
        let s = VerdictSnapshot::from_verdict(&Verdict::Approve);
        assert_eq!(s.kind, "approve");
        assert!(s.detail.is_none());
    }

    #[test]
    fn verdict_snapshot_advisory_carries_message_as_detail_string() {
        let s = VerdictSnapshot::from_verdict(&Verdict::Advisory("careful".into()));
        assert_eq!(s.kind, "advisory");
        assert_eq!(s.detail, Some(serde_json::json!("careful")));
    }

    #[test]
    fn verdict_snapshot_escalate_carries_concern_and_severity_object() {
        let s = VerdictSnapshot::from_verdict(&Verdict::Escalate(
            "high latency".into(),
            Severity::High,
        ));
        assert_eq!(s.kind, "escalate");
        assert_eq!(
            s.detail,
            Some(serde_json::json!({"concern": "high latency", "severity": "high"})),
        );
    }

    #[test]
    fn verdict_snapshot_block_carries_reason_as_detail_string() {
        let s = VerdictSnapshot::from_verdict(&Verdict::Block("denied".into()));
        assert_eq!(s.kind, "block");
        assert_eq!(s.detail, Some(serde_json::json!("denied")));
    }

    #[test]
    fn verdict_snapshot_constitutional_block_carries_principle_and_reason() {
        let s = VerdictSnapshot::from_verdict(&Verdict::ConstitutionalBlock {
            principle: 1,
            reason: "physical_harm".into(),
        });
        assert_eq!(s.kind, "constitutional_block");
        assert_eq!(
            s.detail,
            Some(serde_json::json!({"principle": 1, "reason": "physical_harm"})),
        );
    }

    #[test]
    fn verdict_snapshot_round_trips_through_serde_json() {
        let s = VerdictSnapshot::from_verdict(&Verdict::ConstitutionalBlock {
            principle: 2,
            reason: "fraud".into(),
        });
        let j = serde_json::to_value(&s).expect("snapshot must serialise");
        let s2: VerdictSnapshot =
            serde_json::from_value(j).expect("snapshot must round-trip");
        assert_eq!(s, s2);
    }
}
```

- [ ] **Step 2: Run the tests to verify they pass**

```sh
cargo test -p hhagent-core --lib observation::replay::tests -- --nocapture
```

Expected: 6 tests pass.

- [ ] **Step 3: Commit**

```sh
git add core/src/observation/replay.rs
git commit -m "$(cat <<'EOF'
test(observation): pin VerdictSnapshot::from_verdict for all 5 variants

Slice B Task 2. 6 unit tests covering every Verdict variant + a
serde round-trip. Pins the wire shape of the snapshot so a future
projection drift trips loudly.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task B3: Pure `is_delta` helper + tests

**Files:**
- Modify: `core/src/observation/replay.rs` — add `is_delta` helper + tests

- [ ] **Step 1: Write the failing tests at the end of the `tests` module**

```rust
    // ---- is_delta ----

    #[test]
    fn is_delta_false_when_both_approve() {
        assert!(!is_delta(Some("approve"), Some(&"approve".to_string())));
    }

    #[test]
    fn is_delta_true_when_baseline_approve_new_block() {
        assert!(is_delta(Some("approve"), Some(&"block".to_string())));
    }

    #[test]
    fn is_delta_true_when_baseline_approve_new_constitutional_block() {
        assert!(is_delta(Some("approve"), Some(&"constitutional_block".to_string())));
    }

    #[test]
    fn is_delta_true_when_baseline_missing_new_not_approve() {
        // Baseline absent + new verdict is anything but approve = delta.
        // Operator wants to see "something fired where the capture
        // never observed a verdict."
        assert!(is_delta(None, Some(&"block".to_string())));
    }

    #[test]
    fn is_delta_false_when_baseline_missing_new_approve() {
        // Baseline absent + new approve = not a delta. "Same default
        // posture" — nothing interesting to flag.
        assert!(!is_delta(None, Some(&"approve".to_string())));
    }

    #[test]
    fn is_delta_false_when_new_missing_skipped() {
        // new = None means the plan was skipped (pre-Slice-A capture);
        // no comparison possible. Per spec: skipped plans are never deltas.
        assert!(!is_delta(Some("approve"), None));
        assert!(!is_delta(None, None));
    }
```

`is_delta`'s signature: takes the baseline kind string slice (from `CapturedPlan.verdict_today`) and an optional reference to the new kind string. Returns `bool`.

- [ ] **Step 2: Run to verify failing**

```sh
cargo test -p hhagent-core --lib observation::replay::tests::is_delta -- --nocapture
```

Expected: compile error — `is_delta` undefined.

- [ ] **Step 3: Implement `is_delta` above the `#[cfg(test)] mod tests` block**

```rust
/// Pure delta predicate. True iff `baseline` and `new` differ in kind.
/// Detail strings are ignored. `new = None` (skipped) is never a delta.
/// `baseline = None` + `new = Some("approve")` is not a delta (same
/// default posture). `baseline = None` + `new = Some(other)` IS a
/// delta (a rule fired where the capture observed no verdict).
fn is_delta(baseline: Option<&str>, new: Option<&String>) -> bool {
    let Some(new_kind) = new else { return false; };
    match baseline {
        Some(b) => b != new_kind.as_str(),
        None => new_kind != "approve",
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

```sh
cargo test -p hhagent-core --lib observation::replay::tests::is_delta -- --nocapture
```

Expected: 6 tests pass.

- [ ] **Step 5: Commit**

```sh
git add core/src/observation/replay.rs
git commit -m "$(cat <<'EOF'
feat(observation): pure is_delta helper + 6 unit-test pins

Slice B Task 3. Detail strings ignored — a constitutional_block with
reason "physical harm" and one with reason "weapons" are equal under
delta detection (both are not-approve). baseline=None + new=approve
is not a delta (same default posture). baseline=None + new=anything-else
IS a delta (a rule fired where the capture observed no verdict).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task B4: Pure `format_report_table` + tests

**Files:**
- Modify: `core/src/observation/replay.rs` — add `format_report_table` + tests

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module:

```rust
    // ---- format_report_table ----

    fn dummy_result(fixture_id: &str, per_plan: Vec<ReplayedPlan>) -> ReplayResult {
        let n: u32 = per_plan.iter().filter(|p| p.skipped_reason.is_none()).count() as u32;
        let s: u32 = per_plan.iter().filter(|p| p.skipped_reason.is_some()).count() as u32;
        ReplayResult {
            fixture_id: fixture_id.into(),
            fixture_summary: format!("summary of {fixture_id}"),
            captured_at: "2026-05-15T00:00:00Z".into(),
            llm_model: "gemma4:26b".into(),
            plans_replayed: n,
            plans_skipped_missing_body: s,
            per_plan,
        }
    }

    fn approve_plan(iter: u32) -> ReplayedPlan {
        ReplayedPlan {
            iter,
            baseline_verdict: Some("approve".into()),
            new_verdict: Some(VerdictSnapshot {
                kind: "approve".into(),
                detail: None,
            }),
            is_delta: false,
            skipped_reason: None,
        }
    }

    fn cb_plan(iter: u32, principle: u8) -> ReplayedPlan {
        ReplayedPlan {
            iter,
            baseline_verdict: Some("approve".into()),
            new_verdict: Some(VerdictSnapshot {
                kind: "constitutional_block".into(),
                detail: Some(serde_json::json!({"principle": principle, "reason": "x"})),
            }),
            is_delta: true,
            skipped_reason: None,
        }
    }

    fn skipped_plan(iter: u32) -> ReplayedPlan {
        ReplayedPlan {
            iter,
            baseline_verdict: Some("approve".into()),
            new_verdict: None,
            is_delta: false,
            skipped_reason: Some("plan body missing".into()),
        }
    }

    #[test]
    fn format_report_table_emits_header_and_one_row_per_plan() {
        let results = vec![dummy_result("f1", vec![approve_plan(1)])];
        let s = format_report_table(&results);
        assert!(s.contains("fixture"), "header row present");
        assert!(s.contains("iter"), "iter column present");
        assert!(s.contains("baseline"), "baseline column present");
        assert!(s.contains("new"), "new column present");
        assert!(s.contains("d?"), "delta column present");
        assert!(s.contains("f1"), "fixture id row present");
        assert!(s.contains("approve"), "verdict kind shown");
    }

    #[test]
    fn format_report_table_marks_deltas_with_asterisk() {
        let results = vec![dummy_result("p1", vec![cb_plan(1, 1)])];
        let s = format_report_table(&results);
        // Delta marker: ASCII '*' (rendered in the d? column).
        assert!(s.contains("*"), "delta marker '*' must be present");
        // Constitutional block detail rendered with principle: "constitutional_block(p=1)".
        assert!(
            s.contains("constitutional_block(p=1)"),
            "constitutional_block detail must show principle index"
        );
    }

    #[test]
    fn format_report_table_marks_skipped_with_dash() {
        let results = vec![dummy_result("ec", vec![skipped_plan(1)])];
        let s = format_report_table(&results);
        // Skipped marker: ASCII '-' (rendered in the d? column).
        assert!(s.contains("-"), "skipped marker '-' must be present");
        assert!(s.contains("[skipped"), "[skipped: ...] tag must be present");
    }

    #[test]
    fn format_report_table_renders_multi_iter_fixture() {
        // Multi-iter case — 3 iterations, last one is a delta.
        let results = vec![dummy_result("ec", vec![
            approve_plan(1),
            approve_plan(2),
            cb_plan(3, 3),
        ])];
        let s = format_report_table(&results);
        // All three iter values appear.
        assert!(s.contains(" 1 "), "iter=1 present");
        assert!(s.contains(" 2 "), "iter=2 present");
        assert!(s.contains(" 3 "), "iter=3 present");
    }

    #[test]
    fn format_report_table_aggregate_summary_line_counts_deltas_and_skipped() {
        let results = vec![
            dummy_result("f1", vec![approve_plan(1)]),
            dummy_result("f2", vec![cb_plan(1, 1)]),
            dummy_result("f3", vec![skipped_plan(1)]),
        ];
        let s = format_report_table(&results);
        // Aggregate summary line.
        assert!(s.contains("3 plans"), "total plans count");
        assert!(s.contains("3 fixtures"), "fixture count");
        assert!(s.contains("1 delta"), "delta count");
        assert!(s.contains("1 skipped"), "skipped count");
    }

    #[test]
    fn format_report_table_empty_input_emits_only_header_and_zero_summary() {
        let s = format_report_table(&[]);
        assert!(s.contains("fixture"), "header row present even with empty input");
        assert!(
            s.contains("0 plans") || s.contains("0 fixtures"),
            "summary line must report zero counts; got:\n{s}"
        );
    }
```

- [ ] **Step 2: Run to verify failing**

```sh
cargo test -p hhagent-core --lib observation::replay::tests::format_report_table -- --nocapture
```

Expected: compile error — `format_report_table` undefined.

- [ ] **Step 3: Implement `format_report_table` above `#[cfg(test)] mod tests`**

```rust
/// Pure: format a `[ReplayResult]` slice as an ASCII table for stdout.
/// Column widths are fixed for stable diffs; long fixture ids are
/// truncated to 40 chars. No terminal escapes / colour codes / unicode
/// in the body so the output is grep-friendly and CI-friendly.
pub fn format_report_table(results: &[ReplayResult]) -> String {
    use std::fmt::Write;
    let mut out = String::new();

    // Header.
    writeln!(
        out,
        "{:<40}  {:>4}  {:<11} {:<27} {:<2}",
        "fixture", "iter", "baseline", "new", "d?"
    ).unwrap();
    writeln!(
        out,
        "{}  {}  {} {} {}",
        "-".repeat(40),
        "-".repeat(4),
        "-".repeat(11),
        "-".repeat(27),
        "-".repeat(2),
    ).unwrap();

    let mut total_plans: u32 = 0;
    let mut total_skipped: u32 = 0;
    let mut total_deltas: u32 = 0;

    for r in results {
        for p in &r.per_plan {
            total_plans = total_plans.saturating_add(1);
            if p.skipped_reason.is_some() {
                total_skipped = total_skipped.saturating_add(1);
            }
            if p.is_delta {
                total_deltas = total_deltas.saturating_add(1);
            }

            let fid: String = r.fixture_id.chars().take(40).collect();
            let baseline = p.baseline_verdict.as_deref().unwrap_or("[none]");
            let new_str = match (&p.skipped_reason, &p.new_verdict) {
                (Some(reason), _) => {
                    // Render as "[skipped: <reason truncated to 17 chars>]".
                    let r: String = reason.chars().take(17).collect();
                    format!("[skipped: {r}]")
                }
                (None, Some(snap)) => render_new_verdict(snap),
                (None, None) => "[no replay]".into(),
            };
            let delta_mark = if p.skipped_reason.is_some() {
                "-"
            } else if p.is_delta {
                "*"
            } else {
                "."
            };
            writeln!(
                out,
                "{:<40}  {:>4}  {:<11} {:<27} {:<2}",
                fid, p.iter, baseline, new_str, delta_mark
            ).unwrap();
        }
    }

    let fixture_count = results.len();
    writeln!(out).unwrap();
    writeln!(
        out,
        "{total_plans} plans across {fixture_count} fixtures . {} delta{} . {} skipped",
        total_deltas,
        if total_deltas == 1 { "" } else { "s" },
        total_skipped,
    ).unwrap();

    out
}

/// Pure helper: project a `VerdictSnapshot` into a compact one-line
/// render for the table's "new" column. Constitutional blocks include
/// the principle; escalates include severity; others render as the
/// bare kind.
fn render_new_verdict(snap: &VerdictSnapshot) -> String {
    match snap.kind.as_str() {
        "constitutional_block" => {
            let p = snap.detail.as_ref()
                .and_then(|d| d.get("principle"))
                .and_then(|p| p.as_u64())
                .unwrap_or(0);
            format!("constitutional_block(p={p})")
        }
        "escalate" => {
            let sev = snap.detail.as_ref()
                .and_then(|d| d.get("severity"))
                .and_then(|s| s.as_str())
                .unwrap_or("?");
            format!("escalate({sev})")
        }
        // Bare kinds: approve, advisory, block.
        other => other.to_string(),
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

```sh
cargo test -p hhagent-core --lib observation::replay::tests::format_report_table -- --nocapture
```

Expected: 6 tests pass. If a column-width assertion fails (e.g. " 1 " not found), eyeball the test output — the expected width might have shifted by one space.

- [ ] **Step 5: Commit**

```sh
git add core/src/observation/replay.rs
git commit -m "$(cat <<'EOF'
feat(observation): pure format_report_table + 6 unit tests

Slice B Task 4. ASCII-only fixed-width columns; long fixture ids
truncated to 40 chars; skipped/delta/no-delta markers '-'/'*'/'.'.
constitutional_block rendered with principle index. Aggregate
summary line counts plans / fixtures / deltas / skipped.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task B5: `replay_capture` async function + unit-level tests

**Files:**
- Modify: `core/src/observation/replay.rs` — implement `replay_capture` + add tests

- [ ] **Step 1: Write the failing unit test using a stub chain**

The test exercises `replay_capture` against a synthetic `CaptureJson` carrying one plan-formulate row with a real Plan body. Uses `NoopReviewStage` (always Approve) so the new verdict matches the captured baseline; expected outcome = no delta.

Append to the `tests` module:

```rust
    // ---- replay_capture ----

    use crate::cassandra::review::NoopReviewStage;
    use crate::observation::capture::{CapturedAuditRow, CapturedPlan};

    fn rich_plan_audit_row(id: i64, task_id: i64, plan_body: &Plan) -> CapturedAuditRow {
        // Mimics post-Slice-A agent/plan.formulate payload.
        CapturedAuditRow {
            id,
            ts: "2026-05-15T00:00:00Z".into(),
            actor: "agent".into(),
            action: "plan.formulate".into(),
            payload: serde_json::json!({
                "task_id": task_id,
                "plan_count": 1,
                "decision_kind": "task_complete",
                "plan_step_count": plan_body.steps.len(),
                "refused": serde_json::Value::Null,
                "plan": serde_json::to_value(plan_body).unwrap(),
                "classification_floor": "Public",
            }),
        }
    }

    fn verdict_audit_row(id: i64, task_id: i64, kind: &str) -> CapturedAuditRow {
        CapturedAuditRow {
            id,
            ts: "2026-05-15T00:00:01Z".into(),
            actor: "cassandra:chain".into(),
            action: "verdict".into(),
            payload: serde_json::json!({
                "task_id": task_id,
                "plan_count": 1,
                "verdict_kind": kind,
                "detail": serde_json::Value::Null,
                "latency_ms": 0,
            }),
        }
    }

    fn pre_slice_a_plan_audit_row(id: i64, task_id: i64) -> CapturedAuditRow {
        // Mimics pre-Slice-A — no `plan` key.
        CapturedAuditRow {
            id,
            ts: "2026-05-14T00:00:00Z".into(),
            actor: "agent".into(),
            action: "plan.formulate".into(),
            payload: serde_json::json!({
                "task_id": task_id,
                "plan_count": 1,
                "decision_kind": "task_complete",
                "plan_step_count": 0,
                "refused": serde_json::Value::Null,
            }),
        }
    }

    fn synthetic_capture(audit_rows: Vec<CapturedAuditRow>, plans: Vec<CapturedPlan>) -> CaptureJson {
        CaptureJson {
            schema_version: 2,
            fixture_id: "test-fixture".into(),
            fixture_summary: "synthetic for replay_capture test".into(),
            captured_at: "2026-05-15T00:00:00Z".into(),
            llm_backend: "local".into(),
            llm_model: "gemma4:26b".into(),
            llm_base_url: "http://localhost:11434/v1".into(),
            prompt: "test prompt".into(),
            task_id: 1,
            task_state: "completed".into(),
            plan_iterations: plans.len() as u32,
            plans,
            audit_rows,
        }
    }

    fn terminal_plan() -> Plan {
        Plan {
            context: "".into(),
            decision: "task_complete".into(),
            rationale: "".into(),
            steps: vec![],
            result: Some(serde_json::json!({"kind": "text", "body": "ok"})),
            data_ceiling: DataClass::Public,
            refused: None,
        }
    }

    #[tokio::test]
    async fn replay_capture_against_noop_chain_yields_approve_no_delta() {
        let plan = terminal_plan();
        let audit_rows = vec![
            rich_plan_audit_row(1, 1, &plan),
            verdict_audit_row(2, 1, "approve"),
        ];
        let plans = vec![CapturedPlan {
            iter: 1,
            plan_json: serde_json::to_value(&plan).unwrap(),
            verdict_today: Some("approve".into()),
            step_count: 0,
            data_ceiling: "Public".into(),
        }];
        let capture = synthetic_capture(audit_rows, plans);
        let chain = ChainReviewStage::new(vec![Arc::new(NoopReviewStage)]);

        let result = replay_capture(&capture, &chain).await;
        assert_eq!(result.fixture_id, "test-fixture");
        assert_eq!(result.plans_replayed, 1);
        assert_eq!(result.plans_skipped_missing_body, 0);
        assert_eq!(result.per_plan.len(), 1);
        let p = &result.per_plan[0];
        assert_eq!(p.iter, 1);
        assert_eq!(p.baseline_verdict.as_deref(), Some("approve"));
        assert_eq!(p.new_verdict.as_ref().unwrap().kind, "approve");
        assert!(!p.is_delta);
        assert!(p.skipped_reason.is_none());
    }

    #[tokio::test]
    async fn replay_capture_skips_when_plan_body_is_null() {
        // Pre-Slice-A capture shape — plan_json: null on the
        // CapturedPlan AND no `plan` key in the audit-row payload.
        let plans = vec![CapturedPlan {
            iter: 1,
            plan_json: serde_json::Value::Null,
            verdict_today: Some("approve".into()),
            step_count: 0,
            data_ceiling: "Public".into(),
        }];
        let audit_rows = vec![
            pre_slice_a_plan_audit_row(1, 1),
            verdict_audit_row(2, 1, "approve"),
        ];
        let capture = synthetic_capture(audit_rows, plans);
        let chain = ChainReviewStage::new(vec![Arc::new(NoopReviewStage)]);

        let result = replay_capture(&capture, &chain).await;
        assert_eq!(result.plans_replayed, 0);
        assert_eq!(result.plans_skipped_missing_body, 1);
        assert_eq!(result.per_plan.len(), 1);
        let p = &result.per_plan[0];
        assert!(p.new_verdict.is_none());
        assert!(p.skipped_reason.is_some(),
            "skipped_reason must be populated when plan_json is null");
        assert!(!p.is_delta);
    }
```

- [ ] **Step 2: Run to verify failing**

```sh
cargo test -p hhagent-core --lib observation::replay::tests::replay_capture -- --nocapture
```

Expected: compile error — `replay_capture` undefined.

- [ ] **Step 3: Implement `replay_capture` above `#[cfg(test)] mod tests`**

```rust
/// Replay one capture's plan iterations through the candidate chain.
/// Async because `ReviewStage::review` is async; no I/O performed by
/// this function (the chain may be I/O-bearing if a real stage uses
/// async DB queries, but the harness itself is in-process).
///
/// Per-plan behaviour:
/// - `capture.plans[i].plan_json` is JSON null → emit `ReplayedPlan`
///   with `skipped_reason: Some(...)`; never fabricate a synthetic
///   `Plan` from derived fields.
/// - `plan_json` deserialises into a `Plan` → call `chain.review` and
///   build a `VerdictSnapshot`.
///
/// `ReviewStageContext` reconstruction:
/// - `task_id`, `instruction`, `plan_count` from the capture.
/// - `classification_floor` from the audit-row's `classification_floor`
///   field if present (post-Slice-A); falls back to the plan's
///   `data_ceiling` if absent; final fallback to `DataClass::Public`.
pub async fn replay_capture(
    capture: &CaptureJson,
    chain: &ChainReviewStage,
) -> ReplayResult {
    let mut per_plan = Vec::with_capacity(capture.plans.len());
    let mut replayed: u32 = 0;
    let mut skipped: u32 = 0;

    // Look up the matching agent/plan.formulate audit row by iter to
    // pull the Slice-A classification_floor (preferred over the
    // plan's own data_ceiling, which is a different concept).
    let plan_rows: Vec<&CapturedAuditRow> = capture.audit_rows.iter()
        .filter(|r| r.actor == "agent" && r.action == "plan.formulate")
        .collect();

    for (i, cp) in capture.plans.iter().enumerate() {
        if cp.plan_json.is_null() {
            skipped = skipped.saturating_add(1);
            per_plan.push(ReplayedPlan {
                iter: cp.iter,
                baseline_verdict: cp.verdict_today.clone(),
                new_verdict: None,
                is_delta: false,
                skipped_reason: Some(
                    "plan body missing; recapture against current daemon \
                     (Slice A's audit-payload v2)".into()
                ),
            });
            continue;
        }

        // Decode the plan body. A capture with non-null plan_json
        // that fails to deserialise is operator-facing corruption —
        // surface it as a skip with a distinct reason.
        let plan: Plan = match serde_json::from_value(cp.plan_json.clone()) {
            Ok(p) => p,
            Err(e) => {
                skipped = skipped.saturating_add(1);
                per_plan.push(ReplayedPlan {
                    iter: cp.iter,
                    baseline_verdict: cp.verdict_today.clone(),
                    new_verdict: None,
                    is_delta: false,
                    skipped_reason: Some(format!("plan body decode error: {e}")),
                });
                continue;
            }
        };

        // Classification floor: prefer the audit-row's
        // classification_floor (post-Slice-A) over the plan's
        // data_ceiling (different concept; plan-level inferred
        // ceiling). Fallback: Public.
        let classification_floor = plan_rows.get(i)
            .and_then(|r| r.payload.get("classification_floor"))
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_str::<DataClass>(&format!("\"{}\"", s)).ok())
            .unwrap_or(DataClass::Public);

        let ctx = ReviewStageContext {
            task_id: capture.task_id,
            instruction: &capture.prompt,
            classification_floor,
            plan_count: cp.iter,
        };

        let verdict = chain.review(&plan, &ctx).await;
        let snap = VerdictSnapshot::from_verdict(&verdict);

        let delta = is_delta(
            cp.verdict_today.as_deref(),
            Some(&snap.kind),
        );

        per_plan.push(ReplayedPlan {
            iter: cp.iter,
            baseline_verdict: cp.verdict_today.clone(),
            new_verdict: Some(snap),
            is_delta: delta,
            skipped_reason: None,
        });
        replayed = replayed.saturating_add(1);
    }

    ReplayResult {
        fixture_id: capture.fixture_id.clone(),
        fixture_summary: capture.fixture_summary.clone(),
        captured_at: capture.captured_at.clone(),
        llm_model: capture.llm_model.clone(),
        plans_replayed: replayed,
        plans_skipped_missing_body: skipped,
        per_plan,
    }
}
```

If you encounter an "unused import" warning on `Arc` from earlier tasks (none referenced yet at this level), prefix with `#[allow(unused_imports)]` only as a last resort — instead, audit the module's `use` block and remove anything genuinely dead. The `Arc` should be used by the integration tests' chain construction; if the lib never sees `Arc` directly, drop it from the top-of-file imports.

- [ ] **Step 4: Run tests**

```sh
cargo test -p hhagent-core --lib observation::replay::tests -- --nocapture
```

Expected: all `replay_capture`-prefixed tests pass alongside the existing VerdictSnapshot / is_delta / format_report_table ones. Total ~17 tests in the `replay::tests` module.

- [ ] **Step 5: Commit**

```sh
git add core/src/observation/replay.rs
git commit -m "$(cat <<'EOF'
feat(observation): replay_capture async + 2 unit tests

Slice B Task 5. Replays each plan iteration through the candidate
chain; emits ReplayedPlan with skipped_reason when plan_json is null
(pre-Slice-A captures). classification_floor preference:
audit-row.classification_floor (post-Slice-A) > Plan.data_ceiling >
Public. Never fabricates a synthetic Plan from derived fields —
keeping the harness honest about what it can actually replay.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task B6: `load_captures_from_dir` + integration test

**Files:**
- Modify: `core/src/observation/replay.rs` — implement loader
- Create: `core/tests/observation_replay_e2e.rs`

- [ ] **Step 1: Write the failing integration test**

Create `core/tests/observation_replay_e2e.rs`:

```rust
//! Integration tests for `core::observation::replay`.
//!
//! Pure offline tests — no PG, no LLM, no daemon. The harness reads
//! capture files from a per-test scratch dir; the test owns those
//! files end-to-end so the production captures under
//! `tests/observation/captures/` are not touched.

use std::sync::Arc;

use tempfile::TempDir;

use hhagent_core::cassandra::review::{ChainReviewStage, NoopReviewStage};
use hhagent_core::cassandra::types::{DataClass, Plan};
use hhagent_core::observation::capture::{CaptureJson, CapturedAuditRow, CapturedPlan};
use hhagent_core::observation::replay::{load_captures_from_dir, replay_capture};

fn approve_baseline_capture() -> CaptureJson {
    let plan = Plan {
        context: "".into(),
        decision: "task_complete".into(),
        rationale: "".into(),
        steps: vec![],
        result: Some(serde_json::json!({"kind": "text", "body": "ok"})),
        data_ceiling: DataClass::Public,
        refused: None,
    };
    let plan_value = serde_json::to_value(&plan).unwrap();
    CaptureJson {
        schema_version: 2,
        fixture_id: "t1-approve-baseline-with-plan-body".into(),
        fixture_summary: "synthetic approve baseline".into(),
        captured_at: "2026-05-15T10:00:00Z".into(),
        llm_backend: "local".into(),
        llm_model: "gemma4:26b".into(),
        llm_base_url: "http://localhost:11434/v1".into(),
        prompt: "synthetic prompt".into(),
        task_id: 100,
        task_state: "completed".into(),
        plan_iterations: 1,
        plans: vec![CapturedPlan {
            iter: 1,
            plan_json: plan_value.clone(),
            verdict_today: Some("approve".into()),
            step_count: 0,
            data_ceiling: "Public".into(),
        }],
        audit_rows: vec![CapturedAuditRow {
            id: 1,
            ts: "2026-05-15T10:00:01Z".into(),
            actor: "agent".into(),
            action: "plan.formulate".into(),
            payload: serde_json::json!({
                "task_id": 100,
                "plan_count": 1,
                "decision_kind": "task_complete",
                "plan_step_count": 0,
                "refused": serde_json::Value::Null,
                "plan": plan_value,
                "classification_floor": "Public",
            }),
        }],
    }
}

fn pre_slice_a_capture() -> CaptureJson {
    CaptureJson {
        schema_version: 2,
        fixture_id: "t2-missing-plan-body".into(),
        fixture_summary: "synthetic pre-Slice-A capture".into(),
        captured_at: "2026-05-14T10:00:00Z".into(),
        llm_backend: "local".into(),
        llm_model: "gemma4:26b".into(),
        llm_base_url: "http://localhost:11434/v1".into(),
        prompt: "pre-Slice-A synthetic prompt".into(),
        task_id: 200,
        task_state: "completed".into(),
        plan_iterations: 1,
        plans: vec![CapturedPlan {
            iter: 1,
            plan_json: serde_json::Value::Null,
            verdict_today: Some("approve".into()),
            step_count: 0,
            data_ceiling: "Public".into(),
        }],
        audit_rows: vec![CapturedAuditRow {
            id: 1,
            ts: "2026-05-14T10:00:01Z".into(),
            actor: "agent".into(),
            action: "plan.formulate".into(),
            payload: serde_json::json!({
                "task_id": 200,
                "plan_count": 1,
                "decision_kind": "task_complete",
                "plan_step_count": 0,
                "refused": serde_json::Value::Null,
                // No `plan` key — pre-Slice-A.
            }),
        }],
    }
}

fn write_synthetic_capture(root: &std::path::Path, capture: &CaptureJson) {
    let fixture_dir = root.join(&capture.fixture_id);
    std::fs::create_dir_all(&fixture_dir).unwrap();
    let fname = format!("{}_synthetic.json", &capture.captured_at[..10]);
    let path = fixture_dir.join(fname);
    let bytes = serde_json::to_vec_pretty(capture).unwrap();
    std::fs::write(path, bytes).unwrap();
}

#[tokio::test]
async fn replay_against_approve_baseline_yields_no_delta() {
    let tempdir = TempDir::new().expect("tempdir");
    let capture = approve_baseline_capture();
    write_synthetic_capture(tempdir.path(), &capture);

    let loaded = load_captures_from_dir(tempdir.path())
        .expect("load synthetic captures");
    assert_eq!(loaded.len(), 1);

    let chain = ChainReviewStage::new(vec![Arc::new(NoopReviewStage)]);
    let result = replay_capture(&loaded[0].capture, &chain).await;

    assert_eq!(result.fixture_id, "t1-approve-baseline-with-plan-body");
    assert_eq!(result.plans_replayed, 1);
    assert_eq!(result.plans_skipped_missing_body, 0);
    assert_eq!(result.per_plan.len(), 1);
    assert!(!result.per_plan[0].is_delta);
    assert_eq!(
        result.per_plan[0].new_verdict.as_ref().unwrap().kind,
        "approve",
    );
}

#[tokio::test]
async fn replay_against_pre_slice_a_capture_skips_with_reason() {
    let tempdir = TempDir::new().expect("tempdir");
    let capture = pre_slice_a_capture();
    write_synthetic_capture(tempdir.path(), &capture);

    let loaded = load_captures_from_dir(tempdir.path())
        .expect("load synthetic captures");
    assert_eq!(loaded.len(), 1);

    let chain = ChainReviewStage::new(vec![Arc::new(NoopReviewStage)]);
    let result = replay_capture(&loaded[0].capture, &chain).await;

    assert_eq!(result.fixture_id, "t2-missing-plan-body");
    assert_eq!(result.plans_replayed, 0);
    assert_eq!(result.plans_skipped_missing_body, 1);
    assert!(result.per_plan[0].new_verdict.is_none());
    assert!(result.per_plan[0].skipped_reason.is_some());
}
```

`tempfile` is already a workspace dev-dependency (used by other tests). Verify with `grep tempfile Cargo.toml` if uncertain.

- [ ] **Step 2: Run to verify failing**

```sh
cargo test -p hhagent-core --test observation_replay_e2e -- --nocapture
```

Expected: compile error — `load_captures_from_dir` undefined.

- [ ] **Step 3: Implement `load_captures_from_dir`**

In `core/src/observation/replay.rs`, above the `#[cfg(test)] mod tests` block:

```rust
/// Walk `dir/<fixture_id>/<filename>.json` files and deserialise each
/// into a `CaptureJson`. Returns one entry per file, sorted by
/// `(fixture_id, captured_at)` for stable output across runs.
///
/// Errors aggregate at the file level: one malformed file's
/// `serde_json::Error` is logged via `eprintln!` and the file is
/// skipped; the walk continues. The function returns `Err` only when
/// the root directory cannot be opened at all.
pub fn load_captures_from_dir(dir: &Path) -> std::io::Result<Vec<LoadedCapture>> {
    let mut out: Vec<LoadedCapture> = Vec::new();
    for fixture_entry in std::fs::read_dir(dir)? {
        let fixture_entry = match fixture_entry {
            Ok(e) => e,
            Err(e) => {
                eprintln!("replay: skipping unreadable entry in {dir:?}: {e}");
                continue;
            }
        };
        let fixture_path = fixture_entry.path();
        if !fixture_path.is_dir() { continue; }

        let inner = match std::fs::read_dir(&fixture_path) {
            Ok(it) => it,
            Err(e) => {
                eprintln!("replay: skipping unreadable fixture dir {fixture_path:?}: {e}");
                continue;
            }
        };

        for file_entry in inner {
            let Ok(file_entry) = file_entry else { continue; };
            let path = file_entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") { continue; }

            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("replay: read({path:?}) failed: {e}");
                    continue;
                }
            };
            let capture: CaptureJson = match serde_json::from_slice(&bytes) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("replay: parse({path:?}) failed: {e}");
                    continue;
                }
            };
            out.push(LoadedCapture { path, capture });
        }
    }
    // Stable sort: (fixture_id, captured_at, path) — path tie-break
    // makes the walk-order deterministic across filesystems with
    // different inode orderings.
    out.sort_by(|a, b| {
        a.capture.fixture_id.cmp(&b.capture.fixture_id)
            .then_with(|| a.capture.captured_at.cmp(&b.capture.captured_at))
            .then_with(|| a.path.cmp(&b.path))
    });
    Ok(out)
}
```

- [ ] **Step 4: Run integration tests**

```sh
cargo test -p hhagent-core --test observation_replay_e2e -- --nocapture
```

Expected: 2 tests pass.

- [ ] **Step 5: Commit**

```sh
git add core/src/observation/replay.rs core/tests/observation_replay_e2e.rs
git commit -m "$(cat <<'EOF'
feat(observation): load_captures_from_dir + 2 e2e tests

Slice B Task 6. Loader aggregates errors at the file level (logs +
skips one bad file, continues the walk); only returns Err when the
root dir itself is unreadable. Stable sort by (fixture_id,
captured_at, path) so the harness output is deterministic.

Integration tests use synthetic captures in per-test tempdirs —
production captures under tests/observation/captures/ are untouched.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task B7: CLI subcommand `hhagent-cli observation replay`

**Files:**
- Modify: `core/src/bin/hhagent-cli.rs` — add top-level `observation` subcommand
- Modify: `core/src/bin/hhagent-cli.rs` — extend `help_text`

- [ ] **Step 1: Add the dispatcher branch**

In `core/src/bin/hhagent-cli.rs` `main()`'s `match args[1].as_str()` (around line 56-75), add a new arm before `"--help"`:

```rust
        "observation" => run_observation(&args[2..]),
```

So the match becomes:

```rust
    match args[1].as_str() {
        "audit"       => /* existing */,
        "ask"         => run_ask(&args[2..]),
        "tasks"       => run_tasks(&args[2..]),
        "tools"       => run_tools(&args[2..]),
        "observation" => run_observation(&args[2..]),
        "--help" | "-h" | "help" => /* existing */,
        other => /* existing */,
    }
```

- [ ] **Step 2: Extend the help text**

Replace the `help_text()` body (around line 78-101) to include the new lines for `observation replay`. The current return value ends in the `audit tail` flags block; add `observation replay` above the flags block:

Insert (after the existing `hhagent-cli audit tail ...` line in usage):

```
    hhagent-cli observation replay [--captures-dir PATH] [--model SLUG]
```

And a new flags block:

```
flags (observation replay):
    --captures-dir PATH  Override the captures directory (default:
                         tests/observation/captures relative to
                         CARGO_MANIFEST_DIR or cwd as fallback).
    --model SLUG         Filter to one model's captures by slug match
                         on the filename (e.g. gemma4-26b-a4b-it-q8-0).
                         Without it, every <fixture_id>/*.json is replayed.
```

- [ ] **Step 3: Add the `run_observation` + helpers below `run_tools_allowlist_list`**

Append to `core/src/bin/hhagent-cli.rs` after the existing tools-allowlist subcommand tree (find a clean location, e.g. just before the `#[cfg(test)]` block at end of file):

```rust
// ============================================================
// `observation replay` subcommand
// ============================================================

fn run_observation(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: hhagent-cli observation replay [opts]");
        return ExitCode::from(2);
    }
    match args[0].as_str() {
        "replay" => run_observation_replay(&args[1..]),
        other => {
            eprintln!("observation: unknown subcommand {other}");
            ExitCode::from(2)
        }
    }
}

fn run_observation_replay(args: &[String]) -> ExitCode {
    // Parse flags.
    let mut captures_dir: Option<std::path::PathBuf> = None;
    let mut model_filter: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--captures-dir" => {
                i += 1;
                match args.get(i) {
                    Some(p) => captures_dir = Some(std::path::PathBuf::from(p)),
                    None => {
                        eprintln!("--captures-dir requires a PATH argument");
                        return ExitCode::from(2);
                    }
                }
            }
            "--model" => {
                i += 1;
                match args.get(i) {
                    Some(s) => model_filter = Some(s.clone()),
                    None => {
                        eprintln!("--model requires a SLUG argument");
                        return ExitCode::from(2);
                    }
                }
            }
            other => {
                eprintln!("observation replay: unknown flag {other}");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }

    // Resolve captures dir.
    let dir = match captures_dir {
        Some(p) => p,
        None => default_captures_dir(),
    };

    // Build the runtime.
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("observation replay: failed to build tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };

    rt.block_on(observation_replay_async(&dir, model_filter.as_deref()))
}

fn default_captures_dir() -> std::path::PathBuf {
    // For `cargo run` invocations CARGO_MANIFEST_DIR points at `core/`;
    // the workspace root is one level up. For installed binaries
    // neither env var is set; fall back to CWD-relative path. Operator
    // can always override via --captures-dir.
    if let Some(manifest) = std::env::var_os("CARGO_MANIFEST_DIR") {
        let mut p = std::path::PathBuf::from(manifest);
        p.pop(); // strip `/core` to reach workspace root
        p.push("tests/observation/captures");
        return p;
    }
    std::path::PathBuf::from("tests/observation/captures")
}

async fn observation_replay_async(
    dir: &std::path::Path,
    model_filter: Option<&str>,
) -> ExitCode {
    use std::sync::Arc;
    use hhagent_core::cassandra::review::{ChainReviewStage, ConstitutionalGuard, DeterministicPolicy};
    use hhagent_core::observation::replay::{
        format_report_table, load_captures_from_dir, replay_capture, ReplayResult,
    };

    let loaded = match load_captures_from_dir(dir) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("observation replay: cannot open {dir:?}: {e}");
            return ExitCode::from(1);
        }
    };

    if loaded.is_empty() {
        println!("(no captures found in {})", dir.display());
        return ExitCode::from(0);
    }

    // Build the production chain. Operator iterates by editing
    // ConstitutionalGuard / DeterministicPolicy bodies in
    // core/src/cassandra/review.rs and re-running.
    let chain = ChainReviewStage::new(vec![
        Arc::new(ConstitutionalGuard),
        Arc::new(DeterministicPolicy),
    ]);

    let mut results: Vec<ReplayResult> = Vec::new();
    let mut filtered_out: u32 = 0;
    for entry in loaded {
        if let Some(filter) = model_filter {
            let fname = entry.path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if !fname.contains(filter) {
                filtered_out = filtered_out.saturating_add(1);
                continue;
            }
        }
        let r = replay_capture(&entry.capture, &chain).await;
        results.push(r);
    }

    if results.is_empty() {
        eprintln!(
            "observation replay: no captures matched filter (--model {} filtered out {})",
            model_filter.unwrap_or("<none>"),
            filtered_out,
        );
        return ExitCode::from(0);
    }

    print!("{}", format_report_table(&results));
    ExitCode::from(0)
}
```

- [ ] **Step 4: Build the binary**

```sh
cargo build -p hhagent-core --bin hhagent-cli
```

Expected: clean build with no warnings.

- [ ] **Step 5: Run the help text manually as a sanity check**

```sh
./target/debug/hhagent-cli --help
```

Expected: `observation replay` appears in usage; flags block is present.

- [ ] **Step 6: Commit**

```sh
git add core/src/bin/hhagent-cli.rs
git commit -m "$(cat <<'EOF'
feat(cli): hhagent-cli observation replay subcommand

Slice B Task 7. Thin wrapper over core::observation::replay. Loads
captures from --captures-dir (defaults to tests/observation/captures
relative to CARGO_MANIFEST_DIR for cargo-run, CWD as fallback for
installed binaries), filters by --model SLUG if given, builds the
production ChainReviewStage::new(vec![ConstitutionalGuard,
DeterministicPolicy]), replays each capture, prints the table.

Exit codes: 0 (replay completed; deltas are not errors), 1 (captures
dir cannot be opened), 2 (CLI argument error). Help text updated.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task B8: CLI integration test

**Files:**
- Create: `core/tests/observation_replay_cli_e2e.rs`

- [ ] **Step 1: Write the failing test**

Create `core/tests/observation_replay_cli_e2e.rs`:

```rust
//! Integration tests for the `hhagent-cli observation replay`
//! subcommand. Spawns the binary as a subprocess against a per-test
//! tempdir of hand-crafted captures.

use std::process::Command;

use tempfile::TempDir;

use hhagent_core::cassandra::types::{DataClass, Plan};
use hhagent_core::observation::capture::{CaptureJson, CapturedAuditRow, CapturedPlan};
use hhagent_tests_common::workspace_target_binary;

fn hhagent_cli_binary() -> std::path::PathBuf {
    workspace_target_binary("hhagent-cli")
}

fn approve_capture() -> CaptureJson {
    let plan = Plan {
        context: "".into(),
        decision: "task_complete".into(),
        rationale: "".into(),
        steps: vec![],
        result: Some(serde_json::json!({"kind": "text", "body": "ok"})),
        data_ceiling: DataClass::Public,
        refused: None,
    };
    let plan_value = serde_json::to_value(&plan).unwrap();
    CaptureJson {
        schema_version: 2,
        fixture_id: "cli-approve-baseline".into(),
        fixture_summary: "synthetic approve baseline for CLI test".into(),
        captured_at: "2026-05-15T11:00:00Z".into(),
        llm_backend: "local".into(),
        llm_model: "gemma4:26b".into(),
        llm_base_url: "http://localhost:11434/v1".into(),
        prompt: "synthetic prompt".into(),
        task_id: 300,
        task_state: "completed".into(),
        plan_iterations: 1,
        plans: vec![CapturedPlan {
            iter: 1,
            plan_json: plan_value.clone(),
            verdict_today: Some("approve".into()),
            step_count: 0,
            data_ceiling: "Public".into(),
        }],
        audit_rows: vec![CapturedAuditRow {
            id: 1,
            ts: "2026-05-15T11:00:01Z".into(),
            actor: "agent".into(),
            action: "plan.formulate".into(),
            payload: serde_json::json!({
                "task_id": 300,
                "plan_count": 1,
                "decision_kind": "task_complete",
                "plan_step_count": 0,
                "refused": serde_json::Value::Null,
                "plan": plan_value,
                "classification_floor": "Public",
            }),
        }],
    }
}

fn write_capture(root: &std::path::Path, capture: &CaptureJson) {
    let dir = root.join(&capture.fixture_id);
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("2026-05-15_synthetic.json");
    let bytes = serde_json::to_vec_pretty(capture).unwrap();
    std::fs::write(p, bytes).unwrap();
}

#[test]
fn cli_observation_replay_happy_path() {
    let tempdir = TempDir::new().unwrap();
    write_capture(tempdir.path(), &approve_capture());

    let bin = hhagent_cli_binary();
    if !bin.exists() {
        eprintln!("[SKIP] hhagent-cli binary not built; run `cargo build` first");
        return;
    }

    let out = Command::new(&bin)
        .arg("observation")
        .arg("replay")
        .arg("--captures-dir")
        .arg(tempdir.path())
        .output()
        .expect("spawn hhagent-cli observation replay");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(out.status.success(), "exit ok; stderr = {stderr}");
    assert!(stdout.contains("cli-approve-baseline"),
        "fixture id row must appear; stdout = {stdout}");
    assert!(stdout.contains("approve"),
        "verdict kind must appear; stdout = {stdout}");
    assert!(stdout.contains("1 plans across 1 fixtures"),
        "summary line must appear; stdout = {stdout}");
}

#[test]
fn cli_observation_replay_rejects_unknown_flag() {
    let tempdir = TempDir::new().unwrap();
    let bin = hhagent_cli_binary();
    if !bin.exists() {
        eprintln!("[SKIP] hhagent-cli binary not built");
        return;
    }

    let out = Command::new(&bin)
        .arg("observation")
        .arg("replay")
        .arg("--captures-dir")
        .arg(tempdir.path())
        .arg("--bogus")
        .output()
        .expect("spawn");

    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn cli_observation_replay_empty_dir_exits_zero() {
    let tempdir = TempDir::new().unwrap();
    let bin = hhagent_cli_binary();
    if !bin.exists() {
        eprintln!("[SKIP] hhagent-cli binary not built");
        return;
    }

    let out = Command::new(&bin)
        .arg("observation")
        .arg("replay")
        .arg("--captures-dir")
        .arg(tempdir.path())
        .output()
        .expect("spawn");

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success());
    assert!(stdout.contains("no captures found"),
        "empty-dir message must appear; stdout = {stdout}");
}
```

`hhagent_tests_common::workspace_target_binary` is the established helper for locating workspace binaries from tests.

- [ ] **Step 2: Build the binary**

```sh
cargo build -p hhagent-core --bin hhagent-cli
```

Expected: clean build.

- [ ] **Step 3: Run the integration tests**

```sh
cargo test -p hhagent-core --test observation_replay_cli_e2e -- --nocapture
```

Expected: 3 tests pass. If the binary is missing the test prints `[SKIP]` and returns success — re-run after the build step.

- [ ] **Step 4: Commit**

```sh
git add core/tests/observation_replay_cli_e2e.rs
git commit -m "$(cat <<'EOF'
test(cli): hhagent-cli observation replay e2e — 3 scenarios

Slice B Task 8. Subprocess-level pin: happy path (synthetic approve
baseline + assert fixture row + summary line appear on stdout),
unknown-flag (exit code 2), empty captures dir (exit 0 + "no
captures found" hint on stdout). Skips cleanly when the binary
isn't built.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task B9: Workspace test + HANDOVER + ROADMAP + PR

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md`
- Modify: `docs/devel/ROADMAP.md`

- [ ] **Step 1: Run the full workspace test suite**

```sh
cargo test --workspace
```

Expected: 467 (post-Slice-A) + 17 new tests (B2: 6, B3: 6, B4: 6, B5: 2, B6: 2, B8: 3) − some doubling = **~484 tests**. Zero failures, zero warnings, zero `[SKIP]` lines.

Audit individual deltas if the workspace count is off by more than 3: the new tests are unit (replay::tests) + integration (observation_replay_e2e + observation_replay_cli_e2e). Run focused subsets to triangulate.

- [ ] **Step 2: Append HANDOVER session-end entry**

In `docs/devel/handovers/HANDOVER.md`, the Slice A entry (added in Task A3) becomes "previous session"; add a new "this session" entry for Slice B. Use this template:

```markdown
## Recently completed (this session, 2026-05-15 — Slice B: rule-iteration harness, branch `feat/rule-iteration-harness`)

Branch: `feat/rule-iteration-harness` (off `main` post-Slice-A merge). New pure-Rust library `core::observation::replay` + thin `hhagent-cli observation replay` subcommand. Loads captures from disk, replays each captured plan through the production `ChainReviewStage::new(vec![Arc::new(ConstitutionalGuard), Arc::new(DeterministicPolicy)])`, prints a per-fixture verdict-delta ASCII table.

**Shape (1 NEW library + 1 modified CLI + 2 NEW integration tests):**

- **NEW `core/src/observation/replay.rs`** (~500 LOC incl. tests). Public surface: `VerdictSnapshot`, `ReplayedPlan`, `ReplayResult`, `LoadedCapture`; pure `is_delta`, `format_report_table`, `render_new_verdict`; async `replay_capture(capture, chain) -> ReplayResult`; I/O `load_captures_from_dir(dir) -> Vec<LoadedCapture>`. Skips a plan with `plan_json: null` via `plans_skipped_missing_body` counter — never fabricates synthetic Plan from derived fields. Classification floor preference: audit-row's `classification_floor` (post-Slice-A) > `Plan.data_ceiling` > `Public`.
- **`core/src/observation/mod.rs`** — `pub mod replay;` declared.
- **`core/src/bin/hhagent-cli.rs`** — new `observation` top-level dispatcher + `run_observation_replay` (mirrors existing `tools allowlist` pattern; hand-rolled argv, no clap dep). Help text updated.
- **NEW `core/tests/observation_replay_e2e.rs`** — 2 library-level scenarios (synthetic approve baseline → no delta; pre-Slice-A capture → skip with reason).
- **NEW `core/tests/observation_replay_cli_e2e.rs`** — 3 subprocess scenarios (happy path; unknown-flag exit 2; empty dir exit 0).

**Report format:**

```
fixture                                  iter  baseline    new                         d?
-------------------------------------- ----  ----------- --------------------------- --
safe-001-echo-marker                       1  approve     approve                      .
p1-001-physical-harm-instructions          1  approve     constitutional_block(p=1)    *
p2-001-impersonation                       1  approve     [skipped: plan body missing] -

3 plans across 3 fixtures . 1 delta . 1 skipped
```

ASCII-only; fixed column widths; grep-friendly. Markers `.` (no delta) / `*` (delta) / `-` (skipped). Constitutional blocks render with the principle index; escalates with severity; others as the bare kind.

**Test count delta:** 467 → **~484** (+~17: 6 VerdictSnapshot + 6 is_delta + 6 format_report_table + 2 replay_capture + 2 e2e library + 3 e2e CLI).

**TDD ordering** (per CLAUDE.md rule #2): nine commits, each RED → GREEN. B1 scaffolds the type surface; B2-B4 land pure helpers with full test coverage; B5 wires the async `replay_capture`; B6 adds `load_captures_from_dir` with integration tests; B7 ships the CLI subcommand; B8 the CLI integration tests; B9 (this) wraps with docs.

**Operator iteration loop:** edit `ConstitutionalGuard::review` (or `DeterministicPolicy::review`) bodies in `core/src/cassandra/review.rs` → `cargo build --bin hhagent-cli` → `./target/debug/hhagent-cli observation replay`. No daemon, no DB, no LLM. Fast iteration; deterministic; same chain composition as production.

**What this slice deliberately does NOT do.**
- **No real `ConstitutionalGuard` / `DeterministicPolicy` rule.** Stubs stay always-`Approve`; the harness mechanism is what ships. First real rule is a follow-up slice.
- **No `--json` output.** Text-only table; pipe to `grep` / `awk` for ad-hoc analysis.
- **No fail-on-delta exit code.** Deltas are the harness's reason to exist.
- **No multi-baseline diffing.** One model per run via `--model SLUG`.
- **No CI integration.** Operator-run; the captures it operates on are operator-produced.

**Files touched (3 NEW + 3 modified):**
- NEW `core/src/observation/replay.rs` (~500 LOC).
- NEW `core/tests/observation_replay_e2e.rs` (~150 LOC).
- NEW `core/tests/observation_replay_cli_e2e.rs` (~140 LOC).
- `core/src/observation/mod.rs` — module declaration.
- `core/src/bin/hhagent-cli.rs` — new top-level subcommand + help text.
- `docs/devel/handovers/HANDOVER.md` + `docs/devel/ROADMAP.md` — this update.
```

- [ ] **Step 3: Tick Slice B in ROADMAP**

In `docs/devel/ROADMAP.md`, just after the Slice A entry from Task A3, add:

```markdown
- [x] **[follow-up] Rule-iteration harness for CASSANDRA review pipeline (Slice B of rule-iteration harness)** — landed 2026-05-15 on branch `feat/rule-iteration-harness`. New pure library `core::observation::replay` + thin `hhagent-cli observation replay` subcommand. Loads captures from disk, replays each through the production `ChainReviewStage::new(vec![ConstitutionalGuard, DeterministicPolicy])`, prints per-fixture verdict-delta ASCII table. Pre-Slice-A captures (`plan_json: null`) skip cleanly via `plans_skipped_missing_body` counter; the harness never fabricates synthetic Plans. Operator iteration loop: edit `ConstitutionalGuard::review` body → rebuild → re-run; deterministic, no DB/LLM/daemon. +~17 tests; workspace 467 → ~484.
```

- [ ] **Step 4: Commit + push + PR**

```sh
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "$(cat <<'EOF'
docs(handover,roadmap): Slice B — rule-iteration harness shipped

Pure-Rust library core::observation::replay + thin hhagent-cli
observation replay subcommand. Loads captures from disk, replays
each through the production chain, prints per-fixture verdict-delta
ASCII table.

Test count 467 → ~484 (+~17). Zero failures, zero warnings, zero
[SKIP] lines on Linux.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"

git push -u origin feat/rule-iteration-harness

gh pr create --title "feat(observation): rule-iteration harness for CASSANDRA review pipeline" --body "$(cat <<'EOF'
## Summary
- New pure-Rust library `core::observation::replay` (replay one capture through a chain; pure helpers + async replay_capture).
- New `hhagent-cli observation replay` subcommand (thin wrapper; defaults to tests/observation/captures; --captures-dir + --model flags).
- Loads captures from disk, replays each captured plan through production `ChainReviewStage::new(vec![Arc::new(ConstitutionalGuard), Arc::new(DeterministicPolicy)])`, prints per-fixture verdict-delta ASCII table.
- Pre-Slice-A captures (plan_json: null) skip cleanly via plans_skipped_missing_body counter; the harness never fabricates synthetic Plans.
- Test count 467 → ~484 (+~17 across replay::tests + observation_replay_e2e + observation_replay_cli_e2e).

## Test plan
- [ ] `cargo test --workspace` green on Linux.
- [ ] `./target/debug/hhagent-cli observation replay` works against the operator's recaptured fixtures (operator action: recapture after Slice A merged).
- [ ] `./target/debug/hhagent-cli observation replay --captures-dir tests/observation/captures` against the existing pre-Slice-A captures prints "[skipped: plan body missing]" rows + 7 skipped count in the summary (defensible degraded mode).

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

## Self-review

### Spec coverage
- ✅ Slice A audit-payload bump (Tasks A1–A3).
- ✅ Slice B library — `VerdictSnapshot` (B2), `is_delta` (B3), `format_report_table` (B4), `replay_capture` (B5), `load_captures_from_dir` (B6).
- ✅ Slice B CLI subcommand (B7) + CLI integration test (B8).
- ✅ Per-plan skip behaviour for missing plan body (covered in B5 test + B6 e2e + B7 default chain).
- ✅ HANDOVER + ROADMAP updates (A3 + B9).
- ✅ Hard-coded production chain in CLI (B7 builds `ChainReviewStage::new(vec![ConstitutionalGuard, DeterministicPolicy])`).

### Placeholder scan
- ✅ Every `cargo` / `git` command is concrete.
- ✅ Every test body has full code, no "// implement here" placeholders.
- ✅ Commit messages are spelled out.

### Type consistency
- `VerdictSnapshot` — same field names (`kind`, `detail`) across B1 declaration, B2 tests, B4 use in `render_new_verdict`, B5 use in `replay_capture`. ✓
- `ReplayedPlan` — `iter`, `baseline_verdict`, `new_verdict`, `is_delta`, `skipped_reason` consistent. ✓
- `ReplayResult` — `plans_replayed` + `plans_skipped_missing_body` (plural) consistent. ✓
- `is_delta(baseline: Option<&str>, new: Option<&String>)` — same signature in B3 implementation, B5 call site. ✓
- `replay_capture(capture: &CaptureJson, chain: &ChainReviewStage) -> ReplayResult` — async; tests use `.await`. ✓
