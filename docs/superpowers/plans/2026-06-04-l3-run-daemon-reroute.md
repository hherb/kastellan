# L3 `run` daemon reroute (#179 Opt-3) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `hhagent-cli memory l3 run <id>` execute inside the daemon against its single live `ToolRegistry` (via the existing Postgres task queue) instead of rebuilding the registry in-process from the operator's env — removing the #179 env-divergence cliff and retiring the in-process execution path entirely.

**Architecture:** `run` enqueues an `l3_run` task (`{kind, memory_id, args, execute}`) on the long lane and waits on `LISTEN tasks_completed` (same channel `ask` uses). The daemon's `drain_lane`, on claiming a task whose payload `kind == "l3_run"`, routes it to a new `scheduler::l3_run` handler that loads the L3 row and calls the **existing** `invoke_l3` with the daemon's live dispatcher (`dispatcher.known_tools()` as the live tool set), then finalizes the task with the serialized `InvokeReport`. The CLI deserializes that result and renders Refused/DryRun/Executed exactly as today. The interim `diagnose_registry_divergence` diagnostic (PR #180) is deleted as now-obsolete.

**Tech Stack:** Rust (tokio, sqlx, serde_json), Postgres `tasks` table + `LISTEN/NOTIFY`, the `hhagent-core` scheduler + `memory::l3_invoke` modules.

**Spec:** [`docs/superpowers/specs/2026-06-04-l3-run-daemon-reroute-design.md`](../specs/2026-06-04-l3-run-daemon-reroute-design.md)

**Branch:** `fix/issue-179-l3-run-daemon-reroute` (already created, off `main` at `ef01ae3`).

**Build/test reminders (this repo):**
- `source "$HOME/.cargo/env"` first (cargo not on non-interactive PATH).
- Unit tests: `cargo test -p hhagent-core <name>`.
- Live-PG e2e on the DGX (native): `cargo test -p hhagent-core --test <file> -- --nocapture` with a Postgres bin dir configured (`HHAGENT_PG_BIN_DIR`). Sandbox/daemon e2e print `[SKIP]` if `bwrap`/userns unavailable — re-check with `--nocapture`.
- Clippy gate: `cargo clippy --workspace --all-targets --locked -- -D warnings`.

---

## File Structure

| File | Responsibility | Change |
|------|----------------|--------|
| `core/src/memory/l3_invoke/operator.rs` | `InvokeReport` enum + `invoke_l3` orchestration | Add `Serialize, Deserialize` to `InvokeReport` |
| `core/src/memory/l3_invoke/pure.rs` | pure invoke helpers | **Delete** `RegistryDivergence` + `diagnose_registry_divergence` |
| `core/src/memory/l3_invoke/tests.rs` | l3_invoke unit tests | **Delete** the diagnose tests; **add** `InvokeReport` serde round-trip |
| `core/src/memory/l3_invoke/mod.rs` | re-exports | Drop `diagnose_registry_divergence` / `RegistryDivergence` from the glob/explicit re-exports if listed |
| `core/src/scheduler/l3_run.rs` | **NEW** — daemon-side `l3_run` payload parse + handler | Create |
| `core/src/scheduler/mod.rs` | scheduler module list | Add `pub mod l3_run;` |
| `core/src/scheduler/runner.rs` | `drain_lane` claim loop | Branch to the `l3_run` handler on `kind == "l3_run"` |
| `core/src/bin/hhagent-cli/memory_l3/run.rs` | the `run` CLI handler | Rewrite body: submit + wait + render; keep `parse_run_argv`; add `render_invoke_report` |
| `core/src/bin/hhagent-cli/memory_l3/shared.rs` | shared CLI helpers | (No change unless `latest_registry_tools` becomes unused — see Task 5) |
| `core/tests/cli_memory_l3_run_e2e.rs` | in-process `invoke_l3` tests | Update module doc only (now documents daemon-side machinery) |
| `core/tests/cli_memory_l3_run_daemon_e2e.rs` | **NEW** — real-daemon + CLI-subprocess e2e | Create |
| `core/tests/cli_memory_l3_e2e.rs` | CLI subprocess e2e | **Delete** the obsolete divergence-hint scenario (scenario 9) |

---

## Task 1: Make `InvokeReport` serializable

The daemon must write the report into `tasks.result` and the CLI must read it back. `InvokeReport`'s fields (`Vec<String>`, `Vec<L3TemplateStep>`, `Vec<StepOutcome>`, `usize`) already serialize; `L3TemplateStep` and `StepOutcome` already derive both `Serialize` and `Deserialize`.

**Files:**
- Modify: `core/src/memory/l3_invoke/operator.rs` (the `InvokeReport` enum, ~line 52)
- Test: `core/src/memory/l3_invoke/tests.rs`

- [ ] **Step 1: Write the failing serde round-trip test**

In `core/src/memory/l3_invoke/tests.rs`, add (adjust the `use super::*;` / import path to match the file's existing imports — the sibling test module imports the parent module items):

```rust
#[test]
fn invoke_report_serde_round_trips_each_variant() {
    use hhagent_core::scheduler::inner_loop::StepOutcome;
    // NOTE: inside the crate the path is `crate::...`; this test module is a
    // sibling of l3_invoke, so use the same import style already present in
    // this file for InvokeReport / L3TemplateStep.
    let refused = InvokeReport::Refused { reasons: vec!["nope".into()] };
    let dry = InvokeReport::DryRun { steps: vec![] };
    let exec = InvokeReport::Executed {
        outcomes: vec![StepOutcome::Ok(serde_json::json!({"ok": true}))],
        steps_total: 1,
    };
    for report in [refused, dry, exec] {
        let v = serde_json::to_value(&report).expect("serialize");
        let back: InvokeReport = serde_json::from_value(v).expect("deserialize");
        // Compare via Debug strings (InvokeReport has no PartialEq).
        assert_eq!(format!("{report:?}"), format!("{back:?}"));
    }
}
```

> If `InvokeReport` / `L3TemplateStep` are not already in scope in `tests.rs`, add the matching `use` lines used elsewhere in that file (the existing tests reference these types; copy their import path).

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p hhagent-core invoke_report_serde_round_trips`
Expected: FAIL — `InvokeReport` does not implement `Serialize`/`Deserialize` (compile error: `the trait bound InvokeReport: Serialize is not satisfied`).

- [ ] **Step 3: Add the derive**

In `core/src/memory/l3_invoke/operator.rs`, change the `InvokeReport` derive line from:

```rust
/// Result of an [`invoke_l3`] call.
#[derive(Debug)]
pub enum InvokeReport {
```

to:

```rust
/// Result of an [`invoke_l3`] call.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub enum InvokeReport {
```

(If `serde::{Serialize, Deserialize}` is already imported at the top of `operator.rs`, use the bare `#[derive(Debug, Serialize, Deserialize)]` form to match the file's style.)

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p hhagent-core invoke_report_serde_round_trips`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add core/src/memory/l3_invoke/operator.rs core/src/memory/l3_invoke/tests.rs
git commit -m "feat(l3): make InvokeReport (de)serializable for daemon-side run results"
```

---

## Task 2: Daemon-side `l3_run` payload + handler module

A new module owns the `l3_run` task contract: detect it, parse it, and run it against the daemon's dispatcher by reusing `invoke_l3`.

**Files:**
- Create: `core/src/scheduler/l3_run.rs`
- Modify: `core/src/scheduler/mod.rs` (add `pub mod l3_run;`)
- Test: inline `#[cfg(test)] mod tests` in `l3_run.rs` (pure payload tests)

- [ ] **Step 1: Register the module**

In `core/src/scheduler/mod.rs`, add (alphabetically near the other `pub mod` lines):

```rust
pub mod l3_run;
```

- [ ] **Step 2: Write the pure payload functions + their tests**

These pure functions are written together with their tests (legitimate — no I/O to mock; they pass immediately). The async handler comes in Step 4 (it needs live PG, so it is covered by the Task 6 e2e, not a unit test).

Create `core/src/scheduler/l3_run.rs` with the pure surface + the test module:

```rust
//! Daemon-side handling of operator-submitted `l3_run` tasks (issue #179).
//!
//! `hhagent-cli memory l3 run <id>` no longer executes in-process. It enqueues
//! a `tasks` row whose payload `kind == "l3_run"`; the scheduler claims it on a
//! lane loop and routes it here. We load the L3 skill row and call the existing
//! [`crate::memory::l3_invoke::invoke_l3`] with the daemon's live dispatcher —
//! so execution uses the daemon's single `ToolRegistry`, eliminating the
//! operator-env divergence the in-process rebuild suffered (#179 Opt 3).

use std::collections::BTreeMap;

use serde_json::Value;

/// The `kind` discriminator written by the CLI into `tasks.payload`.
pub const L3_RUN_KIND: &str = "l3_run";

/// Parsed `l3_run` task payload.
#[derive(Debug, PartialEq, Eq)]
pub struct L3RunRequest {
    pub memory_id: i64,
    pub args: BTreeMap<String, String>,
    pub execute: bool,
}

/// True iff this task payload is an `l3_run` directive.
pub fn is_l3_run_payload(payload: &Value) -> bool {
    payload.get("kind").and_then(|v| v.as_str()) == Some(L3_RUN_KIND)
}

/// Parse an `l3_run` payload. Returns a human-readable error string on any
/// shape violation (the caller turns it into an `InvokeReport::Refused`).
pub fn parse_l3_run_payload(payload: &Value) -> Result<L3RunRequest, String> {
    if !is_l3_run_payload(payload) {
        return Err("payload kind is not 'l3_run'".to_string());
    }
    let memory_id = payload
        .get("memory_id")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| "l3_run payload missing integer 'memory_id'".to_string())?;
    let execute = payload.get("execute").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut args = BTreeMap::new();
    if let Some(obj) = payload.get("args") {
        let map = obj
            .as_object()
            .ok_or_else(|| "l3_run payload 'args' is not an object".to_string())?;
        for (k, v) in map {
            let s = v
                .as_str()
                .ok_or_else(|| format!("l3_run arg '{k}' is not a string"))?;
            args.insert(k.clone(), s.to_string());
        }
    }
    Ok(L3RunRequest { memory_id, args, execute })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_l3_run_kind() {
        assert!(is_l3_run_payload(&serde_json::json!({"kind": "l3_run"})));
        assert!(!is_l3_run_payload(&serde_json::json!({"kind": "ask"})));
        assert!(!is_l3_run_payload(&serde_json::json!({})));
    }

    #[test]
    fn parses_full_payload() {
        let p = serde_json::json!({
            "kind": "l3_run", "memory_id": 42,
            "args": {"name": "world"}, "execute": true
        });
        let got = parse_l3_run_payload(&p).unwrap();
        assert_eq!(got.memory_id, 42);
        assert_eq!(got.args.get("name").map(String::as_str), Some("world"));
        assert!(got.execute);
    }

    #[test]
    fn execute_defaults_false_and_args_optional() {
        let p = serde_json::json!({"kind": "l3_run", "memory_id": 7});
        let got = parse_l3_run_payload(&p).unwrap();
        assert!(!got.execute);
        assert!(got.args.is_empty());
    }

    #[test]
    fn rejects_missing_memory_id() {
        let p = serde_json::json!({"kind": "l3_run", "execute": true});
        assert!(parse_l3_run_payload(&p).is_err());
    }

    #[test]
    fn rejects_non_string_arg_value() {
        let p = serde_json::json!({"kind": "l3_run", "memory_id": 1, "args": {"n": 5}});
        assert!(parse_l3_run_payload(&p).unwrap_err().contains("not a string"));
    }

    #[test]
    fn rejects_wrong_kind() {
        let p = serde_json::json!({"kind": "ask", "memory_id": 1});
        assert!(parse_l3_run_payload(&p).is_err());
    }
}
```

- [ ] **Step 3: Run the pure tests to verify they pass**

Run: `cargo test -p hhagent-core --lib scheduler::l3_run`
Expected: PASS (6 tests). The async handler is not present yet — that is fine; these tests cover only the pure functions.

- [ ] **Step 4: Implement the async handler**

Add the handler below the pure functions (above the `#[cfg(test)]` block), with these imports added to the top-of-file `use` block:

```rust
use sqlx::PgPool;

use crate::cassandra::types::L3SkillCandidate;
use crate::memory::l3_approval::SkillTrust;
use crate::memory::l3_invoke::{invoke_l3, InvokeReport};
use crate::scheduler::inner_loop::StepDispatcher;
use hhagent_db::memories::{fetch_by_ids, MemoryLayer};
```

Add the handler:

```rust
/// Execute an operator-submitted `l3_run` task against the daemon's live
/// dispatcher. Pure-failure cases (bad payload, missing/wrong-layer/unparseable
/// skill) are surfaced as `InvokeReport::Refused` so the CLI renders a refusal
/// rather than a task crash. Dispatch + audit are delegated to `invoke_l3`,
/// which audits with `actor='cli'` — preserving operator provenance even though
/// the steps physically run inside the daemon.
pub async fn run_l3_run_task(
    pool: &PgPool,
    dispatcher: &dyn StepDispatcher,
    payload: &Value,
) -> InvokeReport {
    let req = match parse_l3_run_payload(payload) {
        Ok(r) => r,
        Err(e) => return InvokeReport::Refused { reasons: vec![e] },
    };

    let row = match fetch_by_ids(pool, &[req.memory_id]).await {
        Ok(mut v) => v.pop(),
        Err(e) => {
            return InvokeReport::Refused {
                reasons: vec![format!("loading skill id={}: {e}", req.memory_id)],
            }
        }
    };
    let row = match row {
        Some(r) if r.layer == MemoryLayer::Skill => r,
        _ => {
            return InvokeReport::Refused {
                reasons: vec![format!("no layer-3 skill with id={}", req.memory_id)],
            }
        }
    };
    let template: L3SkillCandidate = match row
        .metadata
        .get("template")
        .cloned()
        .and_then(|t| serde_json::from_value(t).ok())
    {
        Some(t) => t,
        None => {
            return InvokeReport::Refused {
                reasons: vec![format!("skill id={} has no parseable template", req.memory_id)],
            }
        }
    };
    let trust = SkillTrust::from_metadata_str(
        row.metadata.get("trust").and_then(|v| v.as_str()).unwrap_or(""),
    );
    let body_sha256 = row.metadata.get("body_sha256").and_then(|v| v.as_str()).unwrap_or("");

    // The daemon's dispatcher exposes its live registry's tool names — the
    // authoritative set, with no operator-env rebuild (this is the #179 fix).
    let live_tools = dispatcher.known_tools();

    invoke_l3(
        pool,
        req.memory_id,
        dispatcher,
        &template,
        trust,
        body_sha256,
        &req.args,
        &live_tools,
        req.execute,
    )
    .await
}
```

The full top-of-file `use` block is then: `std::collections::BTreeMap`, `serde_json::Value`, `sqlx::PgPool`, `crate::cassandra::types::L3SkillCandidate`, `crate::memory::l3_approval::SkillTrust`, `crate::memory::l3_invoke::{invoke_l3, InvokeReport}`, `crate::scheduler::inner_loop::StepDispatcher`, `hhagent_db::memories::{fetch_by_ids, MemoryLayer}`.

- [ ] **Step 5: Run the tests to verify they pass + clippy**

Run: `cargo test -p hhagent-core --lib scheduler::l3_run`
Expected: PASS (6 tests).
Run: `cargo clippy -p hhagent-core --all-targets --locked -- -D warnings`
Expected: exit 0.

- [ ] **Step 6: Commit**

```bash
git add core/src/scheduler/l3_run.rs core/src/scheduler/mod.rs
git commit -m "feat(scheduler): daemon-side l3_run task handler (reuses invoke_l3)"
```

---

## Task 3: Route `l3_run` tasks in `drain_lane`

The lane loop must recognise an `l3_run` task and run it via the new handler instead of the `ask` agent loop (`run_one`), then finalize it.

**Files:**
- Modify: `core/src/scheduler/runner.rs` (`drain_lane`, the claim block ~lines 161–249)

- [ ] **Step 1: Add the branch**

In `core/src/scheduler/runner.rs`, inside `drain_lane`, immediately **after** the `write_lifecycle_row(pool, ACTION_TASK_RUNNING, …)` call (currently ~line 187) and **before** `let result = run_one(…)`, insert:

```rust
        // Operator-submitted L3 skill run (issue #179): execute in-daemon
        // against the live registry, then finalize. A refusal still finalizes
        // `completed` — it is a valid outcome the CLI renders, not a crash.
        if crate::scheduler::l3_run::is_l3_run_payload(&claimed.payload) {
            let report = crate::scheduler::l3_run::run_l3_run_task(
                pool,
                dispatcher.as_ref(),
                &claimed.payload,
            )
            .await;
            let result_payload = serde_json::to_value(&report).ok();
            if let Err(e) =
                tasks::finalize(pool, claimed.id, "completed", result_payload).await
            {
                tracing::warn!(
                    lane = lane.as_sql(), task_id = claimed.id, error = %e,
                    "l3_run finalize UPDATE failed"
                );
            }
            write_lifecycle_row(
                pool,
                &action_task_terminal("completed"),
                claimed.id,
                claimed.lane,
                0,
            )
            .await;
            continue;
        }
```

> `dispatcher` is the `Arc<dyn StepDispatcher>` param of `drain_lane`; `dispatcher.as_ref()` yields `&dyn StepDispatcher`. `action_task_terminal` and `tasks` are already imported in `runner.rs` (used just below). The `l3_run` path deliberately skips the finalize-summary row and the L1/L3 crystallisation hooks — those are agent-task concerns.

- [ ] **Step 2: Build to verify it compiles**

Run: `cargo build -p hhagent-core`
Expected: success. (Behaviour is covered by the Task 6 e2e; there is no cheap unit test for `drain_lane`, which needs a live DB + dispatcher.)

- [ ] **Step 3: Clippy**

Run: `cargo clippy -p hhagent-core --all-targets --locked -- -D warnings`
Expected: exit 0.

- [ ] **Step 4: Commit**

```bash
git add core/src/scheduler/runner.rs
git commit -m "feat(scheduler): route l3_run tasks to the daemon-side handler in drain_lane"
```

---

## Task 4: Rewrite the `run` CLI to submit + wait + render

The CLI stops building any registry/dispatcher. It submits an `l3_run` task, waits for completion (with a no-daemon grace-timeout that cancels the task), reads the serialized `InvokeReport` from `tasks.result`, and renders it with a pure helper.

**Files:**
- Modify: `core/src/bin/hhagent-cli/memory_l3/run.rs` (rewrite `memory_l3_run`; keep `parse_run_argv` + its tests; add `render_invoke_report`; delete `DryRunNeverDispatches`)
- Test: the inline `#[cfg(test)] mod tests` in `run.rs`

- [ ] **Step 1: Write failing tests for `render_invoke_report`**

Add to the `#[cfg(test)] mod tests` in `run.rs` (the module currently imports `super::{parse_run_argv, RunArgv}` — extend it):

```rust
    use super::render_invoke_report;
    use hhagent_core::memory::l3_invoke::InvokeReport;
    use hhagent_core::scheduler::inner_loop::StepOutcome;

    #[test]
    fn render_refused_is_nonzero_and_lists_reasons() {
        let (text, code) = render_invoke_report(
            5, "echo",
            &InvokeReport::Refused { reasons: vec!["tool x not in registry".into()] },
        );
        assert_eq!(code, 1);
        assert!(text.contains("REFUSED"));
        assert!(text.contains("tool x not in registry"));
    }

    #[test]
    fn render_dry_run_is_zero() {
        let (text, code) = render_invoke_report(
            5, "echo", &InvokeReport::DryRun { steps: vec![] },
        );
        assert_eq!(code, 0);
        assert!(text.contains("dry-run"));
    }

    #[test]
    fn render_executed_all_ok_is_zero() {
        let (_text, code) = render_invoke_report(
            5, "echo",
            &InvokeReport::Executed {
                outcomes: vec![StepOutcome::Ok(serde_json::json!({"ok": true}))],
                steps_total: 1,
            },
        );
        assert_eq!(code, 0);
    }

    #[test]
    fn render_executed_with_error_is_nonzero() {
        let (text, code) = render_invoke_report(
            5, "echo",
            &InvokeReport::Executed {
                outcomes: vec![StepOutcome::Err { code: "BOOM".into(), detail: "nope".into() }],
                steps_total: 2,
            },
        );
        assert_eq!(code, 1);
        assert!(text.contains("BOOM"));
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p hhagent-core --bin hhagent-cli render_`
Expected: FAIL — `render_invoke_report` not defined.

> If `--bin hhagent-cli` is not the exact bin name, find it with `grep -n "name = \"hhagent-cli\"" core/Cargo.toml` or run the whole bin test set: `cargo test -p hhagent-core --bins render_`.

- [ ] **Step 3: Add the pure `render_invoke_report` helper**

Add to `run.rs` (top-level, after `parse_run_argv`). It returns `(stdout_or_stderr_text, exit_code)`; the caller decides the stream. To keep parity with today, success lines go to stdout, error/refusal lines to stderr — so the helper returns a struct-free `(String, i32)` where the *text* is what to print and the code drives stdout-vs-stderr at the call site. Simplest: build one combined string and let the caller print Refused/Executed-error text to stderr, DryRun/ok text to stdout. To keep the helper pure and testable, return `(text, code)` and have the helper assemble the human text; the call site prints to stdout when `code == 0` else stderr (the existing behaviour split error lines to stderr — acceptable simplification; document it):

```rust
/// Render an [`InvokeReport`] to operator-facing text + an exit code.
///
/// Pure (no I/O) so it is unit-testable. The caller prints the text to stdout
/// when `code == 0`, else to stderr. Exit codes match the pre-reroute CLI:
/// DryRun and all-ok Executed → 0; Refused and any-error Executed → 1.
pub(super) fn render_invoke_report(
    id: i64,
    skill_name: &str,
    report: &InvokeReport,
) -> (String, i32) {
    use std::fmt::Write as _;
    let mut out = String::new();
    match report {
        InvokeReport::Refused { reasons } => {
            let _ = writeln!(out, "REFUSED to run skill '{skill_name}' (#{id}):");
            for r in reasons {
                let _ = writeln!(out, "  - {r}");
            }
            (out, 1)
        }
        InvokeReport::DryRun { steps } => {
            let _ = writeln!(
                out,
                "dry-run: skill '{skill_name}' (#{id}) would dispatch {} step(s):",
                steps.len()
            );
            for (n, s) in steps.iter().enumerate() {
                let _ = writeln!(out, "  [{n}] {}/{} {}", s.tool, s.method, s.parameters);
            }
            let _ = write!(out, "(re-run with --execute to dispatch)");
            (out, 0)
        }
        InvokeReport::Executed { outcomes, steps_total } => {
            let any_err = outcomes.iter().any(|o| o.is_err());
            let _ = writeln!(
                out,
                "executed skill '{skill_name}' (#{id}): {}/{} step(s)",
                outcomes.len(),
                steps_total
            );
            for (n, o) in outcomes.iter().enumerate() {
                match o {
                    StepOutcome::Ok(v) => {
                        let _ = writeln!(out, "  [{n}] ok: {v}");
                    }
                    StepOutcome::Err { code, detail } => {
                        let _ = writeln!(out, "  [{n}] ERR {code}: {detail}");
                    }
                }
            }
            (out, if any_err { 1 } else { 0 })
        }
    }
}
```

Add the imports `render_invoke_report` needs at the top of the module body (or inside the fn): `use hhagent_core::memory::l3_invoke::InvokeReport;` and `use hhagent_core::scheduler::inner_loop::StepOutcome;` — place them with the other `use` lines used by `memory_l3_run`.

- [ ] **Step 4: Run the render tests to verify they pass**

Run: `cargo test -p hhagent-core --bins render_`
Expected: PASS (4 tests).

- [ ] **Step 5: Rewrite `memory_l3_run` to submit + wait**

Replace the entire body of `memory_l3_run` (the part after argv parsing) and delete `DryRunNeverDispatches`. The new body, after `parse_run_argv` + `parse_args`:

```rust
pub(super) async fn memory_l3_run(args: &[String]) -> ExitCode {
    use std::time::Duration;

    use hhagent_core::cli_audit::submit_and_audit; // cancel_and_audit is used inside the wait helper
    use hhagent_core::memory::l3_invoke::{parse_args, InvokeReport};
    use hhagent_db::pool::connect_runtime_pool;
    use hhagent_db::tasks::{get, Lane};
    use sqlx::postgres::PgListener;

    // --- parse argv ----------------------------------------------------
    let RunArgv { id, arg_tokens, execute } = match parse_run_argv(args) {
        Ok(v) => v,
        Err(msg) => { eprintln!("{msg}"); return ExitCode::from(2); }
    };
    let args_map = match parse_args(&arg_tokens) {
        Ok(m) => m,
        Err(e) => { eprintln!("memory l3 run: {e}"); return ExitCode::from(2); }
    };

    // --- connect -------------------------------------------------------
    let spec = match resolve_connect_spec() {
        Ok(s) => s,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let pool = match connect_runtime_pool(&spec).await {
        Ok(p) => p,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };

    // --- LISTEN before submit (avoid the NOTIFY-before-listen race) ----
    let mut listener = match PgListener::connect_with(&pool).await {
        Ok(l) => l,
        Err(e) => { eprintln!("memory l3 run: listener connect failed: {e}"); return ExitCode::from(1); }
    };
    if let Err(e) = listener.listen("tasks_completed").await {
        eprintln!("memory l3 run: listen failed: {e}");
        return ExitCode::from(1);
    }

    // --- submit the l3_run task ----------------------------------------
    let payload = serde_json::json!({
        "kind": "l3_run",
        "memory_id": id,
        "args": args_map,
        "execute": execute,
    });
    let task_id = match submit_and_audit(&pool, Lane::Long, payload).await {
        Ok(i) => i,
        Err(e) => { eprintln!("memory l3 run: submit failed: {e}"); return ExitCode::from(1); }
    };
    eprintln!("memory l3 run: submitted task {task_id} (lane=long); waiting for the daemon…");

    // --- wait for completion, detecting a missing daemon ---------------
    // Phase 1: until the task leaves 'pending' (daemon claimed it) or the
    // grace window elapses. If still 'pending' after grace, no lane loop is
    // consuming it → the daemon is not running; cancel so it can't be run
    // silently later, and error out.
    let grace = Duration::from_secs(env_secs("HHAGENT_L3_RUN_GRACE_SECS", 5));
    let overall = Duration::from_secs(env_secs("HHAGENT_L3_RUN_TIMEOUT_SECS", 1800));

    if let Err(code) = wait_until_claimed_or_no_daemon(&pool, &mut listener, task_id, grace).await {
        // wait_until_claimed_or_no_daemon already printed + cancelled as needed.
        return code;
    }

    // Phase 2: wait for the terminal NOTIFY (bounded by `overall`).
    let completed = tokio::time::timeout(overall, async {
        loop {
            match listener.recv().await {
                Ok(n) if n.payload() == task_id.to_string() => return Ok(()),
                Ok(_) => continue,
                Err(e) => return Err(format!("listener.recv: {e}")),
            }
        }
    })
    .await;
    match completed {
        Ok(Ok(())) => {}
        Ok(Err(e)) => { eprintln!("memory l3 run: {e}"); return ExitCode::from(1); }
        Err(_) => {
            eprintln!("memory l3 run: timed out after {}s waiting for task {task_id}", overall.as_secs());
            return ExitCode::from(1);
        }
    }

    // --- read + render the result --------------------------------------
    let task = match get(&pool, task_id).await {
        Ok(Some(t)) => t,
        Ok(None) => { eprintln!("memory l3 run: task {task_id} disappeared"); return ExitCode::from(1); }
        Err(e) => { eprintln!("memory l3 run: get failed: {e}"); return ExitCode::from(1); }
    };
    let report: InvokeReport = match task.result {
        Some(r) => match serde_json::from_value(r) {
            Ok(rep) => rep,
            Err(e) => { eprintln!("memory l3 run: unreadable result for task {task_id}: {e}"); return ExitCode::from(1); }
        },
        None => {
            eprintln!("memory l3 run: task {task_id} ended in state '{}' with no result", task.state);
            return ExitCode::from(1);
        }
    };
    // The skill name is inside the report's steps only for DryRun; carry the
    // id and let the report supply specifics. Use the id for the header; the
    // skill name is not needed for correctness (kept generic).
    let (text, code) = render_invoke_report(id, "<skill>", &report);
    if code == 0 { println!("{text}"); } else { eprintln!("{text}"); }
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

/// Parse a u64 seconds env var with a default; non-numeric ⇒ default.
fn env_secs(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Phase-1 wait: return `Ok(())` once the task is observed in a non-`pending`
/// state (daemon claimed it). If it is still `pending` after `grace`, print a
/// "daemon not running" error, cancel the task, and return `Err(exit_code)`.
async fn wait_until_claimed_or_no_daemon(
    pool: &sqlx::PgPool,
    listener: &mut sqlx::postgres::PgListener,
    task_id: i64,
    grace: std::time::Duration,
) -> Result<(), std::process::ExitCode> {
    use hhagent_core::cli_audit::cancel_and_audit;
    use hhagent_db::tasks::get;

    // A NOTIFY may arrive during the grace window if the task completes very
    // fast; treat that as "claimed" (Phase 2's recv will see the same id again
    // is not guaranteed — so we re-check state below rather than rely on it).
    let _ = tokio::time::timeout(grace, async {
        loop {
            match listener.recv().await {
                Ok(n) if n.payload() == task_id.to_string() => return,
                _ => continue,
            }
        }
    })
    .await; // ignore timeout result; we authoritatively check state next.

    match get(pool, task_id).await {
        Ok(Some(t)) if t.state == "pending" => {
            eprintln!(
                "memory l3 run: the daemon does not appear to be running \
                 (task {task_id} still pending after {}s). Cancelling.",
                grace.as_secs()
            );
            let _ = cancel_and_audit(pool, task_id).await;
            Err(std::process::ExitCode::from(1))
        }
        Ok(_) => Ok(()), // running or already terminal — proceed to Phase 2 / read
        Err(e) => {
            eprintln!("memory l3 run: get failed: {e}");
            Err(std::process::ExitCode::from(1))
        }
    }
}
```

> **Subtlety:** if the task already reached a terminal state during Phase 1 (fast completion), Phase 2's `listener.recv` would block waiting for a NOTIFY that already fired. Guard against it: after `wait_until_claimed_or_no_daemon` returns `Ok`, re-`get` the task; if it is already terminal (`state != "running" && state != "pending"`), skip Phase 2 and go straight to rendering. Add that check at the top of Phase 2:

```rust
    // Fast-path: already terminal? Skip the Phase-2 wait.
    let already_done = matches!(
        get(&pool, task_id).await,
        Ok(Some(ref t)) if t.state != "running" && t.state != "pending"
    );
    if !already_done {
        // ... the tokio::time::timeout(overall, …) loop from above ...
    }
```

Fold this in so Phase 2 only waits when the task is still `running`.

- [ ] **Step 6: Remove dead code + stale doc**

Delete `struct DryRunNeverDispatches` and its `impl StepDispatcher`. Replace the long `## Operator-environment prerequisite` doc comment on `memory_l3_run` with a short one:

```rust
/// `memory l3 run <id> [--arg name=value]… [--execute]`
///
/// Submits an `l3_run` task to the daemon and waits for it to execute the
/// approved skill against the daemon's live tool registry (issue #179, Opt 3 —
/// the in-process registry rebuild and its env-divergence cliff are retired).
/// Dry-run by default (no `--execute`): the daemon validates + returns the
/// concrete steps without dispatching. Requires a running daemon; if none is
/// consuming the lane, the submit is cancelled and an error is printed.
```

Remove now-unused imports (the old `BTreeSet`, `Arc`, `ToolHostStepDispatcher`, `registry_build`, `SkillTrust`, `L3SkillCandidate`, `fetch_by_ids`, etc.). Let the compiler/clippy tell you which.

- [ ] **Step 7: Build, test, clippy**

Run: `cargo test -p hhagent-core --bins -- parse_run_argv render_`
Expected: PASS (the 11 `parse_run_argv` tests + 4 render tests).
Run: `cargo clippy -p hhagent-core --all-targets --locked -- -D warnings`
Expected: exit 0.

- [ ] **Step 8: Commit**

```bash
git add core/src/bin/hhagent-cli/memory_l3/run.rs
git commit -m "feat(cli): reroute 'memory l3 run' through the daemon via the task queue (#179)"
```

---

## Task 5: Delete the obsolete divergence diagnostic

With the in-process rebuild gone, `diagnose_registry_divergence` / `RegistryDivergence` have no caller. Remove them and their tests.

**Files:**
- Modify: `core/src/memory/l3_invoke/pure.rs` (delete enum + Display + fn, ~lines 300–390)
- Modify: `core/src/memory/l3_invoke/tests.rs` (delete the diagnose tests)
- Modify: `core/src/memory/l3_invoke/mod.rs` (drop any explicit re-export of the removed names)
- Modify: `core/tests/cli_memory_l3_e2e.rs` (delete the divergence-hint scenario, "scenario 9")
- Modify: `core/src/bin/hhagent-cli/memory_l3/shared.rs` (only if `latest_registry_tools` is now unused — verify)

- [ ] **Step 1: Find every reference**

Run: `grep -rn "diagnose_registry_divergence\|RegistryDivergence" --include=*.rs`
Note each location. Expected references (pre-deletion): `pure.rs` (defn), `tests.rs` (tests), `cli_memory_l3_e2e.rs` (scenario 9). The `run.rs` reference was removed in Task 4.

- [ ] **Step 2: Delete the definitions**

In `pure.rs`, delete the `pub enum RegistryDivergence { … }`, its `impl std::fmt::Display for RegistryDivergence { … }`, and `pub fn diagnose_registry_divergence(…) -> Vec<RegistryDivergence> { … }` (the contiguous block ~lines 300–390). If `mod.rs` re-exports them by name (e.g. `pub use pure::{…, diagnose_registry_divergence, RegistryDivergence};`), remove those two names from the re-export list.

- [ ] **Step 3: Delete their unit tests**

In `core/src/memory/l3_invoke/tests.rs`, delete the test fns that exercise `diagnose_registry_divergence` / `RegistryDivergence` (grep within the file for the names; remove each `#[test] fn …` that references them).

- [ ] **Step 4: Delete the obsolete e2e scenario**

In `core/tests/cli_memory_l3_e2e.rs`, delete the scenario-9 test that asserts the `hint:` line for the divergence case (grep for `diagnose_registry_divergence`, `MissingLocallyButInSnapshot`, or `hint:` to locate it). Remove any now-unused helper imports it alone used.

- [ ] **Step 5: Check `latest_registry_tools`**

Run: `grep -rn "latest_registry_tools" --include=*.rs`
If the only remaining references are its definition in `shared.rs` and (still) the `approve`/`pin` paths, leave it. If the `run.rs` removal made it dead, the compiler will warn — only remove it if genuinely unused (approve/pin likely still use it; expect it to stay).

- [ ] **Step 6: Verify clean**

Run: `grep -rn "diagnose_registry_divergence\|RegistryDivergence" --include=*.rs`
Expected: **no matches.**
Run: `cargo build -p hhagent-core && cargo clippy -p hhagent-core --all-targets --locked -- -D warnings`
Expected: build OK, clippy exit 0 (no unused-import/dead-code warnings).

- [ ] **Step 7: Commit**

```bash
git add core/src/memory/l3_invoke/pure.rs core/src/memory/l3_invoke/tests.rs \
        core/src/memory/l3_invoke/mod.rs core/tests/cli_memory_l3_e2e.rs
git commit -m "refactor(l3): drop the obsolete registry-divergence diagnostic (#179 cause removed)"
```

---

## Task 6: Daemon-driven e2e — the #179 regression pin

Prove the cause is gone: a real daemon with `shell-exec` registered, and a `hhagent-cli` subprocess run **without** `HHAGENT_SHELL_EXEC_BIN`, where `memory l3 run --execute` now **succeeds**. Plus dry-run-via-daemon and no-daemon scenarios. Reuse the daemon bring-up + CLI-subprocess machinery from `core/tests/cli_ask_e2e.rs` and the skill-insertion/approval helpers from `core/tests/cli_memory_l3_e2e.rs`.

**Files:**
- Create: `core/tests/cli_memory_l3_run_daemon_e2e.rs`
- Modify: `core/tests/cli_memory_l3_run_e2e.rs` (module-doc note only)

- [ ] **Step 1: Note the existing invoke_l3 e2e still tests the daemon machinery**

At the top of `core/tests/cli_memory_l3_run_e2e.rs`, update the module doc to add:

```rust
//! NOTE (post-#179): the operator CLI no longer calls `invoke_l3` in-process —
//! `memory l3 run` submits an `l3_run` task that the daemon executes via the
//! same `invoke_l3` entry point. These scenarios therefore exercise the
//! **daemon-side** execution machinery directly (no CLI subprocess). The
//! end-to-end CLI→daemon path (including the #179 divergence fix) is covered by
//! `cli_memory_l3_run_daemon_e2e.rs`.
```

(No functional change to its tests.)

- [ ] **Step 2: Scaffold the new e2e with the divergence-fixed test**

Create `core/tests/cli_memory_l3_run_daemon_e2e.rs`. Model the daemon bring-up + subprocess invocation on `cli_ask_e2e.rs` (its `bring_up_daemon`, `cli_binary`, and `Command::new(cli_binary()).args([...]).env(...)` blocks — copy them) and the L3 skill insert + approve flow on `cli_memory_l3_e2e.rs`. The decisive assertion:

```rust
//! Live-PG + real-daemon e2e for `memory l3 run` after the #179 reroute.
//!
//! The daemon registers `shell-exec` via its environment; the CLI subprocess
//! runs WITHOUT `HHAGENT_SHELL_EXEC_BIN`. Pre-#179 the in-process rebuild would
//! refuse ("tool 'shell-exec' not in registry"); after the reroute the daemon
//! executes against its own live registry and the run SUCCEEDS.

// imports: copy the set from cli_ask_e2e.rs (Command, PgListener, tests_common
// bring_up helpers, seed_tool_allowlist, shell_exec_worker_binary, skip guards)
// + the L3 skill insert/approve helpers from cli_memory_l3_e2e.rs.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_succeeds_against_daemon_registry_without_operator_env() {
    // 0. skip guards (sandbox + supervisor + pg bin dir), as in cli_ask_e2e.
    // 1. bring up a PG cluster + seed the shell-exec allowlist (echo).
    // 2. bring up the daemon WITH HHAGENT_SHELL_EXEC_BIN set (so its registry
    //    has shell-exec) — reuse bring_up_daemon from cli_ask_e2e.
    // 3. insert an L3 skill whose single step is shell-exec/<echo argv>, and
    //    flip its trust to user_approved (reuse the crystallise + set_skill_trust
    //    helpers / the approve CLI path from cli_memory_l3_e2e). Capture its id.
    // 4. run the CLI subprocess WITHOUT HHAGENT_SHELL_EXEC_BIN:
    //        Command::new(cli_binary())
    //          .args(["memory","l3","run", &id.to_string(), "--execute"])
    //          .env("PATH","/usr/bin:/bin").env("LC_ALL","C").env("USER",&user)
    //          .env("HHAGENT_DATA_DIR", cluster.data_dir … )
    //          // NB: intentionally NO HHAGENT_SHELL_EXEC_BIN
    //          .output()
    // 5. assert success:
    let status_ok = output.status.success();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        status_ok,
        "run --execute should succeed via the daemon registry; stdout={stdout}, stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("executed skill"), "stdout was: {stdout}");
}
```

> Fill in the bring-up/insert/approve bodies by copying the concrete helpers from `cli_ask_e2e.rs` (daemon) and `cli_memory_l3_e2e.rs` (skill insert + approve). Keep the file under the 500-LOC cap; if shared bring-up grows large, factor a local helper as those files do.

- [ ] **Step 3: Add the dry-run-via-daemon scenario**

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dry_run_previews_steps_via_daemon() {
    // Same setup as above (daemon has shell-exec, skill approved), but run
    // WITHOUT --execute. Assert exit 0 and stdout contains "dry-run" and the
    // concrete step line. Nothing is dispatched (no tool:shell-exec audit row
    // for this task — optional assertion via audit query).
}
```

- [ ] **Step 4: Add the no-daemon scenario**

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_with_no_daemon_cancels_and_errors() {
    // Bring up ONLY a PG cluster (no daemon). Insert + approve a skill.
    // Set HHAGENT_L3_RUN_GRACE_SECS=1 in the subprocess env to keep the test
    // fast. Run `memory l3 run <id>` (dry-run is fine).
    // Assert: non-zero exit; stderr mentions "daemon does not appear to be
    // running"; and the task row is in state 'cancelled' (query via
    // hhagent_db::tasks::get against the pool).
}
```

- [ ] **Step 5: Run the new e2e (DGX, native, with PG configured)**

Run: `cargo test -p hhagent-core --test cli_memory_l3_run_daemon_e2e -- --nocapture`
Expected: 3 tests PASS, zero `[SKIP]` (on a host with bwrap + PG). If `[SKIP]` lines appear, the sandbox/supervisor/PG isn't available — resolve per CLAUDE.md before claiming green.

- [ ] **Step 6: Run the preserved invoke_l3 e2e to confirm it still passes**

Run: `cargo test -p hhagent-core --test cli_memory_l3_run_e2e -- --nocapture`
Expected: the existing scenarios (A–E) PASS unchanged.

- [ ] **Step 7: Commit**

```bash
git add core/tests/cli_memory_l3_run_daemon_e2e.rs core/tests/cli_memory_l3_run_e2e.rs
git commit -m "test(l3): real-daemon e2e for the #179 run reroute (divergence fixed + no-daemon)"
```

---

## Final verification (before PR)

- [ ] `cargo test --workspace` green on the DGX (native Linux). Record the count delta: `+` Task-1 serde test, `+6` Task-2 payload tests, `+4` Task-4 render tests, `+3` Task-6 daemon e2e; `−` the deleted diagnose tests + the deleted cli_memory_l3_e2e scenario 9. Note the exact before/after in the handover.
- [ ] `cargo clippy --workspace --all-targets --locked -- -D warnings` exit 0.
- [ ] `grep -rn "diagnose_registry_divergence\|RegistryDivergence" --include=*.rs` → no matches.
- [ ] Doc-links unchanged vs `main` (run the repo's doc-link check).
- [ ] Manual smoke (optional, DGX): start the daemon, `hhagent-cli memory l3 run <approved-id>` in a plain shell (no `HHAGENT_SHELL_EXEC_BIN`) → dry-run preview; `--execute` → executes.
- [ ] File-size check: `core/src/scheduler/runner.rs` and `core/src/bin/hhagent-cli/memory_l3/run.rs` LOC after changes; if `runner.rs` nears 500, note the `l3_run` handler already lives in its own module (good). Update HANDOVER + ROADMAP and open the PR.

---

## Spec-coverage self-check

- Task-queue transport (no new socket) → Tasks 3–4. ✓
- Retire in-process path entirely → Task 4 (deletes rebuild + `DryRunNeverDispatches`), Task 5 (deletes diagnostic). ✓
- Daemon executes via existing `invoke_l3` + `known_tools()` → Task 2. ✓
- `actor='cli'` provenance preserved → Task 2 (delegates to `invoke_l3`, which audits with `CLI_AUDIT_ACTOR`). ✓
- Dry-run also via daemon → Task 4 submits regardless of `execute`; daemon returns `DryRun`. ✓
- No-daemon grace-timeout + cancel → Task 4 (`wait_until_claimed_or_no_daemon`). ✓
- Result schema = serialized `InvokeReport` → Task 1 + Task 3 (`to_value`) + Task 4 (`from_value`). ✓
- `l3_run` payload schema + parse → Task 2. ✓
- Delete `diagnose_registry_divergence` → Task 5. ✓
- `approve` out of scope → untouched (verified by Task 5 Step 5 leaving `latest_registry_tools`). ✓
- Tests: render unit, serde round-trip, payload round-trip, divergence-fixed e2e, no-daemon e2e → Tasks 1,2,4,6. ✓
