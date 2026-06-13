//! Pure invocation gate for Python skills + the one-step builder. No I/O.
//!
//! The gate mirrors `l3_invoke::pure::prepare_invocation` but: (1) there are
//! no args to substitute (Python code is verbatim); (2) there is no
//! tool-existence check (a Python skill dispatches no tools — the python-exec
//! jail is its entire capability ceiling); (3) it adds a SHA-256 re-hash
//! against the stored digest — the TOCTOU close that guarantees the bytes the
//! operator read and approved are the bytes that run.

use crate::cassandra::types::{L3TemplateStep, PythonSkillCandidate};
use crate::memory::l3_approval::ApprovalDecision;
use crate::memory::l3_approval::SkillTrust;
use crate::memory::l3_invoke::InvokeRefusal;
use crate::memory::l3py_approval::evaluate_python_approval;
use crate::memory::l3py_crystallise::compute_python_sha256;

/// The tool name the python-exec worker registers as (see
/// `core/src/workers/python_exec.rs`).
pub const PY_EXEC_TOOL: &str = "python-exec";
/// The JSON-RPC method the python-exec worker serves (see
/// `workers/python-exec/src/handler.rs`).
pub const PY_EXEC_METHOD: &str = "python.exec";

/// True iff this trust level may run via the operator CLI. Identical
/// membership to [`crate::memory::l3_invoke::is_runnable`] — reused for the
/// templated path; spelled here for the Python path so the gate is local.
fn is_runnable(trust: SkillTrust) -> bool {
    matches!(trust, SkillTrust::UserApproved | SkillTrust::Pinned)
}

/// Build the single `python.exec` step that runs `code` verbatim.
pub fn python_exec_step(code: &str) -> L3TemplateStep {
    L3TemplateStep {
        tool: PY_EXEC_TOOL.to_string(),
        method: PY_EXEC_METHOD.to_string(),
        parameters: serde_json::json!({ "code": code }),
    }
}

/// PURE decision: may this stored Python skill run, and if so, what code?
///
/// 1. trust must be runnable (`UserApproved | Pinned`);
/// 2. re-run [`evaluate_python_approval`] (structural re-validation + the
///    `secret://` re-scan over the code);
/// 3. re-compute [`compute_python_sha256`] and confirm it equals
///    `stored_sha256` — refuse on drift (the code TOCTOU close).
///
/// Returns the verbatim `code` on success, else an [`InvokeRefusal`]
/// collecting every reason.
pub fn prepare_python_invocation(
    candidate: &PythonSkillCandidate,
    stored_trust: SkillTrust,
    stored_sha256: &str,
) -> Result<String, InvokeRefusal> {
    let mut reasons: Vec<String> = Vec::new();

    if !is_runnable(stored_trust) {
        reasons.push(format!(
            "skill is not runnable (trust='{}'; requires user_approved or pinned)",
            stored_trust.as_str()
        ));
        return Err(InvokeRefusal { reasons });
    }

    if let ApprovalDecision::Reject { reasons: rs } = evaluate_python_approval(candidate) {
        reasons.extend(rs.iter().map(|r| r.to_string()));
        return Err(InvokeRefusal { reasons });
    }

    let recomputed = compute_python_sha256(candidate);
    if recomputed != stored_sha256 {
        reasons.push(format!(
            "body sha256 drift: stored={stored_sha256} recomputed={recomputed} \
             (the approved code is not the code on disk; refusing)"
        ));
        return Err(InvokeRefusal { reasons });
    }

    Ok(candidate.code.clone())
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
    fn untrusted_is_refused() {
        let c = cand();
        let sha = compute_python_sha256(&c);
        let err = prepare_python_invocation(&c, SkillTrust::Untrusted, &sha).unwrap_err();
        assert!(err.reasons.iter().any(|r| r.contains("not runnable")), "{err:?}");
    }

    #[test]
    fn user_approved_runs_and_returns_verbatim_code() {
        let c = cand();
        let sha = compute_python_sha256(&c);
        let code = prepare_python_invocation(&c, SkillTrust::UserApproved, &sha).unwrap();
        assert_eq!(code, "print('hi')\n");
    }

    #[test]
    fn pinned_runs() {
        let c = cand();
        let sha = compute_python_sha256(&c);
        assert!(prepare_python_invocation(&c, SkillTrust::Pinned, &sha).is_ok());
    }

    #[test]
    fn sha_drift_is_refused() {
        let c = cand();
        let wrong = "0".repeat(64);
        let err = prepare_python_invocation(&c, SkillTrust::Pinned, &wrong).unwrap_err();
        assert!(err.reasons.iter().any(|r| r.contains("sha")), "{err:?}");
    }

    #[test]
    fn embedded_secret_ref_is_refused() {
        let mut c = cand();
        c.code = "x = 'secret://db/password'\n".to_string();
        let sha = compute_python_sha256(&c);
        let err = prepare_python_invocation(&c, SkillTrust::UserApproved, &sha).unwrap_err();
        assert!(err.reasons.iter().any(|r| r.contains("secret://")), "{err:?}");
    }

    #[test]
    fn builds_one_python_exec_step() {
        let step = python_exec_step("print(1)\n");
        assert_eq!(step.tool, "python-exec");
        assert_eq!(step.method, "python.exec");
        assert_eq!(step.parameters, serde_json::json!({"code": "print(1)\n"}));
    }
}
