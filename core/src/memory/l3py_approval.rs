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
/// Note: step (1) runs `validate_python_skill` FIRST, which itself rejects
/// `secret://` in code — so within this function step (2) is effectively
/// unreachable (any secret-bearing body, hand-edited row included, is already
/// stopped by the structural check with a `StructuralInvalid`). The scan is
/// kept as belt-and-suspenders and is exercised directly via the extracted
/// [`scan_code_secret_refs`] helper's own tests, not through this path.
pub fn evaluate_python_approval(candidate: &PythonSkillCandidate) -> ApprovalDecision {
    let candidate = match validate_python_skill(candidate) {
        Ok(norm) => norm,
        Err(e) => {
            return ApprovalDecision::Reject {
                reasons: vec![RejectReason::StructuralInvalid(e.to_string())],
            }
        }
    };

    let reasons = scan_code_secret_refs(&candidate.code);
    if reasons.is_empty() {
        ApprovalDecision::Approve
    } else {
        ApprovalDecision::Reject { reasons }
    }
}

/// Scan Python source for every `secret://` occurrence, returning one
/// [`RejectReason::CodeSecretRef`] per match (byte `offset` + the matched
/// token up to the next whitespace/quote). PURE.
///
/// Extracted so the scan path is unit-testable directly: in the normal flow
/// [`evaluate_python_approval`] never reaches a `secret://`-bearing body
/// (the crystallise validator rejects it first), so this is the only place
/// the defense-in-depth re-scan logic is exercised.
///
/// Terminating + UTF-8-safe: `REF_PREFIX` (`secret://`) is pure ASCII, so
/// `str::find`'s byte offsets and the `+ prefix.len()` advance always land on
/// a char boundary, and `search_from` strictly increases each iteration.
fn scan_code_secret_refs(code: &str) -> Vec<RejectReason> {
    let mut reasons = Vec::new();
    let prefix = crate::secrets::REF_PREFIX;
    let mut search_from = 0;
    while let Some(rel) = code[search_from..].find(prefix) {
        let offset = search_from + rel;
        // Capture the ref token up to the next whitespace/quote for the message.
        let tail = &code[offset..];
        let end = tail
            .find(|c: char| c.is_whitespace() || c == '\'' || c == '"')
            .unwrap_or(tail.len());
        reasons.push(RejectReason::CodeSecretRef {
            offset,
            found: tail[..end].to_string(),
        });
        search_from = offset + prefix.len();
    }
    reasons
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

    #[test]
    fn scan_code_secret_refs_finds_all_occurrences_with_offsets() {
        // Directly exercise the defense-in-depth scan path that
        // evaluate_python_approval never reaches (the validator rejects
        // secret:// first). Two refs on one line → two CodeSecretRef reasons
        // with the correct byte offsets and extracted tokens.
        let code = "a = 'secret://aaaa1111'\nb = secret://bbbb2222\n";
        let reasons = scan_code_secret_refs(code);
        assert_eq!(reasons.len(), 2, "got {reasons:?}");
        match (&reasons[0], &reasons[1]) {
            (
                RejectReason::CodeSecretRef { offset: o0, found: f0 },
                RejectReason::CodeSecretRef { offset: o1, found: f1 },
            ) => {
                assert_eq!(f0, "secret://aaaa1111");
                assert_eq!(&code[*o0..*o0 + f0.len()], f0, "offset0 points at the token");
                assert_eq!(f1, "secret://bbbb2222");
                assert_eq!(&code[*o1..*o1 + f1.len()], f1, "offset1 points at the token");
            }
            other => panic!("expected two CodeSecretRef, got {other:?}"),
        }
    }

    #[test]
    fn scan_code_secret_refs_empty_on_clean_code() {
        assert!(scan_code_secret_refs("print('hello world')\n").is_empty());
    }
}
