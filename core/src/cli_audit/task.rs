//! Producer-side task-lifecycle audit helpers (`cancel` / `submit`).
//!
//! These are the headline reason the `cli_audit` family exists: a CLI
//! cancel of a never-claimed `pending` task, or a submit followed by a
//! scheduler outage, previously left no audit row at all. See the
//! [`crate::cli_audit`] module doc for the full producer-vs-observer
//! posture and the two-rows-per-event rationale.

use kastellan_db::audit;
use kastellan_db::tasks::{insert_pending, mark_cancelled, mark_cancelled_if_pending, Lane, Task};
use kastellan_db::DbError;
use sqlx::PgPool;
use time::OffsetDateTime;

use crate::cli_audit::CLI_AUDIT_ACTOR;
use crate::scheduler::audit::{
    action_task_terminal, build_lifecycle_payload, build_producer_cancel_finalize_payload,
    build_producer_cancel_suspended_finalize_payload, ACTION_TASK_FINALIZE, ACTION_TASK_SUBMITTED,
};

/// Outcome of [`cancel_and_audit`].
///
/// `Cancelled` carries the post-update row so callers can display the new
/// state without re-fetching, and whether every audit row was written. `NotCancellable` means the row
/// was already in a terminal state or does not exist; no SQL UPDATE
/// happened and no audit row was written.
#[derive(Debug)]
pub enum CancelOutcome {
    /// Row was flipped to `cancelled`. One `actor='cli'
    /// action='task.cancelled'` audit row was attempted (best-effort:
    /// a DB error on the audit insert is logged but the outcome stays
    /// `Cancelled` — the SQL UPDATE already committed). `audited` is
    /// false when any of the cancel's rows could not be written, so a
    /// caller whose user never sees the log (a front end, #988) can say so.
    Cancelled { task: Task, audited: bool },
    /// Row does not exist, or is already in a terminal state. No SQL
    /// UPDATE, no audit row. Returned even for the bogus-id case
    /// because the two are indistinguishable from one SQL UPDATE: the
    /// caller can call `tasks::get` first if it cares about the
    /// distinction.
    NotCancellable,
}

/// Producer-side cancellation with audit-row emission.
///
/// Calls [`mark_cancelled`] and, on `Some(task)`, writes producer rows
/// to `audit_log`:
///
/// 1. **Always** one `actor='cli' action='task.cancelled'` row with the
///    canonical lifecycle payload `{task_id, lane, plan_count}` built
///    via [`build_lifecycle_payload`] — same shape as the scheduler's
///    `task.<state>` rows so observation-phase SQL on
///    `action LIKE 'task.%'` captures both producer intent and
///    scheduler observation.
/// 2. **Only when no scheduler-side observer will finalize the task**:
///    one `actor='cli' action='task.finalize'` summary row with
///    `state='cancelled'`, `started_at: null`, and zero counters /
///    duration. Rationale: without this row observation-phase SQL
///    grouping on `action='task.finalize'` would silently undercount by
///    exactly the population no observer covers. The counters
///    are **known** zeros (the task ran zero plan iterations and zero
///    step dispatches) — distinct from the crashed-task finalize where
///    they are JSON `null` because the dead daemon's counters were
///    unrecoverable.
/// 3. **When the cancel destroyed pending asks**: one `actor='cli'
///    action='ask.cancelled'` row (#564), so a withdrawn human question
///    leaves a trace.
///
/// The discriminator for (2) is the task's state **before** the cancel,
/// carried on [`kastellan_db::tasks::Cancellation::previous_state`] — see
/// [`scheduler_will_emit_finalize`]. Only a task cancelled out of
/// `running` has a live inner loop whose `observe_state` poll will write
/// the scheduler's own finalize row; emitting a producer finalize for it
/// too would double-count. A task cancelled out of `pending` was never
/// claimed, and one cancelled out of `awaiting_operator` was claimed but
/// has no live loop — both need the producer row.
///
/// Both audit inserts are best-effort (chokepoint posture); DB errors
/// there are logged via `tracing::warn!` and swallowed so a transient
/// audit failure cannot mask the successful SQL UPDATE.
pub async fn cancel_and_audit(pool: &PgPool, task_id: i64) -> Result<CancelOutcome, DbError> {
    let Some(cancellation) = mark_cancelled(pool, task_id).await? else {
        return Ok(CancelOutcome::NotCancellable);
    };
    let audited = emit_cancel_audit_rows(
        pool,
        &cancellation.task,
        &cancellation.previous_state,
        cancellation.asks_cancelled,
    )
    .await;
    Ok(CancelOutcome::Cancelled { task: cancellation.task, audited })
}

/// Like [`cancel_and_audit`], but cancels **only if the task is still
/// `pending`** (via [`mark_cancelled_if_pending`]).
///
/// Returns `NotCancellable` when the task is anything but `pending` —
/// crucially including `running`, i.e. a task the daemon has just
/// claimed. The `memory l3 run` no-daemon path uses this so a daemon that
/// wins the race against the liveness check keeps its claim (the CLI then
/// waits for the real result) instead of having a live `--execute`
/// cancelled out from under it (issue #179 follow-up). Audit-row emission
/// is identical to `cancel_and_audit` — a pending-only cancel is by
/// definition never-claimed, so both the lifecycle and the producer
/// `task.finalize` rows fire.
pub async fn cancel_if_pending_and_audit(
    pool: &PgPool,
    task_id: i64,
) -> Result<CancelOutcome, DbError> {
    let Some(task) = mark_cancelled_if_pending(pool, task_id).await? else {
        return Ok(CancelOutcome::NotCancellable);
    };
    // Pending-only by construction, so the pre-cancel state is `pending`
    // and there can be no ask: `raise` only ever suspends a `running`
    // task, so a never-claimed task has none.
    let audited = emit_cancel_audit_rows(pool, &task, "pending", 0).await;
    Ok(CancelOutcome::Cancelled { task, audited })
}

/// Does the scheduler's inner loop have a live observer that will write
/// its own `actor='scheduler' action='task.finalize'` row for a task
/// cancelled out of `previous_state`?
///
/// **`running` is the only state for which the answer is yes**, and that is
/// narrower than the `started_at.is_some()` test this used to be. A task
/// cancelled out of `awaiting_operator` (#564) also has `started_at` set —
/// `claim_one` stamped it before the ask suspended the task — but its lane
/// released it when the ask was raised, so `run_one` has already returned
/// and there is no `observe_state` poll left to notice the cancel. Under
/// the old test such a task got **no** `task.finalize` row from anyone, and
/// observation-phase SQL grouping on `action='task.finalize'` undercounted
/// by exactly the cancelled-while-suspended population — the identical
/// undercount the producer row was introduced to close for never-claimed
/// tasks.
///
/// Keyed on the pre-cancel state rather than on post-cancel column values
/// because after the UPDATE the two cases are indistinguishable. (They can
/// be told apart by `lease_expires_at`, which `suspend_for_ask` nulls — but
/// that is an implicit coupling to a column another function clears for
/// unrelated reasons, and it would break silently.)
fn scheduler_will_emit_finalize(previous_state: &str) -> bool {
    previous_state == "running"
}

/// Emit the producer audit rows for a cancelled task. Shared by
/// [`cancel_and_audit`] and [`cancel_if_pending_and_audit`]; the `task`
/// passed in is the already-cancelled row returned by the `mark_*` UPDATE,
/// and `previous_state` is the state it held immediately before.
///
/// 1. **Always** one `actor='cli' action='task.cancelled'` lifecycle row.
/// 2. **When no scheduler-side observer will finalize this task** (see
///    [`scheduler_will_emit_finalize`]) one producer `task.finalize`
///    summary row, so the finalize stream neither under- nor over-counts.
/// 3. **When the cancel destroyed one or more pending asks**, one
///    `actor='cli' action='ask.cancelled'` row. A human had an outstanding
///    question; without this it disappears from `asks::list_pending` with
///    nothing in `audit_log` recording that it was ever asked.
///
/// All inserts are best-effort (chokepoint posture): a DB error is logged
/// via `tracing::warn!` and swallowed so a transient audit failure cannot
/// mask the successful SQL UPDATE. Returns whether every row was written.
async fn emit_cancel_audit_rows(
    pool: &PgPool,
    task: &Task,
    previous_state: &str,
    asks_cancelled: u64,
) -> bool {
    let mut audited = true;
    let action = action_task_terminal("cancelled");
    let payload = build_lifecycle_payload(task.id, task.lane, task.plan_count);
    if let Err(e) = audit::insert(pool, CLI_AUDIT_ACTOR, &action, payload).await {
        audited = false;
        tracing::warn!(
            task_id = task.id,
            error = %e,
            "cli_audit::emit_cancel_audit_rows: lifecycle audit insert failed (cancel itself succeeded)",
        );
    }
    if !scheduler_will_emit_finalize(previous_state) {
        audited &= emit_producer_cancel_finalize(pool, task, previous_state).await;
    }
    if asks_cancelled > 0 {
        let ask_payload = serde_json::json!({
            "task_id": task.id,
            "asks_cancelled": asks_cancelled,
            "task_state_before_cancel": previous_state,
        });
        if let Err(e) =
            audit::insert(pool, CLI_AUDIT_ACTOR, ACTION_ASK_CANCELLED, ask_payload).await
        {
            audited = false;
            tracing::warn!(
                task_id = task.id,
                asks_cancelled,
                error = %e,
                "cli_audit::emit_cancel_audit_rows: ask.cancelled audit insert failed \
                 (cancel itself succeeded)",
            );
        }
    }
    audited
}

/// `action` for the row recording that cancelling a task destroyed the
/// human's outstanding question(s). A `const` so the observation-phase
/// query and the writer cannot drift onto two spellings of one event —
/// same reason `asks::ASK_TIMEOUT_DETAIL` is one.
pub const ACTION_ASK_CANCELLED: &str = "ask.cancelled";

/// Insert one `actor='cli' action='task.finalize'` row for a
/// producer-cancelled `pending` task. Best-effort, same posture as the
/// lifecycle row in [`cancel_and_audit`]; returns whether it was written.
///
/// The counters and duration are pinned to **known zeros** inside
/// [`build_producer_cancel_finalize_payload`] — the task ran zero
/// plan iterations and zero step dispatches before being cancelled, so
/// no computation is needed. `started_at` is always JSON `null` (the
/// wire signal "task was never claimed"). These known zeros are
/// wire-distinguishable from the crashed-task finalize's JSON-`null`
/// counters, where the values were genuinely unrecoverable — the
/// `provenance` field (issue #50 schema-v2) makes the distinction
/// explicit without consumers having to reason about it.
///
/// `finished_at` falls back to the local clock if `task.finished_at` is
/// somehow `None` — operationally dead code (the `mark_cancelled`
/// UPDATE always sets it via `now()`). The fallback exists so the row
/// is still emitted with a plausible timestamp instead of panicking,
/// and the violation is surfaced via `tracing::error!` so the
/// impossible case is loud, not silent.
///
/// Wire shape: [`build_producer_cancel_finalize_payload`], including the
/// `provenance="producer_cancel_pending"` discriminator added in issue
/// #50 schema-v2.
async fn emit_producer_cancel_finalize(pool: &PgPool, task: &Task, previous_state: &str) -> bool {
    let finished_at = task.finished_at.unwrap_or_else(|| {
        tracing::error!(
            task_id = task.id,
            "cli_audit::emit_producer_cancel_finalize: task.finished_at is None after \
             mark_cancelled — expected unconditional `UPDATE … SET finished_at = now()`; \
             falling back to local clock so the audit row still emits",
        );
        OffsetDateTime::now_utc()
    });
    // Two shapes, because the never-claimed payload's hardcoded fields are
    // FALSE for a suspended task: it was claimed (`started_at` is set), it
    // burned plan iterations before escalating, and its duration is not
    // zero. Emitting the pending shape for it would fabricate a record in
    // the one log whose whole job is to be trustworthy.
    let payload = if previous_state == "awaiting_operator" {
        build_producer_cancel_suspended_finalize_payload(
            task.id,
            task.lane,
            task.plan_count,
            task.started_at,
            finished_at,
        )
    } else {
        build_producer_cancel_finalize_payload(task.id, task.lane, task.plan_count, finished_at)
    };
    if let Err(e) =
        audit::insert(pool, CLI_AUDIT_ACTOR, ACTION_TASK_FINALIZE, payload).await
    {
        tracing::warn!(
            task_id = task.id,
            error = %e,
            "cli_audit::cancel_and_audit: finalize audit insert failed (cancel itself succeeded)",
        );
        return false;
    }
    true
}

/// Producer-side task submission with audit-row emission.
///
/// Calls [`insert_pending`] and writes one `actor='cli'
/// action='task.submitted'` row to `audit_log` with the canonical
/// lifecycle payload `{task_id, lane, plan_count}` built via
/// [`build_lifecycle_payload`] (`plan_count` is `0` by definition at
/// submit time — included for shape parity with the rest of the
/// `task.<state>` family so consumers don't need a special case).
///
/// On success returns the new task id, and whether its audit row was
/// written ([`Submitted`]). The audit insert is best-effort:
/// a transient DB issue is logged at WARN but the id still propagates,
/// because the SQL INSERT already committed and the task is now a real
/// row in the `tasks` table — failing the call would be strictly worse
/// than a missing audit row, and would couple submit liveness to audit
/// availability the same way the cancel-slice trade-off documents.
///
/// # Two-rows-on-one-event note
///
/// `kastellan-cli ask` will produce two rows in `audit_log` for one
/// logical task entry: this producer row at submit time, and the
/// scheduler's later `task.running` observation row on claim. The split
/// is intentional — observation queries asking "who submitted" use
/// `actor='cli'`, queries asking "what did the scheduler observe" use
/// `actor='scheduler'`.
///
/// # Ordering race vs `task.running`
///
/// `insert_pending` commits the new task row before this helper writes
/// the audit row. A fast scheduler can claim the task and write its
/// `actor='scheduler' action='task.running'` row before this helper's
/// audit insert returns, leaving the two rows out of order by `ts` (and
/// by `audit_log.id`, since both are assigned at INSERT time). Submit-
/// to-claim latency queries that compute `running_ts - submit_ts` may
/// therefore occasionally see negative deltas under contention. This
/// is consistent with the cancel slice's non-transactional posture and
/// is accepted — fixing it would require a transactional wrap that
/// couples submit liveness to audit availability. Consumers must
/// tolerate (or filter) the rare inverted-pair case rather than assume
/// monotonic ordering between the producer and observation rows.
pub async fn submit_and_audit(
    pool: &PgPool,
    lane: Lane,
    payload: serde_json::Value,
) -> Result<Submitted, DbError> {
    let id = insert_pending(pool, lane, payload).await?;

    let row_payload = build_lifecycle_payload(id, lane, 0);
    let audited = match audit::insert(pool, CLI_AUDIT_ACTOR, ACTION_TASK_SUBMITTED, row_payload).await {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!(
                task_id = id,
                error = %e,
                "cli_audit::submit_and_audit: audit insert failed (task itself was submitted)",
            );
            false
        }
    };

    Ok(Submitted { id, audited })
}

/// What [`submit_and_audit`] did: the new task's `id`, and whether its
/// `task.submitted` row was written (`false` = logged at WARN only, which
/// a front end reading stdout never sees, #988).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Submitted {
    pub id: i64,
    pub audited: bool,
}
