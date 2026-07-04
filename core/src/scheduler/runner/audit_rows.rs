//! Best-effort `actor='scheduler'` audit-row writers called by
//! [`super::drain_lane`] after each task finalizes.
//!
//! Every writer here shares one posture: the canonical lifecycle state
//! lives in the `tasks` table (and, for L1/L3, in the `memories` table);
//! these rows are observation-phase aids, not correctness signals. A DB
//! error inserting any of them is logged at `WARN` via `tracing` and
//! swallowed — a degraded audit write must never abort task finalize.
//!
//! The family:
//! - [`write_lifecycle_row`] — `task.<state>` pending→running / →terminal.
//! - [`write_finalize_row`] — the per-task `task.finalize` summary counters.
//! - [`write_l1_promoted_row`] — agent-raised L1 insight promotion.
//! - [`write_l3_crystallised_row`] — agent-raised templated-skill crystallisation.
//! - [`write_python_skill_crystallised_row`] — agent-raised Python-skill
//!   crystallisation (same row shape, tagged `kind: "python"`).

use sqlx::PgPool;
use time::OffsetDateTime;

use kastellan_db::tasks::{Lane, Task};

use crate::entity_extraction::EntityExtractor;
use crate::memory::embedder::Embedder;
use crate::scheduler::audit::{
    build_finalize_payload, build_l1_write_payload, build_l3_write_payload,
    build_lifecycle_payload, compute_duration_ms, TaskFinalizeStats, ACTION_L1_PROMOTED,
    ACTION_L3_CRYSTALLISED, ACTION_TASK_FINALIZE, SCHEDULER_AUDIT_ACTOR,
};
use crate::scheduler::inner_loop::InnerLoopResult;

/// Insert a `scheduler/task.<...>` lifecycle row. Errors are logged
/// at WARN and swallowed — the canonical lifecycle state lives in the
/// `tasks` table and the row is an observation-phase aid, not a
/// correctness signal.
pub(super) async fn write_lifecycle_row(
    pool: &PgPool,
    action: &str,
    task_id: i64,
    lane: Lane,
    plan_count: i32,
) {
    let payload = build_lifecycle_payload(task_id, lane, plan_count);
    if let Err(e) =
        kastellan_db::audit::insert(pool, SCHEDULER_AUDIT_ACTOR, action, payload).await
    {
        tracing::warn!(
            task_id, action, error = %e,
            "audit insert for scheduler lifecycle row failed (best-effort)"
        );
    }
}

/// Insert the per-task `scheduler/task.finalize` summary row. Best-
/// effort, same posture as [`write_lifecycle_row`].
pub(super) async fn write_finalize_row(
    pool: &PgPool,
    claimed: &Task,
    final_state: &str,
    result: &InnerLoopResult,
    finished_at: OffsetDateTime,
) {
    let stats = TaskFinalizeStats {
        // `result.plan_count` is the inner loop's u32 counter; the DB
        // column is i32. The cap on plans is small (single digits in
        // practice), so the saturation is operationally dead code, but
        // a silent `as i32` truncation would be a subtle bug if a
        // future change ever lifted the cap.
        plan_count: i32::try_from(result.plan_count).unwrap_or(i32::MAX),
        total_llm_calls: result.plan_count,
        total_dispatch_calls: result.dispatch_count,
        total_duration_ms: compute_duration_ms(claimed.started_at, finished_at),
        started_at: claimed.started_at,
        finished_at,
    };
    let payload = build_finalize_payload(claimed.id, claimed.lane, final_state, &stats);
    if let Err(e) = kastellan_db::audit::insert(
        pool, SCHEDULER_AUDIT_ACTOR, ACTION_TASK_FINALIZE, payload,
    ).await {
        tracing::warn!(
            task_id = claimed.id, state = final_state, error = %e,
            "audit insert for scheduler task.finalize row failed (best-effort)"
        );
    }
}

/// Best-effort agent-raised L1 promotion writer. Called by
/// [`super::drain_lane`] after the `task.finalize` audit row is written.
///
/// Posture: errors are logged at WARN and swallowed. The task is
/// already finalized in the canonical `tasks` table; the L1 row +
/// audit row are observability aids, not correctness signals.
/// Validation errors from `promote_l1` are also swallowed (with
/// distinct WARN diagnostics so the operator can see which path failed).
pub(super) async fn write_l1_promoted_row(
    pool: &PgPool,
    extractor: &dyn EntityExtractor,
    embedder: &dyn Embedder,
    task_id: i64,
    insight: &str,
) {
    use crate::memory::l1_promote::{promote_l1, L1Error, L1Source};

    let source = L1Source::AgentRaised { task_id };
    let outcome = match promote_l1(pool, extractor, embedder, insight, source.clone()).await {
        Ok(o) => o,
        Err(L1Error::Validation(msg)) => {
            tracing::warn!(
                task_id, error = %msg,
                "agent-raised L1 promotion rejected on validation (skipping audit row)"
            );
            return;
        }
        Err(L1Error::Db(e)) => {
            tracing::warn!(
                task_id, error = %e,
                "agent-raised L1 promotion DB error (skipping audit row)"
            );
            return;
        }
    };

    let body_sha256 = crate::memory::l1_promote::compute_body_sha256(insight.trim());
    let payload = build_l1_write_payload(&outcome, &source, &body_sha256);

    if let Err(e) = kastellan_db::audit::insert(
        pool, SCHEDULER_AUDIT_ACTOR, ACTION_L1_PROMOTED, payload,
    ).await {
        tracing::warn!(
            task_id, error = %e,
            "audit insert for scheduler l1.promoted row failed (best-effort)"
        );
    }
}

/// Crystallise the agent-raised L3 skill + emit one `actor='scheduler'
/// action='l3.crystallised'` audit row. Best-effort: errors (validation
/// or DB) are logged at WARN and swallowed — the task is already
/// finalized; the L3 row + audit row are observability aids, not
/// correctness signals.
pub(super) async fn write_l3_crystallised_row(
    pool: &PgPool,
    task_id: i64,
    skill: &crate::cassandra::types::L3SkillCandidate,
) {
    use crate::memory::l3_crystallise::{
        compute_template_sha256, crystallise_l3, validate_l3_skill, L3Error, L3Source,
    };

    let source = L3Source::AgentRaised { task_id };
    let outcome = match crystallise_l3(pool, skill, source.clone()).await {
        Ok(o) => o,
        Err(L3Error::Validation(msg)) => {
            tracing::warn!(
                task_id, error = %msg,
                "agent-raised L3 crystallisation rejected on validation (skipping audit row)"
            );
            return;
        }
        Err(L3Error::Db(e)) => {
            tracing::warn!(
                task_id, error = %e,
                "agent-raised L3 crystallisation DB error (skipping audit row)"
            );
            return;
        }
    };

    // Recompute over the SAME normalised candidate the writer stored, so
    // the audited body_sha256 + skill_name match the stored row exactly.
    // crystallise_l3 already validated successfully above, so this
    // re-validation cannot fail; the Err arm is defensive/unreachable.
    let normalised = match validate_l3_skill(skill) {
        Ok(n) => n,
        Err(_) => return,
    };
    let body_sha256 = compute_template_sha256(&normalised);
    let payload = build_l3_write_payload(&outcome, &source, &normalised.name, &body_sha256);

    if let Err(e) = kastellan_db::audit::insert(
        pool, SCHEDULER_AUDIT_ACTOR, ACTION_L3_CRYSTALLISED, payload,
    ).await {
        tracing::warn!(
            task_id, error = %e,
            "audit insert for scheduler l3.crystallised row failed (best-effort)"
        );
    }
}

/// Crystallise the agent-raised Python skill + emit one `actor='scheduler'
/// action='l3.crystallised'` audit row carrying `kind: "python"`. Best-effort
/// (validation/DB errors logged at WARN and swallowed), mirroring
/// [`write_l3_crystallised_row`].
pub(super) async fn write_python_skill_crystallised_row(
    pool: &PgPool,
    task_id: i64,
    skill: &crate::cassandra::types::PythonSkillCandidate,
) {
    use crate::memory::l3_crystallise::L3Source;
    use crate::memory::l3py_crystallise::{
        compute_python_sha256, crystallise_python_skill, validate_python_skill, PyError,
        PyWriteOutcome,
    };

    let source = L3Source::AgentRaised { task_id };
    let outcome = match crystallise_python_skill(pool, skill, source.clone()).await {
        Ok(o) => o,
        Err(PyError::Validation(msg)) => {
            tracing::warn!(
                task_id,
                error = %msg,
                "agent-raised python skill rejected on validation (skipping audit row)"
            );
            return;
        }
        Err(PyError::Db(e)) => {
            tracing::warn!(
                task_id,
                error = %e,
                "agent-raised python skill DB error (skipping audit row)"
            );
            return;
        }
    };

    // Recompute over the SAME normalised candidate the writer stored, so
    // the audited body_sha256 + skill_name match the stored row exactly.
    // crystallise_python_skill already validated successfully above, so
    // this re-validation cannot fail; the Err arm is defensive/unreachable.
    let normalised = match validate_python_skill(skill) {
        Ok(n) => n,
        Err(_) => return,
    };
    let body_sha256 = compute_python_sha256(&normalised);

    // Reuse the L3 crystallise payload shape; PyWriteOutcome maps 1:1 to
    // the L3WriteOutcome arms the builder expects. Add `kind: "python"` so
    // the audit tail can distinguish Python skills from templated ones.
    let l3_outcome = match outcome {
        PyWriteOutcome::Inserted { memory_id } => {
            crate::memory::l3_crystallise::L3WriteOutcome::Inserted { memory_id }
        }
        PyWriteOutcome::SkippedDuplicate { memory_id } => {
            crate::memory::l3_crystallise::L3WriteOutcome::SkippedDuplicate { memory_id }
        }
    };
    let mut payload =
        build_l3_write_payload(&l3_outcome, &source, &normalised.name, &body_sha256);
    if let serde_json::Value::Object(ref mut m) = payload {
        m.insert("kind".into(), serde_json::Value::String("python".into()));
    }

    if let Err(e) = kastellan_db::audit::insert(
        pool,
        SCHEDULER_AUDIT_ACTOR,
        ACTION_L3_CRYSTALLISED,
        payload,
    )
    .await
    {
        tracing::warn!(
            task_id,
            error = %e,
            "audit insert for scheduler python l3.crystallised row failed (best-effort)"
        );
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn write_l1_promoted_row_signature_compile_pin() {
        // Compile-only: the function exists with the widened signature
        // (pool, extractor, embedder, task_id, insight). Full DB-backed coverage is in
        // core/tests/scheduler_lanes_e2e.rs.
        fn _signature_pin<'a>(
            pool: &'a sqlx::PgPool,
            extractor: &'a crate::entity_extraction::NoOpEntityExtractor,
            embedder: &'a crate::memory::embedder::NoOpEmbedder,
            task_id: i64,
            insight: &'a str,
        ) -> impl std::future::Future<Output = ()> + 'a {
            super::write_l1_promoted_row(pool, extractor, embedder, task_id, insight)
        }
        let _ = _signature_pin;
    }
}
