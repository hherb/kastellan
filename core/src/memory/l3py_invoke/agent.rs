//! Agent-path Python-skill invocation: the stricter pinned-only gate + the
//! pure [`expand_python_for_agent`] expansion (re-validate + SHA re-hash,
//! classify at the invoking plan's data ceiling) + the
//! [`load_pinned_python_skill_by_name`] resolver the inner loop calls when an
//! agent-emitted `invoke_skill` directive names no pinned *templated* skill.
//!
//! Mirrors [`crate::memory::l3_invoke::agent`] one payload over: the agent may
//! invoke ONLY `pinned` skills, and the single expanded `python.exec` step
//! still flows through the unchanged CASSANDRA review → sandboxed-dispatch →
//! audit pipeline.

use sqlx::PgPool;

use kastellan_db::memories::{load_layer_by_trust, MemoryLayer};

use crate::cassandra::types::{DataClass, PlannedStep, PythonSkillCandidate};
use crate::memory::l3_approval::SkillTrust;
use crate::memory::l3_invoke::{is_autonomously_invocable, InvokeRefusal};

use super::pure::{prepare_python_invocation, python_exec_step, validate_python_params};

/// A pinned Python skill loaded for agent-autonomous invocation.
pub struct PinnedPythonSkill {
    pub memory_id: i64,
    pub candidate: PythonSkillCandidate,
    pub body_sha256: String,
}

/// PURE agent expansion: strict pinned-only gate → [`prepare_python_invocation`]
/// (re-validate + `secret://` re-scan + SHA-drift refuse) → optional runtime
/// `params` validation via [`validate_python_params`] → one [`PlannedStep`]
/// built by [`python_exec_step`], classified at the invoking plan's
/// `data_ceiling` (so the deterministic policy's I2/I3 invariants hold
/// automatically, exactly as the templated agent path). Refuses non-pinned
/// trust, SHA drift, or an invalid params object, collecting every reason.
/// Empty params (`{}` or `null`) are passed through and omitted from the step's
/// `parameters` so param-less calls remain byte-identical to the pre-params
/// shape (back-compat).
pub fn expand_python_for_agent(
    candidate: &PythonSkillCandidate,
    stored_trust: SkillTrust,
    stored_sha256: &str,
    data_ceiling: DataClass,
    params: &serde_json::Value,
) -> Result<Vec<PlannedStep>, InvokeRefusal> {
    if !matches!(stored_trust, SkillTrust::Pinned) {
        return Err(InvokeRefusal {
            reasons: vec![format!(
                "skill is not autonomously invocable (trust='{}'; requires pinned)",
                stored_trust.as_str()
            )],
        });
    }
    // prepare_python_invocation enforces runnable-trust + structural re-validate
    // + SHA re-hash; pinned satisfies runnable, so this adds the structural +
    // drift checks and returns the verbatim code.
    let code = prepare_python_invocation(candidate, stored_trust, stored_sha256)?;
    let params = validate_python_params(params)
        .map_err(|e| InvokeRefusal { reasons: vec![e.to_string()] })?;
    let step = python_exec_step(&code, &params);
    Ok(vec![PlannedStep {
        tool: step.tool,
        method: step.method,
        parameters: step.parameters,
        returns: String::new(),
        done_when: String::new(),
        classification: data_ceiling,
    }])
}

/// Load the newest `pinned` Python skill whose `metadata.python.name` matches
/// `name`. Mirrors [`crate::memory::l3_invoke::load_pinned_skill_by_name`]:
/// trust is filtered in SQL (`load_layer_by_trust(Skill, ["pinned"], …)`), with
/// a defensive `kind == "python"` + parseable-payload re-check so a malformed
/// or non-python row is skipped fail-safe. `Ok(None)` when no pinned Python
/// skill of that name exists — the inner loop turns that into a refusal.
pub async fn load_pinned_python_skill_by_name(
    pool: &PgPool,
    name: &str,
) -> Result<Option<PinnedPythonSkill>, kastellan_db::DbError> {
    // Same SCAN_CAP rationale as the templated resolver: caps total pinned rows
    // scanned newest-first; 64 distinct pinned skills is a generous ceiling for
    // a deliberate, rare human action.
    const SCAN_CAP: usize = 64;
    let rows = load_layer_by_trust(pool, MemoryLayer::Skill, &["pinned"], SCAN_CAP).await?;
    for row in rows {
        let meta = &row.metadata;
        if meta.get("kind").and_then(|k| k.as_str()) != Some("python") {
            continue; // not a python skill (templated or other) — skip
        }
        // Defense-in-depth: re-check the row's stored trust in Rust even though
        // SQL already filtered `trust = "pinned"`. Mirrors the templated
        // resolver — guards against a future SQL/Rust divergence so a
        // non-pinned row can never reach the agent's autonomous path. The
        // inner loop passes a hardcoded `SkillTrust::Pinned` to
        // `expand_python_for_agent`, so this loader IS the trust gate.
        let trust = meta.get("trust").and_then(|v| v.as_str()).unwrap_or("");
        if !is_autonomously_invocable(SkillTrust::from_metadata_str(trust)) {
            continue;
        }
        let candidate: PythonSkillCandidate = match meta
            .get("python")
            .cloned()
            .and_then(|p| serde_json::from_value(p).ok())
        {
            Some(c) => c,
            None => continue, // unparseable python payload — skip fail-safe
        };
        if candidate.name != name {
            continue;
        }
        let body_sha256 = meta
            .get("body_sha256")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        return Ok(Some(PinnedPythonSkill { memory_id: row.id, candidate, body_sha256 }));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cassandra::types::{DataClass, PythonSkillCandidate};
    use crate::memory::l3_approval::SkillTrust;
    use crate::memory::l3py_crystallise::compute_python_sha256;

    fn cand() -> PythonSkillCandidate {
        PythonSkillCandidate {
            name: "say_hi".to_string(),
            description: "d".to_string(),
            code: "print('hi')\n".to_string(),
        }
    }

    #[test]
    fn user_approved_is_not_autonomously_invocable() {
        let c = cand();
        let sha = compute_python_sha256(&c);
        let err = expand_python_for_agent(
            &c, SkillTrust::UserApproved, &sha, DataClass::Public, &serde_json::json!({}),
        )
        .unwrap_err();
        assert!(err.reasons.iter().any(|r| r.contains("pinned")), "{err:?}");
    }

    #[test]
    fn pinned_expands_to_one_python_exec_planned_step() {
        let c = cand();
        let sha = compute_python_sha256(&c);
        let steps = expand_python_for_agent(
            &c, SkillTrust::Pinned, &sha, DataClass::Secret, &serde_json::json!({}),
        )
        .expect("pinned expands");
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].tool, "python-exec");
        assert_eq!(steps[0].method, "python.exec");
        assert_eq!(steps[0].classification, DataClass::Secret);
        assert_eq!(steps[0].parameters, serde_json::json!({"code": "print('hi')\n"}));
    }

    #[test]
    fn pinned_with_sha_drift_refuses() {
        let c = cand();
        let err = expand_python_for_agent(
            &c, SkillTrust::Pinned, &"0".repeat(64), DataClass::Secret, &serde_json::json!({}),
        )
        .unwrap_err();
        assert!(err.reasons.iter().any(|r| r.contains("sha")), "{err:?}");
    }

    #[test]
    fn pinned_with_params_expands_step_carrying_params() {
        let c = cand();
        let sha = compute_python_sha256(&c);
        let steps = expand_python_for_agent(
            &c, SkillTrust::Pinned, &sha, DataClass::Secret, &serde_json::json!({"id": 9}),
        )
        .expect("pinned expands");
        assert_eq!(
            steps[0].parameters,
            serde_json::json!({"code": "print('hi')\n", "params": {"id": 9}})
        );
        assert_eq!(steps[0].classification, DataClass::Secret);
    }

    #[test]
    fn bad_params_refuses() {
        let c = cand();
        let sha = compute_python_sha256(&c);
        let err = expand_python_for_agent(
            &c, SkillTrust::Pinned, &sha, DataClass::Public, &serde_json::json!("flat"),
        )
        .unwrap_err();
        assert!(err.reasons.iter().any(|r| r.contains("object")), "{err:?}");
    }

    #[test]
    fn pinned_with_null_params_expands_without_params_key() {
        // The agent path forwards InvokeDirective.params (default Value::Null);
        // a no-param pinned invoke must expand to a bare {"code": ...} step.
        let c = cand();
        let sha = compute_python_sha256(&c);
        let steps = expand_python_for_agent(
            &c, SkillTrust::Pinned, &sha, DataClass::Public, &serde_json::Value::Null,
        )
        .expect("pinned with null params expands");
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].parameters, serde_json::json!({"code": "print('hi')\n"}));
    }
}
