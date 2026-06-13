# python-exec skill catalog — SLICE 2 (invocation + surfacing) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make an approved/pinned Python skill *runnable* — operator-triggered (`memory l3 run <id>`, daemon-side against the live registry), agent-autonomous (`Plan.invoke_skill` resolving a pinned Python skill), and *visible* to the planner (kind-aware `<skills>` surfacing, code never shown) — with a SHA-256 re-hash TOCTOU close so the approved bytes are the executed bytes.

**Architecture:** Mirror the templated L3 invocation arc one payload over, reusing — not generalizing — the shared machinery. A Python skill's single execution step is one `python.exec` call (`tool="python-exec"`, `method="python.exec"`, `parameters={"code": <verbatim>}`), expressed as a one-element `Vec<L3TemplateStep>` so the existing `run_steps`, `planned_step_from_l3`, `InvokeReport`, and audit-payload builders all flow unchanged through the daemon → serialize → CLI → render pipeline. New parallel module `l3py_invoke/` (facade + `pure`/`operator`/`agent` siblings, mirroring `l3_invoke/`); the daemon `run_l3_run_task` and the inner loop each grow an additive `kind=="python"` branch; `l3_surface::parse_surfaced_skill` grows an additive `kind`-aware branch.

**Tech Stack:** Rust (kastellan-core), sqlx/PgPool, serde_json, sha2 (already in tree). TDD via `cargo test`, `clippy --workspace --all-targets -D warnings`.

---

## Background facts (verified against the tree — do not re-derive)

- **Stored Python-skill row** (`l3py_crystallise.rs`): `layer=3`, `metadata` =
  `{source, task_id, trust, kind:"python", body_sha256, created_at, python:{name,description,code}}`.
  Templated rows instead carry `metadata.template` and **no** `kind` key (absent `kind` ⇒ templated).
- **`PythonSkillCandidate`** (`core/src/cassandra/types.rs:138`): `{ name, description, code }` (all `String`).
- **Pure helpers already built** (`core/src/memory/l3py_crystallise.rs`):
  `validate_python_skill(&PythonSkillCandidate) -> Result<PythonSkillCandidate, PyError>`,
  `compute_python_sha256(&PythonSkillCandidate) -> String` (lowercase hex of canonical JSON).
- **Approval gate already built** (`core/src/memory/l3py_approval.rs`):
  `evaluate_python_approval(&PythonSkillCandidate) -> ApprovalDecision` (reuses `ApprovalDecision`/`RejectReason` from `l3_approval`). No live-registry dependency (a Python skill dispatches no tools).
- **Trust** (`core/src/memory/l3_approval.rs`): `SkillTrust{Untrusted|UserApproved|Pinned}`,
  `SkillTrust::from_metadata_str(&str)`, `SkillTrust::as_str()`. Kind-agnostic; `set_skill_trust` already flips Python rows.
- **Reusable invoke machinery** (`core/src/memory/l3_invoke/`):
  - `InvokeRefusal { reasons: Vec<String> }` (pure.rs).
  - `InvokeReport { Refused{reasons} | DryRun{steps: Vec<L3TemplateStep>} | Executed{outcomes: Vec<StepOutcome>, steps_total} }` (operator.rs) — `Serialize + Deserialize`.
  - `run_steps(&dyn StepDispatcher, &[L3TemplateStep]) -> Vec<StepOutcome>` (operator.rs) — maps each via `planned_step_from_l3` (classification `Secret`, unused on operator path) and dispatches, stopping at first `Err`.
  - `is_runnable(SkillTrust) -> bool` (UserApproved|Pinned), `is_autonomously_invocable(SkillTrust) -> bool` (Pinned only) (pure.rs).
  - `planned_step_from_l3_with_class(&L3TemplateStep, DataClass) -> PlannedStep` (pure.rs) — agent path.
- **`L3TemplateStep`** (`core/src/cassandra/types.rs:106`): `{ tool: String, method: String, parameters: serde_json::Value }`.
- **Audit builders** (`core/src/scheduler/audit.rs`): `build_l3_invoked_payload`, `build_l3_invoke_outcome_payload`, `build_l3_invoke_rejected_payload`, `build_l3_invoke_rejected_agent_payload`; actions `ACTION_L3_INVOKED`/`ACTION_L3_INVOKE_OUTCOME`/`ACTION_L3_INVOKE_REJECTED`.
- **Daemon run path** (`core/src/scheduler/l3_run.rs`): `run_l3_run_task(pool, &dyn StepDispatcher, &Value) -> InvokeReport` loads the row by `memory_id`, reads `metadata.template`/`trust`/`body_sha256`, calls `invoke_l3`. The CLI submits a `{kind:"l3_run", memory_id, args, execute}` task; `drain_lane` (`runner.rs:205`) routes it and serializes the `InvokeReport` to `tasks.result`.
- **CLI render** (`core/src/bin/kastellan-cli/memory_l3/run.rs`): `render_invoke_report(id, skill_name, &InvokeReport) -> (String, i32)` — already renders a `DryRun` step as `[n] {tool}/{method} {parameters}` and an `Executed` `StepOutcome`. `resolve_skill_name` reads `metadata.template.name`.
- **Surfacing** (`core/src/memory/l3_surface.rs`): `parse_surfaced_skill(&Value) -> Option<SurfacedSkill>` reads only `metadata.template` ⇒ Python rows are silently dropped today; `load_l3_skills_for_prompt` SQL-filters `trust ∈ {user_approved, pinned}` then projects each row.
- **Worker step**: `tool="python-exec"`, `method="python.exec"`, `params={"code": <source>}` (`core/src/workers/python_exec.rs`, `workers/python-exec/src/handler.rs`).
- **Inner-loop invoke_skill expansion** (`core/src/scheduler/inner_loop.rs:339-406`): on `plan.invoke_skill`, `validate_invoke()` → `load_pinned_skill_by_name(pool,&name)` → `expand_for_agent(...)` → `plan.steps = steps`. Audits via `SCHEDULER_AUDIT_ACTOR`.

## File structure

- **Create** `core/src/memory/l3py_invoke.rs` — facade (`mod pure/operator/agent; pub use *; #[cfg(test)] mod tests;`).
- **Create** `core/src/memory/l3py_invoke/pure.rs` — `prepare_python_invocation` gate + `python_exec_step` builder + `PY_EXEC_TOOL`/`PY_EXEC_METHOD` consts.
- **Create** `core/src/memory/l3py_invoke/operator.rs` — `invoke_python_skill` async orchestration (reuses `InvokeReport`, `run_steps`, audit builders + `kind:"python"`).
- **Create** `core/src/memory/l3py_invoke/agent.rs` — `PinnedPythonSkill`, `load_pinned_python_skill_by_name`, `expand_python_for_agent`.
- **Create** `core/src/memory/l3py_invoke/tests.rs` — pure + operator + agent unit tests.
- **Modify** `core/src/memory/mod.rs` — add `pub mod l3py_invoke;` after `l3_invoke`.
- **Modify** `core/src/scheduler/l3_run.rs` — `kind=="python"` branch in `run_l3_run_task`.
- **Modify** `core/src/memory/l3_surface.rs` — `kind`-aware `parse_surfaced_skill`.
- **Modify** `core/src/bin/kastellan-cli/memory_l3/run.rs` — `resolve_skill_name` reads either payload's name.
- **Modify** `core/src/scheduler/inner_loop.rs` — Python fallback in the `invoke_skill` expansion.
- **Create** `core/tests/cli_memory_l3py_run_daemon_e2e.rs` — PG+sandbox e2e mirror.

---

## Task 1: `l3py_invoke/pure.rs` — the pure invocation gate

**Files:**
- Create: `core/src/memory/l3py_invoke/pure.rs`
- Create: `core/src/memory/l3py_invoke.rs` (facade)
- Modify: `core/src/memory/mod.rs`
- Test: inline `#[cfg(test)]` in `pure.rs` for now (lifted to `tests.rs` in Task 5 if the file approaches cap)

- [ ] **Step 1: Create the facade and wire the module**

`core/src/memory/l3py_invoke.rs`:
```rust
//! Python-skill invocation — the execution "DOOR" for agent-authored Python
//! skills, mirroring [`crate::memory::l3_invoke`] one payload over. A Python
//! skill runs as exactly one `python.exec` step (verbatim code, no params),
//! SHA-256-bound so the approved bytes are the executed bytes.
//!
//! - [`pure`] — the [`prepare_python_invocation`] decision gate (trust →
//!   re-validate → re-hash vs `stored_sha256`) and the one-step builder.
//!   No I/O; deterministic and unit-testable.
//! - [`operator`] — operator-CLI async orchestration ([`invoke_python_skill`]):
//!   dry-run by default, no CASSANDRA review (an operator running their own
//!   approved skill is authorised). Reuses the templated dispatcher + report.
//! - [`agent`] — the stricter pinned-only agent path
//!   ([`expand_python_for_agent`] + [`load_pinned_python_skill_by_name`]).
//!
//! See `docs/superpowers/specs/2026-06-13-python-exec-skill-catalog-design.md`.

mod agent;
mod operator;
mod pure;

pub use agent::*;
pub use operator::*;
pub use pure::*;

#[cfg(test)]
mod tests;
```

`core/src/memory/mod.rs` — add after the `l3_invoke` line (currently `pub mod l3_invoke;`):
```rust
pub mod l3py_invoke;
```

(Leave `l3py_invoke/operator.rs`, `agent.rs`, `tests.rs` as empty placeholder files so the facade compiles — they are filled in by later tasks. Create them now with a single `//! placeholder — filled in Task N` line each so `mod` resolves.)

- [ ] **Step 2: Write the failing tests in `pure.rs`**

`core/src/memory/l3py_invoke/pure.rs` (append the test module; the impl below is added in Step 4):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cassandra::types::PythonSkillCandidate;
    use crate::memory::l3_approval::SkillTrust;
    use crate::memory::l3py_crystallise::compute_python_sha256;

    fn cand() -> PythonSkillCandidate {
        PythonSkillCandidate {
            name: "say_hi".to_string(),
            description: "prints hi".to_string(),
            code: "print('hi')\n".to_string(),
        }
    }

    #[test]
    fn untrusted_is_refused() {
        let c = cand();
        let sha = compute_python_sha256(&c);
        let err = prepare_python_invocation(&c, SkillTrust::Untrusted, &sha).unwrap_err();
        assert!(err.reasons.iter().any(|r| r.contains("not runnable")), "{err:?}");
    }

    #[test]
    fn user_approved_runs_and_returns_verbatim_code() {
        let c = cand();
        let sha = compute_python_sha256(&c);
        let code = prepare_python_invocation(&c, SkillTrust::UserApproved, &sha).unwrap();
        assert_eq!(code, "print('hi')\n");
    }

    #[test]
    fn pinned_runs() {
        let c = cand();
        let sha = compute_python_sha256(&c);
        assert!(prepare_python_invocation(&c, SkillTrust::Pinned, &sha).is_ok());
    }

    #[test]
    fn sha_drift_is_refused() {
        let c = cand();
        let wrong = "0".repeat(64);
        let err = prepare_python_invocation(&c, SkillTrust::Pinned, &wrong).unwrap_err();
        assert!(err.reasons.iter().any(|r| r.contains("sha")), "{err:?}");
    }

    #[test]
    fn embedded_secret_ref_is_refused() {
        let mut c = cand();
        c.code = "x = 'secret://db/password'\n".to_string();
        let sha = compute_python_sha256(&c);
        let err = prepare_python_invocation(&c, SkillTrust::UserApproved, &sha).unwrap_err();
        assert!(err.reasons.iter().any(|r| r.contains("secret://")), "{err:?}");
    }

    #[test]
    fn builds_one_python_exec_step() {
        let step = python_exec_step("print(1)\n");
        assert_eq!(step.tool, "python-exec");
        assert_eq!(step.method, "python.exec");
        assert_eq!(step.parameters, serde_json::json!({"code": "print(1)\n"}));
    }
}
```

- [ ] **Step 3: Run the tests, verify they fail to compile**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core l3py_invoke::pure 2>&1 | tail -20`
Expected: compile error — `prepare_python_invocation` / `python_exec_step` not found.

- [ ] **Step 4: Write the implementation in `pure.rs` (above the test module)**

```rust
//! Pure invocation gate for Python skills + the one-step builder. No I/O.
//!
//! The gate mirrors `l3_invoke::pure::prepare_invocation` but: (1) there are
//! no args to substitute (Python code is verbatim); (2) there is no
//! tool-existence check (a Python skill dispatches no tools — the python-exec
//! jail is its entire capability ceiling); (3) it adds a SHA-256 re-hash
//! against the stored digest — the TOCTOU close that guarantees the bytes the
//! operator read and approved are the bytes that run.

use crate::cassandra::types::{L3TemplateStep, PythonSkillCandidate};
use crate::memory::l3_approval::ApprovalDecision;
use crate::memory::l3_invoke::InvokeRefusal;
use crate::memory::l3_approval::SkillTrust;
use crate::memory::l3py_approval::evaluate_python_approval;
use crate::memory::l3py_crystallise::compute_python_sha256;

/// The tool name the python-exec worker registers as (see
/// `core/src/workers/python_exec.rs`).
pub const PY_EXEC_TOOL: &str = "python-exec";
/// The JSON-RPC method the python-exec worker serves (see
/// `workers/python-exec/src/handler.rs`).
pub const PY_EXEC_METHOD: &str = "python.exec";

/// True iff this trust level may run via the operator CLI. Identical
/// membership to [`crate::memory::l3_invoke::is_runnable`] — reused for the
/// templated path; spelled here for the Python path so the gate is local.
fn is_runnable(trust: SkillTrust) -> bool {
    matches!(trust, SkillTrust::UserApproved | SkillTrust::Pinned)
}

/// Build the single `python.exec` step that runs `code` verbatim.
pub fn python_exec_step(code: &str) -> L3TemplateStep {
    L3TemplateStep {
        tool: PY_EXEC_TOOL.to_string(),
        method: PY_EXEC_METHOD.to_string(),
        parameters: serde_json::json!({ "code": code }),
    }
}

/// PURE decision: may this stored Python skill run, and if so, what code?
///
/// 1. trust must be runnable (`UserApproved | Pinned`);
/// 2. re-run [`evaluate_python_approval`] (structural re-validation + the
///    `secret://` re-scan over the code);
/// 3. re-compute [`compute_python_sha256`] and confirm it equals
///    `stored_sha256` — refuse on drift (the code TOCTOU close).
///
/// Returns the verbatim `code` on success, else an [`InvokeRefusal`]
/// collecting every reason.
pub fn prepare_python_invocation(
    candidate: &PythonSkillCandidate,
    stored_trust: SkillTrust,
    stored_sha256: &str,
) -> Result<String, InvokeRefusal> {
    let mut reasons: Vec<String> = Vec::new();

    if !is_runnable(stored_trust) {
        reasons.push(format!(
            "skill is not runnable (trust='{}'; requires user_approved or pinned)",
            stored_trust.as_str()
        ));
        return Err(InvokeRefusal { reasons });
    }

    if let ApprovalDecision::Reject { reasons: rs } = evaluate_python_approval(candidate) {
        reasons.extend(rs.iter().map(|r| r.to_string()));
        return Err(InvokeRefusal { reasons });
    }

    let recomputed = compute_python_sha256(candidate);
    if recomputed != stored_sha256 {
        reasons.push(format!(
            "body sha256 drift: stored={stored_sha256} recomputed={recomputed} \
             (the approved code is not the code on disk; refusing)"
        ));
        return Err(InvokeRefusal { reasons });
    }

    Ok(candidate.code.clone())
}
```

- [ ] **Step 5: Run the tests, verify pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core l3py_invoke::pure 2>&1 | tail -20`
Expected: all 6 tests PASS.

- [ ] **Step 6: clippy + commit**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-core --all-targets -- -D warnings 2>&1 | tail -5`
Expected: clean.
```bash
git add core/src/memory/l3py_invoke.rs core/src/memory/l3py_invoke/ core/src/memory/mod.rs
git commit -m "feat(python-exec): l3py_invoke pure gate — prepare_python_invocation + SHA-drift TOCTOU close"
```

---

## Task 2: `l3py_invoke/operator.rs` — operator orchestration

**Files:**
- Modify: `core/src/memory/l3py_invoke/operator.rs`
- Test: inline `#[cfg(test)]` in `operator.rs`

- [ ] **Step 1: Write the failing tests**

`core/src/memory/l3py_invoke/operator.rs` test module (uses a fake dispatcher; no PG — the audit writes are best-effort and tolerate a closed pool only via a real `PgPool`, so these tests cover the **pure-shape** outcomes by checking the returned `InvokeReport`, not the audit rows; the audit is exercised in the e2e). To avoid needing a `PgPool`, split the dispatch logic into a pool-free inner function `invoke_python_steps` that the test drives directly:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cassandra::types::PythonSkillCandidate;
    use crate::memory::l3_approval::SkillTrust;
    use crate::memory::l3_invoke::InvokeReport;
    use crate::memory::l3py_crystallise::compute_python_sha256;
    use crate::scheduler::inner_loop::{StepDispatcher, StepOutcome};
    use async_trait::async_trait;

    struct OkDispatcher;
    #[async_trait]
    impl StepDispatcher for OkDispatcher {
        async fn dispatch_step(
            &self,
            _task_id: i64,
            step: &crate::cassandra::types::PlannedStep,
        ) -> StepOutcome {
            StepOutcome::Ok(serde_json::json!({"echo": step.parameters.clone()}))
        }
    }

    fn cand() -> PythonSkillCandidate {
        PythonSkillCandidate {
            name: "say_hi".to_string(),
            description: "prints hi".to_string(),
            code: "print('hi')\n".to_string(),
        }
    }

    #[test]
    fn untrusted_refuses_without_dispatch() {
        let c = cand();
        let sha = compute_python_sha256(&c);
        let report = futures::executor::block_on(invoke_python_steps(
            &OkDispatcher, &c, SkillTrust::Untrusted, &sha, false,
        ));
        assert!(matches!(report, InvokeReport::Refused { .. }));
    }

    #[test]
    fn approved_dry_run_returns_one_python_exec_step() {
        let c = cand();
        let sha = compute_python_sha256(&c);
        let report = futures::executor::block_on(invoke_python_steps(
            &OkDispatcher, &c, SkillTrust::UserApproved, &sha, false,
        ));
        match report {
            InvokeReport::DryRun { steps } => {
                assert_eq!(steps.len(), 1);
                assert_eq!(steps[0].tool, "python-exec");
                assert_eq!(steps[0].method, "python.exec");
            }
            other => panic!("expected DryRun, got {other:?}"),
        }
    }

    #[test]
    fn approved_execute_dispatches_and_reports_executed() {
        let c = cand();
        let sha = compute_python_sha256(&c);
        let report = futures::executor::block_on(invoke_python_steps(
            &OkDispatcher, &c, SkillTrust::Pinned, &sha, true,
        ));
        match report {
            InvokeReport::Executed { outcomes, steps_total } => {
                assert_eq!(steps_total, 1);
                assert_eq!(outcomes.len(), 1);
                assert!(outcomes[0].is_ok());
            }
            other => panic!("expected Executed, got {other:?}"),
        }
    }
}
```

> NOTE: confirm `futures` is a dev-dep of `kastellan-core` (the `l3_invoke/tests.rs` uses a runtime). If `l3_invoke/tests.rs` uses `tokio::runtime` or `#[tokio::test]`, mirror that instead of `futures::executor::block_on`. Check before writing: `grep -n "block_on\|tokio::test\|tokio::runtime" core/src/memory/l3_invoke/tests.rs`. Use whichever the existing tests use.

- [ ] **Step 2: Run, verify fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core l3py_invoke::operator 2>&1 | tail -20`
Expected: compile error — `invoke_python_steps` not found.

- [ ] **Step 3: Write the implementation**

`core/src/memory/l3py_invoke/operator.rs` (above the tests):
```rust
//! Operator-path orchestration of an approved Python skill. Reuses the
//! templated [`crate::memory::l3_invoke::InvokeReport`] + `run_steps` so the
//! daemon → serialize → CLI → render pipeline is byte-for-byte the same as
//! the templated path; only the gate ([`super::pure::prepare_python_invocation`])
//! and the one-step build differ. Audit rows reuse the L3 invoke actions with
//! `kind:"python"` injected, keeping one coherent skill-lifecycle stream.
//!
//! Like the templated operator path: dry-run by default, NO CASSANDRA review
//! (an operator running their own approved skill is an authorised action; the
//! reviewer polices agent-formulated plans — see [`super::agent`]).

use serde_json::Value;
use sqlx::PgPool;

use crate::cassandra::types::PythonSkillCandidate;
use crate::cli_audit::CLI_AUDIT_ACTOR;
use crate::memory::l3_approval::SkillTrust;
use crate::memory::l3_invoke::{run_steps, InvokeRefusal, InvokeReport};
use crate::scheduler::audit::{
    build_l3_invoke_outcome_payload, build_l3_invoke_rejected_payload, build_l3_invoked_payload,
    ACTION_L3_INVOKED, ACTION_L3_INVOKE_OUTCOME, ACTION_L3_INVOKE_REJECTED,
};
use crate::scheduler::inner_loop::StepDispatcher;

use super::pure::{prepare_python_invocation, python_exec_step};

/// Inject `kind:"python"` into an L3 invoke audit payload so the lifecycle
/// stream distinguishes Python from templated skills without a new action.
fn with_python_kind(mut payload: Value) -> Value {
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("kind".to_string(), Value::String("python".to_string()));
    }
    payload
}

/// Pool-free core: gate → (dry-run | dispatch). Returns the same
/// [`InvokeReport`] the templated path uses. Unit-tested directly; the
/// audited wrapper [`invoke_python_skill`] adds the best-effort rows.
pub async fn invoke_python_steps(
    dispatcher: &dyn StepDispatcher,
    candidate: &PythonSkillCandidate,
    stored_trust: SkillTrust,
    stored_sha256: &str,
    execute: bool,
) -> InvokeReport {
    let code = match prepare_python_invocation(candidate, stored_trust, stored_sha256) {
        Ok(code) => code,
        Err(InvokeRefusal { reasons }) => return InvokeReport::Refused { reasons },
    };
    let steps = vec![python_exec_step(&code)];
    if !execute {
        return InvokeReport::DryRun { steps };
    }
    let steps_total = steps.len();
    let outcomes = run_steps(dispatcher, &steps).await;
    InvokeReport::Executed { outcomes, steps_total }
}

/// Orchestrate operator-triggered invocation of an approved Python skill,
/// writing the best-effort audit envelope rows (refusal always; invoked +
/// outcome only on `execute`). `memory_id` / `stored_trust` / `stored_sha256`
/// come from the stored row; `candidate` is its `metadata.python` payload.
pub async fn invoke_python_skill(
    pool: &PgPool,
    memory_id: i64,
    dispatcher: &dyn StepDispatcher,
    candidate: &PythonSkillCandidate,
    stored_trust: SkillTrust,
    stored_sha256: &str,
    execute: bool,
) -> InvokeReport {
    // Refusal is audited regardless of execute (a refused run is a security
    // event); mirror invoke_l3 by gating once here for the audit, then
    // delegating the dispatch to the pool-free core.
    if let Err(InvokeRefusal { reasons }) =
        prepare_python_invocation(candidate, stored_trust, stored_sha256)
    {
        let payload = with_python_kind(build_l3_invoke_rejected_payload(
            memory_id, &candidate.name, stored_sha256, &reasons,
        ));
        best_effort_audit(pool, ACTION_L3_INVOKE_REJECTED, payload).await;
        return InvokeReport::Refused { reasons };
    }

    if !execute {
        // Re-derive the dry-run steps via the core (no dispatch).
        return invoke_python_steps(dispatcher, candidate, stored_trust, stored_sha256, false).await;
    }

    let invoked = with_python_kind(build_l3_invoked_payload(
        memory_id, &candidate.name, stored_sha256, &[], 1,
    ));
    best_effort_audit(pool, ACTION_L3_INVOKED, invoked).await;

    let report =
        invoke_python_steps(dispatcher, candidate, stored_trust, stored_sha256, true).await;
    if let InvokeReport::Executed { outcomes, steps_total } = &report {
        let any_err = outcomes.iter().any(|o| o.is_err());
        let outcome_payload = with_python_kind(build_l3_invoke_outcome_payload(
            memory_id, &candidate.name, outcomes.len(), *steps_total, any_err,
        ));
        best_effort_audit(pool, ACTION_L3_INVOKE_OUTCOME, outcome_payload).await;
    }
    report
}

async fn best_effort_audit(pool: &PgPool, action: &str, payload: Value) {
    if let Err(e) = kastellan_db::audit::insert(pool, CLI_AUDIT_ACTOR, action, payload).await {
        tracing::warn!(error = %e, action, "l3py invoke audit insert failed (best-effort)");
    }
}
```

- [ ] **Step 4: Run, verify pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core l3py_invoke::operator 2>&1 | tail -20`
Expected: 3 tests PASS. (If `futures` isn't a dev-dep, switch the test bodies to the runtime form used by `l3_invoke/tests.rs`.)

- [ ] **Step 5: clippy + commit**

Run: `source "$HOME/.cargo/env" && cargo clippy -p kastellan-core --all-targets -- -D warnings 2>&1 | tail -5`
```bash
git add core/src/memory/l3py_invoke/operator.rs
git commit -m "feat(python-exec): l3py_invoke operator path — invoke_python_skill (reuses InvokeReport + run_steps + L3 audit)"
```

---

## Task 3: Daemon `l3_run` Python branch (fail-closed)

**Files:**
- Modify: `core/src/scheduler/l3_run.rs`
- Test: inline `#[cfg(test)]` in `l3_run.rs`

- [ ] **Step 1: Write the failing tests** (append to `l3_run.rs` tests module)

```rust
    #[test]
    fn detects_python_kind_row() {
        let meta = serde_json::json!({
            "kind": "python",
            "trust": "user_approved",
            "body_sha256": "abc",
            "python": {"name": "say_hi", "description": "d", "code": "print(1)\n"}
        });
        assert!(is_python_skill_metadata(&meta));
        let meta2 = serde_json::json!({"template": {"name": "x"}});
        assert!(!is_python_skill_metadata(&meta2));
    }

    #[test]
    fn parses_python_candidate_from_metadata() {
        let meta = serde_json::json!({
            "kind": "python",
            "python": {"name": "say_hi", "description": "d", "code": "print(1)\n"}
        });
        let c = parse_python_candidate(&meta).expect("parse");
        assert_eq!(c.name, "say_hi");
        assert_eq!(c.code, "print(1)\n");
    }

    #[test]
    fn missing_python_payload_is_none() {
        let meta = serde_json::json!({"kind": "python"});
        assert!(parse_python_candidate(&meta).is_none());
    }
```

- [ ] **Step 2: Run, verify fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core scheduler::l3_run 2>&1 | tail -20`
Expected: compile error — `is_python_skill_metadata` / `parse_python_candidate` not found.

- [ ] **Step 3: Implement the helpers + branch**

Add imports at the top of `l3_run.rs`:
```rust
use crate::cassandra::types::PythonSkillCandidate;
use crate::memory::l3py_invoke::invoke_python_skill;
```

Add helpers (above `run_l3_run_task`):
```rust
/// True iff a stored layer-3 row's `metadata` describes a Python skill
/// (`kind == "python"`). Absent `kind` ⇒ templated (back-compat).
pub fn is_python_skill_metadata(metadata: &Value) -> bool {
    metadata.get("kind").and_then(|v| v.as_str()) == Some("python")
}

/// Parse a Python skill's `{name, description, code}` out of `metadata.python`.
/// Returns `None` (fail-safe) if the payload is missing or malformed.
pub fn parse_python_candidate(metadata: &Value) -> Option<PythonSkillCandidate> {
    let p = metadata.get("python")?;
    serde_json::from_value(p.clone()).ok()
}
```

In `run_l3_run_task`, **after** the `row` is confirmed layer-3 (after the `let row = match row { ... }` block, before the templated `let template: L3SkillCandidate = ...`), insert the Python branch:
```rust
    // Python-skill branch: dispatch one `python.exec` step. A Python skill
    // dispatches no tools, so there is no live-tool re-validation; the gate
    // is trust + structural re-validate + SHA re-hash (see l3py_invoke). If
    // python-exec is NOT registered in the daemon, the single dispatch fails
    // closed with a clear tool-not-found error surfaced in the outcome — never
    // a silent no-op.
    if is_python_skill_metadata(&row.metadata) {
        let candidate = match parse_python_candidate(&row.metadata) {
            Some(c) => c,
            None => {
                return InvokeReport::Refused {
                    reasons: vec![format!(
                        "python skill id={} has no parseable metadata.python",
                        req.memory_id
                    )],
                }
            }
        };
        let trust = SkillTrust::from_metadata_str(
            row.metadata.get("trust").and_then(|v| v.as_str()).unwrap_or(""),
        );
        let body_sha256 = row.metadata.get("body_sha256").and_then(|v| v.as_str()).unwrap_or("");
        return invoke_python_skill(
            pool, req.memory_id, dispatcher, &candidate, trust, body_sha256, req.execute,
        )
        .await;
    }
```

> NOTE on fail-closed: when python-exec is unregistered, `dispatcher.dispatch_step` returns a `StepOutcome::Err` (tool-not-found) → `InvokeReport::Executed { outcomes: [Err], steps_total: 1 }`, which `render_invoke_report` prints as `[0] ERR …` with exit 1. That satisfies "fail closed with a clear error, never silently no-op." Add an explicit e2e/assertion in Task 7; the dispatcher's not-found error string is the surfaced reason.

- [ ] **Step 4: Run, verify pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core scheduler::l3_run 2>&1 | tail -20`
Expected: all l3_run tests PASS (existing + 3 new).

- [ ] **Step 5: clippy + commit**

```bash
git add core/src/scheduler/l3_run.rs
git commit -m "feat(python-exec): daemon l3_run python branch — dispatch python.exec, fail-closed when unregistered"
```

---

## Task 4: CLI `run.rs` — kind-aware skill-name resolution

**Files:**
- Modify: `core/src/bin/kastellan-cli/memory_l3/run.rs`
- Test: inline `#[cfg(test)]` (the existing render tests already cover the unified `InvokeReport`; add no new render path)

The CLI already submits `{kind:"l3_run", memory_id, args, execute}` and renders `InvokeReport` uniformly — a Python skill needs **no** new submit/render code (its single `python.exec` step renders through the existing `DryRun`/`Executed` arms). The only gap: `resolve_skill_name` reads `metadata.template.name`, so a Python skill's header prints `<skill>`. Fix it to read either payload.

- [ ] **Step 1: Update `resolve_skill_name`**

Replace the body of `resolve_skill_name` (`core/src/bin/kastellan-cli/memory_l3/run.rs`):
```rust
async fn resolve_skill_name(pool: &sqlx::PgPool, id: i64) -> String {
    use kastellan_db::memories::fetch_by_ids;
    fetch_by_ids(pool, &[id])
        .await
        .ok()
        .and_then(|mut rows| rows.pop())
        .and_then(|row| {
            // Python skill: metadata.python.name; templated: metadata.template.name.
            let m = &row.metadata;
            m.get("python")
                .and_then(|p| p.get("name"))
                .or_else(|| m.get("template").and_then(|t| t.get("name")))
                .and_then(|n| n.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "<skill>".to_string())
}
```

- [ ] **Step 2: Verify the full crate still builds + tests pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core 2>&1 | tail -15`
Expected: workspace-green (skip-as-pass on Mac); no regressions in `memory_l3::run::tests`.

- [ ] **Step 3: clippy + commit**

```bash
git add core/src/bin/kastellan-cli/memory_l3/run.rs
git commit -m "feat(python-exec): CLI memory l3 run resolves python skill name for the header"
```

---

## Task 5: Surfacing — kind-aware `parse_surfaced_skill`

**Files:**
- Modify: `core/src/memory/l3_surface.rs`
- Test: inline `#[cfg(test)]` in `l3_surface.rs` (or its tests sibling if one exists — check `grep -n "mod tests" core/src/memory/l3_surface.rs`)

- [ ] **Step 1: Write the failing tests**

Add to the `l3_surface.rs` test module:
```rust
    #[test]
    fn surfaces_python_skill_name_description_no_params() {
        let meta = serde_json::json!({
            "kind": "python",
            "trust": "user_approved",
            "python": {"name": "say_hi", "description": "prints hi", "code": "print('hi')\n"}
        });
        let s = parse_surfaced_skill(&meta).expect("python skill surfaces");
        assert_eq!(s.name, "say_hi");
        assert_eq!(s.description, "prints hi");
        assert!(s.params.is_empty(), "python skills have no params");
        assert!(!s.invocable, "user_approved is not autonomously invocable");
    }

    #[test]
    fn pinned_python_skill_is_invocable() {
        let meta = serde_json::json!({
            "kind": "python", "trust": "pinned",
            "python": {"name": "p", "description": "d", "code": "pass\n"}
        });
        let s = parse_surfaced_skill(&meta).expect("surfaces");
        assert!(s.invocable);
    }

    #[test]
    fn python_skill_never_exposes_code() {
        // The SurfacedSkill type has no code field; assert the rendered entry
        // contains neither the source nor a `code` token.
        let meta = serde_json::json!({
            "kind": "python", "trust": "pinned",
            "python": {"name": "p", "description": "d", "code": "SECRET_SOURCE_MARKER\n"}
        });
        let s = parse_surfaced_skill(&meta).expect("surfaces");
        let rendered = render_skill_entry(&s);
        assert!(!rendered.contains("SECRET_SOURCE_MARKER"));
        assert!(!rendered.contains("code"));
    }

    #[test]
    fn malformed_python_payload_is_skipped() {
        let meta = serde_json::json!({"kind": "python", "python": {"name": "x"}});
        // missing description/code → from_value fails → None (fail-safe)
        assert!(parse_surfaced_skill(&meta).is_none());
    }
```

- [ ] **Step 2: Run, verify fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core l3_surface 2>&1 | tail -20`
Expected: the python tests FAIL (`parse_surfaced_skill` returns `None` for a `kind:"python"` row today).

- [ ] **Step 3: Add the kind-aware branch to `parse_surfaced_skill`**

Replace the function body (keep the templated path; prepend the python branch). Add the import for `PythonSkillCandidate` at the top of `l3_surface.rs` (`use crate::cassandra::types::PythonSkillCandidate;`) if not present:
```rust
pub fn parse_surfaced_skill(metadata: &serde_json::Value) -> Option<SurfacedSkill> {
    let trust = metadata.get("trust").and_then(|v| v.as_str()).unwrap_or("");
    let invocable = is_autonomously_invocable(SkillTrust::from_metadata_str(trust));

    // Python skill: project name + description, NO params, code never surfaced.
    if metadata.get("kind").and_then(|k| k.as_str()) == Some("python") {
        let py = metadata.get("python")?;
        let cand: PythonSkillCandidate = serde_json::from_value(py.clone()).ok()?;
        return Some(SurfacedSkill {
            name: cand.name,
            description: cand.description,
            params: Vec::new(),
            invocable,
        });
    }

    // Templated skill (back-compat: absent `kind`).
    let template = metadata.get("template")?;
    let cand: L3SkillCandidate = serde_json::from_value(template.clone()).ok()?;
    Some(SurfacedSkill {
        name: cand.name,
        description: cand.description,
        params: cand.parameters,
        invocable,
    })
}
```

- [ ] **Step 4: Run, verify pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core l3_surface 2>&1 | tail -20`
Expected: all PASS (templated + 4 new python).

- [ ] **Step 5: clippy + commit**

```bash
git add core/src/memory/l3_surface.rs
git commit -m "feat(python-exec): kind-aware l3 surfacing — python skills surface name+description, code never shown"
```

---

## Task 6: Agent-autonomous `invoke_skill` Python resolution

**Files:**
- Modify: `core/src/memory/l3py_invoke/agent.rs`
- Modify: `core/src/scheduler/inner_loop.rs`
- Test: inline `#[cfg(test)]` in `agent.rs` (pure expand); inner-loop wiring is covered by the e2e

- [ ] **Step 1: Write the failing tests for `expand_python_for_agent`** (`agent.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cassandra::types::PythonSkillCandidate;
    use crate::memory::l3_approval::SkillTrust;
    use crate::memory::l3py_crystallise::compute_python_sha256;
    use crate::scheduler::policy::DataClass;

    fn cand() -> PythonSkillCandidate {
        PythonSkillCandidate {
            name: "say_hi".to_string(),
            description: "d".to_string(),
            code: "print('hi')\n".to_string(),
        }
    }

    #[test]
    fn user_approved_is_not_autonomously_invocable() {
        let c = cand();
        let sha = compute_python_sha256(&c);
        let err = expand_python_for_agent(&c, SkillTrust::UserApproved, &sha, DataClass::Public)
            .unwrap_err();
        assert!(err.reasons.iter().any(|r| r.contains("pinned")), "{err:?}");
    }

    #[test]
    fn pinned_expands_to_one_python_exec_planned_step() {
        let c = cand();
        let sha = compute_python_sha256(&c);
        let steps = expand_python_for_agent(&c, SkillTrust::Pinned, &sha, DataClass::Secret)
            .expect("pinned expands");
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].tool, "python-exec");
        assert_eq!(steps[0].method, "python.exec");
        assert_eq!(steps[0].classification, DataClass::Secret);
    }

    #[test]
    fn pinned_with_sha_drift_refuses() {
        let c = cand();
        let err = expand_python_for_agent(&c, SkillTrust::Pinned, &"0".repeat(64), DataClass::Secret)
            .unwrap_err();
        assert!(err.reasons.iter().any(|r| r.contains("sha")), "{err:?}");
    }
}
```

> Confirm `DataClass`'s path: `grep -rn "pub enum DataClass" core/src`. Use the correct `use` (likely `crate::scheduler::policy::DataClass` — verify).

- [ ] **Step 2: Run, verify fail**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core l3py_invoke::agent 2>&1 | tail -20`
Expected: compile error — `expand_python_for_agent` / `load_pinned_python_skill_by_name` / `PinnedPythonSkill` not found.

- [ ] **Step 3: Implement `agent.rs`**

```rust
//! Agent-autonomous path for Python skills: the inner loop resolves an
//! agent-emitted `invoke_skill` directive whose name matches a **pinned**
//! Python skill, expanding it to a single `python.exec` step that then passes
//! through the unchanged CASSANDRA review → dispatch → audit pipeline. Only
//! `Pinned` is autonomously invocable (a strict subset of the operator gate).

use sqlx::PgPool;

use crate::cassandra::types::{PlannedStep, PythonSkillCandidate};
use crate::memory::l3_approval::SkillTrust;
use crate::memory::l3_invoke::InvokeRefusal;
use crate::memory::l3py_crystallise::compute_python_sha256;
use crate::scheduler::policy::DataClass;

use super::pure::{prepare_python_invocation, python_exec_step, PY_EXEC_METHOD, PY_EXEC_TOOL};

/// A pinned Python skill loaded for agent-autonomous invocation.
pub struct PinnedPythonSkill {
    pub memory_id: i64,
    pub candidate: PythonSkillCandidate,
    pub body_sha256: String,
}

/// PURE agent expansion: strict pinned-only gate → [`prepare_python_invocation`]
/// → one [`PlannedStep`] classified at the plan's `data_ceiling` (so the
/// deterministic policy's I2/I3 invariants hold automatically, exactly as the
/// templated agent path). Refuses non-pinned trust or SHA drift.
pub fn expand_python_for_agent(
    candidate: &PythonSkillCandidate,
    stored_trust: SkillTrust,
    stored_sha256: &str,
    data_ceiling: DataClass,
) -> Result<Vec<PlannedStep>, InvokeRefusal> {
    if !matches!(stored_trust, SkillTrust::Pinned) {
        return Err(InvokeRefusal {
            reasons: vec![format!(
                "skill is not autonomously invocable (trust='{}'; requires pinned)",
                stored_trust.as_str()
            )],
        });
    }
    // prepare_python_invocation enforces runnable-trust + re-validate + SHA;
    // pinned satisfies runnable, so this adds the structural + drift checks.
    let code = prepare_python_invocation(candidate, stored_trust, stored_sha256)?;
    let template = python_exec_step(&code);
    Ok(vec![PlannedStep {
        tool: PY_EXEC_TOOL.to_string(),
        method: PY_EXEC_METHOD.to_string(),
        parameters: template.parameters,
        returns: String::new(),
        done_when: String::new(),
        classification: data_ceiling,
    }])
}

/// Load the newest `pinned` Python skill whose `metadata.python.name` matches
/// `name`. Scans the top pinned layer-3 rows (mirrors
/// `l3_invoke::load_pinned_skill_by_name`); returns `Ok(None)` when no pinned
/// Python skill matches. Defensive: re-checks `kind=="python"` + pinned trust.
pub async fn load_pinned_python_skill_by_name(
    pool: &PgPool,
    name: &str,
) -> Result<Option<PinnedPythonSkill>, kastellan_db::DbError> {
    use kastellan_db::memories::load_layer_by_trust;
    // Reuse the same trust-filtered loader the surfacing path uses; pinned only.
    let rows = load_layer_by_trust(pool, 3, &["pinned"], 64).await?;
    for row in rows {
        let meta = &row.metadata;
        if meta.get("kind").and_then(|k| k.as_str()) != Some("python") {
            continue;
        }
        let cand: PythonSkillCandidate = match meta
            .get("python")
            .cloned()
            .and_then(|p| serde_json::from_value(p).ok())
        {
            Some(c) => c,
            None => continue,
        };
        if cand.name != name {
            continue;
        }
        let body_sha256 = meta
            .get("body_sha256")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        return Ok(Some(PinnedPythonSkill { memory_id: row.id, candidate: cand, body_sha256 }));
    }
    Ok(None)
}
```

> NOTE: verify the exact signature of `load_layer_by_trust` — `grep -n "pub.*fn load_layer_by_trust" db/src/memories*.rs db/src/memories/*.rs`. The templated `load_pinned_skill_by_name` already calls it; mirror its argument order and row type (`row.id`, `row.metadata`). Adjust the call above to match (the trust-markers arg may be `&[&str]` or a slice of owned strings).

- [ ] **Step 4: Run, verify pass**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core l3py_invoke::agent 2>&1 | tail -20`
Expected: 3 pure tests PASS.

- [ ] **Step 5: Wire the inner-loop fallback**

In `core/src/scheduler/inner_loop.rs`, import at the top (next to the existing `l3_invoke` import):
```rust
use crate::memory::l3py_invoke::{expand_python_for_agent, load_pinned_python_skill_by_name};
```

In the `invoke_skill` expansion (the `Ok((name, args)) => match load_pinned_skill_by_name(...)` block, ~line 367), change the `None` arm so it tries a pinned Python skill before refusing:
```rust
                Ok((name, args)) => {
                    let templated = load_pinned_skill_by_name(pool, &name).await?;
                    match templated {
                        Some(pinned) => {
                            let live_tools = dispatcher.known_tools();
                            match expand_for_agent(
                                &pinned.template,
                                SkillTrust::Pinned,
                                &args,
                                &live_tools,
                                plan.data_ceiling,
                            ) {
                                Err(refusal) => refuse_invoke!(
                                    &name, Some(pinned.memory_id),
                                    Some(pinned.body_sha256.as_str()), refusal.reasons
                                ),
                                Ok(steps) => {
                                    let arg_names: Vec<String> = args.keys().cloned().collect();
                                    let payload = build_l3_invoked_payload(
                                        pinned.memory_id, &name, &pinned.body_sha256,
                                        &arg_names, steps.len(),
                                    );
                                    kastellan_db::audit::insert(
                                        pool, SCHEDULER_AUDIT_ACTOR, ACTION_L3_INVOKED, payload,
                                    ).await?;
                                    plan.steps = steps;
                                    invoke_used = true;
                                    current_invoke = Some((pinned.memory_id, name));
                                }
                            }
                        }
                        None => match load_pinned_python_skill_by_name(pool, &name).await? {
                            None => refuse_invoke!(&name, None, None,
                                vec![format!("unknown or non-pinned skill: {name}")]),
                            Some(py) => match expand_python_for_agent(
                                &py.candidate, SkillTrust::Pinned, &py.body_sha256, plan.data_ceiling,
                            ) {
                                Err(refusal) => refuse_invoke!(
                                    &name, Some(py.memory_id),
                                    Some(py.body_sha256.as_str()), refusal.reasons
                                ),
                                Ok(steps) => {
                                    let payload = build_l3_invoked_payload(
                                        py.memory_id, &name, &py.body_sha256, &[], steps.len(),
                                    );
                                    kastellan_db::audit::insert(
                                        pool, SCHEDULER_AUDIT_ACTOR, ACTION_L3_INVOKED, payload,
                                    ).await?;
                                    plan.steps = steps;
                                    invoke_used = true;
                                    current_invoke = Some((py.memory_id, name));
                                }
                            },
                        },
                    }
                }
```

> Preserve the surrounding `match validated { Err(...) => ..., Ok(...) => ... }` shape — only the `Ok` arm body changes. Keep `refuse_invoke!`, `invoke_used`, `current_invoke` exactly as they are used elsewhere. Re-read lines 358-405 before editing and splice carefully.

- [ ] **Step 6: Build + run the inner_loop unit tests**

Run: `source "$HOME/.cargo/env" && cargo test -p kastellan-core scheduler::inner_loop 2>&1 | tail -20`
Expected: existing inner_loop tests still PASS (no behaviour change for `invoke_skill: None` and templated skills).

- [ ] **Step 7: clippy + commit**

```bash
git add core/src/memory/l3py_invoke/agent.rs core/src/scheduler/inner_loop.rs
git commit -m "feat(python-exec): agent-autonomous invoke_skill resolves a pinned python skill (CASSANDRA-reviewed)"
```

---

## Task 7: e2e `cli_memory_l3py_run_daemon_e2e` + audit kind assertions

**Files:**
- Create: `core/tests/cli_memory_l3py_run_daemon_e2e.rs`
- Reference: `core/tests/cli_memory_l3_run_daemon_e2e.rs` (mirror its harness verbatim where possible)

- [ ] **Step 1: Read the templated e2e end-to-end**

Run: `sed -n '1,260p' core/tests/cli_memory_l3_run_daemon_e2e.rs`
Note: how it brings up PG (`bring_up_pg_cluster` / `KASTELLAN_PG_BIN_DIR` skip-as-pass), starts the daemon (supervisor + `core_service_spec`), seeds the skill row, runs the CLI subprocess, and asserts. Reproduce that scaffold.

- [ ] **Step 2: Write the e2e** (skip-as-pass without PG + sandbox + worker bins, mirroring the templated one)

The test must:
1. Bring up PG; if unavailable, `eprintln!("[SKIP] …"); return;` (mirror the templated guard exactly).
2. Crystallise a Python skill row directly via `kastellan_core::memory::l3py_crystallise::crystallise_python_skill` with `code = "print('hi from skill')\n"`, capturing `memory_id`.
3. Flip trust to `user_approved` via `kastellan_db::memories::set_skill_trust` (kind-agnostic).
4. Start a daemon whose `core_service_spec` env sets `KASTELLAN_PYTHON_EXEC_ENABLE=1` **and** `KASTELLAN_PYTHON_EXEC_BIN` to the built worker (mirror how the templated e2e points `KASTELLAN_SHELL_EXEC_BIN`). Use `tests-common` worker-binary discovery.
5. Run the CLI subprocess `kastellan-cli memory l3 run <id> --execute` (env: connect spec only; deliberately NO `KASTELLAN_PYTHON_EXEC_BIN` in the CLI env — the daemon owns the registry, the #179 pin) and assert exit 0 + stdout contains `hi from skill`.
6. Assert an `l3.invoke_outcome` audit row exists with `payload.kind == "python"` (query `audit_log` via `kastellan_db::audit`).
7. **Fail-closed scenario**: a second skill run against a daemon started **without** `KASTELLAN_PYTHON_EXEC_ENABLE=1` → exit 1 + stderr names a tool-not-found / not-registered error (never silent success). (If reusing one daemon is simpler, assert this with a separate daemon instance or document why it's deferred to a unit-level check on the `is_python_skill_metadata` branch.)

Skeleton (fill the harness from the templated file — do not invent helper names):
```rust
//! PG + real-daemon e2e for the python-exec skill catalog invocation path
//! (slice 2). Mirrors `cli_memory_l3_run_daemon_e2e.rs`: an approved Python
//! skill, submitted via `memory l3 run --execute`, executes against the
//! daemon's live registry under the real python-exec jail and returns the
//! snippet's stdout. Pins the #179 invariant (CLI env carries no worker bin)
//! and the `kind:"python"` audit field. Skip-as-pass without PG/sandbox/bins.

// … bring_up_pg_cluster + daemon scaffold copied from the templated e2e …
```

- [ ] **Step 3: Run the e2e (skip-as-pass on Mac is acceptable; run live where PG is available)**

Run (Mac, individual suite under live PG per the handover's PG-bin override):
`source "$HOME/.cargo/env" && KASTELLAN_PG_BIN_DIR="/Applications/Postgres 2.app/Contents/Versions/18/bin" cargo test -p kastellan-core --test cli_memory_l3py_run_daemon_e2e -- --nocapture 2>&1 | tail -40`
Expected: the round-trip PASSES (stdout `hi from skill`, exit 0, `kind:"python"` audit row), OR a clean `[SKIP]` if the env can't host it.

- [ ] **Step 4: Full workspace + clippy**

Run: `source "$HOME/.cargo/env" && cargo test --workspace 2>&1 | tail -15 && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -5`
Expected: workspace green (skip-as-pass posture on Mac), clippy clean.

- [ ] **Step 5: Commit**

```bash
git add core/tests/cli_memory_l3py_run_daemon_e2e.rs
git commit -m "test(python-exec): cli_memory_l3py_run_daemon_e2e — approved python skill executes via daemon, kind:python audited"
```

---

## Final verification (before PR)

- [ ] **Workspace test + clippy both platforms-relevant:**
  - Mac (skip-as-pass): `cargo test --workspace` green; `cargo clippy --workspace --all-targets -- -D warnings` clean.
  - Live-PG individual suites (Mac, `KASTELLAN_PG_BIN_DIR` override) for the new e2e + `l3py_*` DB-touching paths.
  - Per the handover, DGX native acceptance is only needed if new sandbox/seccomp surface is added — slice 2 adds none (it reuses the python-exec jail from slice #1, already DGX-accepted). Note this explicitly in the PR.
- [ ] **File-size cap:** `wc -l core/src/memory/l3py_invoke/*.rs core/src/scheduler/l3_run.rs core/src/memory/l3_surface.rs core/src/scheduler/inner_loop.rs` — each ≤ 500 (lift inline tests to `l3py_invoke/tests.rs` if a sibling crosses cap; the facade already declares `#[cfg(test)] mod tests;`).
- [ ] **Update HANDOVER.md + ROADMAP.md**, commit, push, open PR linking the design spec + this plan.

---

## Self-review notes (author)

- **Spec coverage:** Task 1 ⇄ §4.5 gate (steps 1-3 + SHA close); Task 2 ⇄ §4.5 operator path; Task 3 ⇄ §5 daemon run + §6 fail-closed; Task 4 ⇄ §5 CLI kind-aware; Task 5 ⇄ §4.6 surfacing (code never shown); Task 6 ⇄ §4.5 agent-autonomous; Task 7 ⇄ §8 e2e + audit `kind`. Deferred (params, operator `register`, per-trust ceilings, macOS scratch) stay out of scope per §7.
- **Type consistency:** `prepare_python_invocation(&PythonSkillCandidate, SkillTrust, &str) -> Result<String, InvokeRefusal>` used identically in Tasks 1/2/6. `python_exec_step(&str) -> L3TemplateStep` reused in Tasks 1/2; `PY_EXEC_TOOL`/`PY_EXEC_METHOD` consts shared by `pure`/`agent`. `InvokeReport` reused unchanged. `invoke_python_skill`/`invoke_python_steps` signatures fixed in Task 2 and called in Task 3.
- **Verify-before-write hooks flagged inline:** test runtime form (`block_on` vs `#[tokio::test]`), `DataClass` import path, `load_layer_by_trust` exact signature/row type, and the templated e2e harness helper names — each Task names the `grep`/`sed` to run first.
