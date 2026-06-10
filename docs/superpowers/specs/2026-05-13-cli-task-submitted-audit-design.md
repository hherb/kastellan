# Producer-side `task.submitted` audit row from `kastellan-cli ask`

**Date:** 2026-05-13
**Author:** session driven by HANDOVER "Immediate next pickups"
**Status:** spec — awaiting plan
**Slice predecessor:** `feat/cli-cancel-audit` (PR #43, merged at `fdf1a52`)

## Why this slice now

The cancel slice (PR #43) shipped the first producer-side audit row family with
`actor='cli'`. It closed the gap where a CLI cancel of a `pending` task that
was never claimed left no trace in `audit_log` (the scheduler only writes
lifecycle rows when it *observes* a transition, and a never-claimed task is
invisible to it).

`kastellan-cli ask` has the same shape of gap, only in the other direction:

1. CLI calls `tasks::insert_pending` → row appears in `tasks` with `state='pending'`.
2. Scheduler eventually claims the row → writes `scheduler/task.running`.
3. Lifecycle completes → writes `scheduler/task.<terminal>` + `scheduler/task.finalize`.

The lifecycle stream visible in `audit_log` starts at step 2. There is no row
recording step 1. An observation-phase query asking "how long did task N sit
in `pending` before being claimed?" can be answered only by joining
`audit_log.scheduler/task.running.ts` against `tasks.created_at` — a separate
table, separate clock, no single audit stream. Worse: tasks submitted while
the scheduler is down (no claim ever happens) leave no row at all.

This slice closes that gap so a single `WHERE action LIKE 'task.%'` query
returns the full timeline from submit to terminal, regardless of who emitted
which row.

## Audit-row contract

| When                                    | actor | action          | payload keys                       |
| --------------------------------------- | ----- | --------------- | ---------------------------------- |
| `kastellan-cli ask "..."` inserts a row   | `cli` | `task.submitted`| `{task_id, lane, plan_count: 0}`   |

Payload shape is `build_lifecycle_payload(id, lane, 0)` — byte-shape-identical
to the scheduler's existing lifecycle rows except for the `actor` column.
Consumers can `UNION ALL` producer + scheduler rows without a special case.

`plan_count: 0` is always literal at submit-time but is included for shape
parity. Consumers grouping by `action` see the same 3-key set across the
entire `task.<state>` family.

## Design

### 1. `db::tasks` — unchanged

`insert_pending(pool, lane, payload) -> Result<i64, DbError>` stays as-is. All
fields needed for the lifecycle payload are known at the call site without
`RETURNING *`:

- `task_id` comes from the existing `RETURNING id`,
- `lane` is an input arg,
- `plan_count` is `0` by definition (the row is being inserted).

The cancel slice widened `mark_cancelled` to `Result<Option<Task>, _>` because
`plan_count` could have advanced between submit and cancel. That reasoning
does not apply here.

### 2. `core::scheduler::audit` — one new constant

Add next to the existing `ACTION_TASK_RUNNING` / `ACTION_TASK_FINALIZE` / `ACTION_TASK_PREFIX` block:

```rust
/// `action` column for the producer-side row written by `kastellan-cli ask`
/// after `tasks::insert_pending` succeeds. Pairs with `CLI_AUDIT_ACTOR` from
/// `core::cli_audit`. Observation queries grouping by `(actor, action)` see
/// `('cli', 'task.submitted')` rows that share the `task.<state>` family's
/// lifecycle-payload shape.
pub const ACTION_TASK_SUBMITTED: &str = "task.submitted";
```

Choice rationale: a constant matches the fixed-string `ACTION_TASK_RUNNING` /
`ACTION_TASK_FINALIZE` pattern. The dynamic `action_task_terminal(state)`
builder is for the 5-variant terminal family (`completed` / `failed` /
`cancelled` / `timed_out` / `blocked`); submit is not in that family.

### 3. `core::cli_audit` — one new helper

Add next to `cancel_and_audit`, with byte-identical posture (best-effort audit,
chokepoint logging on failure):

```rust
/// Insert a fresh `pending` task and emit the producer-side
/// `actor='cli' action='task.submitted'` audit row.
///
/// On success returns the new task id. The audit insert is best-effort:
/// a transient DB issue is logged at WARN but the id still propagates,
/// because the SQL INSERT already committed and the task is now a real
/// row in the `tasks` table — failing the call would be strictly worse
/// than a missing audit row.
///
/// # Two-row-on-one-event note (for callers who care about double-counting)
///
/// `kastellan-cli ask` will produce two rows for one logical task entry:
/// the producer row here at submit time, and the scheduler's later
/// `task.running` observation row on claim. Observation queries asking
/// "who submitted" use `actor='cli'`; queries asking "what did the
/// scheduler observe" use `actor='scheduler'`. The split is intentional.
pub async fn submit_and_audit(
    pool: &PgPool,
    lane: Lane,
    payload: serde_json::Value,
) -> Result<i64, DbError> {
    let id = tasks::insert_pending(pool, lane, payload).await?;

    let row_payload = scheduler::audit::build_lifecycle_payload(id, lane, 0);
    if let Err(e) = kastellan_db::audit::insert(
        pool,
        CLI_AUDIT_ACTOR,
        scheduler::audit::ACTION_TASK_SUBMITTED,
        row_payload,
    )
    .await
    {
        tracing::warn!(
            task_id = id,
            error = %e,
            "submit_and_audit: producer audit row failed (task still submitted)",
        );
    }

    Ok(id)
}
```

### 4. `core/src/bin/kastellan-cli.rs::ask_async` — one-line rewiring

Line 267 currently calls `insert_pending` directly:

```rust
let id = match insert_pending(
    &pool,
    lane,
    serde_json::json!({"instruction": instruction, "kind": "ask"}),
)
.await
{
    Ok(i) => i,
    Err(e) => { eprintln!("ask: insert failed: {e}"); return ExitCode::from(1); }
};
```

Becomes:

```rust
let id = match submit_and_audit(
    &pool,
    lane,
    serde_json::json!({"instruction": instruction, "kind": "ask"}),
)
.await
{
    Ok(i) => i,
    Err(e) => { eprintln!("ask: insert failed: {e}"); return ExitCode::from(1); }
};
```

The `use kastellan_db::tasks::{get, insert_pending};` line drops `insert_pending`
and the existing `use kastellan_core::cli_audit::cancel_and_audit;` line widens
to `cancel_and_audit, submit_and_audit`. No other call-site changes.

## Test plan

### New integration test — `core/tests/cli_submit_audit_e2e.rs`

Mirrors `cli_cancel_audit_e2e.rs` in shape (per-test PG cluster, helper called
directly with no subprocess overhead). One `#[test]`:

```
submit_and_audit_emits_producer_task_submitted_row
```

Assertions:

1. Helper returns `Ok(id)` for both `Lane::Fast` and `Lane::Long` (one
   call per lane in the same test to pin the lane round-trip).
2. `tasks` table has exactly 2 rows after both calls (one per lane), each
   with `state='pending'`, the expected lane, and the input payload byte-shape.
3. `audit_log` has exactly 3 rows after the two calls: one probe bring-up row
   + two `cli/task.submitted` rows.
4. Both producer rows pin the canonical contract:
   - `actor == "cli"`,
   - `action == "task.submitted"`,
   - payload key-set is exactly `{task_id, lane, plan_count}` (BTreeSet pin
     so a future accidental extra field trips the test, matching the cancel
     slice's pin style),
   - `payload["plan_count"] == 0`,
   - `payload["task_id"]` matches the returned id,
   - `payload["lane"]` round-trips (`"fast"` / `"long"`).

### Bumped test — `core/tests/cli_ask_e2e.rs`

Today's `cli_ask_e2e` asserts an exact audit multiset on every `kastellan-cli
ask` invocation. After this slice, every `ask` call emits one new
`cli/task.submitted` row. Two multiset bumps:

- **Happy path** (`ask_subprocess_completes_planned_task_end_to_end`): the
  existing `cli/task.submitted` count is `0`; bump to `1`. Total expected row
  count rises by 1.
- **Failure path** (`ask_subprocess_fails_after_plan_iteration_cap`): same +1
  bump.

No new spot-checks added — the new e2e file pins the row shape; cli_ask_e2e
only needs to count it.

### Unit tests — none added

`core::cli_audit::tests` already pins `CLI_AUDIT_ACTOR` (carried over from the
cancel slice). The new `ACTION_TASK_SUBMITTED` constant could be pinned but
would be a near-tautology — the integration test asserts the literal string
in the audit row, which is the same pin with stronger coverage.

## Test count delta

353 → **354** (+1 integration test in new file). `cli_ask_e2e` multiset bumps
don't add `#[test]` functions.

## What this slice deliberately does NOT do

- **No producer row from future channel adapters.** No channel adapter exists
  today. When one lands, the helper can be promoted (take `actor: &str`) or a
  separate `CHANNEL_AUDIT_ACTOR` const added in the same module — the wire
  shape is identical. YAGNI today.
- **No producer `task.failed` row from `kastellan-cli tasks fail`.** Operator
  escape hatch; rare; scheduler's `task.crashed` lifecycle row already covers
  the running→crashed-after-restart case.
- **No DB transaction wrapping `insert_pending` + audit insert.** Best-effort
  matches the chokepoint and cancel-slice posture. Documented at the helper
  doc-comment level (the cancel slice has the same trade-off documented in
  post-review commit `8840e34`).
- **No `submit_and_audit` callers other than `kastellan-cli ask`.** Only one
  production `insert_pending` call site exists; no other callers to wire.

## Rollback plan

If the producer row turns out to interfere with an existing observation
consumer (unlikely — the `actor='cli'` namespace is new):

1. Revert `core/src/bin/kastellan-cli.rs::ask_async` back to direct
   `insert_pending`.
2. Leave `submit_and_audit` + `ACTION_TASK_SUBMITTED` in place — neither has
   external callers; both are inert.
3. Bump `cli_ask_e2e` multiset assertions back down by 1.

The DB-layer surface is unchanged so no migration churn.

## Files touched (planned)

- `core/src/scheduler/audit.rs` — `ACTION_TASK_SUBMITTED` const added.
- `core/src/cli_audit.rs` — `submit_and_audit` helper added.
- `core/src/bin/kastellan-cli.rs` — one-line rewiring at `ask_async:267`; use
  statement widened to import `submit_and_audit`.
- NEW `core/tests/cli_submit_audit_e2e.rs` — single integration test (~180 LOC).
- `core/tests/cli_ask_e2e.rs` — multiset bumps (+1 in happy path, +1 in
  failure path).
- `docs/devel/handovers/HANDOVER.md` — refresh header (PR #43 merged
  observation) + "Recently completed (this session)" entry.
- `docs/devel/ROADMAP.md` — tick the new producer row entry.
