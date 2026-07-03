//! Producer-side audit helpers for the L1 / L3 memory layers.
//!
//! Each helper composes a memory-layer mutation (promote / remove /
//! trust-flip) with a best-effort `actor='cli'` audit row, returning the
//! DB outcome plus the audit row id (`0` when the best-effort audit
//! insert failed — logged at WARN, never propagated). The gate decisions
//! (approval / pin eligibility) are made by the callers in
//! `crate::memory::l3_approval`; these helpers only compose the mutation
//! with its audit row. See the [`crate::cli_audit`] module doc for the
//! shared best-effort rationale.

use sqlx::PgPool;

use crate::cli_audit::CLI_AUDIT_ACTOR;
use crate::scheduler::audit::{
    build_l1_write_payload, build_l3_approve_rejected_payload, build_l3_approved_payload,
    build_l3_pin_rejected_payload, build_l3_pinned_payload, build_l3_revoked_payload,
    ACTION_L1_ADDED, ACTION_L1_REMOVED, ACTION_L3_APPROVED, ACTION_L3_APPROVE_REJECTED,
    ACTION_L3_PINNED, ACTION_L3_PIN_REJECTED, ACTION_L3_REMOVED, ACTION_L3_REVOKED,
};

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
    use crate::memory::embedder::NoOpEmbedder;
    use crate::memory::l1_promote::{compute_body_sha256, promote_l1, validate_l1_body, L1Source};

    // Validate first so the body we audit and the body we SHA-256
    // both come from the same trimmed slice as promote_l1's internal
    // validation. validate_l1_body is cheap (pure CPU) so running it
    // twice (once here, once inside promote_l1) is fine.
    let trimmed = validate_l1_body(body)?.to_string();
    let source = L1Source::Operator;
    let outcome = promote_l1(pool, extractor, &NoOpEmbedder::new(), &trimmed, source.clone()).await?;
    let body_sha256 = compute_body_sha256(&trimmed);

    let payload = build_l1_write_payload(&outcome, &source, &body_sha256);
    let audit_id = match kastellan_db::audit::insert(
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
) -> Result<(bool, i64), kastellan_db::DbError> {
    use crate::memory::l1_promote::remove_l1;

    let deleted = remove_l1(pool, memory_id).await?;
    let payload = serde_json::json!({"memory_id": memory_id, "deleted": deleted});

    let audit_id = match kastellan_db::audit::insert(
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

/// Compose `memory::l3_crystallise::remove_l3` with one `actor='cli'
/// action='l3.removed'` audit row. The row is written even when
/// `deleted = false` (records the operator intent + missing-id outcome).
pub async fn l3_remove_and_audit(
    pool: &PgPool,
    memory_id: i64,
) -> Result<(bool, i64), kastellan_db::DbError> {
    use crate::memory::l3_crystallise::remove_l3;

    let deleted = remove_l3(pool, memory_id).await?;
    let payload = serde_json::json!({"memory_id": memory_id, "deleted": deleted});

    let audit_id = match kastellan_db::audit::insert(
        pool, CLI_AUDIT_ACTOR, ACTION_L3_REMOVED, payload,
    ).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, "l3.removed audit insert failed (best-effort)");
            0
        }
    };

    Ok((deleted, audit_id))
}

/// Flip an L3 row to `user_approved` and emit one `actor='cli'
/// action='l3.approved'` row. The gate decision is made by the caller
/// ([`crate::memory::l3_approval::evaluate_approval`]); this helper only
/// composes the trust flip with its audit row. Best-effort audit.
pub async fn l3_approve_and_audit(
    pool: &PgPool,
    memory_id: i64,
    skill_name: &str,
    body_sha256: &str,
    tools: &[String],
) -> Result<i64, kastellan_db::DbError> {
    use crate::memory::l3_approval::SkillTrust;

    kastellan_db::memories::set_skill_trust(pool, memory_id, SkillTrust::UserApproved.as_str()).await?;
    let payload = build_l3_approved_payload(memory_id, skill_name, body_sha256, tools);
    let audit_id = match kastellan_db::audit::insert(
        pool, CLI_AUDIT_ACTOR, ACTION_L3_APPROVED, payload,
    ).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, "l3.approved audit insert failed (best-effort)");
            0
        }
    };
    Ok(audit_id)
}

/// Emit one `actor='cli' action='l3.approve_rejected'` row. NO trust
/// change — the gate refused. Best-effort audit. Returns the audit id.
pub async fn l3_approve_rejected_audit(
    pool: &PgPool,
    memory_id: i64,
    skill_name: Option<&str>,
    body_sha256: Option<&str>,
    reasons: &[String],
) -> Result<i64, kastellan_db::DbError> {
    let payload = build_l3_approve_rejected_payload(memory_id, skill_name, body_sha256, reasons);
    let audit_id = match kastellan_db::audit::insert(
        pool, CLI_AUDIT_ACTOR, ACTION_L3_APPROVE_REJECTED, payload,
    ).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, "l3.approve_rejected audit insert failed (best-effort)");
            0
        }
    };
    Ok(audit_id)
}

/// Flip an L3 row to `untrusted` (a downgrade — no gate) and emit one
/// `actor='cli' action='l3.revoked'` row. Returns `(updated, audit_id)`,
/// mirroring [`l3_remove_and_audit`]. Best-effort audit.
pub async fn l3_revoke_and_audit(
    pool: &PgPool,
    memory_id: i64,
) -> Result<(bool, i64), kastellan_db::DbError> {
    use crate::memory::l3_approval::SkillTrust;

    let updated = kastellan_db::memories::set_skill_trust(pool, memory_id, SkillTrust::Untrusted.as_str()).await?;
    let payload = build_l3_revoked_payload(memory_id, updated);
    let audit_id = match kastellan_db::audit::insert(
        pool, CLI_AUDIT_ACTOR, ACTION_L3_REVOKED, payload,
    ).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, "l3.revoked audit insert failed (best-effort)");
            0
        }
    };
    Ok((updated, audit_id))
}

/// Flip an already-`user_approved` L3 row to `pinned` and emit one
/// `actor='cli' action='l3.pinned'` row. The gate (must currently be
/// `user_approved` + pass `evaluate_approval`) is enforced by the caller
/// (`memory_l3_pin`); this helper only composes the trust flip with its
/// audit row. Returns the audit row id (0 on best-effort audit failure).
pub async fn l3_pin_and_audit(
    pool: &PgPool,
    memory_id: i64,
    skill_name: &str,
    body_sha256: &str,
) -> Result<i64, kastellan_db::DbError> {
    use crate::memory::l3_approval::SkillTrust;

    kastellan_db::memories::set_skill_trust(pool, memory_id, SkillTrust::Pinned.as_str()).await?;
    let payload = build_l3_pinned_payload(memory_id, skill_name, body_sha256);
    let audit_id = match kastellan_db::audit::insert(
        pool, CLI_AUDIT_ACTOR, ACTION_L3_PINNED, payload,
    ).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, "l3.pinned audit insert failed (best-effort)");
            0
        }
    };
    Ok(audit_id)
}

/// Emit one `actor='cli' action='l3.pin_rejected'` row WITHOUT changing
/// trust (a refused pin leaves the row as-is). Best-effort audit.
pub async fn l3_pin_rejected_audit(
    pool: &PgPool,
    memory_id: i64,
    skill_name: Option<&str>,
    reasons: &[String],
) -> i64 {
    let payload = build_l3_pin_rejected_payload(memory_id, skill_name, reasons);
    match kastellan_db::audit::insert(pool, CLI_AUDIT_ACTOR, ACTION_L3_PIN_REJECTED, payload).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, "l3.pin_rejected audit insert failed (best-effort)");
            0
        }
    }
}
