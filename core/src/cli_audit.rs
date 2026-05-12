//! Producer-side audit-row helpers for the `hhagent-cli` binary.
//!
//! The scheduler writes `actor='scheduler' action='task.<state>'` rows when
//! it **observes** lifecycle transitions on tasks it has claimed. The CLI
//! is a *producer*: it initiates task submissions and cancellations.
//! Those producer events have their own audit row family with
//! `actor='cli'`, distinct from the scheduler's observation rows.
//!
//! Both families share the same `action` strings
//! (`action_task_terminal("<state>")` from [`crate::scheduler::audit`]) so
//! observation-phase SQL filtering on `action LIKE 'task.%'` captures
//! every transition regardless of producer/observer. The `actor` column
//! is the structural signal that separates intent from observation.
//!
//! ## Why two rows for one cancellation can be normal
//!
//! When the CLI cancels a task that is already `running`, two events
//! happen in sequence:
//!
//! 1. The CLI's producer-side intent — recorded here as
//!    `actor='cli', action='task.cancelled'`.
//! 2. The scheduler's observation — recorded in
//!    [`crate::scheduler::runner`] as `actor='scheduler',
//!    action='task.cancelled'` once the inner loop's `observe_state`
//!    poll sees the new state.
//!
//! These are two distinct events for one logical cancellation, and
//! observation-phase queries on `actor='cli' AND action='task.cancelled'`
//! vs `actor='scheduler' AND action='task.cancelled'` answer two different
//! questions: "who tried to cancel" vs "what did the scheduler observe".
//!
//! When the CLI cancels a `pending` task that has never been claimed,
//! only the producer row fires — the scheduler never observes the
//! transition. This file's headline reason for existing is closing that
//! audit-trail gap: before this slice the CLI cancel of a pending task
//! was completely invisible at the SQL layer.
//!
//! ## Posture
//!
//! Audit insert is best-effort, matching the [`crate::tool_host::dispatch`]
//! chokepoint pattern: a transient DB failure must not mask a successful
//! cancellation, so the SQL UPDATE's success is the load-bearing event.
//! Audit-emission failures are logged via `tracing::warn!` and swallowed.

use hhagent_db::audit;
use hhagent_db::tasks::{mark_cancelled, Task};
use hhagent_db::DbError;
use sqlx::PgPool;

use crate::scheduler::audit::{action_task_terminal, build_lifecycle_payload};

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
/// Calls [`mark_cancelled`] and, on `Some(task)`, writes one
/// `actor='cli' action='task.cancelled'` row to `audit_log` with the
/// canonical lifecycle payload `{task_id, lane, plan_count}` built via
/// [`build_lifecycle_payload`] — same shape as the scheduler's
/// `task.<state>` rows so observation-phase SQL on
/// `action LIKE 'task.%'` captures both producer intent and scheduler
/// observation.
///
/// The audit insert is best-effort (chokepoint posture); a DB error
/// there is logged via `tracing::warn!` and swallowed so a transient
/// audit failure cannot mask the successful SQL UPDATE.
pub async fn cancel_and_audit(pool: &PgPool, task_id: i64) -> Result<CancelOutcome, DbError> {
    let Some(task) = mark_cancelled(pool, task_id).await? else {
        return Ok(CancelOutcome::NotCancellable);
    };
    let action = action_task_terminal("cancelled");
    let payload = build_lifecycle_payload(task.id, task.lane, task.plan_count);
    if let Err(e) = audit::insert(pool, CLI_AUDIT_ACTOR, &action, payload).await {
        tracing::warn!(
            task_id,
            error = %e,
            "cli_audit::cancel_and_audit: audit insert failed (cancel itself succeeded)",
        );
    }
    Ok(CancelOutcome::Cancelled(task))
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
