//! `memory l3 {approve,pin}` — operator trust-ladder transitions.
//!
//! `approve` flips an untrusted crystallised skill to `user_approved`
//! (operator-CLI runnable + planner-surfaced); `pin` promotes an
//! already-`user_approved` skill to `pinned` (the strongest trust:
//! agent-autonomous invocation). Both open with the shared
//! [`load_skill_row`] prologue and re-run the approval gate via
//! [`decide_against_registry`]; the only trust flip is on the gate's
//! `Approve` arm. Every reject/error path leaves trust untouched and audits
//! the rejection (the security trail).

use std::collections::BTreeSet;
use std::process::ExitCode;

use kastellan_core::cassandra::types::{L3SkillCandidate, PythonSkillCandidate};
use kastellan_core::cli_audit::{
    l3_approve_and_audit, l3_approve_rejected_audit, l3_pin_and_audit, l3_pin_rejected_audit,
};
use kastellan_core::memory::l3_approval::{ApprovalDecision, SkillTrust};
use kastellan_core::memory::l3py_approval::evaluate_python_approval;

use super::shared::{decide_against_registry, load_skill_row};

pub(super) async fn memory_l3_approve(args: &[String]) -> ExitCode {
    // --- fetch + layer-guard the row (shared prologue) -------------------
    let (pool, row) = match load_skill_row(args, "approve").await {
        Ok(x) => x,
        Err(code) => return code,
    };
    let id = row.id;
    let body_sha256 = row.metadata.get("body_sha256").and_then(|v| v.as_str());

    // Python skills gate WITHOUT a registry snapshot (no tools to verify);
    // templated skills keep the registry re-gate below.
    if row.metadata.get("kind").and_then(|v| v.as_str()) == Some("python") {
        return approve_python_skill(&pool, &row).await;
    }

    // --- parse the stored template ---------------------------------------
    let template: L3SkillCandidate = match row
        .metadata
        .get("template")
        .cloned()
        .and_then(|t| serde_json::from_value(t).ok())
    {
        Some(t) => t,
        None => {
            let reasons = vec!["stored L3 row has no parseable 'template'".to_string()];
            let _ = l3_approve_rejected_audit(&pool, id, None, body_sha256, &reasons).await;
            eprintln!("memory l3 approve: id={id} has no parseable template; not approved");
            return ExitCode::from(1);
        }
    };
    let skill_name = template.name.clone();

    // --- registry snapshot → decision ------------------------------------
    let decision = match decide_against_registry(&pool, &template, "approve").await {
        Ok(d) => d,
        Err(code) => return code,
    };

    match decision {
        ApprovalDecision::Approve => {
            let tools: Vec<String> = {
                let mut s = BTreeSet::new();
                for st in &template.steps { s.insert(st.tool.clone()); }
                s.into_iter().collect()
            };
            let sha = body_sha256.unwrap_or("");
            if let Err(e) = l3_approve_and_audit(&pool, id, &skill_name, sha, &tools).await {
                eprintln!("memory l3 approve: {e}");
                return ExitCode::from(1);
            }
            println!("approved skill '{skill_name}' (#{id}) → trust=user_approved");
            ExitCode::from(0)
        }
        ApprovalDecision::Reject { reasons } => {
            let rendered: Vec<String> = reasons.iter().map(|r| r.to_string()).collect();
            let _ = l3_approve_rejected_audit(&pool, id, Some(&skill_name), body_sha256, &rendered).await;
            eprintln!("approval REJECTED for skill '{skill_name}' (#{id}):");
            for r in &rendered { eprintln!("  - {r}"); }
            ExitCode::from(1)
        }
    }
}

/// `memory l3 pin <id>` — promote an already-`user_approved` skill to
/// `pinned` (the strongest trust: agent-autonomous invocation).
///
/// Enforces the trust ladder: the skill must currently be `user_approved`
/// (a refusal otherwise leaves trust untouched and audits `l3.pin_rejected`).
/// Because `pinned` grants autonomous invocation, the approval gate is
/// re-run against the latest `registry.loaded` snapshot as defence-in-depth —
/// the same check `approve` performs. The only trust flip is on the `Approve`
/// arm, via `l3_pin_and_audit`.
pub(super) async fn memory_l3_pin(args: &[String]) -> ExitCode {
    // --- fetch + layer-guard the row (shared prologue) -------------------
    let (pool, row) = match load_skill_row(args, "pin").await {
        Ok(x) => x,
        Err(code) => return code,
    };
    let id = row.id;
    let body_sha256 = row.metadata.get("body_sha256").and_then(|v| v.as_str());

    if row.metadata.get("kind").and_then(|v| v.as_str()) == Some("python") {
        return pin_python_skill(&pool, &row).await;
    }

    // --- ladder guard: must currently be user_approved -------------------
    let current = SkillTrust::from_metadata_str(
        row.metadata.get("trust").and_then(|v| v.as_str()).unwrap_or(""),
    );
    if current != SkillTrust::UserApproved {
        let reasons = vec![format!(
            "skill must be user_approved before pinning (current: {})",
            current.as_str()
        )];
        let _ = l3_pin_rejected_audit(&pool, id, None, &reasons).await;
        eprintln!(
            "memory l3 pin: id={id} is '{}', not user_approved; approve it first",
            current.as_str()
        );
        return ExitCode::from(1);
    }

    // --- parse the stored template ---------------------------------------
    let template: L3SkillCandidate = match row
        .metadata.get("template").cloned().and_then(|t| serde_json::from_value(t).ok())
    {
        Some(t) => t,
        None => {
            let reasons = vec!["stored L3 row has no parseable 'template'".to_string()];
            let _ = l3_pin_rejected_audit(&pool, id, None, &reasons).await;
            eprintln!("memory l3 pin: id={id} has no parseable template; not pinned");
            return ExitCode::from(1);
        }
    };
    let skill_name = template.name.clone();

    // --- registry snapshot → decision (defence-in-depth re-gate) ---------
    let decision = match decide_against_registry(&pool, &template, "pin").await {
        Ok(d) => d,
        Err(code) => return code,
    };

    match decision {
        ApprovalDecision::Approve => {
            let sha = body_sha256.unwrap_or("");
            if let Err(e) = l3_pin_and_audit(&pool, id, &skill_name, sha).await {
                eprintln!("memory l3 pin: {e}");
                return ExitCode::from(1);
            }
            println!(
                "pinned skill '{skill_name}' (#{id}) → trust=pinned (agent-autonomously invocable)"
            );
            ExitCode::from(0)
        }
        ApprovalDecision::Reject { reasons } => {
            let rendered: Vec<String> = reasons.iter().map(|r| r.to_string()).collect();
            let _ = l3_pin_rejected_audit(&pool, id, Some(&skill_name), &rendered).await;
            eprintln!("pin REJECTED for skill '{skill_name}' (#{id}):");
            for r in &rendered { eprintln!("  - {r}"); }
            ExitCode::from(1)
        }
    }
}

/// Approve a Python skill: parse `metadata.python`, run the pure
/// `evaluate_python_approval` gate (no registry), flip trust on Approve.
async fn approve_python_skill(pool: &sqlx::PgPool, row: &kastellan_db::memories::Memory) -> ExitCode {
    let id = row.id;
    let body_sha256 = row.metadata.get("body_sha256").and_then(|v| v.as_str());
    let cand: PythonSkillCandidate = match row
        .metadata.get("python").cloned().and_then(|p| serde_json::from_value(p).ok())
    {
        Some(c) => c,
        None => {
            let reasons = vec!["stored row has kind=python but no parseable 'python'".to_string()];
            let _ = l3_approve_rejected_audit(pool, id, None, body_sha256, &reasons).await;
            eprintln!("memory l3 approve: id={id} has no parseable python payload; not approved");
            return ExitCode::from(1);
        }
    };
    let skill_name = cand.name.clone();
    match evaluate_python_approval(&cand) {
        ApprovalDecision::Approve => {
            let sha = body_sha256.unwrap_or("");
            // Python skills reference no tools — empty tools list in the audit.
            let tools: Vec<String> = Vec::new();
            if let Err(e) = l3_approve_and_audit(pool, id, &skill_name, sha, &tools).await {
                eprintln!("memory l3 approve: {e}");
                return ExitCode::from(1);
            }
            println!("approved python skill '{skill_name}' (#{id}) → trust=user_approved");
            ExitCode::from(0)
        }
        ApprovalDecision::Reject { reasons } => {
            let rendered: Vec<String> = reasons.iter().map(|r| r.to_string()).collect();
            let _ = l3_approve_rejected_audit(pool, id, Some(&skill_name), body_sha256, &rendered).await;
            eprintln!("approval REJECTED for python skill '{skill_name}' (#{id}):");
            for r in &rendered { eprintln!("  - {r}"); }
            ExitCode::from(1)
        }
    }
}

/// Pin a Python skill: ladder-guard (must be user_approved) then re-gate with
/// `evaluate_python_approval`, flip to pinned on Approve.
async fn pin_python_skill(pool: &sqlx::PgPool, row: &kastellan_db::memories::Memory) -> ExitCode {
    let id = row.id;
    let body_sha256 = row.metadata.get("body_sha256").and_then(|v| v.as_str());
    let current = SkillTrust::from_metadata_str(
        row.metadata.get("trust").and_then(|v| v.as_str()).unwrap_or(""),
    );
    if current != SkillTrust::UserApproved {
        let reasons = vec![format!(
            "skill must be user_approved before pinning (current: {})",
            current.as_str()
        )];
        let _ = l3_pin_rejected_audit(pool, id, None, &reasons).await;
        eprintln!("memory l3 pin: id={id} is '{}', not user_approved; approve it first", current.as_str());
        return ExitCode::from(1);
    }
    let cand: PythonSkillCandidate = match row
        .metadata.get("python").cloned().and_then(|p| serde_json::from_value(p).ok())
    {
        Some(c) => c,
        None => {
            let reasons = vec!["stored row has kind=python but no parseable 'python'".to_string()];
            let _ = l3_pin_rejected_audit(pool, id, None, &reasons).await;
            eprintln!("memory l3 pin: id={id} has no parseable python payload; not pinned");
            return ExitCode::from(1);
        }
    };
    let skill_name = cand.name.clone();
    match evaluate_python_approval(&cand) {
        ApprovalDecision::Approve => {
            let sha = body_sha256.unwrap_or("");
            if let Err(e) = l3_pin_and_audit(pool, id, &skill_name, sha).await {
                eprintln!("memory l3 pin: {e}");
                return ExitCode::from(1);
            }
            println!("pinned python skill '{skill_name}' (#{id}) → trust=pinned (agent-autonomously invocable)");
            ExitCode::from(0)
        }
        ApprovalDecision::Reject { reasons } => {
            let rendered: Vec<String> = reasons.iter().map(|r| r.to_string()).collect();
            let _ = l3_pin_rejected_audit(pool, id, Some(&skill_name), &rendered).await;
            eprintln!("pin REJECTED for python skill '{skill_name}' (#{id}):");
            for r in &rendered { eprintln!("  - {r}"); }
            ExitCode::from(1)
        }
    }
}
