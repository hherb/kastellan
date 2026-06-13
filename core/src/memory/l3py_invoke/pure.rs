//! Pure invocation gate for Python skills + the one-step builder. No I/O.
//!
//! The gate mirrors `l3_invoke::pure::prepare_invocation` but: (1) there are
//! no args to substitute (Python code is verbatim); (2) there is no
//! tool-existence check (a Python skill dispatches no tools — the python-exec
//! jail is its entire capability ceiling); (3) it adds a SHA-256 re-hash
//! against the stored digest — the TOCTOU close that guarantees the bytes the
//! operator read and approved are the bytes that run.

use serde_json::Value;

use crate::cassandra::types::{L3TemplateStep, PythonSkillCandidate};
use crate::memory::l3_approval::ApprovalDecision;
use crate::memory::l3_approval::SkillTrust;
// Reuse the templated path's runnable gate verbatim — it is the single source
// of truth for "which trust levels may run", so the Python and templated paths
// can never drift (a new trust variant updates one place, not two).
use crate::memory::l3_invoke::{is_runnable, InvokeRefusal};
use crate::memory::l3py_approval::evaluate_python_approval;
use crate::memory::l3py_crystallise::compute_python_sha256;

/// The tool name the python-exec worker registers as (see
/// `core/src/workers/python_exec.rs`).
pub const PY_EXEC_TOOL: &str = "python-exec";
/// The JSON-RPC method the python-exec worker serves (see
/// `workers/python-exec/src/handler.rs`).
pub const PY_EXEC_METHOD: &str = "python.exec";

/// Byte cap on serialized runtime params. Keep in sync with the worker's
/// authoritative cap (`workers/python-exec/src/exec.rs::MAX_PARAMS_BYTES`);
/// core enforces it early for a clean refusal, the worker enforces it as the
/// real boundary.
pub const MAX_PARAMS_BYTES: usize = 64 * 1024;

/// Why a runtime params object was rejected at the core gate.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PyParamError {
    #[error("params must be a JSON object")]
    NotObject,
    #[error("params name '{0}' is not snake_case")]
    BadKey(String),
    #[error("params serialize to {got} bytes; cap is {max}")]
    TooLarge { got: usize, max: usize },
}

/// `true` iff `s` is a strict snake_case identifier (`[a-z][a-z0-9_]*`).
fn is_snake_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// PURE gate for a runtime params object: must be a JSON object, every
/// TOP-LEVEL key snake_case (param names; nested structure is opaque author
/// data), serialized ≤ [`MAX_PARAMS_BYTES`]. Returns the validated object
/// unchanged. Unlike the templated arg guard there is NO newline/control-char
/// rejection — serde escapes control chars inside JSON strings, so long
/// multi-line text passes freely.
pub fn validate_python_params(params: &Value) -> Result<Value, PyParamError> {
    let obj = params.as_object().ok_or(PyParamError::NotObject)?;
    for key in obj.keys() {
        if !is_snake_ident(key) {
            return Err(PyParamError::BadKey(key.clone()));
        }
    }
    let serialized = serde_json::to_string(params).unwrap_or_default();
    if serialized.len() > MAX_PARAMS_BYTES {
        return Err(PyParamError::TooLarge { got: serialized.len(), max: MAX_PARAMS_BYTES });
    }
    Ok(params.clone())
}

/// `true` iff `params` carries no values (JSON null or an empty object) — used
/// to decide whether the `python.exec` step omits the `params` key entirely
/// (back-compat with param-less rows + their tests).
pub fn params_is_empty(params: &Value) -> bool {
    match params {
        Value::Null => true,
        Value::Object(m) => m.is_empty(),
        _ => false,
    }
}

/// Build the single `python.exec` step that runs `code` verbatim. When
/// `params` is non-empty it is added as a `params` key on the step's
/// `parameters` (where the dispatch chokepoint's recursive secret-ref walker
/// will materialise any `secret://` leaves); an empty params object is omitted
/// so a no-param call is byte-identical to the pre-params shape.
pub fn python_exec_step(code: &str, params: &Value) -> L3TemplateStep {
    let mut parameters = serde_json::json!({ "code": code });
    if !params_is_empty(params) {
        parameters
            .as_object_mut()
            .expect("parameters is an object")
            .insert("params".to_string(), params.clone());
    }
    L3TemplateStep {
        tool: PY_EXEC_TOOL.to_string(),
        method: PY_EXEC_METHOD.to_string(),
        parameters,
    }
}

/// Inject `kind:"python"` into an L3 invoke audit payload so the lifecycle
/// stream distinguishes Python from templated skills without a new action. The
/// single source of truth for the tag, shared by the operator path
/// ([`super::operator`]) and the inner-loop agent path so the field name/value
/// can never drift between them. A non-object payload is returned unchanged
/// (defensive; the audit builders always produce objects).
pub fn with_python_kind(mut payload: serde_json::Value) -> serde_json::Value {
    if let Some(obj) = payload.as_object_mut() {
        obj.insert(
            "kind".to_string(),
            serde_json::Value::String("python".to_string()),
        );
    }
    payload
}

/// PURE decision: may this stored Python skill run, and if so, what code?
///
/// 1. trust must be runnable (`UserApproved | Pinned`);
/// 2. re-run [`evaluate_python_approval`] (structural re-validation + the
///    `secret://` re-scan over the code);
/// 3. re-compute [`compute_python_sha256`] and confirm it equals
///    `stored_sha256` — refuse on drift (the code TOCTOU close).
///
/// Returns the verbatim `code` on success, else an [`InvokeRefusal`]. The
/// three checks are sequential short-circuits (the first failure returns), so
/// each refusal carries exactly its own reason(s) — there is no cross-check
/// accumulation, mirroring `l3_invoke::pure::prepare_invocation`.
pub fn prepare_python_invocation(
    candidate: &PythonSkillCandidate,
    stored_trust: SkillTrust,
    stored_sha256: &str,
) -> Result<String, InvokeRefusal> {
    // 1. Trust gate.
    if !is_runnable(stored_trust) {
        return Err(InvokeRefusal {
            reasons: vec![format!(
                "skill is not runnable (trust='{}'; requires user_approved or pinned)",
                stored_trust.as_str()
            )],
        });
    }

    // 2. Structural re-validation + `secret://` re-scan over the code.
    if let ApprovalDecision::Reject { reasons } = evaluate_python_approval(candidate) {
        return Err(InvokeRefusal {
            reasons: reasons.iter().map(|r| r.to_string()).collect(),
        });
    }

    // 3. SHA-256 re-hash vs the stored digest — the code TOCTOU close.
    let recomputed = compute_python_sha256(candidate);
    if recomputed != stored_sha256 {
        return Err(InvokeRefusal {
            reasons: vec![format!(
                "body sha256 drift: stored={stored_sha256} recomputed={recomputed} \
                 (the approved code is not the code on disk; refusing)"
            )],
        });
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
        let step = python_exec_step("print(1)\n", &serde_json::json!({}));
        assert_eq!(step.tool, "python-exec");
        assert_eq!(step.method, "python.exec");
        assert_eq!(step.parameters, serde_json::json!({"code": "print(1)\n"}));
    }

    #[test]
    fn validate_params_accepts_object_with_snake_case_keys() {
        let v = serde_json::json!({"repo_path": "/tmp/x", "limit": 5, "tags": ["a", "b"]});
        let got = validate_python_params(&v).expect("valid");
        assert_eq!(got, v);
    }

    #[test]
    fn validate_params_rejects_non_object() {
        assert!(validate_python_params(&serde_json::json!([1, 2])).is_err());
        assert!(validate_python_params(&serde_json::json!("flat")).is_err());
    }

    #[test]
    fn validate_params_rejects_non_snake_case_top_level_key() {
        // Top-level keys are param NAMES; nested keys are opaque author data.
        let v = serde_json::json!({"BadKey": 1});
        assert!(validate_python_params(&v).is_err());
    }

    #[test]
    fn validate_params_allows_arbitrary_nested_keys() {
        // Nested object keys are data, NOT param names — no snake_case rule.
        let v = serde_json::json!({"payload": {"CamelCase": 1, "with space": 2}});
        assert!(validate_python_params(&v).is_ok());
    }

    #[test]
    fn validate_params_rejects_over_cap() {
        let big = "x".repeat(MAX_PARAMS_BYTES);
        let v = serde_json::json!({"k": big});
        assert!(validate_python_params(&v).is_err());
    }

    #[test]
    fn params_is_empty_is_true_for_null_and_empty_object() {
        assert!(params_is_empty(&serde_json::Value::Null));
        assert!(params_is_empty(&serde_json::json!({})));
        assert!(!params_is_empty(&serde_json::json!({"a": 1})));
    }

    #[test]
    fn step_omits_params_when_empty() {
        let step = python_exec_step("print(1)\n", &serde_json::json!({}));
        assert_eq!(step.parameters, serde_json::json!({"code": "print(1)\n"}));
    }

    #[test]
    fn step_carries_params_when_present() {
        let step = python_exec_step("print(1)\n", &serde_json::json!({"n": 3}));
        assert_eq!(
            step.parameters,
            serde_json::json!({"code": "print(1)\n", "params": {"n": 3}})
        );
    }
}
