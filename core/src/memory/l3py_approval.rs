//! Operator approval gate for crystallised **Python** L3 skills. The
//! opaque-code analogue of [`crate::memory::l3_approval::evaluate_approval`]:
//! it re-runs structural validation + a `secret://` scan, but has **no
//! live-registry / tool-existence check** — a Python skill dispatches no
//! tools (its entire capability ceiling is the python-exec jail), so there is
//! nothing to check against the registry. The human reading the source via
//! `memory l3 show` is the real gate; this is the machine-checkable floor.
//!
//! See `docs/superpowers/specs/2026-06-13-python-exec-skill-catalog-design.md`.

use crate::cassandra::types::PythonSkillCandidate;
use crate::memory::l3_approval::{ApprovalDecision, RejectReason};
use crate::memory::l3py_crystallise::validate_python_skill;

/// Decide whether a stored Python skill may be promoted to `UserApproved`.
/// **PURE** — no I/O, no registry dependency. Collects ALL reasons:
/// 1. structural re-validation (short-circuits — a malformed skill yields
///    exactly one `StructuralInvalid`);
/// 2. every `secret://` occurrence in the source (one `CodeSecretRef` each).
///
/// Note: `validate_python_skill` already rejects `secret://` in code, so a
/// stored row that passed crystallisation will not trip (2); the re-scan is
/// defense-in-depth against a hand-edited SQL row and keeps the gate honest.
pub fn evaluate_python_approval(candidate: &PythonSkillCandidate) -> ApprovalDecision {
    let candidate = match validate_python_skill(candidate) {
        Ok(norm) => norm,
        Err(e) => {
            return ApprovalDecision::Reject {
                reasons: vec![RejectReason::StructuralInvalid(e.to_string())],
            }
        }
    };

    let mut reasons = Vec::new();
    let prefix = crate::secrets::REF_PREFIX;
    let mut search_from = 0;
    while let Some(rel) = candidate.code[search_from..].find(prefix) {
        let offset = search_from + rel;
        // Capture the ref token up to the next whitespace/quote for the message.
        let tail = &candidate.code[offset..];
        let end = tail
            .find(|c: char| c.is_whitespace() || c == '\'' || c == '"')
            .unwrap_or(tail.len());
        reasons.push(RejectReason::CodeSecretRef {
            offset,
            found: tail[..end].to_string(),
        });
        search_from = offset + prefix.len();
    }

    if reasons.is_empty() {
        ApprovalDecision::Approve
    } else {
        ApprovalDecision::Reject { reasons }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean() -> PythonSkillCandidate {
        PythonSkillCandidate {
            name: "sum_stdin".into(),
            description: "Sum integers from stdin".into(),
            code: "import sys\nprint(sum(int(x) for x in sys.stdin))\n".into(),
        }
    }

    #[test]
    fn approves_clean_skill() {
        assert_eq!(evaluate_python_approval(&clean()), ApprovalDecision::Approve);
    }

    #[test]
    fn rejects_structurally_invalid_skill() {
        let mut c = clean();
        c.name = "Bad Name".into();
        match evaluate_python_approval(&c) {
            ApprovalDecision::Reject { reasons } => {
                assert_eq!(reasons.len(), 1);
                assert!(matches!(reasons[0], RejectReason::StructuralInvalid(_)));
            }
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    #[test]
    fn rejects_skill_with_secret_ref_in_code() {
        // validate_python_skill rejects secret:// in code, so this short-circuits
        // at the structural check; the CodeSecretRef loop is belt-and-suspenders
        // for a hand-edited SQL row. Either way the rejection mentions "secret".
        let mut c = clean();
        c.code = "tok = 'secret://abc12345'\n".into();
        match evaluate_python_approval(&c) {
            ApprovalDecision::Reject { reasons } => {
                assert!(reasons.iter().any(|r| r.to_string().contains("secret")));
            }
            other => panic!("expected Reject, got {other:?}"),
        }
    }
}
