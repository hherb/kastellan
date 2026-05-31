//! Writer for `MemoryLayer::Skill` (L3) rows. One caller:
//!
//! **Agent-raised** — via `Plan.l3_skill` consumed by
//! `crate::scheduler::runner::drain_lane` on `Outcome::Completed`
//! when the task executed >= 1 tool step (the grounding gate).
//!
//! The agent emits a parameterised tool-call template on a terminal
//! plan; this module validates it (structural + `{{placeholder}}`
//! closed-world integrity + reserved-tag + caps), dedups on a
//! canonical SHA-256 over the template, and inserts at `layer = 3`
//! marked `trust: "untrusted"` via
//! [`hhagent_db::memories::insert_memory_at_layer`].
//!
//! **Crystallised skills are non-executable in this slice.** There is
//! no invocation path; `trust: "untrusted"` is a forward-compatible
//! placeholder for the future Skill trust enum. Unlike `l1_promote`,
//! there is NO entity auto-link and NO operator write path.
//!
//! See `docs/superpowers/specs/2026-05-31-l3-skill-crystallisation-design.md`.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cassandra::types::L3SkillCandidate;

/// Max bytes for the skill `name` (a stable identifier).
pub const L3_MAX_NAME_BYTES: usize = 64;
/// Max bytes for the skill `description` (becomes the memory `body`).
pub const L3_MAX_DESC_BYTES: usize = 512;
/// Max bytes for a single parameter's `description`.
pub const L3_MAX_PARAM_DESC_BYTES: usize = 256;
/// Max declared parameters.
pub const L3_MAX_PARAMS: usize = 16;
/// Max template steps (lower bound is 1 — the grounding floor).
pub const L3_MAX_STEPS: usize = 32;
/// Max bytes for a `tool` or `method` identifier.
pub const L3_MAX_IDENT_BYTES: usize = 64;
/// Max bytes for the canonical-serialised template (under the audit 4 KiB cap).
pub const L3_MAX_TEMPLATE_BYTES: usize = 4096;

/// Reserved substrings that would close/open the future `<skills>`
/// render block. Defensive against prompt-injection (threat-model §6),
/// symmetric with `l1_promote`'s `<l1_insights>` defence.
const RESERVED_TAG_OPEN: &str = "<skills>";
const RESERVED_TAG_CLOSE: &str = "</skills>";

/// Provenance for an L3 row write. The audit-row `source` field is
/// never producer-supplied; only `runner::drain_lane` constructs this.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum L3Source {
    /// Agent-raised write from `runner::drain_lane` after
    /// `Outcome::Completed`. The originating `task_id` rides in the
    /// audit-row payload for cross-restart trace stitching.
    AgentRaised { task_id: i64 },
}

/// Error kinds the L3 writer can produce.
#[derive(Debug, thiserror::Error)]
pub enum L3Error {
    #[error("L3 skill validation failed: {0}")]
    Validation(String),
    #[error("L3 db error: {0}")]
    Db(#[from] hhagent_db::DbError),
}

/// Outcome of a single `crystallise_l3` call.
#[derive(Clone, Debug)]
pub enum L3WriteOutcome {
    /// New L3 row inserted at the carried `memory_id`.
    Inserted { memory_id: i64 },
    /// A row with the same `body_sha256` already exists at `layer = 3`.
    SkippedDuplicate { memory_id: i64 },
}

impl L3WriteOutcome {
    pub fn memory_id(&self) -> i64 {
        match self {
            L3WriteOutcome::Inserted { memory_id }
            | L3WriteOutcome::SkippedDuplicate { memory_id } => *memory_id,
        }
    }
}

/// `true` iff `s` is a strict snake_case identifier: starts with
/// `[a-z]`, then `[a-z0-9_]*`. Used for skill name + parameter names.
fn is_snake_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// `true` iff `s` is a tool/method identifier: starts with `[a-z0-9]`,
/// then `[a-z0-9_.-]*`. Looser than snake_case because tool names carry
/// hyphens (`shell-exec`) and methods carry dots (`shell.exec`).
fn is_tool_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.' || c == '-')
}

/// Scan a single string for `{{name}}` placeholders, inserting each
/// referenced name into `out`. Rejects an unterminated `{{` and a
/// placeholder whose body is not a snake_case identifier.
fn scan_placeholders(s: &str, out: &mut BTreeSet<String>) -> Result<(), L3Error> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            let start = i + 2;
            let mut j = start;
            while j + 1 < bytes.len() && !(bytes[j] == b'}' && bytes[j + 1] == b'}') {
                j += 1;
            }
            if !(j + 1 < bytes.len() && bytes[j] == b'}' && bytes[j + 1] == b'}') {
                return Err(L3Error::Validation("unterminated placeholder '{{'".into()));
            }
            let ident = &s[start..j];
            if !is_snake_ident(ident) {
                return Err(L3Error::Validation(format!(
                    "malformed placeholder name '{ident}'"
                )));
            }
            out.insert(ident.to_string());
            i = j + 2;
        } else {
            i += 1;
        }
    }
    Ok(())
}

/// Recursively collect every `{{name}}` placeholder from a step's
/// `parameters` JSON value (placeholders only live in string leaves).
fn collect_placeholders(v: &serde_json::Value, out: &mut BTreeSet<String>) -> Result<(), L3Error> {
    match v {
        serde_json::Value::String(s) => scan_placeholders(s, out),
        serde_json::Value::Array(a) => {
            for e in a {
                collect_placeholders(e, out)?;
            }
            Ok(())
        }
        serde_json::Value::Object(m) => {
            for e in m.values() {
                collect_placeholders(e, out)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Validate an L3 skill candidate. On success returns a normalised
/// candidate (trimmed string fields) so the writer never stores
/// leading/trailing whitespace. On failure returns
/// [`L3Error::Validation`] with a human-readable reason.
pub fn validate_l3_skill(c: &L3SkillCandidate) -> Result<L3SkillCandidate, L3Error> {
    // --- name ---
    let name = c.name.trim();
    if name.is_empty() {
        return Err(L3Error::Validation("name is empty after trim".into()));
    }
    if name.len() > L3_MAX_NAME_BYTES {
        return Err(L3Error::Validation(format!(
            "name exceeds {L3_MAX_NAME_BYTES} bytes ({})",
            name.len()
        )));
    }
    if !is_snake_ident(name) {
        return Err(L3Error::Validation(format!(
            "name '{name}' is not snake_case ([a-z][a-z0-9_]*)"
        )));
    }

    // --- description ---
    if c.description.contains('\n') || c.description.contains('\r') {
        return Err(L3Error::Validation("description contains newline".into()));
    }
    let description = c.description.trim();
    if description.is_empty() {
        return Err(L3Error::Validation("description is empty after trim".into()));
    }
    if description.bytes().any(|b| b < 0x20) {
        return Err(L3Error::Validation("description contains control character".into()));
    }
    if description.contains(RESERVED_TAG_OPEN) || description.contains(RESERVED_TAG_CLOSE) {
        return Err(L3Error::Validation("description contains reserved tag substring".into()));
    }
    if description.len() > L3_MAX_DESC_BYTES {
        return Err(L3Error::Validation(format!(
            "description exceeds {L3_MAX_DESC_BYTES} bytes ({})",
            description.len()
        )));
    }

    // --- parameters ---
    if c.parameters.len() > L3_MAX_PARAMS {
        return Err(L3Error::Validation(format!(
            "too many parameters ({} > {L3_MAX_PARAMS})",
            c.parameters.len()
        )));
    }
    let mut declared: BTreeSet<String> = BTreeSet::new();
    let mut norm_params: Vec<crate::cassandra::types::L3Param> =
        Vec::with_capacity(c.parameters.len());
    for p in &c.parameters {
        let pn = p.name.trim();
        if !is_snake_ident(pn) {
            return Err(L3Error::Validation(format!(
                "parameter name '{pn}' is not snake_case"
            )));
        }
        if !declared.insert(pn.to_string()) {
            return Err(L3Error::Validation(format!("duplicate parameter '{pn}'")));
        }
        let pd = p.description.trim();
        if pd.is_empty() {
            return Err(L3Error::Validation(format!(
                "parameter '{pn}' has empty description"
            )));
        }
        if pd.len() > L3_MAX_PARAM_DESC_BYTES {
            return Err(L3Error::Validation(format!(
                "parameter '{pn}' description exceeds {L3_MAX_PARAM_DESC_BYTES} bytes"
            )));
        }
        norm_params.push(crate::cassandra::types::L3Param {
            name: pn.to_string(),
            description: pd.to_string(),
        });
    }

    // --- steps ---
    if c.steps.is_empty() {
        return Err(L3Error::Validation("skill must have at least one step".into()));
    }
    if c.steps.len() > L3_MAX_STEPS {
        return Err(L3Error::Validation(format!(
            "too many steps ({} > {L3_MAX_STEPS})",
            c.steps.len()
        )));
    }
    let mut referenced: BTreeSet<String> = BTreeSet::new();
    let mut norm_steps: Vec<crate::cassandra::types::L3TemplateStep> =
        Vec::with_capacity(c.steps.len());
    for s in &c.steps {
        let tool = s.tool.trim();
        let method = s.method.trim();
        if tool.len() > L3_MAX_IDENT_BYTES || !is_tool_ident(tool) {
            return Err(L3Error::Validation(format!("step tool '{tool}' is invalid")));
        }
        if method.len() > L3_MAX_IDENT_BYTES || !is_tool_ident(method) {
            return Err(L3Error::Validation(format!("step method '{method}' is invalid")));
        }
        if !s.parameters.is_object() {
            return Err(L3Error::Validation(
                "step parameters must be a JSON object".into(),
            ));
        }
        collect_placeholders(&s.parameters, &mut referenced)?;
        norm_steps.push(crate::cassandra::types::L3TemplateStep {
            tool: tool.to_string(),
            method: method.to_string(),
            parameters: s.parameters.clone(),
        });
    }

    // --- closed-world placeholder invariant ---
    for r in &referenced {
        if !declared.contains(r) {
            return Err(L3Error::Validation(format!("undeclared placeholder '{r}'")));
        }
    }
    for d in &declared {
        if !referenced.contains(d) {
            return Err(L3Error::Validation(format!("unused parameter '{d}'")));
        }
    }

    let normalised = L3SkillCandidate {
        name: name.to_string(),
        description: description.to_string(),
        parameters: norm_params,
        steps: norm_steps,
    };

    // --- total size cap (canonical form) ---
    let canonical = canonical_json(&normalised);
    if canonical.len() > L3_MAX_TEMPLATE_BYTES {
        return Err(L3Error::Validation(format!(
            "template exceeds {L3_MAX_TEMPLATE_BYTES} bytes ({})",
            canonical.len()
        )));
    }

    Ok(normalised)
}

/// Recursively sort all object keys so two candidates that differ only
/// in JSON key order serialise identically. Load-bearing for dedup:
/// a non-canonical serialiser would under-dedup.
fn sort_value_keys(v: serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(m) => {
            let mut entries: Vec<(String, serde_json::Value)> = m.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let mut sorted = serde_json::Map::new();
            for (k, val) in entries {
                sorted.insert(k, sort_value_keys(val));
            }
            serde_json::Value::Object(sorted)
        }
        serde_json::Value::Array(a) => {
            serde_json::Value::Array(a.into_iter().map(sort_value_keys).collect())
        }
        other => other,
    }
}

/// Deterministic JSON string for a candidate (object keys sorted at
/// every depth; array order — i.e. step order — preserved).
pub fn canonical_json(c: &L3SkillCandidate) -> String {
    let v = serde_json::to_value(c).expect("L3SkillCandidate serialises");
    serde_json::to_string(&sort_value_keys(v)).expect("canonical serialise")
}

/// SHA-256 over the canonical template, lowercase 64-char hex.
pub fn compute_template_sha256(c: &L3SkillCandidate) -> String {
    let mut h = Sha256::new();
    h.update(canonical_json(c).as_bytes());
    format!("{:x}", h.finalize())
}

/// Build the `metadata` JSONB for a new L3 row. Schema:
/// `{source, task_id, trust, body_sha256, created_at, template}`.
/// `template` is the full normalised candidate (name/parameters/steps;
/// the `description` is duplicated there + as the memory `body`).
///
/// **Coupling note:** the literal `"agent_raised"` MUST match
/// `L3Source`'s serde `rename_all = "snake_case"` output. Cross-pinned
/// by `build_l3_metadata_serde_agrees_with_l3_source`.
///
/// Called by Task 4's `crystallise_l3` async writer (not yet in tree).
#[allow(dead_code)]
pub(crate) fn build_l3_metadata(
    source: &L3Source,
    candidate: &L3SkillCandidate,
    body_sha256: &str,
    created_at_rfc3339: &str,
) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    match source {
        L3Source::AgentRaised { task_id } => {
            obj.insert(
                "source".into(),
                serde_json::Value::String("agent_raised".into()),
            );
            obj.insert(
                "task_id".into(),
                serde_json::Value::Number(serde_json::Number::from(*task_id)),
            );
        }
    }
    obj.insert(
        "trust".into(),
        serde_json::Value::String("untrusted".into()),
    );
    obj.insert(
        "body_sha256".into(),
        serde_json::Value::String(body_sha256.into()),
    );
    obj.insert(
        "created_at".into(),
        serde_json::Value::String(created_at_rfc3339.into()),
    );
    obj.insert(
        "template".into(),
        serde_json::to_value(candidate).expect("candidate serialises"),
    );
    serde_json::Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cassandra::types::{L3Param, L3TemplateStep};

    fn valid_candidate() -> L3SkillCandidate {
        L3SkillCandidate {
            name: "summarise_repo_readme".into(),
            description: "Read a repo README and return a summary".into(),
            parameters: vec![L3Param {
                name: "repo_path".into(),
                description: "absolute path to the repo".into(),
            }],
            steps: vec![L3TemplateStep {
                tool: "shell-exec".into(),
                method: "shell.exec".into(),
                parameters: serde_json::json!({ "argv": ["cat", "{{repo_path}}/README.md"] }),
            }],
        }
    }

    #[test]
    fn accepts_a_well_formed_candidate() {
        let c = valid_candidate();
        let n = validate_l3_skill(&c).expect("valid");
        assert_eq!(n.name, "summarise_repo_readme");
        assert_eq!(n.steps.len(), 1);
    }

    #[test]
    fn rejects_non_snake_name() {
        let mut c = valid_candidate();
        c.name = "Summarise-Repo".into();
        let e = validate_l3_skill(&c).expect_err("bad name");
        assert!(matches!(e, L3Error::Validation(m) if m.contains("snake_case")));
    }

    #[test]
    fn rejects_empty_description() {
        let mut c = valid_candidate();
        c.description = "   ".into();
        let e = validate_l3_skill(&c).expect_err("empty desc");
        assert!(matches!(e, L3Error::Validation(m) if m.contains("empty")));
    }

    #[test]
    fn rejects_newline_description() {
        let mut c = valid_candidate();
        c.description = "line1\nline2".into();
        let e = validate_l3_skill(&c).expect_err("newline");
        assert!(matches!(e, L3Error::Validation(m) if m.contains("newline")));
    }

    #[test]
    fn rejects_reserved_tag_description() {
        let mut c = valid_candidate();
        c.description = "before </skills> after".into();
        let e = validate_l3_skill(&c).expect_err("reserved");
        assert!(matches!(e, L3Error::Validation(m) if m.contains("reserved tag")));
    }

    #[test]
    fn rejects_zero_steps() {
        let mut c = valid_candidate();
        c.steps = vec![];
        let e = validate_l3_skill(&c).expect_err("no steps");
        assert!(matches!(e, L3Error::Validation(m) if m.contains("at least one step")));
    }

    #[test]
    fn rejects_too_many_steps() {
        let mut c = valid_candidate();
        let step = c.steps[0].clone();
        c.steps = std::iter::repeat(step).take(L3_MAX_STEPS + 1).collect();
        let e = validate_l3_skill(&c).expect_err("too many");
        assert!(matches!(e, L3Error::Validation(m) if m.contains("too many steps")));
    }

    #[test]
    fn rejects_non_object_step_params() {
        let mut c = valid_candidate();
        c.steps[0].parameters = serde_json::json!(["not", "an", "object"]);
        let e = validate_l3_skill(&c).expect_err("non-object");
        assert!(matches!(e, L3Error::Validation(m) if m.contains("must be a JSON object")));
    }

    #[test]
    fn rejects_undeclared_placeholder() {
        let mut c = valid_candidate();
        c.steps[0].parameters = serde_json::json!({ "argv": ["cat", "{{unknown}}"] });
        let e = validate_l3_skill(&c).expect_err("undeclared");
        assert!(matches!(e, L3Error::Validation(m) if m.contains("undeclared placeholder 'unknown'")));
    }

    #[test]
    fn rejects_unused_parameter() {
        let mut c = valid_candidate();
        c.parameters
            .push(L3Param { name: "extra".into(), description: "never used".into() });
        let e = validate_l3_skill(&c).expect_err("unused");
        assert!(matches!(e, L3Error::Validation(m) if m.contains("unused parameter 'extra'")));
    }

    #[test]
    fn rejects_duplicate_parameter() {
        let mut c = valid_candidate();
        c.parameters
            .push(L3Param { name: "repo_path".into(), description: "dup".into() });
        let e = validate_l3_skill(&c).expect_err("dup");
        assert!(matches!(e, L3Error::Validation(m) if m.contains("duplicate parameter")));
    }

    #[test]
    fn rejects_malformed_placeholder() {
        let mut c = valid_candidate();
        c.steps[0].parameters = serde_json::json!({ "argv": ["cat", "{{repo-path}}"] });
        let e = validate_l3_skill(&c).expect_err("malformed");
        assert!(matches!(e, L3Error::Validation(m) if m.contains("malformed placeholder")));
    }

    #[test]
    fn rejects_unterminated_placeholder() {
        let mut c = valid_candidate();
        c.steps[0].parameters = serde_json::json!({ "argv": ["{{foo"] });
        let e = validate_l3_skill(&c).expect_err("unterminated");
        assert!(matches!(e, L3Error::Validation(m) if m.contains("unterminated")));
    }

    #[test]
    fn accepts_tool_and_method_with_hyphen_and_dot() {
        let c = valid_candidate(); // shell-exec / shell.exec
        assert!(validate_l3_skill(&c).is_ok());
    }

    #[test]
    fn canonical_json_is_key_order_independent() {
        let mut c = valid_candidate();
        c.steps[0].parameters = serde_json::json!({ "b": "{{repo_path}}", "a": 1 });
        c.parameters[0] = L3Param { name: "repo_path".into(), description: "p".into() };
        let s1 = canonical_json(&c);
        c.steps[0].parameters = serde_json::json!({ "a": 1, "b": "{{repo_path}}" });
        let s2 = canonical_json(&c);
        assert_eq!(s1, s2, "canonical_json must be key-order independent");
    }

    #[test]
    fn compute_template_sha256_is_deterministic_and_64_hex() {
        let c = valid_candidate();
        let a = compute_template_sha256(&c);
        let b = compute_template_sha256(&c);
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert!(a
            .chars()
            .all(|ch| ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase()));
    }

    #[test]
    fn build_l3_metadata_has_expected_keys() {
        let c = validate_l3_skill(&valid_candidate()).expect("valid");
        let m = build_l3_metadata(
            &L3Source::AgentRaised { task_id: 7 },
            &c,
            "abc",
            "2026-05-31T00:00:00Z",
        );
        let obj = m.as_object().expect("object");
        assert_eq!(obj.get("source").unwrap(), "agent_raised");
        assert_eq!(obj.get("task_id").unwrap(), 7);
        assert_eq!(obj.get("trust").unwrap(), "untrusted");
        assert_eq!(obj.get("body_sha256").unwrap(), "abc");
        assert_eq!(obj.get("created_at").unwrap(), "2026-05-31T00:00:00Z");
        assert!(obj.get("template").unwrap().get("name").is_some());
        assert_eq!(obj.len(), 6, "exactly 6 metadata keys");
    }

    #[test]
    fn build_l3_metadata_serde_agrees_with_l3_source() {
        let v = serde_json::to_value(L3Source::AgentRaised { task_id: 1 }).expect("ser");
        assert_eq!(v.get("source").unwrap().as_str().unwrap(), "agent_raised");
        let c = validate_l3_skill(&valid_candidate()).expect("valid");
        let m = build_l3_metadata(
            &L3Source::AgentRaised { task_id: 1 },
            &c,
            "s",
            "2026-05-31T00:00:00Z",
        );
        assert_eq!(m.get("source").unwrap().as_str().unwrap(), "agent_raised");
    }
}
