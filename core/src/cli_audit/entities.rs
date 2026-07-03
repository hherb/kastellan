//! Producer-side audit helpers for the entity-review workflow
//! (approve / reject / merge extracted entity mentions).
//!
//! Each helper composes the corresponding `kastellan_db::entities`
//! mutation with a best-effort `actor='cli'` audit row, emitted **only
//! on the state-changing variant** — `AlreadyApproved` / `NotFound` and
//! the merge precondition errors produce no audit row. The outcome is
//! returned to the caller so the CLI can print a distinct stderr line
//! per variant. See the [`crate::cli_audit`] module doc for the shared
//! best-effort rationale.

use crate::cli_audit::CLI_AUDIT_ACTOR;
use crate::scheduler::audit::{
    build_entities_approved_payload, build_entities_merged_payload,
    build_entities_rejected_payload, ACTION_ENTITIES_APPROVED, ACTION_ENTITIES_MERGED,
    ACTION_ENTITIES_REJECTED,
};

/// Compose `kastellan_db::entities::approve_entity` with one
/// `actor='cli' action='entities.approved'` audit row. The audit row is
/// emitted ONLY on the `Approved` variant (state-changing path);
/// `AlreadyApproved` and `NotFound` produce no audit row.
///
/// Returns the `ApproveOutcome` so the CLI can produce distinct stderr
/// lines per outcome.
pub async fn entities_approve_and_audit(
    pool: &sqlx::PgPool,
    id: i64,
) -> Result<kastellan_db::entities::ApproveOutcome, kastellan_db::entities::EntitiesError> {
    let outcome = kastellan_db::entities::approve_entity(pool, id).await?;
    if let kastellan_db::entities::ApproveOutcome::Approved { kind, name } = &outcome {
        let payload = build_entities_approved_payload(id, kind, name);
        if let Err(e) = kastellan_db::audit::insert(
            pool, CLI_AUDIT_ACTOR, ACTION_ENTITIES_APPROVED, payload,
        ).await {
            tracing::warn!(error = %e, entity_id = id,
                "entities_approve_and_audit: audit insert failed (best-effort)");
        }
    }
    Ok(outcome)
}

/// Compose `kastellan_db::entities::reject_entity` with one
/// `actor='cli' action='entities.rejected'` audit row. The audit row is
/// emitted ONLY on the `Rejected` variant; `NotFound` produces no row.
pub async fn entities_reject_and_audit(
    pool: &sqlx::PgPool,
    id: i64,
) -> Result<kastellan_db::entities::RejectOutcome, kastellan_db::entities::EntitiesError> {
    let outcome = kastellan_db::entities::reject_entity(pool, id).await?;
    if let kastellan_db::entities::RejectOutcome::Rejected { kind, name, mentions_dropped } = &outcome {
        let payload = build_entities_rejected_payload(id, kind, name, *mentions_dropped);
        if let Err(e) = kastellan_db::audit::insert(
            pool, CLI_AUDIT_ACTOR, ACTION_ENTITIES_REJECTED, payload,
        ).await {
            tracing::warn!(error = %e, entity_id = id,
                "entities_reject_and_audit: audit insert failed (best-effort)");
        }
    }
    Ok(outcome)
}

/// Compose `kastellan_db::entities::merge_entities` with one
/// `actor='cli' action='entities.merged'` audit row on the successful
/// path. Precondition errors (KindMismatch / NotFound / NoDropIds /
/// KeepInDropList) propagate to the caller without an audit row.
pub async fn entities_merge_and_audit(
    pool: &sqlx::PgPool,
    keep_id: i64,
    drop_ids: &[i64],
) -> Result<kastellan_db::entities::MergeOutcome, kastellan_db::entities::EntitiesError> {
    let outcome = kastellan_db::entities::merge_entities(pool, keep_id, drop_ids).await?;
    let payload = build_entities_merged_payload(
        outcome.kept_id,
        &outcome.kept_kind,
        &outcome.kept_name,
        &outcome.dropped_ids,
        outcome.links_retargeted,
        outcome.links_dropped_as_duplicate,
    );
    if let Err(e) = kastellan_db::audit::insert(
        pool, CLI_AUDIT_ACTOR, ACTION_ENTITIES_MERGED, payload,
    ).await {
        tracing::warn!(error = %e, kept_id = outcome.kept_id,
            "entities_merge_and_audit: audit insert failed (best-effort)");
    }
    Ok(outcome)
}
