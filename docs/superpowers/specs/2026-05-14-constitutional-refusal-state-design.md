# Design: distinguish constitutional refusal from completion in `tasks.state`

**Date:** 2026-05-14
**Issue:** [#23](https://github.com/hherb/hhagent/issues/23) — _scheduler: constitutional refusals are recorded as state='completed', not 'blocked'_
**Status:** Approved (spec). Implementation plan to follow.

---

## Problem

[`prompts/agent_planner.md:107-112`](../../../prompts/agent_planner.md#L107-L112) instructs the planner that, when a user instruction would violate one of the five constitutional principles, it must emit a *terminal* plan:

```
decision: "task_complete"
steps:    []
result:   { "kind": "text", "body": "<explanation of which principle and why>" }
```

The inner loop ([core/src/scheduler/inner_loop.rs:240-244](../../../core/src/scheduler/inner_loop.rs#L240-L244)) sees `plan.is_terminal() == true` and returns `Outcome::Completed(result)`. The lane runner writes `tasks.state = 'completed'`.

A constitutional refusal is therefore wire-indistinguishable from a successful task completion in the `tasks` table — same `state`, same `result.kind = "text"`. Operators wanting to count refusals or surface them in a UI must prose-pattern-match `result.body`.

The reviewer-side `Verdict::ConstitutionalBlock` path *does* map to `Outcome::Blocked` / `tasks.state = 'blocked'` (inner_loop.rs:204-205). The asymmetry is between **agent self-refusal** (collapses into `'completed'`) and **reviewer-detected block** (gets `'blocked'`).

## Goal

Make agent self-refusal distinguishable from successful completion in the `tasks` table itself, without prose pattern-matching. Preserve provenance (agent self-refusal vs reviewer-detected block) so operators can break refusals down by source.

## Design

### 1. Types and schema

**`core/src/cassandra/types.rs`** — new struct and one optional field on `Plan`:

```rust
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RefusedReason {
    pub principle: u8,    // 1..=5
    pub reason:    String,
}

pub struct Plan {
    pub context: String,
    pub decision: String,
    pub rationale: String,
    pub steps: Vec<PlannedStep>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    pub data_ceiling: DataClass,

    /// Present iff the agent self-declared a constitutional refusal.
    /// Drives `Outcome::Refused` short-circuit in the inner loop;
    /// surfaced verbatim in the `agent/plan.formulate` audit-row
    /// payload as the structured operator-visible signal. Absent on
    /// every non-refusal plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<RefusedReason>,
}
```

`Plan::is_terminal()` is unchanged. A new pure helper `Plan::is_refused() -> bool` returns `self.refused.is_some()` — readability sugar at call sites; the field itself remains the source of truth.

**`core/src/scheduler/inner_loop.rs`** — new `Outcome` variant:

```rust
pub enum Outcome {
    Completed(serde_json::Value),
    Failed(String),
    Cancelled,
    TimedOut,
    Blocked { principle: u8, reason: String },
    Refused { principle: u8, reason: String, body: String },   // NEW
}

impl Outcome {
    pub fn final_state(&self) -> &'static str {
        match self {
            // ... existing arms ...
            Outcome::Refused { .. } => "refused",
        }
    }

    pub fn result_payload(&self) -> Option<serde_json::Value> {
        match self {
            // ... existing arms ...
            Outcome::Refused { principle, reason, body } => Some(serde_json::json!({
                "kind": "refused",
                "principle": principle,
                "reason": reason,
                "body": body,
            })),
        }
    }
}
```

`body` is the planner's `result.body` prose preserved verbatim — the user-facing explanation. `principle` + `reason` are the structured operator signals.

### 2. Inner-loop ordering

The per-iteration body in [core/src/scheduler/inner_loop.rs:200-244](../../../core/src/scheduler/inner_loop.rs#L200-L244) becomes, in order:

1. `verdict = review.review(&plan, &rctx).await` — unchanged. **Reviewer always runs**, even on refusal plans (defense in depth).
2. `write_audit_verdict(...)` — unchanged.
3. **`Verdict::ConstitutionalBlock`** → return `Outcome::Blocked` — unchanged. Reviewer's independent detection takes precedence even when the agent also refused; provenance is recorded in the verdict audit row + the `agent/plan.formulate` row's `refused` field.
4. **NEW: if `plan.refused.is_some()`** → return `Outcome::Refused { principle, reason, body }`. Extract `body` from `plan.result` by reading `result.body` as a string if present (`plan.result.as_ref().and_then(|v| v.get("body")).and_then(|b| b.as_str()).map(String::from).unwrap_or_default()`); if the planner emitted no `result` or no `body` field, `body` is the empty string. Skips the existing Block/Advisory/Escalate matching, the terminal check, and step execution. The reviewer's non-CB verdict was already audit-logged above.
5. Match remaining verdicts: `Block`/`Escalate` → `continue` (existing); `Advisory` → push + proceed (existing); `Approve` → proceed (existing).
6. `if plan.is_terminal()` → return `Outcome::Completed(result)`. (Existing.)
7. Execute steps. (Existing.)

#### Malformed refusal: `refused.is_some()` AND `!is_terminal()`

If the planner emits a refusal marker but also non-empty `steps` (planner bug or LLM error), the new step-4 short-circuit still fires. The steps are silently dropped — the agent's refusal is the stronger signal. The `agent/plan.formulate` audit row records the malformed shape (both `refused` and `plan_step_count > 0` are visible) so the planner-prompt regression can be diagnosed. Rationale: dropping declared-refused steps is safer than executing them.

#### Precedence table

| Reviewer verdict          | `plan.refused.is_some()` | Outcome                                          |
| ------------------------- | ------------------------ | ------------------------------------------------ |
| `ConstitutionalBlock`     | any                      | `Outcome::Blocked` (reviewer's principle wins)   |
| `Block` / `Escalate`      | true                     | `Outcome::Refused`                               |
| `Block` / `Escalate`      | false                    | `continue` (existing retry)                      |
| `Advisory` / `Approve`    | true                     | `Outcome::Refused`                               |
| `Advisory` / `Approve`    | false, plan terminal     | `Outcome::Completed`                             |
| `Advisory` / `Approve`    | false, plan with steps   | execute (existing)                               |

### 3. Audit log and DB

#### Audit-row changes

`agent/plan.formulate` payload gains two fields:

- `refused: { principle: u8, reason: String } | null` — the structured marker, mirrors the new field on `Plan`.
- `decision_kind` (existing field at inner_loop.rs:287) gains a third value: `"task_complete" | "act" | "refused"`. Set to `"refused"` whenever `plan.refused.is_some()`, regardless of malformed-shape edge case. Coarse SQL filter: `WHERE payload->>'decision_kind' = 'refused'`.

No new audit-row family is introduced. The `cassandra:chain/verdict` row continues to record what the reviewer said.

#### Lifecycle and finalize rows

`scheduler/task.refused` lifecycle and `scheduler/task.finalize` summary rows ride on the existing per-task machinery in [core/src/scheduler/runner.rs:189-208](../../../core/src/scheduler/runner.rs#L189-L208). The lifecycle action is derived from `Outcome::final_state()` via `action_task_terminal()` ([core/src/scheduler/audit.rs](../../../core/src/scheduler/audit.rs)), so as soon as `Outcome::Refused.final_state() == "refused"` lands the action string `"task.refused"` is emitted automatically — no further change to the lifecycle-row code.

#### DB migration `0012_tasks_state_refused.sql`

The existing `tasks.state` CHECK constraint (defined in `db/migrations/0005_tasks_scheduler.sql`) lists the allowed terminal values. The migration drops and re-adds the constraint with `'refused'` included:

```sql
ALTER TABLE tasks DROP CONSTRAINT tasks_state_check;
ALTER TABLE tasks ADD CONSTRAINT tasks_state_check
    CHECK (state IN (
        'pending', 'running',
        'completed', 'failed', 'cancelled', 'blocked',
        'timed_out', 'crashed', 'refused'
    ));
```

The migration must also widen the `finished_at`-setting trigger function at [db/migrations/0005_tasks_scheduler.sql:82-83](../../../db/migrations/0005_tasks_scheduler.sql#L82-L83), whose body enumerates the same terminal-state set as the CHECK constraint. The trigger is `CREATE OR REPLACE FUNCTION`-able, so the migration drops in a refreshed function body with `'refused'` appended.

Brief `ACCESS EXCLUSIVE` lock; acceptable because `tasks` is small and there are no production rows. No data migration needed.

#### Terminal-state caller audit

`db::tasks::mark_failed_running` and any other helper that switches on a closed set of `tasks.state` strings gets a once-over to confirm `'refused'` is handled as terminal-don't-retry, same as `'blocked'`. Sites identified by `grep -rn 'state =' db/src/tasks.rs core/src/`.

### 4. Planner-prompt update

[`prompts/agent_planner.md:107-112`](../../../prompts/agent_planner.md#L107-L112) — the existing paragraph instructing the planner to emit a terminal plan on principle violation gets one additional sentence:

> Also emit a top-level `refused` object with `{ "principle": <1..5>, "reason": "<short structured reason>" }`. The `result.body` remains the prose explanation for the user; the `refused` object is the structured signal operators query.

The JSON-schema example earlier in the prompt ([prompts/agent_planner.md:37-55](../../../prompts/agent_planner.md#L37-L55)) gets `"refused": null,` as an explicit default with a one-line comment noting it is populated only on constitutional refusal.

The `agent_prompts` SHA-256 ledger (`db/migrations/0006_agent_prompts.sql` + the composite-PK migration `0011_agent_prompts_composite_pk.sql` shipped this week) automatically captures the edited prompt as a fresh row on next daemon start. No migration is needed for the prompt body itself.

### 5. Tests

TDD ordering follows the CLAUDE.md rule #2 (red → green per layer):

| File / module                                    | Test name                                                                                  | Pins                                                                                  |
| ------------------------------------------------ | ------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------- |
| `core/src/cassandra/types.rs::tests`             | `plan_round_trips_refused_field_some`                                                      | Serde round-trip with `Some(RefusedReason)`                                           |
| `core/src/cassandra/types.rs::tests`             | `plan_omits_refused_key_when_none`                                                         | `skip_serializing_if = Option::is_none` honoured                                      |
| `core/src/cassandra/types.rs::tests`             | `plan_is_refused_is_independent_of_is_terminal`                                            | The two helpers don't conflate; both can be true, false, or one of each independently |
| `core/src/scheduler/inner_loop.rs::tests`        | `outcome_final_state_mapping_includes_refused` (extends existing)                          | `Outcome::Refused.final_state() == "refused"`                                         |
| `core/src/scheduler/inner_loop.rs::tests`        | `outcome_refused_result_payload_carries_principle_reason_and_body`                         | Payload shape: keys `{kind, principle, reason, body}`                                 |
| `core/tests/scheduler_inner_loop_e2e.rs`         | New scenario 5: `refusal_plan_terminates_with_state_refused`                               | Scripted formulator → `Outcome::Refused`, `tasks.state='refused'`, payload pinned     |
| `core/tests/scheduler_inner_loop_e2e.rs`         | New scenario 6: `reviewer_constitutional_block_wins_over_agent_refusal`                    | Refused plan + scripted CB reviewer → `Outcome::Blocked` with reviewer's principle    |
| `db/tests/postgres_e2e.rs`                       | `tasks_state_refused_passes_check_constraint`                                              | INSERT `state='refused'` succeeds after `0012`; `state='garbage'` still rejected      |
| `core/tests/cli_ask_e2e.rs` or new audit e2e     | Mock-formulator scenario asserting `agent/plan.formulate` payload                          | Carries `refused: {…}` + `decision_kind = "refused"`                                  |

**Existing-test impact:** any production `match Outcome { ... }` without a `_ =>` arm gets a new branch. Survey confirms only `final_state()` and `result_payload()` (inside `Outcome` itself) match exhaustively; the lane runner uses the helpers and is unaffected. `cli_ask_e2e` may gain a multiset bump if we add an audit-row pin there.

**Test-count delta target:** +7-9 `#[test]` functions across the layers above.

## Deliberate non-goals

- **Real `ConstitutionalGuard` reviewer rules.** Still waiting on observation-phase dataset. This slice ships the rails (state, types, audit-row shape, prompt hook) so real rules can land cleanly afterwards.
- **CLI-side "show refusals" surface.** `hhagent-cli tasks list --state refused` works for free with the new state value; no special-case viewer.
- **Channel-bus refusal notifications.** No channel-bus exists.
- **Retroactive migration of older rows.** No `state='completed'` row is currently a constitutional refusal (CASSANDRA stubs always Approve; no operator-side refusals captured).
- **`Plan::refused` validation that `principle ∈ 1..=5`.** Field-shape validation could be added at the deserialiser, but the value is operator-visible in the audit log either way; preferring "fail loud at observation time" over "fail loud at runtime" for a debug-only signal.

## Files touched (summary)

| File                                                            | Change                                                                  |
| --------------------------------------------------------------- | ----------------------------------------------------------------------- |
| `core/src/cassandra/types.rs`                                   | NEW `RefusedReason` struct; new optional `refused` field on `Plan`; new `is_refused()` helper |
| `core/src/scheduler/inner_loop.rs`                              | New `Outcome::Refused` variant + `final_state` + `result_payload` arms; new step 4 in loop body |
| `core/src/scheduler/inner_loop.rs::write_audit_plan_formulate`  | Adds `refused` + extended `decision_kind` to payload                    |
| `db/migrations/0012_tasks_state_refused.sql`                    | NEW migration — CHECK-constraint widening                               |
| `prompts/agent_planner.md`                                      | One additional sentence + schema example update                         |
| `core/tests/scheduler_inner_loop_e2e.rs`                        | Two new scenarios                                                       |
| `db/tests/postgres_e2e.rs`                                      | One new CHECK-constraint test                                           |
| `core/tests/cli_ask_e2e.rs` (or new file)                       | Audit-row pin                                                           |

No production-caller wiring changes; the change is additive across every layer.
