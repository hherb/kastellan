//! Operator-path orchestration of an approved Python skill. Reuses the
//! templated [`crate::memory::l3_invoke::InvokeReport`] + `run_steps` so the
//! daemon → serialize → CLI → render pipeline is byte-for-byte the same as the
//! templated path; only the gate ([`super::pure::prepare_python_invocation`])
//! and the single-step build differ. Audit rows reuse the L3 invoke actions
//! with `kind:"python"` injected, keeping one coherent skill-lifecycle stream.
//!
//! Like the templated operator path: dry-run by default, NO CASSANDRA review —
//! an operator running their own approved skill with explicit intent is an
//! authorised action; the reviewer polices *agent*-formulated plans (see
//! [`super::agent`]).

use serde_json::Value;
use sqlx::PgPool;

use crate::cassandra::types::{L3TemplateStep, PythonSkillCandidate};
use crate::cli_audit::CLI_AUDIT_ACTOR;
use crate::memory::l3_approval::SkillTrust;
use crate::memory::l3_invoke::{run_steps, InvokeRefusal, InvokeReport};
use crate::scheduler::audit::{
    build_l3_invoke_outcome_payload, build_l3_invoke_rejected_payload, build_l3_invoked_payload,
    ACTION_L3_INVOKED, ACTION_L3_INVOKE_OUTCOME, ACTION_L3_INVOKE_REJECTED,
};
use crate::scheduler::inner_loop::StepDispatcher;

use super::pure::{prepare_python_invocation, python_exec_step, with_python_kind};

/// Pool-free, unit-testable seam: gate via [`prepare_python_invocation`], and
/// on success build the single `python.exec` step. Returns an [`InvokeRefusal`]
/// on any gate failure.
///
/// This is a pure-ish function (no I/O) so the operator logic can be tested
/// independently of the database.
pub fn prepare_python_steps(
    candidate: &PythonSkillCandidate,
    stored_trust: SkillTrust,
    stored_sha256: &str,
) -> Result<Vec<L3TemplateStep>, InvokeRefusal> {
    let code = prepare_python_invocation(candidate, stored_trust, stored_sha256)?;
    Ok(vec![python_exec_step(&code)])
}

/// Orchestrate operator-triggered invocation of an approved Python skill.
///
/// `memory_id` and `stored_sha256` come from the stored Python-skill row;
/// `candidate` is the freshly-loaded skill. `execute == false` ⇒ dry-run: no
/// dispatch and no `l3.invoked` / `l3.invoke_outcome` rows — but a refusal is
/// **always** audited (`l3.invoke_rejected`) regardless of `execute`, because a
/// refused run attempt is a security event worth a trail.
///
/// Audit writes are best-effort (warn-on-failure), matching the chokepoint
/// posture in the templated operator path. The `kind:"python"` field is
/// injected into every audit payload so one query distinguishes Python from
/// templated skill events.
#[allow(clippy::too_many_arguments)]
pub async fn invoke_python_skill(
    pool: &PgPool,
    memory_id: i64,
    dispatcher: &dyn StepDispatcher,
    candidate: &PythonSkillCandidate,
    stored_trust: SkillTrust,
    stored_sha256: &str,
    execute: bool,
) -> InvokeReport {
    // Gate once — the entire function never retries or re-gates.
    let steps = match prepare_python_steps(candidate, stored_trust, stored_sha256) {
        Ok(steps) => steps,
        Err(InvokeRefusal { reasons }) => {
            let payload = with_python_kind(build_l3_invoke_rejected_payload(
                memory_id,
                &candidate.name,
                stored_sha256,
                &reasons,
            ));
            best_effort_audit(pool, ACTION_L3_INVOKE_REJECTED, payload).await;
            return InvokeReport::Refused { reasons };
        }
    };

    if !execute {
        return InvokeReport::DryRun { steps };
    }

    // Python skills take no args, so arg_names is always empty; step_count is 1.
    let invoked = with_python_kind(build_l3_invoked_payload(
        memory_id,
        &candidate.name,
        stored_sha256,
        &[],
        steps.len(),
    ));
    best_effort_audit(pool, ACTION_L3_INVOKED, invoked).await;

    let steps_total = steps.len();
    let outcomes = run_steps(dispatcher, &steps).await;
    let any_err = outcomes.iter().any(|o| o.is_err());
    let outcome_payload = with_python_kind(build_l3_invoke_outcome_payload(
        memory_id,
        &candidate.name,
        outcomes.len(),
        steps_total,
        any_err,
    ));
    best_effort_audit(pool, ACTION_L3_INVOKE_OUTCOME, outcome_payload).await;

    InvokeReport::Executed { outcomes, steps_total }
}

/// Write an audit row, logging a warning on failure instead of propagating the
/// error. Matches the posture in `l3_invoke::operator::best_effort_audit`.
async fn best_effort_audit(pool: &PgPool, action: &str, payload: Value) {
    if let Err(e) = kastellan_db::audit::insert(pool, CLI_AUDIT_ACTOR, action, payload).await {
        tracing::warn!(error = %e, action, "l3py invoke audit insert failed (best-effort)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cassandra::types::PythonSkillCandidate;
    use crate::memory::l3_approval::SkillTrust;
    use crate::memory::l3py_crystallise::compute_python_sha256;

    fn cand() -> PythonSkillCandidate {
        PythonSkillCandidate {
            name: "say_hi".to_string(),
            description: "prints hi".to_string(),
            code: "print('hi')\n".to_string(),
        }
    }

    #[test]
    fn untrusted_yields_refusal() {
        let c = cand();
        let sha = compute_python_sha256(&c);
        let err = prepare_python_steps(&c, SkillTrust::Untrusted, &sha).unwrap_err();
        assert!(!err.reasons.is_empty());
    }

    #[test]
    fn approved_builds_exactly_one_python_exec_step() {
        let c = cand();
        let sha = compute_python_sha256(&c);
        let steps = prepare_python_steps(&c, SkillTrust::UserApproved, &sha).unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].tool, "python-exec");
        assert_eq!(steps[0].method, "python.exec");
        assert_eq!(steps[0].parameters, serde_json::json!({"code": "print('hi')\n"}));
    }

    #[test]
    fn sha_drift_yields_refusal() {
        let c = cand();
        let err = prepare_python_steps(&c, SkillTrust::Pinned, &"0".repeat(64)).unwrap_err();
        assert!(err.reasons.iter().any(|r| r.contains("sha")), "{err:?}");
    }

    #[test]
    fn with_python_kind_injects_kind_field() {
        let base = serde_json::json!({"memory_id": 1, "skill_name": "x"});
        let tagged = with_python_kind(base);
        assert_eq!(tagged.get("kind").and_then(|v| v.as_str()), Some("python"));
    }
}
