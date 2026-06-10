# Constitutional refusal state — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make agent self-refusal distinguishable from successful completion in `tasks.state`, by introducing an optional `Plan.refused` field, a new `Outcome::Refused` variant, a new `tasks.state='refused'` value, and a planner-prompt update — without conflating the existing reviewer-detected `'blocked'` path.

**Architecture:** Additive change across five layers. Types layer gains `RefusedReason` struct and optional `Plan.refused` field. Scheduler layer gains `Outcome::Refused` variant + inner-loop short-circuit (after the reviewer runs — defense in depth, `Verdict::ConstitutionalBlock` still wins). DB layer gains migration `0012` widening the `tasks.state` CHECK and the `notify_task_completed` trigger. Audit layer extends `agent/plan.formulate` payload with a structured `refused` object and a third `decision_kind` value. Prompt layer adds one sentence + schema example update so the LLM emits the structured marker.

**Tech Stack:** Rust workspace (`kastellan_core`, `kastellan_db`); `sqlx` + Postgres via UDS; `serde` / `serde_json`; TDD via `cargo test --workspace`.

**Source spec:** [`docs/superpowers/specs/2026-05-14-constitutional-refusal-state-design.md`](../specs/2026-05-14-constitutional-refusal-state-design.md) — already committed on this branch at `162ac4a`. Read it before starting.

**Branch:** `feat/refusal-state` (off `main` at `5f543d2`). All work in this plan commits to this branch.

---

## File map

- **Create:**
  - `db/migrations/0012_tasks_state_refused.sql`
- **Modify:**
  - `core/src/cassandra/types.rs` — new `RefusedReason` struct, new optional `Plan.refused` field, new `Plan::is_refused()` helper; existing tests updated for new field
  - `core/src/cassandra/review.rs` — one test helper (`dummy_plan` at line 121–122) updated for new field
  - `core/src/scheduler/inner_loop.rs` — new `Outcome::Refused` variant + `final_state` + `result_payload` arms; new step-4 short-circuit in `run_to_terminal`; `write_audit_plan_formulate` widened with `refused` + extended `decision_kind`; one existing test (`task_context_plans_so_far_summary_is_compact` at line 383) updated for new field
  - `core/tests/scheduler_inner_loop_e2e.rs` — two helpers (`task_complete_plan`, `one_step_plan`) updated for new field; new scenarios 5 and 6
  - `core/tests/scheduler_lanes_e2e.rs` — two helpers updated for new field
  - `db/tests/postgres_e2e.rs` — new test `tasks_state_refused_passes_check_constraint`
  - `prompts/agent_planner.md` — one new sentence in the refusal paragraph + `"refused": null` in the JSON-schema example
  - `docs/devel/handovers/HANDOVER.md` — session-end refresh
  - `docs/devel/ROADMAP.md` — tick issue #23 entry (if present) or add one

---

## Task 1 — `RefusedReason` struct + `Plan.refused` field + `Plan::is_refused()`

**Files:**
- Modify: `core/src/cassandra/types.rs`
- Modify (struct-literal sites): `core/src/cassandra/types.rs:113`, `core/src/cassandra/types.rs:144`, `core/src/cassandra/review.rs:122`, `core/src/scheduler/inner_loop.rs:383`, `core/tests/scheduler_inner_loop_e2e.rs:352`, `core/tests/scheduler_inner_loop_e2e.rs:363`, `core/tests/scheduler_lanes_e2e.rs:345`, `core/tests/scheduler_lanes_e2e.rs:356`
- Test: `core/src/cassandra/types.rs::tests` (inline)

### Step 1.1 — Write the three failing tests

Add these three tests inside the existing `#[cfg(test)] mod tests { ... }` block in `core/src/cassandra/types.rs`:

```rust
#[test]
fn plan_round_trips_refused_field_some() {
    let p = Plan {
        context: "c".into(),
        decision: "task_complete".into(),
        rationale: "r".into(),
        steps: vec![],
        result: Some(serde_json::json!({
            "kind": "text",
            "body": "Principle 1 would be violated."
        })),
        data_ceiling: DataClass::Public,
        refused: Some(RefusedReason {
            principle: 1,
            reason: "physical_harm".into(),
        }),
    };
    let s = serde_json::to_string(&p).unwrap();
    let p2: Plan = serde_json::from_str(&s).unwrap();
    assert_eq!(p, p2, "Plan with refused: Some(...) must round-trip");
}

#[test]
fn plan_omits_refused_key_when_none() {
    let p = Plan {
        context: "c".into(),
        decision: "act".into(),
        rationale: "r".into(),
        steps: vec![],
        result: None,
        data_ceiling: DataClass::Public,
        refused: None,
    };
    let s = serde_json::to_string(&p).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&s).unwrap();
    assert!(
        parsed.get("refused").is_none(),
        "expected `refused` key absent when None; got JSON: {s}"
    );

    // Round-trip remains lossless via #[serde(default)].
    let p2: Plan = serde_json::from_str(&s).unwrap();
    assert_eq!(p, p2);
}

#[test]
fn plan_is_refused_is_independent_of_is_terminal() {
    // The four corners of the (is_refused × is_terminal) matrix.
    let base = Plan {
        context: "c".into(),
        decision: "act".into(),
        rationale: "r".into(),
        steps: vec![],
        result: None,
        data_ceiling: DataClass::Public,
        refused: None,
    };

    // Neither
    assert!(!base.is_refused() && !base.is_terminal());

    // Terminal only
    let mut p = base.clone();
    p.decision = "task_complete".into();
    p.result = Some(serde_json::json!({"kind": "text", "body": "done"}));
    assert!(!p.is_refused() && p.is_terminal());

    // Refused only (non-terminal — malformed shape, but the helper is independent)
    let mut p = base.clone();
    p.refused = Some(RefusedReason { principle: 2, reason: "fraud".into() });
    assert!(p.is_refused() && !p.is_terminal());

    // Both (the well-formed refusal case)
    let mut p = base.clone();
    p.decision = "task_complete".into();
    p.result = Some(serde_json::json!({"kind": "text", "body": "I cannot."}));
    p.refused = Some(RefusedReason { principle: 1, reason: "physical_harm".into() });
    assert!(p.is_refused() && p.is_terminal());
}
```

- [ ] **Step 1.1 — Tests added**

### Step 1.2 — Run tests to confirm RED

```sh
source "$HOME/.cargo/env"
cargo test -p kastellan-core --lib cassandra::types::tests::plan_round_trips_refused_field_some 2>&1 | tail -20
```

Expected: compile error `no field 'refused' in 'Plan'` and `cannot find type 'RefusedReason'`.

- [ ] **Step 1.2 — RED confirmed**

### Step 1.3 — Implement `RefusedReason` + `Plan.refused` + `is_refused()`

In `core/src/cassandra/types.rs`, just *above* the `pub struct Plan { ... }` definition (so `Plan` can reference it by name), add:

```rust
/// Structured marker the planner attaches to a plan when self-declaring
/// a constitutional refusal. Present iff the agent refuses to proceed;
/// drives [`Outcome::Refused`] short-circuit in the inner loop and
/// surfaces verbatim in the `agent/plan.formulate` audit-row payload.
///
/// `principle` is the 1..=5 index from `prompts/agent_planner.md`.
/// `reason` is a short structured tag (lowercase snake_case) — the
/// human-readable explanation lives in `Plan.result.body`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RefusedReason {
    pub principle: u8,
    pub reason:    String,
}
```

In the `pub struct Plan { ... }` definition, after the existing `result` field, add:

```rust
    /// Present iff the agent self-declared a constitutional refusal.
    /// Drives `Outcome::Refused` short-circuit; surfaced in the
    /// `agent/plan.formulate` audit-row payload as the structured
    /// operator-visible signal. Absent on every non-refusal plan.
    ///
    /// When this is `Some`, the planner is also expected to emit
    /// `decision == "task_complete"`, `steps == []`, and an explanation
    /// in `result.body`. The inner loop honours the refusal even when
    /// the planner-shape is malformed (e.g. non-empty `steps`); see
    /// [`super::inner_loop::run_to_terminal`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<RefusedReason>,
```

In the `impl Plan { ... }` block (just below `is_terminal`), add:

```rust
    /// Returns true iff the agent self-declared a constitutional
    /// refusal on this plan. Independent of `is_terminal` — the two
    /// helpers don't conflate; a well-formed refusal is both, but a
    /// malformed refusal-with-steps is `is_refused()` only and is
    /// still honoured by the inner loop.
    pub fn is_refused(&self) -> bool {
        self.refused.is_some()
    }
```

- [ ] **Step 1.3 — Struct + field + helper implemented**

### Step 1.4 — Update the 8 existing struct-literal sites

The new `refused: Option<RefusedReason>` field is required at struct-literal sites (Rust requires every field; `#[serde(default)]` only affects deserialisation). Each site below needs `refused: None,` appended:

1. `core/src/cassandra/types.rs:113` — `plan_is_terminal_requires_all_three_conditions` test; the literal at the top of the test fn. Add `refused: None,` after `data_ceiling: DataClass::Public,`.
2. `core/src/cassandra/types.rs:144` — `plan_serialises_skipping_none_result` test; same shape.
3. `core/src/cassandra/review.rs:122` — `fn dummy_plan() -> Plan { Plan { ... } }`. Same.
4. `core/src/scheduler/inner_loop.rs:383` — `task_context_plans_so_far_summary_is_compact` test, inside the `c.plans.push((...))` call.
5. `core/tests/scheduler_inner_loop_e2e.rs:352` — `task_complete_plan(body: &str) -> Plan` helper.
6. `core/tests/scheduler_inner_loop_e2e.rs:363` — `one_step_plan(tool: &str, method: &str) -> Plan` helper.
7. `core/tests/scheduler_lanes_e2e.rs:345` — `task_complete_plan` helper.
8. `core/tests/scheduler_lanes_e2e.rs:356` — `one_step_plan` helper.

For each, the only edit is appending one line. Example for site 5:

```rust
fn task_complete_plan(body: &str) -> Plan {
    Plan {
        context: "complete".into(),
        decision: "task_complete".into(),
        rationale: "done".into(),
        steps: vec![],
        result: Some(serde_json::json!({"kind": "text", "body": body})),
        data_ceiling: DataClass::Public,
        refused: None,             // <-- ADD THIS LINE
    }
}
```

To find every remaining site (defence against drift), run:

```sh
grep -rn "Plan {" /home/hherb/src/kastellan --include="*.rs" \
  | grep -v ".claude/worktrees" \
  | grep -v "PlanFormulator\|CapturedPlan"
```

Cross-check the output against the 8 sites above; if the engineer finds a 9th, add `refused: None,` there too.

- [ ] **Step 1.4 — All struct-literal sites updated**

### Step 1.5 — Run tests to confirm GREEN

```sh
cargo test -p kastellan-core --lib cassandra::types::tests 2>&1 | tail -15
cargo build --workspace 2>&1 | tail -10
```

Expected: all `cassandra::types::tests` pass; full workspace `cargo build` succeeds with zero errors and zero warnings.

- [ ] **Step 1.5 — GREEN confirmed**

### Step 1.6 — Commit

```sh
git add core/src/cassandra/types.rs core/src/cassandra/review.rs \
        core/src/scheduler/inner_loop.rs \
        core/tests/scheduler_inner_loop_e2e.rs core/tests/scheduler_lanes_e2e.rs
git commit -m "$(cat <<'EOF'
feat(cassandra): add RefusedReason struct + optional Plan.refused field

New struct RefusedReason { principle: u8, reason: String } and a new
optional Plan.refused field with #[serde(default,
skip_serializing_if = "Option::is_none")] so absent values cost nothing
on the wire. New Plan::is_refused() helper (independent of is_terminal,
proven by unit test).

Updates 6 existing struct-literal sites (2 tests + 1 helper + 3
e2e-test fixture helpers + 1 inner_loop unit test fixture) to include
refused: None — required because Plan has no Default impl. The 8th
site is the new test in this commit.

Issue #23, design spec 2026-05-14-constitutional-refusal-state-design.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 1.6 — Committed**

---

## Task 2 — `Outcome::Refused` variant + arms

**Files:**
- Modify: `core/src/scheduler/inner_loop.rs` — enum definition + `final_state` + `result_payload` + existing unit test

### Step 2.1 — Write the two failing tests

In `core/src/scheduler/inner_loop.rs`, inside `#[cfg(test)] mod tests { ... }`:

**Extend the existing `outcome_final_state_mapping` test** (line ~355–362). Append one assertion line just before the closing brace:

```rust
    assert_eq!(
        Outcome::Refused { principle: 1, reason: "harm".into(), body: "explanation".into() }
            .final_state(),
        "refused",
    );
```

**Add a new test** below it:

```rust
#[test]
fn outcome_refused_result_payload_carries_principle_reason_and_body() {
    let o = Outcome::Refused {
        principle: 2,
        reason: "fraud_or_impersonation".into(),
        body: "Signing under your identity would impersonate you.".into(),
    };
    let p = o.result_payload().unwrap();
    assert_eq!(p["kind"], "refused");
    assert_eq!(p["principle"], 2);
    assert_eq!(p["reason"], "fraud_or_impersonation");
    assert_eq!(p["body"], "Signing under your identity would impersonate you.");

    // Exact key set — guards against accidental payload bloat.
    let keys: std::collections::BTreeSet<String> = p.as_object().unwrap()
        .keys().cloned().collect();
    let expected: std::collections::BTreeSet<String> =
        ["kind", "principle", "reason", "body"].iter().map(|s| s.to_string()).collect();
    assert_eq!(keys, expected);
}
```

- [ ] **Step 2.1 — Tests added**

### Step 2.2 — Run tests to confirm RED

```sh
cargo test -p kastellan-core --lib scheduler::inner_loop::tests::outcome_refused 2>&1 | tail -15
```

Expected: compile error `no variant 'Refused' on enum 'Outcome'`.

- [ ] **Step 2.2 — RED confirmed**

### Step 2.3 — Implement the new variant + arms

In `core/src/scheduler/inner_loop.rs`, modify the `Outcome` enum (lines ~80–86). Add the new variant after `Blocked`:

```rust
#[derive(Clone, Debug)]
pub enum Outcome {
    Completed(serde_json::Value),
    Failed(String),
    Cancelled,
    TimedOut,
    Blocked { principle: u8, reason: String },
    /// Agent self-declared a constitutional refusal. Sourced from
    /// `plan.refused` in the inner loop. Distinct from `Blocked`
    /// (which is the reviewer-detected `Verdict::ConstitutionalBlock`
    /// path). `body` carries the planner's prose `result.body` so the
    /// user-facing explanation is preserved in the audit + DB result.
    Refused { principle: u8, reason: String, body: String },
}
```

In `impl Outcome::final_state` (line ~89–97), add an arm:

```rust
            Outcome::Refused { .. } => "refused",
```

In `impl Outcome::result_payload` (line ~99–107), add an arm before the `_ =>` fallback:

```rust
            Outcome::Refused { principle, reason, body } => Some(serde_json::json!({
                "kind": "refused",
                "principle": principle,
                "reason": reason,
                "body": body,
            })),
```

- [ ] **Step 2.3 — Variant + arms added**

### Step 2.4 — Run tests to confirm GREEN

```sh
cargo test -p kastellan-core --lib scheduler::inner_loop::tests 2>&1 | tail -15
cargo build --workspace 2>&1 | tail -10
```

Expected: every `scheduler::inner_loop::tests::*` test passes; `cargo build --workspace` clean with zero warnings.

- [ ] **Step 2.4 — GREEN confirmed**

### Step 2.5 — Commit

```sh
git add core/src/scheduler/inner_loop.rs
git commit -m "$(cat <<'EOF'
feat(scheduler): add Outcome::Refused variant + final_state + payload arms

New Outcome::Refused { principle: u8, reason: String, body: String }
distinct from Outcome::Blocked (reviewer-detected). final_state maps to
"refused"; result_payload emits the 4-key JSON shape
{kind, principle, reason, body} matching the spec's audit/result
contract. body preserves the planner's prose explanation verbatim.

The inner-loop short-circuit that produces this variant lands in a
following commit; this commit ships only the type widening + tests so
the workspace stays green between steps.

Issue #23.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 2.5 — Committed**

---

## Task 3 — DB migration 0012 + CHECK-constraint integration test

**Files:**
- Create: `db/migrations/0012_tasks_state_refused.sql`
- Modify: `db/tests/postgres_e2e.rs` — new test `tasks_state_refused_passes_check_constraint`

### Step 3.1 — Write the failing integration test

Append a new `#[tokio::test(flavor = "multi_thread", worker_threads = 4)]` function to `db/tests/postgres_e2e.rs`. Read the existing test bodies (`runtime_role_audit_log_revoke_is_enforced` is a good template) to copy the per-test PG cluster bring-up, skip-on-missing-PG pattern, and `connect_runtime_pool` shape.

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tasks_state_refused_passes_check_constraint() {
    let Some(bin_dir) = kastellan_tests_common::pg_bin_dir_or_skip() else { return; };
    let cluster = kastellan_tests_common::bring_up_pg_cluster(&bin_dir).await;

    let pool = kastellan_db::pool::connect_runtime_pool(&cluster.spec)
        .await.expect("runtime pool");

    // Seed a pending task we can flip to 'refused' via raw SQL.
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO tasks (lane, state, payload) \
         VALUES ('fast'::tasks_lane, 'pending', '{}'::jsonb) RETURNING id",
    )
        .fetch_one(&pool)
        .await
        .expect("seed pending task");

    // Positive: 'refused' is accepted by the widened CHECK constraint.
    let ok = sqlx::query(
        "UPDATE tasks SET state = 'refused', finished_at = now() WHERE id = $1",
    )
        .bind(id)
        .execute(&pool)
        .await;
    assert!(ok.is_ok(), "UPDATE to state='refused' should succeed; got {ok:?}");

    // Verify the row landed.
    let final_state: String = sqlx::query_scalar(
        "SELECT state::text FROM tasks WHERE id = $1",
    )
        .bind(id)
        .fetch_one(&pool)
        .await
        .expect("read back state");
    assert_eq!(final_state, "refused");

    // Negative: bogus state still rejected by the constraint.
    let err = sqlx::query(
        "UPDATE tasks SET state = 'garbage' WHERE id = $1",
    )
        .bind(id)
        .execute(&pool)
        .await;
    assert!(err.is_err(), "UPDATE to state='garbage' must be rejected by CHECK; got {err:?}");

    pool.close().await;
    cluster.shutdown().await;
}
```

Adjust `tasks_lane` cast or column names to match the existing schema; cross-reference `db/migrations/0001_init.sql` and `db/migrations/0005_tasks_scheduler.sql` for the exact column types if the bind shape errors at compile time.

- [ ] **Step 3.1 — Test added**

### Step 3.2 — Run test to confirm RED

```sh
cargo test -p kastellan-db --test postgres_e2e tasks_state_refused_passes_check_constraint -- --nocapture 2>&1 | tail -30
```

Expected: fail with a Postgres error along the lines of `new row for relation "tasks" violates check constraint "tasks_state_check"`. (If the host has no PG installed, the test will `[SKIP]` — in that case the engineer needs to install Postgres locally before continuing; per `CLAUDE.md` rule #6 all tests must pass before commit. See `scripts/linux/install-postgres.sh` for Linux.)

- [ ] **Step 3.2 — RED confirmed (or skip-as-pass on hosts without PG)**

### Step 3.3 — Write migration `0012_tasks_state_refused.sql`

Create `db/migrations/0012_tasks_state_refused.sql`:

```sql
-- 0012_tasks_state_refused.sql
-- Adds 'refused' as a valid terminal value of `tasks.state` so the
-- scheduler can record an agent self-declared constitutional refusal
-- distinct from the reviewer-detected 'blocked' state.
--
-- Two CREATE OR REPLACE operations:
--   1. The CHECK constraint `tasks_state_check` (from
--      0005_tasks_scheduler.sql) gets dropped and recreated with
--      'refused' appended to the IN list.
--   2. The `notify_task_completed` trigger function (from
--      0005_tasks_scheduler.sql, line ~76-88) enumerates the same
--      terminal set in two IN clauses; the function body is replaced
--      with 'refused' appended to both.
--
-- `tasks.finished_at` is set by application-level UPDATEs in
-- `db::tasks::{finalize, mark_cancelled, sweep_crashed}` rather than
-- by a trigger, so no trigger-side `finished_at` widening is needed.

ALTER TABLE tasks DROP CONSTRAINT tasks_state_check;
ALTER TABLE tasks
    ADD CONSTRAINT tasks_state_check CHECK (state IN
        ('pending','running','completed','failed','cancelled',
         'blocked','timed_out','crashed','refused'));

CREATE OR REPLACE FUNCTION notify_task_completed()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
BEGIN
    IF NEW.state IN ('completed','failed','cancelled','blocked',
                     'timed_out','crashed','refused')
       AND OLD.state NOT IN ('completed','failed','cancelled','blocked',
                             'timed_out','crashed','refused') THEN
        PERFORM pg_notify('tasks_completed', NEW.id::text);
    END IF;
    RETURN NEW;
END;
$$;
```

(No `DROP TRIGGER` / `CREATE TRIGGER` needed — `CREATE OR REPLACE FUNCTION` swaps the body in place; the existing trigger `tasks_notify_completed` already calls `notify_task_completed()` and picks up the new body automatically.)

- [ ] **Step 3.3 — Migration written**

### Step 3.4 — Run test to confirm GREEN

```sh
cargo test -p kastellan-db --test postgres_e2e tasks_state_refused_passes_check_constraint -- --nocapture 2>&1 | tail -15
```

Expected: PASS. (sqlx will pick up the new migration via `MIGRATOR`'s embed-at-compile-time. The per-test PG cluster runs migrations from scratch each time.)

- [ ] **Step 3.4 — GREEN confirmed**

### Step 3.5 — Commit

```sh
git add db/migrations/0012_tasks_state_refused.sql db/tests/postgres_e2e.rs
git commit -m "$(cat <<'EOF'
feat(db): migration 0012 — add 'refused' to tasks.state CHECK + trigger

New terminal state value 'refused' (agent self-declared constitutional
refusal, distinct from the reviewer-detected 'blocked'). Widens the
tasks_state_check CHECK constraint and the notify_task_completed
trigger function's IN clauses (for both NEW.state and OLD.state) so
the trigger fires `tasks_completed` NOTIFY on transitions into and
out of 'refused' just like the other terminal states.

No data migration: no production rows exist. ACCESS EXCLUSIVE briefly.

Pinned by db/tests/postgres_e2e.rs::tasks_state_refused_passes_check_constraint
(positive: UPDATE → 'refused' succeeds; negative: UPDATE → 'garbage'
still rejected).

Issue #23.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 3.5 — Committed**

---

## Task 4 — Inner-loop short-circuit on `plan.refused.is_some()`

**Files:**
- Modify: `core/src/scheduler/inner_loop.rs` — `run_to_terminal` body
- Modify: `core/tests/scheduler_inner_loop_e2e.rs` — two new scenarios

### Step 4.1 — Write the two failing scenarios

In `core/tests/scheduler_inner_loop_e2e.rs`, append two new test functions following the same shape as the existing scenarios (per-test PG cluster, scripted formulator, scripted reviewer, real inner loop).

First, study the existing scenario set near the top of the file (look for `task_complete_plan` callers) so the new tests follow the exact same setup pattern. Then add:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refusal_plan_terminates_with_state_refused() {
    // ... copy the PG bring-up + runtime-role pool + scripted-stage
    //     plumbing from `inner_loop_completes_terminal_plan` (or whichever
    //     existing scenario is the simplest happy-path).

    // Scripted formulator returns one refusal plan: empty steps,
    // decision = task_complete, refused = Some(...), result.body explains.
    let plan = Plan {
        context: "refusing".into(),
        decision: "task_complete".into(),
        rationale: "principle 1 violated".into(),
        steps: vec![],
        result: Some(serde_json::json!({
            "kind": "text",
            "body": "I cannot help with that; it would risk physical harm.",
        })),
        data_ceiling: DataClass::Public,
        refused: Some(kastellan_core::cassandra::types::RefusedReason {
            principle: 1,
            reason: "physical_harm".into(),
        }),
    };
    let formulator = scripted_formulator(vec![plan.clone()]);
    let reviewer = always_approve_reviewer();   // or whatever helper exists
    let dispatcher = no_dispatcher();           // refusal has no steps to dispatch

    let result = run_to_terminal(&pool, formulator, reviewer, dispatcher, ctx).await
        .expect("inner loop returns terminal result");

    // Outcome shape
    match &result.outcome {
        Outcome::Refused { principle, reason, body } => {
            assert_eq!(*principle, 1);
            assert_eq!(reason, "physical_harm");
            assert!(body.contains("physical harm"));
        }
        other => panic!("expected Outcome::Refused, got {other:?}"),
    }

    // Final state in the tasks row (the lane runner is not in scope here;
    // assert via Outcome::final_state since drain_lane is the layer above)
    assert_eq!(result.outcome.final_state(), "refused");

    // Result payload contract
    let payload = result.outcome.result_payload().expect("Refused carries a payload");
    assert_eq!(payload["kind"], "refused");
    assert_eq!(payload["principle"], 1);
    assert_eq!(payload["reason"], "physical_harm");
    assert!(payload["body"].as_str().unwrap().contains("physical harm"));

    // Counters
    assert_eq!(result.plan_count, 1, "single refusal plan");
    assert_eq!(result.dispatch_count, 0, "no steps to dispatch");

    pool.close().await;
    cluster.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reviewer_constitutional_block_wins_over_agent_refusal() {
    // ... PG bring-up + runtime-role pool ...

    // Plan: agent claims principle 1; reviewer independently detects principle 3.
    let plan = Plan {
        context: "refusing-1".into(),
        decision: "task_complete".into(),
        rationale: "agent claims P1 violation".into(),
        steps: vec![],
        result: Some(serde_json::json!({
            "kind": "text",
            "body": "agent prose mentioning P1",
        })),
        data_ceiling: DataClass::Public,
        refused: Some(kastellan_core::cassandra::types::RefusedReason {
            principle: 1,
            reason: "physical_harm_agent_side".into(),
        }),
    };
    let formulator = scripted_formulator(vec![plan.clone()]);
    // Reviewer returns ConstitutionalBlock with a different principle.
    let reviewer = scripted_reviewer_returning(Verdict::ConstitutionalBlock {
        principle: 3,
        reason: "irreversible_action_no_HITL".into(),
    });
    let dispatcher = no_dispatcher();

    let result = run_to_terminal(&pool, formulator, reviewer, dispatcher, ctx).await
        .expect("inner loop returns terminal result");

    // Reviewer wins for the state column.
    match &result.outcome {
        Outcome::Blocked { principle, reason } => {
            assert_eq!(*principle, 3, "reviewer's principle wins");
            assert_eq!(reason, "irreversible_action_no_HITL");
        }
        other => panic!("expected Outcome::Blocked (reviewer wins), got {other:?}"),
    }
    assert_eq!(result.outcome.final_state(), "blocked");
    // Audit log should contain BOTH the agent's refusal context (in the
    // plan.formulate row, asserted in Task 5) AND the reviewer's
    // ConstitutionalBlock verdict (in the cassandra:chain/verdict row).

    pool.close().await;
    cluster.shutdown().await;
}
```

If a helper like `scripted_formulator`, `always_approve_reviewer`, `scripted_reviewer_returning`, or `no_dispatcher` doesn't already exist in the test file, write it locally in the test module — keep it small and focused. Look at the existing scenarios at the top of the file for the exact pattern; the test file is currently the canonical home for these helpers.

- [ ] **Step 4.1 — Tests added**

### Step 4.2 — Run tests to confirm RED

```sh
cargo test -p kastellan-core --test scheduler_inner_loop_e2e \
    refusal_plan_terminates_with_state_refused \
    -- --nocapture 2>&1 | tail -30

cargo test -p kastellan-core --test scheduler_inner_loop_e2e \
    reviewer_constitutional_block_wins_over_agent_refusal \
    -- --nocapture 2>&1 | tail -30
```

Expected:
- First test FAILS because `run_to_terminal` currently returns `Outcome::Completed(result)` on the refusal plan (no short-circuit yet); the `match &result.outcome { Outcome::Refused { .. } => …, other => panic!(…) }` block trips the panic arm.
- Second test PASSES *almost by accident* — the existing CB short-circuit at line 204–205 already returns `Outcome::Blocked` before the refusal short-circuit would fire. Confirm it does pass; if it doesn't, investigate before continuing.

- [ ] **Step 4.2 — RED confirmed for scenario 5; scenario 6 passes**

### Step 4.3 — Implement the inner-loop short-circuit

In `core/src/scheduler/inner_loop.rs`, modify `run_to_terminal`. Between the existing ConstitutionalBlock match arm (line ~203–205) and the rest of the match block, insert the new step-4 short-circuit. The full body around the verdict match should look like:

```rust
        let verdict_start = std::time::Instant::now();
        let verdict = review.review(&plan, &rctx).await;
        write_audit_verdict(pool, &ctx, &verdict, verdict_start.elapsed().as_millis() as u64).await?;

        // Step 3 (existing): reviewer's ConstitutionalBlock wins — even
        // over an agent's `plan.refused` marker. Reviewer's independent
        // detection takes precedence for the state column; the audit
        // log preserves both signals (the plan.formulate row carries
        // `refused`, the verdict row carries the reviewer's CB).
        if let Verdict::ConstitutionalBlock { principle, reason } = &verdict {
            return finish!(Outcome::Blocked {
                principle: *principle,
                reason: reason.clone(),
            });
        }

        // Step 4 (NEW): agent self-declared a constitutional refusal.
        // Reviewer's non-CB verdict (Approve / Advisory / Block /
        // Escalate) does NOT override; refusal is terminal. The
        // verdict row is still audit-logged above. Steps (if any) are
        // dropped — refusal is the stronger signal and step execution
        // is unsafe under a self-declared violation.
        if let Some(refused) = plan.refused.clone() {
            let body = plan.result.as_ref()
                .and_then(|v| v.get("body"))
                .and_then(|b| b.as_str())
                .map(String::from)
                .unwrap_or_default();
            return finish!(Outcome::Refused {
                principle: refused.principle,
                reason: refused.reason,
                body,
            });
        }

        // Step 5 (existing): remaining verdicts handled the same as
        // before — only the ConstitutionalBlock arm above is unreachable
        // here, so the match is no longer exhaustive on the original
        // enum. Either drop ConstitutionalBlock from the match (handled
        // above) or include an unreachable!() arm. Prefer dropping:
        match verdict {
            Verdict::Block(reason) => {
                ctx.blocks.push(reason.clone());
                continue;
            }
            Verdict::Escalate(reason, sev) => {
                tracing::warn!(
                    task_id = ctx.task_id,
                    plan_count = ctx.plan_count,
                    severity = ?sev,
                    reason = %reason,
                    "Verdict::Escalate degraded to Block (channel-bus not wired)"
                );
                ctx.blocks.push(format!("escalate(no-channel): {reason}"));
                continue;
            }
            Verdict::Advisory(c) => {
                ctx.advisories.push(c.clone());
                // proceed
            }
            Verdict::Approve => { /* proceed */ }
            Verdict::ConstitutionalBlock { .. } => unreachable!(
                "handled by the if-let above"
            ),
        }
```

Note: the existing match starts at `match &verdict { ... }`. After this refactor, the match still needs all five `Verdict` variants covered — the simplest path is to keep the original `match &verdict` *unchanged* and only insert the new `if let Some(refused) = plan.refused.clone()` block between the `write_audit_verdict` call and the existing match. The match's `Verdict::ConstitutionalBlock` arm is still reachable (the if-let just above doesn't consume `verdict`, only inspects it). Apply whichever shape compiles cleanly and yields the precedence table:

| Verdict CB? | `plan.refused.is_some()` | Resulting Outcome |
| ----------- | ------------------------ | ----------------- |
| Yes         | any                      | `Blocked`         |
| No          | Yes                      | `Refused`         |
| No          | No, plan terminal        | `Completed`       |
| No          | No, plan with steps      | execute (continue) |

- [ ] **Step 4.3 — Short-circuit implemented**

### Step 4.4 — Run both scenarios + the full focused suite to confirm GREEN

```sh
cargo test -p kastellan-core --test scheduler_inner_loop_e2e -- --nocapture 2>&1 | tail -30
```

Expected: all scenarios green, including the two new ones. If `reviewer_constitutional_block_wins_over_agent_refusal` flips to RED at this point, the refusal short-circuit was placed before the CB short-circuit — re-order so CB is checked first.

- [ ] **Step 4.4 — Full focused suite GREEN**

### Step 4.5 — Run the full workspace as a regression check

```sh
cargo test --workspace 2>&1 | tail -20
```

Expected: zero failures, zero warnings, zero `[SKIP]` lines on Linux.

- [ ] **Step 4.5 — Workspace GREEN**

### Step 4.6 — Commit

```sh
git add core/src/scheduler/inner_loop.rs core/tests/scheduler_inner_loop_e2e.rs
git commit -m "$(cat <<'EOF'
feat(scheduler): refused-plan short-circuit in run_to_terminal

When the planner emits plan.refused.is_some() the inner loop now
returns Outcome::Refused after the reviewer has run (defense in
depth). The reviewer's ConstitutionalBlock still wins over the agent's
refusal — same provenance contract spec'd in
docs/superpowers/specs/2026-05-14-constitutional-refusal-state-design.md.

Non-CB reviewer verdicts (Approve/Advisory/Block/Escalate) on a refusal
plan do not override the refusal: the verdict row is audit-logged but
the loop returns Refused, not 'continue' (which would loop indefinitely
on an agent that keeps refusing).

Two new e2e scenarios in scheduler_inner_loop_e2e.rs:
  - refusal_plan_terminates_with_state_refused
  - reviewer_constitutional_block_wins_over_agent_refusal

body for Outcome::Refused is extracted from plan.result.body (or empty
string if absent) so the user-facing prose explanation is preserved.

Issue #23.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 4.6 — Committed**

---

## Task 5 — Audit-row payload extension (`refused` + `decision_kind = "refused"`)

**Files:**
- Modify: `core/src/scheduler/inner_loop.rs::write_audit_plan_formulate` — payload widening
- Modify: `core/tests/scheduler_inner_loop_e2e.rs::refusal_plan_terminates_with_state_refused` — add audit-row assertion block

### Step 5.1 — Extend the existing refusal scenario with audit-row assertions

In `core/tests/scheduler_inner_loop_e2e.rs::refusal_plan_terminates_with_state_refused`, after the existing assertion block, add:

```rust
    // Audit-row contract for refusals.
    //
    // Exactly one agent/plan.formulate row, with:
    //   - decision_kind == "refused"
    //   - refused == { principle: 1, reason: "physical_harm" }
    //   - plan_step_count == 0
    let rows = kastellan_db::audit::fetch_since(&pool, 0).await.expect("fetch audit");
    let plan_rows: Vec<_> = rows.iter()
        .filter(|r| r.actor == "agent" && r.action == "plan.formulate")
        .collect();
    assert_eq!(plan_rows.len(), 1, "expected exactly 1 agent/plan.formulate row");
    let payload = &plan_rows[0].payload;
    assert_eq!(payload["decision_kind"], "refused",
        "decision_kind must be 'refused' when plan.refused.is_some()");
    assert_eq!(payload["refused"]["principle"], 1);
    assert_eq!(payload["refused"]["reason"], "physical_harm");
    assert_eq!(payload["plan_step_count"], 0);
```

The exact `kastellan_db::audit::fetch_since` signature may differ (check `db/src/audit.rs` for the right helper); the goal is a SELECT-all of audit rows for this test's task. Pre-existing test scenarios in this file may already have an audit-row helper — reuse it if so.

- [ ] **Step 5.1 — Assertions added**

### Step 5.2 — Run test to confirm RED

```sh
cargo test -p kastellan-core --test scheduler_inner_loop_e2e refusal_plan_terminates_with_state_refused -- --nocapture 2>&1 | tail -30
```

Expected: FAIL. The `decision_kind` field today is `"task_complete"` for a terminal-shape plan, regardless of `refused`. The `refused` key is absent from the payload entirely.

- [ ] **Step 5.2 — RED confirmed**

### Step 5.3 — Extend `write_audit_plan_formulate`

In `core/src/scheduler/inner_loop.rs::write_audit_plan_formulate` (around line ~271–290), modify the payload to include `refused` and re-derive `decision_kind`:

```rust
async fn write_audit_plan_formulate(
    pool: &PgPool,
    ctx: &TaskContext,
    plan: &Plan,
    meta: &FormulationMeta,
) -> Result<(), InnerLoopError> {
    let decision_kind = if plan.is_refused() {
        // Highest-priority discriminator; set regardless of malformed
        // refusal-with-steps shape so observation-phase SQL on
        // `decision_kind = 'refused'` returns the full refusal population.
        "refused"
    } else if plan.is_terminal() {
        crate::cassandra::types::DECISION_TERMINAL
    } else {
        "act"
    };

    // `refused` is the structured operator marker. JSON null when the
    // plan does not carry a refusal — explicit null is wire-distinguishable
    // from "key absent" for downstream JSONB queries.
    let refused = plan.refused.as_ref()
        .map(|r| serde_json::json!({ "principle": r.principle, "reason": r.reason }))
        .unwrap_or(serde_json::Value::Null);

    let payload = serde_json::json!({
        "task_id":          ctx.task_id,
        "plan_count":       ctx.plan_count,
        "prompt_name":      meta.prompt_name,
        "prompt_sha256":    meta.prompt_sha256,
        "llm_model":        meta.llm_model,
        "llm_backend":      meta.llm_backend,
        "latency_ms":       meta.latency_ms,
        "retry_count":      meta.retry_count,
        "plan_step_count":  plan.steps.len(),
        "decision_kind":    decision_kind,
        "refused":          refused,
    });
    kastellan_db::audit::insert(pool, "agent", "plan.formulate", payload).await?;
    Ok(())
}
```

- [ ] **Step 5.3 — write_audit_plan_formulate extended**

### Step 5.4 — Run tests to confirm GREEN

```sh
cargo test -p kastellan-core --test scheduler_inner_loop_e2e -- --nocapture 2>&1 | tail -30
cargo test --workspace 2>&1 | tail -20
```

Expected: refusal scenario passes including the audit-row block; full workspace zero failures, zero warnings. The existing `cli_ask_e2e` multiset assertions check for `agent/plan.formulate` *counts* (not the payload's `decision_kind` field), so they should still pass with the widened payload.

- [ ] **Step 5.4 — Workspace GREEN**

### Step 5.5 — Commit

```sh
git add core/src/scheduler/inner_loop.rs core/tests/scheduler_inner_loop_e2e.rs
git commit -m "$(cat <<'EOF'
feat(audit): agent/plan.formulate carries refused + decision_kind='refused'

When plan.refused.is_some(), the existing agent/plan.formulate audit
row gains:
  - decision_kind = "refused" (third value alongside "task_complete"
    and "act"; takes precedence over is_terminal-derived 'task_complete'
    so a malformed refusal-with-steps still surfaces correctly)
  - refused = { principle: u8, reason: String }  (JSONB; explicit JSON
    null when the plan does not carry a refusal — wire-distinguishable
    from key-absent for downstream JSONB queries)

No new audit-row family. Same row count as before; only the payload
shape widens. Observation-phase queries:
  SELECT * FROM audit_log
  WHERE actor = 'agent' AND action = 'plan.formulate'
    AND payload->>'decision_kind' = 'refused';

Pinned by the audit-row assertion block in
refusal_plan_terminates_with_state_refused.

Issue #23.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 5.5 — Committed**

---

## Task 6 — Planner-prompt update

**Files:**
- Modify: `prompts/agent_planner.md`

### Step 6.1 — Edit the JSON-schema example

In `prompts/agent_planner.md` around line 37–55 (the JSON-schema example for a plan), add `"refused": null,` immediately after the `"result"` line. The example should read:

```jsonc
{
    "context":   "<one to three sentences describing the situation>",
    "decision":  "<one sentence stating what you will do, OR \"task_complete\">",
    "rationale": "<why this approach, and not alternatives>",
    "steps": [
        {
            "tool":           "<tool name>",
            "method":         "<JSON-RPC method on that tool>",
            "parameters":     { /* the arguments */ },
            "returns":        "<what this step will produce>",
            "done_when":      "<observable success criterion>",
            "classification": "<Public | Personal | ClinicalConfidential | Secret>"
        }
    ],
    "result":      null,
    "refused":     null,        // populated ONLY on constitutional refusal; see §"Constitutional Principles"
    "data_ceiling": "<Public | Personal | ClinicalConfidential | Secret>"
}
```

The trailing `//` comment in the example block must be omitted on the actual JSON line (JSON does not support comments) — keep the comment as a separate prose paragraph following the schema:

> The `refused` field is normally `null`. Populate it only on constitutional refusal (see §"Constitutional Principles" below).

- [ ] **Step 6.1 — JSON-schema example updated**

### Step 6.2 — Add the new sentence to the constitutional-refusal paragraph

In `prompts/agent_planner.md` lines 107–112, the existing paragraph reads:

> If a user instruction would require violating a principle, do not proceed with the requested action. Instead, emit a terminal plan where `decision` is exactly `"task_complete"`, `steps` is `[]`, and `result.body` explains which principle would be violated and why. The `decision` field must remain literally `"task_complete"` — name the violated principle in the `result` body, not in `decision`.

Append one sentence:

> Also emit a top-level `refused` object with `{ "principle": <1..5>, "reason": "<short structured tag, lowercase snake_case>" }`. The `result.body` remains the prose explanation for the user; the `refused` object is the structured signal operators query.

- [ ] **Step 6.2 — Refusal paragraph extended**

### Step 6.3 — Confirm no test regressions

```sh
cargo test --workspace 2>&1 | tail -20
```

Expected: PASS (no automated test pins prompt-text shape; the change is content-only and the `agent_prompts` SHA-256 ledger will record the new hash on next daemon start automatically).

- [ ] **Step 6.3 — Workspace GREEN**

### Step 6.4 — Commit

```sh
git add prompts/agent_planner.md
git commit -m "$(cat <<'EOF'
docs(prompt): planner emits structured `refused` object on refusal

Adds one sentence to the constitutional-refusal paragraph instructing
the planner to emit a top-level `refused: { principle, reason }`
object whenever it self-declares a refusal. The `result.body` prose
remains the user-facing explanation; the `refused` object is the
structured operator-visible signal that the inner loop maps to
Outcome::Refused / tasks.state='refused' / agent/plan.formulate
audit-row payload `refused` field.

JSON-schema example updated with `"refused": null` as the explicit
default + prose note that the field is populated only on refusal.

The agent_prompts SHA-256 ledger captures the edited prompt as a
fresh row on next daemon start (migration 0011 composite-PK shape).
No code-level prompt-text test pin exists; correctness validated by
observation-phase re-capture (operator action).

Issue #23.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 6.4 — Committed**

---

## Task 7 — HANDOVER + ROADMAP refresh at session end

**Files:**
- Modify: `docs/devel/handovers/HANDOVER.md` — move "this session's working branch" entry to "Recently completed (this session)", refresh test count, update "Next TODO (pick one)" to mark issue #23 closed
- Modify: `docs/devel/ROADMAP.md` — tick issue #23 entry under Phase 1 (or under "Open follow-up issues" cross-reference)

### Step 7.1 — Final workspace test count

Capture the post-implementation count for the HANDOVER entry:

```sh
cargo test --workspace 2>&1 | grep -E "^test result:" \
  | awk '{p+=$4; f+=$6; i+=$8} END {printf "passed=%d failed=%d ignored=%d\n", p, f, i}'
```

Expected delta from baseline 446: +9-12 new `#[test]` functions (3 in types::tests + 1 extended outcome_final_state + 1 new outcome_refused_result_payload + 1 db CHECK + 2 inner-loop e2e + 1 audit-row assertion-extension; some are extensions of existing test fns rather than new functions). Note the exact final count for HANDOVER.

- [ ] **Step 7.1 — Test count captured**

### Step 7.2 — Update HANDOVER

In `docs/devel/handovers/HANDOVER.md`:

1. Bump the `**Last updated:**` line to reflect today's date + "issue #23 (constitutional refusal state) shipped on `feat/refusal-state`".
2. Bump `**Last commit (main):**` to whatever main is at the moment of update (use `git log -1 --format='%h (%s)' origin/main`).
3. Replace `**This session's working branch:** feat/refusal-state (off main at 5f543d2). Ships a design spec only…` with a `**Previous session's working branch:**` line describing what shipped (a 6-7-commit slice on the same branch; spec → tests → types → outcome → migration → loop → audit → prompt) and how it tested.
4. Move the existing "Design summary" paragraph from the header into a new "Recently completed (this session, 2026-05-14 — constitutional refusal state, branch `feat/refusal-state`)" section, fleshed out per the HANDOVER convention (Why this slice now / Shape / Audit-row contract / TDD ordering / Test count delta / Files touched).
5. In "Next TODO (pick one)" / "Open follow-up issues", strike issue #23 with a `~~…~~` strikethrough and a `closed 2026-05-14 by this session` note + the branch name.
6. Refresh the test-count claim in the "What's green right now" section if it changed.

- [ ] **Step 7.2 — HANDOVER updated**

### Step 7.3 — Update ROADMAP

In `docs/devel/ROADMAP.md`:

- If a Phase-1 line item for issue #23 exists (search for "constitutional refusal" or "#23"), flip it from `[ ]` to `[x]` and append the merge commit hash (use a placeholder commit hash that gets replaced after the PR merges) + the branch name + a one-line summary.
- If no line item exists, append a new `[x]` line under the **`## Phase 1 — Memory & Loop`** section, slot at the appropriate position (e.g., right after the "Real `ConstitutionalGuard` + `DeterministicPolicy`" placeholder line since this work makes the refusal state available to those future stages):

```markdown
- [x] **[follow-up] Distinguish constitutional refusal from successful completion in `tasks.state`** — landed 2026-05-14 on branch `feat/refusal-state`. New optional `Plan.refused: { principle, reason }` field, new `Outcome::Refused { principle, reason, body }` variant, new terminal `tasks.state='refused'` distinct from existing `'blocked'` (reviewer-detected). Inner loop short-circuits on `plan.refused.is_some()` after the reviewer runs (defense in depth — `Verdict::ConstitutionalBlock` wins over agent self-refusal so provenance is preserved). `agent/plan.formulate` audit-row payload gains `refused: {...}` + a third `decision_kind = "refused"` value. Migration `0012_tasks_state_refused.sql` widens both the `tasks_state_check` CHECK and the `notify_task_completed` trigger. Planner prompt adds one sentence + `"refused": null` default in the JSON-schema example. Closes [issue #23](https://github.com/hherb/kastellan/issues/23). Test count: 446 → <final>.
```

- [ ] **Step 7.3 — ROADMAP updated**

### Step 7.4 — Final commit

```sh
git add docs/devel/handovers/HANDOVER.md docs/devel/ROADMAP.md
git commit -m "$(cat <<'EOF'
docs(handover,roadmap): issue #23 shipped — constitutional refusal state

Closes the design + implementation half of issue #23. New optional
Plan.refused field, new Outcome::Refused variant, new tasks.state=
'refused' distinct from reviewer-detected 'blocked'. Six task commits
on this branch + this docs commit.

HANDOVER bumped to reflect the new test count and the closed pickup;
ROADMAP gains a new Phase-1 [x] entry under the CASSANDRA cluster of
follow-ups. Open follow-up issues list strikes #23.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 7.4 — Committed**

### Step 7.5 — Push and open a PR (operator-driven)

The branch `feat/refusal-state` is ready for review. Operator decides whether to push + open a PR via `gh pr create` or hold local for further work. The implementation plan considers Task 7 the terminal state — push/PR is an operator-authorised action and is not part of this plan's checklist.

- [ ] **Step 7.5 — Branch ready for operator review**

---

## Deliberately NOT in scope (re-stated from the spec)

- **Real `ConstitutionalGuard` reviewer rules.** Wait on observation-phase dataset.
- **CLI-side "show refusals" surface.** `kastellan-cli tasks list --state refused` works for free.
- **Channel-bus refusal notifications.** No channel-bus exists.
- **Retroactive migration of older rows.** No `state='completed'` row is currently a constitutional refusal.
- **`Plan::refused` value validation (`principle ∈ 1..=5`).** Field-shape validation could land later; for now the value is operator-visible in the audit log.
