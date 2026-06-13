//! Writer for **Python** L3 skills (`metadata.kind = "python"`). Mirrors
//! [`crate::memory::l3_crystallise`] one payload over: where the templated
//! writer stores a parameterised tool-call template, this stores a *verbatim*
//! Python snippet the agent ran. One caller: `runner::drain_lane` on
//! `Outcome::Completed` with `dispatch_count >= 1` (the grounding gate).
//!
//! Crystallised Python skills land `trust:"untrusted"` and are inert until an
//! operator approves them. There is NO invocation path in this slice.
//!
//! See `docs/superpowers/specs/2026-06-13-python-exec-skill-catalog-design.md`.

use sha2::{Digest, Sha256};

use crate::cassandra::types::PythonSkillCandidate;

/// Max bytes for the skill `name` (a stable identifier). Mirrors L3.
pub const PY_MAX_NAME_BYTES: usize = 64;
/// Max bytes for the skill `description` (becomes the memory `body`).
pub const PY_MAX_DESC_BYTES: usize = 512;
/// Max bytes for the verbatim Python `code`. Well under the worker's 256 KiB
/// `python.exec` code limit; a catalog skill is a small reusable snippet.
pub const PY_CODE_CAP: usize = 64 * 1024;

/// Error kinds the Python-skill writer can produce. Mirrors
/// [`crate::memory::l3_crystallise::L3Error`].
#[derive(Debug, thiserror::Error)]
pub enum PyError {
    #[error("python skill validation failed: {0}")]
    Validation(String),
    #[error("python skill db error: {0}")]
    Db(#[from] kastellan_db::DbError),
}

/// Outcome of a single `crystallise_python_skill` call. Mirrors
/// [`crate::memory::l3_crystallise::L3WriteOutcome`].
#[derive(Clone, Debug)]
pub enum PyWriteOutcome {
    Inserted { memory_id: i64 },
    SkippedDuplicate { memory_id: i64 },
}

impl PyWriteOutcome {
    pub fn memory_id(&self) -> i64 {
        match self {
            PyWriteOutcome::Inserted { memory_id }
            | PyWriteOutcome::SkippedDuplicate { memory_id } => *memory_id,
        }
    }
}

/// `true` iff `s` is a strict snake_case identifier (`[a-z][a-z0-9_]*`).
/// Local copy (mirrors `l3_crystallise::is_snake_ident`).
fn is_snake_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Validate a Python-skill candidate. Returns a normalised candidate
/// (trimmed name + description; **code is NOT trimmed** — leading/trailing
/// whitespace can be significant in Python) on success.
///
/// Key difference from `validate_l3_skill`: `code` is multi-line source, so
/// newlines and tabs are allowed; only a NUL byte is rejected (it cannot
/// occur in legitimate source and would truncate a C string downstream).
pub fn validate_python_skill(c: &PythonSkillCandidate) -> Result<PythonSkillCandidate, PyError> {
    // --- name ---
    let name = c.name.trim();
    if name.is_empty() {
        return Err(PyError::Validation("name is empty after trim".into()));
    }
    if name.len() > PY_MAX_NAME_BYTES {
        return Err(PyError::Validation(format!(
            "name exceeds {PY_MAX_NAME_BYTES} bytes ({})",
            name.len()
        )));
    }
    if !is_snake_ident(name) {
        return Err(PyError::Validation(format!(
            "name '{name}' is not snake_case ([a-z][a-z0-9_]*)"
        )));
    }

    // --- description (same guards as L3: no newline/control, capped) ---
    let description = c.description.trim();
    if description.is_empty() {
        return Err(PyError::Validation("description is empty after trim".into()));
    }
    if description.contains('\n') || description.contains('\r') {
        return Err(PyError::Validation("description contains newline".into()));
    }
    if description.bytes().any(|b| b < 0x20) {
        return Err(PyError::Validation("description contains control character".into()));
    }
    if description.len() > PY_MAX_DESC_BYTES {
        return Err(PyError::Validation(format!(
            "description exceeds {PY_MAX_DESC_BYTES} bytes ({})",
            description.len()
        )));
    }

    // --- code (verbatim; multi-line allowed; reject empty / NUL / over-cap) ---
    if c.code.is_empty() {
        return Err(PyError::Validation("code is empty".into()));
    }
    if c.code.len() > PY_CODE_CAP {
        return Err(PyError::Validation(format!(
            "code exceeds {PY_CODE_CAP} bytes ({})",
            c.code.len()
        )));
    }
    if c.code.bytes().any(|b| b == 0) {
        return Err(PyError::Validation("code contains a NUL byte".into()));
    }
    // Defensive: a skill must not bake in a secret reference (mirrors the
    // approval gate; rejecting at crystallise too means it never even lands).
    if c.code.contains(crate::secrets::REF_PREFIX) {
        return Err(PyError::Validation(format!(
            "code contains a '{}' secret reference (skills must not carry baked-in secrets)",
            crate::secrets::REF_PREFIX
        )));
    }

    Ok(PythonSkillCandidate {
        name: name.to_string(),
        description: description.to_string(),
        code: c.code.clone(),
    })
}

/// Deterministic JSON string for a candidate (top-level keys sorted; the
/// struct is flat so no recursive sort is needed). Load-bearing for dedup.
pub fn canonical_json(c: &PythonSkillCandidate) -> String {
    // serde_json::Map preserves insertion order; build it in sorted-key order.
    let mut map = serde_json::Map::new();
    map.insert("code".into(), serde_json::Value::String(c.code.clone()));
    map.insert("description".into(), serde_json::Value::String(c.description.clone()));
    map.insert("name".into(), serde_json::Value::String(c.name.clone()));
    serde_json::to_string(&serde_json::Value::Object(map)).expect("canonical serialise")
}

/// SHA-256 over the canonical candidate, lowercase 64-char hex.
pub fn compute_python_sha256(c: &PythonSkillCandidate) -> String {
    let mut h = Sha256::new();
    h.update(canonical_json(c).as_bytes());
    format!("{:x}", h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> PythonSkillCandidate {
        PythonSkillCandidate {
            name: "sum_stdin".into(),
            description: "Sum integers from stdin".into(),
            code: "import sys\nprint(sum(int(x) for x in sys.stdin))\n".into(),
        }
    }

    #[test]
    fn validate_accepts_clean_multiline_code() {
        let v = validate_python_skill(&valid()).expect("clean skill validates");
        assert!(v.code.contains('\n'));
        assert!(v.code.ends_with('\n'));
    }

    #[test]
    fn validate_rejects_non_snake_name() {
        let mut c = valid();
        c.name = "SumStdin".into();
        assert!(validate_python_skill(&c).is_err());
    }

    #[test]
    fn validate_rejects_empty_code() {
        let mut c = valid();
        c.code = String::new();
        assert!(validate_python_skill(&c).is_err());
    }

    #[test]
    fn validate_rejects_oversized_code() {
        let mut c = valid();
        c.code = "x".repeat(PY_CODE_CAP + 1);
        assert!(validate_python_skill(&c).is_err());
    }

    #[test]
    fn validate_rejects_nul_in_code() {
        let mut c = valid();
        c.code = "print(1)\u{0}".into();
        assert!(validate_python_skill(&c).is_err());
    }

    #[test]
    fn validate_rejects_secret_ref_in_code() {
        let mut c = valid();
        c.code = "token = 'secret://abc12345'\nprint(token)\n".into();
        assert!(validate_python_skill(&c).is_err());
    }

    #[test]
    fn validate_rejects_newline_in_description() {
        let mut c = valid();
        c.description = "line one\nline two".into();
        assert!(validate_python_skill(&c).is_err());
    }

    #[test]
    fn sha_is_deterministic_and_field_sensitive() {
        let a = compute_python_sha256(&valid());
        assert_eq!(a, compute_python_sha256(&valid()), "deterministic");
        assert_eq!(a.len(), 64);
        let mut c = valid();
        c.code = "print(2)\n".into();
        assert_ne!(a, compute_python_sha256(&c), "sensitive to code");
    }
}
