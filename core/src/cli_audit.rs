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
    action_task_terminal, build_lifecycle_payload, build_l1_write_payload,
    build_producer_cancel_finalize_payload,
    build_entities_approved_payload, build_entities_rejected_payload,
    build_entities_merged_payload,
    ACTION_L1_ADDED, ACTION_L1_REMOVED, ACTION_TASK_FINALIZE, ACTION_TASK_SUBMITTED,
    ACTION_TOOLS_ALLOWLIST_ADD, ACTION_TOOLS_ALLOWLIST_REMOVE,
    ACTION_ENTITIES_APPROVED, ACTION_ENTITIES_REJECTED, ACTION_ENTITIES_MERGED,
    ACTION_RELATION_KINDS_ADD, ACTION_RELATION_KINDS_REMOVE,
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

/// Add one allowlist entry and emit one `actor='cli'
/// action='tools.allowlist.add'` audit row on success.
///
/// Returns the DB-layer bool: `Ok(true)` means a row was INSERTed (and
/// an audit row was emitted, best-effort); `Ok(false)` means the entry
/// already existed and **no audit row is written** (the operator's
/// state-change intent did not materialise; logging it would confuse
/// "what was true at time T" reconstructions).
///
/// Audit-insert posture: best-effort. A transient DB failure on the
/// audit row is logged via `tracing::warn!` and swallowed; the
/// underlying `db::tool_allowlists::add` outcome propagates either way.
pub async fn tools_allowlist_add_and_audit(
    pool: &PgPool,
    tool: &str,
    argv0: &str,
) -> Result<bool, hhagent_db::tool_allowlists::ToolAllowlistError> {
    let inserted = hhagent_db::tool_allowlists::add(pool, tool, argv0, CLI_AUDIT_ACTOR).await?;
    if inserted {
        let payload = serde_json::json!({ "tool": tool, "argv0": argv0 });
        if let Err(e) = hhagent_db::audit::insert(
            pool,
            CLI_AUDIT_ACTOR,
            ACTION_TOOLS_ALLOWLIST_ADD,
            payload,
        )
        .await
        {
            tracing::warn!(
                error = %e,
                tool = tool,
                argv0 = argv0,
                "tools_allowlist_add_and_audit: audit insert failed"
            );
        }
    }
    Ok(inserted)
}

/// Remove one allowlist entry and emit one `actor='cli'
/// action='tools.allowlist.remove'` audit row on success.
///
/// Returns `Ok(true)` if a row was deleted (and audit row emitted
/// best-effort); `Ok(false)` if nothing matched (no audit row).
pub async fn tools_allowlist_remove_and_audit(
    pool: &PgPool,
    tool: &str,
    argv0: &str,
) -> Result<bool, hhagent_db::tool_allowlists::ToolAllowlistError> {
    let removed = hhagent_db::tool_allowlists::remove(pool, tool, argv0).await?;
    if removed {
        let payload = serde_json::json!({ "tool": tool, "argv0": argv0 });
        if let Err(e) = hhagent_db::audit::insert(
            pool,
            CLI_AUDIT_ACTOR,
            ACTION_TOOLS_ALLOWLIST_REMOVE,
            payload,
        )
        .await
        {
            tracing::warn!(
                error = %e,
                tool = tool,
                argv0 = argv0,
                "tools_allowlist_remove_and_audit: audit insert failed"
            );
        }
    }
    Ok(removed)
}

/// Add one relation-kind row and emit one `actor='cli'
/// action='relation_kinds.add'` audit row on success.
///
/// Returns the DB-layer bool: `Ok(true)` means a row was INSERTed (and
/// an audit row was emitted, best-effort); `Ok(false)` means the kind
/// was already present and **no audit row is written** (the operator's
/// state-change intent did not materialise; logging it would confuse
/// "what was true at time T" reconstructions). Mirror of the posture
/// in [`tools_allowlist_add_and_audit`].
///
/// Audit-insert posture: best-effort. A transient DB failure on the
/// audit row is logged via `tracing::warn!` and swallowed; the
/// underlying `db::relation_kinds::add` outcome propagates either way.
///
/// **Requires an admin-pool connection** ([`hhagent_db::pool::connect_admin_pool`])
/// — the runtime role does not have INSERT on `relation_kinds`
/// (migration 0017 REVOKE). Passing a runtime-role pool yields
/// `Err(RelationKindError::Db(...))` carrying a Postgres `permission
/// denied` error.
pub async fn relation_kinds_add_and_audit(
    pool: &PgPool,
    kind: &str,
    description: Option<&str>,
) -> Result<bool, hhagent_db::relation_kinds::RelationKindError> {
    let inserted = hhagent_db::relation_kinds::add(pool, kind, description).await?;
    if inserted {
        // `description: null` is the explicit "unset" wire value so a
        // downstream payload reader can distinguish "operator did not
        // pass --description" from "field absent due to schema drift".
        let payload = serde_json::json!({
            "kind": kind,
            "description": description,
        });
        if let Err(e) = hhagent_db::audit::insert(
            pool,
            CLI_AUDIT_ACTOR,
            ACTION_RELATION_KINDS_ADD,
            payload,
        )
        .await
        {
            tracing::warn!(
                error = %e,
                kind = kind,
                "relation_kinds_add_and_audit: audit insert failed"
            );
        }
    }
    Ok(inserted)
}

/// Remove one relation-kind row and emit one `actor='cli'
/// action='relation_kinds.remove'` audit row on success.
///
/// Returns `Ok(true)` if a row was deleted (and audit row emitted
/// best-effort); `Ok(false)` if nothing matched (no audit row). The
/// `'undefined'` sentinel is rejected up front by `db::relation_kinds::remove`
/// with `RelationKindError::RemovalOfUndefinedRejected`; on that path
/// no row is deleted and no audit row is written.
///
/// **Requires an admin-pool connection** — see [`relation_kinds_add_and_audit`].
pub async fn relation_kinds_remove_and_audit(
    pool: &PgPool,
    kind: &str,
) -> Result<bool, hhagent_db::relation_kinds::RelationKindError> {
    let removed = hhagent_db::relation_kinds::remove(pool, kind).await?;
    if removed {
        let payload = serde_json::json!({ "kind": kind });
        if let Err(e) = hhagent_db::audit::insert(
            pool,
            CLI_AUDIT_ACTOR,
            ACTION_RELATION_KINDS_REMOVE,
            payload,
        )
        .await
        {
            tracing::warn!(
                error = %e,
                kind = kind,
                "relation_kinds_remove_and_audit: audit insert failed"
            );
        }
    }
    Ok(removed)
}

/// Compose `memory::l1_promote::promote_l1` with one `actor='cli'
/// action='l1.added'` audit row. The audit row IS written even on
/// `SkippedDuplicate` (records the operator intent); it is NOT
/// written on `L1Error::Validation` (operator sees the error on
/// stderr; mirrors `l0_seed`'s posture).
///
/// Returns the `L1WriteOutcome` and the audit row id (0 if the
/// audit insert failed; that's logged at WARN but doesn't propagate).
pub async fn l1_add_and_audit(
    pool: &PgPool,
    extractor: &dyn crate::entity_extraction::EntityExtractor,
    body: &str,
) -> Result<(crate::memory::l1_promote::L1WriteOutcome, i64), crate::memory::l1_promote::L1Error> {
    use crate::memory::l1_promote::{compute_body_sha256, promote_l1, validate_l1_body, L1Source};

    // Validate first so the body we audit and the body we SHA-256
    // both come from the same trimmed slice as promote_l1's internal
    // validation. validate_l1_body is cheap (pure CPU) so running it
    // twice (once here, once inside promote_l1) is fine.
    let trimmed = validate_l1_body(body)?.to_string();
    let source = L1Source::Operator;
    let outcome = promote_l1(pool, extractor, &trimmed, source.clone()).await?;
    let body_sha256 = compute_body_sha256(&trimmed);

    let payload = build_l1_write_payload(&outcome, &source, &body_sha256);
    let audit_id = match hhagent_db::audit::insert(
        pool, CLI_AUDIT_ACTOR, ACTION_L1_ADDED, payload,
    ).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, "l1.added audit insert failed (best-effort)");
            0
        }
    };

    Ok((outcome, audit_id))
}

/// Compose `memory::l1_promote::remove_l1` with one `actor='cli'
/// action='l1.removed'` audit row. Audit row is written even when
/// `deleted = false` (records the operator intent + the missing-id
/// outcome).
pub async fn l1_remove_and_audit(
    pool: &PgPool,
    memory_id: i64,
) -> Result<(bool, i64), hhagent_db::DbError> {
    use crate::memory::l1_promote::remove_l1;

    let deleted = remove_l1(pool, memory_id).await?;
    let payload = serde_json::json!({"memory_id": memory_id, "deleted": deleted});

    let audit_id = match hhagent_db::audit::insert(
        pool, CLI_AUDIT_ACTOR, ACTION_L1_REMOVED, payload,
    ).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, "l1.removed audit insert failed (best-effort)");
            0
        }
    };

    Ok((deleted, audit_id))
}

/// Compose `hhagent_db::entities::approve_entity` with one
/// `actor='cli' action='entities.approved'` audit row. The audit row is
/// emitted ONLY on the `Approved` variant (state-changing path);
/// `AlreadyApproved` and `NotFound` produce no audit row.
///
/// Returns the `ApproveOutcome` so the CLI can produce distinct stderr
/// lines per outcome.
pub async fn entities_approve_and_audit(
    pool: &sqlx::PgPool,
    id: i64,
) -> Result<hhagent_db::entities::ApproveOutcome, hhagent_db::entities::EntitiesError> {
    let outcome = hhagent_db::entities::approve_entity(pool, id).await?;
    if let hhagent_db::entities::ApproveOutcome::Approved { kind, name } = &outcome {
        let payload = build_entities_approved_payload(id, kind, name);
        if let Err(e) = hhagent_db::audit::insert(
            pool, CLI_AUDIT_ACTOR, ACTION_ENTITIES_APPROVED, payload,
        ).await {
            tracing::warn!(error = %e, entity_id = id,
                "entities_approve_and_audit: audit insert failed (best-effort)");
        }
    }
    Ok(outcome)
}

/// Compose `hhagent_db::entities::reject_entity` with one
/// `actor='cli' action='entities.rejected'` audit row. The audit row is
/// emitted ONLY on the `Rejected` variant; `NotFound` produces no row.
pub async fn entities_reject_and_audit(
    pool: &sqlx::PgPool,
    id: i64,
) -> Result<hhagent_db::entities::RejectOutcome, hhagent_db::entities::EntitiesError> {
    let outcome = hhagent_db::entities::reject_entity(pool, id).await?;
    if let hhagent_db::entities::RejectOutcome::Rejected { kind, name, mentions_dropped } = &outcome {
        let payload = build_entities_rejected_payload(id, kind, name, *mentions_dropped);
        if let Err(e) = hhagent_db::audit::insert(
            pool, CLI_AUDIT_ACTOR, ACTION_ENTITIES_REJECTED, payload,
        ).await {
            tracing::warn!(error = %e, entity_id = id,
                "entities_reject_and_audit: audit insert failed (best-effort)");
        }
    }
    Ok(outcome)
}

/// Compose `hhagent_db::entities::merge_entities` with one
/// `actor='cli' action='entities.merged'` audit row on the successful
/// path. Precondition errors (KindMismatch / NotFound / NoDropIds /
/// KeepInDropList) propagate to the caller without an audit row.
pub async fn entities_merge_and_audit(
    pool: &sqlx::PgPool,
    keep_id: i64,
    drop_ids: &[i64],
) -> Result<hhagent_db::entities::MergeOutcome, hhagent_db::entities::EntitiesError> {
    let outcome = hhagent_db::entities::merge_entities(pool, keep_id, drop_ids).await?;
    let payload = build_entities_merged_payload(
        outcome.kept_id,
        &outcome.kept_kind,
        &outcome.kept_name,
        &outcome.dropped_ids,
        outcome.links_retargeted,
        outcome.links_dropped_as_duplicate,
    );
    if let Err(e) = hhagent_db::audit::insert(
        pool, CLI_AUDIT_ACTOR, ACTION_ENTITIES_MERGED, payload,
    ).await {
        tracing::warn!(error = %e, kept_id = outcome.kept_id,
            "entities_merge_and_audit: audit insert failed (best-effort)");
    }
    Ok(outcome)
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
        ) -> impl std::future::Future<Output = Result<(bool, i64), hhagent_db::DbError>> + 'a {
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
                hhagent_db::entities::ApproveOutcome,
                hhagent_db::entities::EntitiesError,
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
                hhagent_db::entities::RejectOutcome,
                hhagent_db::entities::EntitiesError,
            >,
        > + 'a {
            entities_reject_and_audit(pool, id)
        }
        let _ = _signature_pin;
    }

    #[test]
    fn entities_merge_and_audit_signature_compile_pin() {
        fn _signature_pin<'a>(
            pool: &'a sqlx::PgPool,
            keep: i64,
            drops: &'a [i64],
        ) -> impl std::future::Future<
            Output = Result<
                hhagent_db::entities::MergeOutcome,
                hhagent_db::entities::EntitiesError,
            >,
        > + 'a {
            entities_merge_and_audit(pool, keep, drops)
        }
        let _ = _signature_pin;
    }
}
