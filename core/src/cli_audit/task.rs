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
    ACTION_TASK_FINALIZE, ACTION_TASK_SUBMITTED,
};

/// Outcome of [`cancel_and_audit`].
///
/// `Cancelled(Task)` carries the post-update row so callers can display
/// the new state without re-fetching. `NotCancellable` means the row
/// was already in a terminal state or does not exist; no SQL UPDATE
/// happened and no audit row was written.
#[derive(Debug)]
pub enum CancelOutcome {
    /// Row was flipped to `cancelled`. One `actor='cli'
    /// action='task.cancelled'` audit row was attempted (best-effort:
    /// a DB error on the audit insert is logged but the outcome stays
    /// `Cancelled` — the SQL UPDATE already committed).
    Cancelled(Task),
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
/// 2. **Only when the task was never claimed** (`task.started_at.is_none()`):
///    one `actor='cli' action='task.finalize'` summary row with
///    `state='cancelled'`, `started_at: null`, and zero counters /
///    duration. Rationale: the scheduler will never observe this task
///    (it never claimed it), so without this row observation-phase SQL
///    grouping on `action='task.finalize'` would silently undercount by
///    exactly the producer-cancelled-pending population. The counters
///    are **known** zeros (the task ran zero plan iterations and zero
///    step dispatches) — distinct from the crashed-task finalize where
///    they are JSON `null` because the dead daemon's counters were
///    unrecoverable.
///
/// When the task was already `running` (`started_at.is_some()`) the
/// scheduler's inner-loop `observe_state` poll will later write its own
/// `actor='scheduler' action='task.finalize'` row; emitting a producer
/// finalize here would double-count the finalize stream, so we skip it.
/// The discriminator is purely DB-state-driven (`started_at IS NOT NULL`
/// after the UPDATE), which is exactly the predicate that distinguishes
/// "scheduler ever touched this task" from "scheduler never saw it."
///
/// Both audit inserts are best-effort (chokepoint posture); DB errors
/// there are logged via `tracing::warn!` and swallowed so a transient
/// audit failure cannot mask the successful SQL UPDATE.
pub async fn cancel_and_audit(pool: &PgPool, task_id: i64) -> Result<CancelOutcome, DbError> {
    let Some(task) = mark_cancelled(pool, task_id).await? else {
        return Ok(CancelOutcome::NotCancellable);
    };
    emit_cancel_audit_rows(pool, &task).await;
    Ok(CancelOutcome::Cancelled(task))
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
    emit_cancel_audit_rows(pool, &task).await;
    Ok(CancelOutcome::Cancelled(task))
}

/// Emit the producer audit rows for a cancelled task. Shared by
/// [`cancel_and_audit`] and [`cancel_if_pending_and_audit`]; the `task`
/// passed in is the already-cancelled row returned by the `mark_*` UPDATE.
///
/// 1. **Always** one `actor='cli' action='task.cancelled'` lifecycle row.
/// 2. **Only when the task was never claimed** (`started_at.is_none()`) one
///    producer `task.finalize` summary row — the scheduler emits its own
///    `task.finalize` for any task it observed (the inner-loop
///    `observe_state` poll catches the cancel of a running task), so
///    emitting one here too would inflate the finalize stream.
///
/// Both inserts are best-effort (chokepoint posture): a DB error is logged
/// via `tracing::warn!` and swallowed so a transient audit failure cannot
/// mask the successful SQL UPDATE.
async fn emit_cancel_audit_rows(pool: &PgPool, task: &Task) {
    let action = action_task_terminal("cancelled");
    let payload = build_lifecycle_payload(task.id, task.lane, task.plan_count);
    if let Err(e) = audit::insert(pool, CLI_AUDIT_ACTOR, &action, payload).await {
        tracing::warn!(
            task_id = task.id,
            error = %e,
            "cli_audit::emit_cancel_audit_rows: lifecycle audit insert failed (cancel itself succeeded)",
        );
    }
    if task.started_at.is_none() {
        emit_producer_cancel_finalize(pool, task).await;
    }
}

/// Insert one `actor='cli' action='task.finalize'` row for a
/// producer-cancelled `pending` task. Best-effort, same posture as the
/// lifecycle row in [`cancel_and_audit`].
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
async fn emit_producer_cancel_finalize(pool: &PgPool, task: &Task) {
    let finished_at = task.finished_at.unwrap_or_else(|| {
        tracing::error!(
            task_id = task.id,
            "cli_audit::emit_producer_cancel_finalize: task.finished_at is None after \
             mark_cancelled — expected unconditional `UPDATE … SET finished_at = now()`; \
             falling back to local clock so the audit row still emits",
        );
        OffsetDateTime::now_utc()
    });
    let payload = build_producer_cancel_finalize_payload(
        task.id,
        task.lane,
        task.plan_count,
        finished_at,
    );
    if let Err(e) =
        audit::insert(pool, CLI_AUDIT_ACTOR, ACTION_TASK_FINALIZE, payload).await
    {
        tracing::warn!(
            task_id = task.id,
            error = %e,
            "cli_audit::cancel_and_audit: finalize audit insert failed (cancel itself succeeded)",
        );
    }
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
/// On success returns the new task id. The audit insert is best-effort:
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
) -> Result<i64, DbError> {
    let id = insert_pending(pool, lane, payload).await?;

    let row_payload = build_lifecycle_payload(id, lane, 0);
    if let Err(e) =
        audit::insert(pool, CLI_AUDIT_ACTOR, ACTION_TASK_SUBMITTED, row_payload).await
    {
        tracing::warn!(
            task_id = id,
            error = %e,
            "cli_audit::submit_and_audit: audit insert failed (task itself was submitted)",
        );
    }

    Ok(id)
}
