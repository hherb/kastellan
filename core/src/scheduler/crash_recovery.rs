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
//! ## Timestamp semantics (read before joining on `audit_log.ts`)
//!
//! The `task.crashed` row's `audit_log.ts` is **detection time, not
//! crash time** — it's stamped when this module's INSERT lands at
//! daemon startup, which can be hours after the previous daemon
//! actually died. `tasks.finished_at` carries the same `now()`-at-sweep
//! value and has the same caveat. Observation-phase queries that
//! compute latency or recency from this row's `ts` will systematically
//! over-attribute time to crashed tasks.
//!
//! The actual crash time is bounded only on the lower side: it
//! happened at or after `tasks.started_at` (set by `claim_one` at
//! claim time; not renewed during execution). `lease_expires_at` is
//! NOT an upper bound — it's the moment the sweep first becomes
//! *eligible* to detect the crash, not when the crash occurred. So
//! `started_at ≤ crash_time ≤ audit_log.ts`, with no tighter upper
//! bound recoverable from the row alone.
//!
//! ## Finalize summary row (added 2026-05-13)
//!
//! For each recovered task this module **also** writes one
//! `actor='scheduler' action='task.finalize'` row immediately after the
//! `task.crashed` lifecycle row, mirroring the live-runtime ordering
//! (`drain_lane` writes `task.<state>` then `task.finalize`). The
//! payload uses [`super::audit::build_crashed_finalize_payload`], which
//! emits `total_llm_calls` and `total_dispatch_calls` as JSON `null`
//! (the dead daemon's in-memory counters are unrecoverable) and a
//! computed `total_duration_ms` from `started_at` (set by `claim_one`)
//! to `finished_at` (set by the sweep's `UPDATE … SET finished_at =
//! now()`). With this row in place, observation-phase SQL grouping on
//! `action='task.finalize'` sees the crashed-task population — which
//! was previously invisible — and can distinguish "0 calls observed"
//! (runtime path) from "unknowable" (sweep path) by the JSON-null
//! marker.
//!
//! ## What this module deliberately does NOT do
//!
//! * **No re-enqueueing.** A crashed task is terminal; if the user
//!   wants to retry, they re-submit. The previous handover note
//!   ("future work; `sweep_crashed` does not yet re-enqueue") still
//!   applies.
//! * **No back-fill of `total_llm_calls` / `total_dispatch_calls` from
//!   the audit log.** In principle one could `SELECT COUNT(*)` the
//!   `agent/plan.formulate` and `tool:*` rows for the crashed task to
//!   recover the counters. Deferred: the cost is per-task SQL on every
//!   startup and observation phase hasn't established that the
//!   counters are needed for crashed tasks. The JSON-null shape is the
//!   honest "we don't know" signal in the meantime.

use sqlx::PgPool;
use time::OffsetDateTime;

use hhagent_db::tasks::{self, Task};
use hhagent_db::DbError;

use super::audit::{
    action_task_terminal, build_crashed_finalize_payload, build_lifecycle_payload,
    ACTION_TASK_FINALIZE, SCHEDULER_AUDIT_ACTOR,
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
        emit_task_finalize_row(pool, task).await;
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

/// Insert one `actor='scheduler' action='task.finalize'` summary row
/// for a crashed task. Same posture as [`emit_task_crashed_row`]
/// (best-effort).
///
/// `task.finished_at` is always `Some` after `sweep_crashed` (the
/// `UPDATE … SET finished_at = now()` is unconditional), but the
/// column type is `Option<OffsetDateTime>` so we defend with a fallback
/// to the local clock if the optional ever surprises us. The fallback
/// is surfaced via `tracing::error!` so the impossible case is loud,
/// not silent — an emitted row with the audit emitter's wall clock as
/// `finished_at` is off by the scheduler-lag delta, and operators
/// looking at the resulting row need to know that.
async fn emit_task_finalize_row(pool: &PgPool, task: &Task) {
    let finished_at = task.finished_at.unwrap_or_else(|| {
        tracing::error!(
            task_id = task.id,
            "scheduler::crash_recovery::emit_task_finalize_row: task.finished_at is None \
             after sweep_crashed — expected unconditional `UPDATE … SET finished_at = now()`; \
             falling back to local clock so the audit row still emits",
        );
        OffsetDateTime::now_utc()
    });
    let payload = build_crashed_finalize_payload(
        task.id,
        task.lane,
        task.plan_count,
        task.started_at,
        finished_at,
    );
    if let Err(e) = hhagent_db::audit::insert(
        pool,
        SCHEDULER_AUDIT_ACTOR,
        ACTION_TASK_FINALIZE,
        payload,
    )
    .await
    {
        tracing::warn!(
            task_id = task.id,
            error = %e,
            "audit insert for scheduler/task.finalize (crashed) row failed (best-effort)"
        );
    }
}
