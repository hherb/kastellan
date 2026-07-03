//! Producer-side audit-row helpers for the `kastellan-cli` binary.
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
//! Two of the task helpers can emit a producer row that is later
//! followed by a scheduler observation row for the same logical event:
//!
//! - **Submit:** `kastellan-cli ask` writes `actor='cli',
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
//!
//! ## Module layout
//!
//! The helpers are grouped by the domain they audit; every one is
//! re-exported here so the public API stays `cli_audit::<name>`:
//!
//! - [`task`] — task-lifecycle cancel / submit (the headline gap-closers).
//! - [`registry`] — tool-allowlist + relation-kind + entity-kind add/remove.
//! - [`memory`] — L1 / L3 memory promote / remove / trust-flip.
//! - [`entities`] — entity-review approve / reject / merge.

mod entities;
mod memory;
mod registry;
mod task;

pub use entities::{
    entities_approve_and_audit, entities_merge_and_audit, entities_reject_and_audit,
};
pub use memory::{
    l1_add_and_audit, l1_remove_and_audit, l3_approve_and_audit, l3_approve_rejected_audit,
    l3_pin_and_audit, l3_pin_rejected_audit, l3_remove_and_audit, l3_revoke_and_audit,
};
pub use registry::{
    entity_kinds_add_and_audit, entity_kinds_remove_and_audit, relation_kinds_add_and_audit,
    relation_kinds_remove_and_audit, tools_allowlist_add_and_audit,
    tools_allowlist_remove_and_audit,
};
pub use task::{cancel_and_audit, cancel_if_pending_and_audit, submit_and_audit, CancelOutcome};

/// Logical `actor` string written into every CLI-emitted audit row.
/// Distinct from [`crate::scheduler::audit::SCHEDULER_AUDIT_ACTOR`] so
/// observation queries can separate producer intent from scheduler
/// observation.
pub const CLI_AUDIT_ACTOR: &str = "cli";

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

    #[test]
    fn l1_add_and_audit_signature_compile_pin() {
        // Compile-only: the function exists with the widened signature
        // (pool, extractor, body). Full DB-backed coverage is in
        // core/tests/memory_l1_promote_e2e.rs.
        fn _signature_pin<'a>(
            pool: &'a sqlx::PgPool,
            extractor: &'a crate::entity_extraction::NoOpEntityExtractor,
            body: &'a str,
        ) -> impl std::future::Future<
            Output = Result<
                (crate::memory::l1_promote::L1WriteOutcome, i64),
                crate::memory::l1_promote::L1Error,
            >,
        > + 'a {
            l1_add_and_audit(pool, extractor, body)
        }
        let _ = _signature_pin;
    }

    #[test]
    fn l1_remove_and_audit_signature_compile_pin() {
        fn _signature_pin<'a>(
            pool: &'a sqlx::PgPool,
            memory_id: i64,
        ) -> impl std::future::Future<Output = Result<(bool, i64), kastellan_db::DbError>> + 'a {
            l1_remove_and_audit(pool, memory_id)
        }
        let _ = _signature_pin;
    }

    #[test]
    fn entities_approve_and_audit_signature_compile_pin() {
        fn _signature_pin<'a>(
            pool: &'a sqlx::PgPool,
            id: i64,
        ) -> impl std::future::Future<
            Output = Result<
                kastellan_db::entities::ApproveOutcome,
                kastellan_db::entities::EntitiesError,
            >,
        > + 'a {
            entities_approve_and_audit(pool, id)
        }
        let _ = _signature_pin;
    }

    #[test]
    fn entities_reject_and_audit_signature_compile_pin() {
        fn _signature_pin<'a>(
            pool: &'a sqlx::PgPool,
            id: i64,
        ) -> impl std::future::Future<
            Output = Result<
                kastellan_db::entities::RejectOutcome,
                kastellan_db::entities::EntitiesError,
            >,
        > + 'a {
            entities_reject_and_audit(pool, id)
        }
        let _ = _signature_pin;
    }

    #[test]
    fn l3_remove_and_audit_signature_compile_pin() {
        fn _pin<'a>(pool: &'a sqlx::PgPool, id: i64)
            -> impl std::future::Future<Output = Result<(bool, i64), kastellan_db::DbError>> + 'a {
            super::l3_remove_and_audit(pool, id)
        }
        let _ = _pin;
    }

    #[test]
    fn entities_merge_and_audit_signature_compile_pin() {
        fn _signature_pin<'a>(
            pool: &'a sqlx::PgPool,
            keep: i64,
            drops: &'a [i64],
        ) -> impl std::future::Future<
            Output = Result<
                kastellan_db::entities::MergeOutcome,
                kastellan_db::entities::EntitiesError,
            >,
        > + 'a {
            entities_merge_and_audit(pool, keep, drops)
        }
        let _ = _signature_pin;
    }
}
