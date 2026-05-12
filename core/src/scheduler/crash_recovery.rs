//! Startup crash-recovery sweep with `scheduler/task.crashed` audit
//! emission.
//!
//! At daemon start the previous instance may have died mid-run leaving
//! tasks in `running` state. [`hhagent_db::tasks::sweep_crashed`] is
//! the SQL-layer fix: it flips every `running` row whose lease elapsed
//! to `crashed`. That alone is enough for `tasks.state` to be correct,
//! but observation-phase SQL queries that pivot on the audit log can't
//! see those transitions — they're a `tasks.state` UPDATE, not an
//! `audit_log` row.
//!
//! [`sweep_and_audit`] is the public entry point [`crate::main`] (and
//! the supervisor_e2e regression test) calls instead of the bare
//! `tasks::sweep_crashed`. It writes one `actor='scheduler'`,
//! `action='task.crashed'` row per recovered task — the same lifecycle
//! shape the lane runner uses for the live `task.<state>` rows
//! (see [`super::audit::build_lifecycle_payload`]).
//!
//! ## Posture
//!
//! * **DB UPDATE is fail-closed** — if the sweep fails, the caller
//!   propagates the error. Running degraded against a half-recovered
//!   `tasks` table corrupts the audit story (live tasks claim
//!   `running` rows that pre-date this daemon).
//! * **Audit insert is best-effort** — same posture as the dispatcher
//!   chokepoint and the lane runner's `write_lifecycle_row`. A
//!   transient `audit_log` insert failure is logged at WARN and
//!   swallowed so we don't roll back a successful sweep.
//!
//! ## What this module deliberately does NOT do
//!
//! * **No `task.finalize` summary row** for crashed tasks. The
//!   finalize-row payload (see [`super::audit::TaskFinalizeStats`])
//!   carries aggregate counters (`total_llm_calls`,
//!   `total_dispatch_calls`, `total_duration_ms`) that died with the
//!   previous daemon. We could write the row with zero counters but
//!   that would be a misleading data shape for the consumers that
//!   subscribe to the finalize stream. Left as a follow-up.
//! * **No re-enqueueing.** A crashed task is terminal; if the user
//!   wants to retry, they re-submit. The previous handover note
//!   ("future work; `sweep_crashed` does not yet re-enqueue") still
//!   applies.

use sqlx::PgPool;

use hhagent_db::tasks::{self, Task};
use hhagent_db::DbError;

use super::audit::{
    action_task_terminal, build_lifecycle_payload, SCHEDULER_AUDIT_ACTOR,
};

/// Run [`tasks::sweep_crashed`] and emit one `scheduler/task.crashed`
/// audit row per recovered task. Returns the number of tasks crashed.
///
/// The DB UPDATE is fail-closed: any error from `sweep_crashed`
/// propagates. The per-row audit inserts are best-effort and never
/// fail this function (errors are logged via `tracing::warn!`).
///
/// Called once at daemon startup from `core/src/main.rs` before the
/// scheduler is spawned. Safe to re-run (the UPDATE is idempotent and
/// the audit rows are stamped with `id = nextval(...)`, so a second
/// call simply finds nothing to sweep).
pub async fn sweep_and_audit(pool: &PgPool) -> Result<usize, DbError> {
    let crashed = tasks::sweep_crashed(pool).await?;
    for task in &crashed {
        emit_task_crashed_row(pool, task).await;
    }
    Ok(crashed.len())
}

/// Insert one `actor='scheduler' action='task.crashed'` row carrying
/// the canonical lifecycle payload. Same posture as
/// [`super::runner::write_lifecycle_row`] — best-effort.
async fn emit_task_crashed_row(pool: &PgPool, task: &Task) {
    let action = action_task_terminal("crashed");
    let payload = build_lifecycle_payload(task.id, task.lane, task.plan_count);
    if let Err(e) =
        hhagent_db::audit::insert(pool, SCHEDULER_AUDIT_ACTOR, &action, payload).await
    {
        tracing::warn!(
            task_id = task.id,
            error = %e,
            "audit insert for scheduler/task.crashed row failed (best-effort)"
        );
    }
}
