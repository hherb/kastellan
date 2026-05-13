//! Producer-side audit-row helpers for the `hhagent-cli` binary.
//!
//! The scheduler writes `actor='scheduler' action='task.<state>'` rows when
//! it **observes** lifecycle transitions on tasks it has claimed. The CLI
//! is a *producer*: it initiates task submissions and cancellations.
//! Those producer events have their own audit row family with
//! `actor='cli'`, distinct from the scheduler's observation rows.
//!
//! Both families share the same `action` strings — the submit row uses
//! [`crate::scheduler::audit::ACTION_TASK_SUBMITTED`] and the terminal
//! rows use `action_task_terminal("<state>")`, both from
//! [`crate::scheduler::audit`] — so observation-phase SQL filtering on
//! `action LIKE 'task.%'` captures every transition regardless of
//! producer/observer. The `actor` column is the structural signal that
//! separates intent from observation.
//!
//! ## Why two rows for one logical event can be normal
//!
//! Two helpers live here and both can emit a producer row that is later
//! followed by a scheduler observation row for the same logical event:
//!
//! - **Submit:** `hhagent-cli ask` writes `actor='cli',
//!   action='task.submitted'` at insert time; the scheduler later writes
//!   `actor='scheduler', action='task.running'` when it claims the same
//!   task. Two rows, one logical task entry.
//! - **Cancel of a `running` task:** the CLI writes
//!   `actor='cli', action='task.cancelled'`; the scheduler's inner-loop
//!   `observe_state` poll later writes `actor='scheduler',
//!   action='task.cancelled'` when it sees the new state. Two rows, one
//!   logical cancellation.
//!
//! Observation-phase queries on `actor='cli' AND action='task.<x>'` vs
//! `actor='scheduler' AND action='task.<x>'` answer two different
//! questions: "what did the producer intend" vs "what did the scheduler
//! observe".
//!
//! When the scheduler never observes the event — a `pending` task
//! cancelled before claim, or a task submitted while the scheduler is
//! down — only the producer row fires. This module's headline reason for
//! existing is closing those audit-trail gaps: before producer rows
//! existed, a CLI cancel of a never-claimed `pending` task was
//! completely invisible at the SQL layer, and a submit followed by a
//! scheduler outage had no row at all.
//!
//! ## Producer-side `task.finalize` for never-claimed pending tasks
//!
//! [`cancel_and_audit`] emits an additional `actor='cli'
//! action='task.finalize'` summary row when (and only when) the cancel
//! flips a task whose `started_at IS NULL` — i.e. one the scheduler
//! never claimed. For these tasks the scheduler will never write its
//! own observation-side finalize row, so observation-phase queries
//! grouping on `action='task.finalize'` previously undercounted by
//! exactly the producer-cancelled-pending population. The counters in
//! that producer finalize row are **known zeros** (the task ran zero
//! plan iterations) — wire-distinguishable from the JSON-`null`
//! counters in the crashed-task finalize, where the values are
//! genuinely unrecoverable.
//!
//! When the cancel flips a `running` task instead, the producer skips
//! the finalize row: the scheduler's inner-loop `observe_state` poll
//! will see the new state and emit its own
//! `actor='scheduler' action='task.finalize'`, so a producer finalize
//! would inflate the finalize stream.
//!
//! ## Posture
//!
//! Audit insert is best-effort, matching the [`crate::tool_host::dispatch`]
//! chokepoint pattern: a transient DB failure must not mask a successful
//! cancellation or submission, so the SQL UPDATE/INSERT's success is the
//! load-bearing event. Audit-emission failures are logged via
//! `tracing::warn!` and swallowed.
//!
//! ### Residual gap accepted by this posture
//!
//! The SQL write and the audit INSERT are two separate statements, not
//! one transaction. A crash (or audit-insert DB error) between them
//! leaves the row's state change real but with no producer audit row.
//! For a CLI cancel of a `running` task the scheduler's later
//! observation row partially covers this — but for a CLI cancel of a
//! never-claimed `pending` task, or for any submit followed by a
//! scheduler outage, the producer row is the **only** audit signal, so
//! losing it reintroduces the very gap this module exists to close.
//!
//! This is accepted, not unintended: a transactional wrap would couple
//! the cancellation's or submission's success to audit availability,
//! which would mask the underlying event behind audit outages. The
//! trade-off favours liveness over audit completeness. Observation-phase
//! queries that depend on a strict 1:1 producer-row:event mapping must
//! treat the audit-row count as a lower bound, not a total.

use hhagent_db::audit;
use hhagent_db::tasks::{insert_pending, mark_cancelled, Lane, Task};
use hhagent_db::DbError;
use sqlx::PgPool;
use time::OffsetDateTime;

use crate::scheduler::audit::{
    action_task_terminal, build_finalize_payload, build_lifecycle_payload, TaskFinalizeStats,
    ACTION_TASK_FINALIZE, ACTION_TASK_SUBMITTED,
};

/// Logical `actor` string written into every CLI-emitted audit row.
/// Distinct from [`crate::scheduler::audit::SCHEDULER_AUDIT_ACTOR`] so
/// observation queries can separate producer intent from scheduler
/// observation.
pub const CLI_AUDIT_ACTOR: &str = "cli";

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

    // 1. Lifecycle row — always.
    let action = action_task_terminal("cancelled");
    let payload = build_lifecycle_payload(task.id, task.lane, task.plan_count);
    if let Err(e) = audit::insert(pool, CLI_AUDIT_ACTOR, &action, payload).await {
        tracing::warn!(
            task_id,
            error = %e,
            "cli_audit::cancel_and_audit: lifecycle audit insert failed (cancel itself succeeded)",
        );
    }

    // 2. Finalize summary row — only when the task was never claimed.
    //    The scheduler will emit its own `task.finalize` for any task it
    //    observed (the inner-loop `observe_state` poll catches the cancel
    //    of a running task), so emitting one here would inflate the
    //    finalize stream.
    if task.started_at.is_none() {
        emit_producer_cancel_finalize(pool, &task).await;
    }

    Ok(CancelOutcome::Cancelled(task))
}

/// Insert one `actor='cli' action='task.finalize'` row for a
/// producer-cancelled `pending` task. Best-effort, same posture as the
/// lifecycle row in [`cancel_and_audit`].
///
/// The counters and duration are pinned to known zeros because the task
/// ran zero plan iterations and zero step dispatches before being
/// cancelled. `started_at: None` is the wire signal "task was never
/// claimed" — `build_finalize_payload` already serialises this as JSON
/// null and falls `total_duration_ms` back to 0 via `compute_duration_ms`.
///
/// `finished_at` falls back to the local clock if `task.finished_at` is
/// somehow `None` — operationally dead code (the `mark_cancelled` UPDATE
/// always sets it via `now()`), but defends the impossible case so a
/// missing column doesn't panic.
async fn emit_producer_cancel_finalize(pool: &PgPool, task: &Task) {
    let finished_at = task
        .finished_at
        .unwrap_or_else(OffsetDateTime::now_utc);
    let stats = TaskFinalizeStats {
        plan_count: task.plan_count,
        total_llm_calls: 0,
        total_dispatch_calls: 0,
        total_duration_ms: 0,
        started_at: None,
        finished_at,
    };
    let payload = build_finalize_payload(task.id, task.lane, "cancelled", &stats);
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
/// `hhagent-cli ask` will produce two rows in `audit_log` for one
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Producer-side actor string is pinned. If a future rename happens
    /// it must touch both this constant and observation-phase SQL
    /// consumers in one go.
    #[test]
    fn cli_audit_actor_string_is_pinned() {
        assert_eq!(CLI_AUDIT_ACTOR, "cli");
    }

    /// The producer actor MUST differ from the scheduler actor so
    /// observation queries can separate intent from observation.
    #[test]
    fn cli_actor_differs_from_scheduler_actor() {
        assert_ne!(
            CLI_AUDIT_ACTOR,
            crate::scheduler::audit::SCHEDULER_AUDIT_ACTOR
        );
    }
}
