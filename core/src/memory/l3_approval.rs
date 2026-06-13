//! Operator approval gate for crystallised L3 skills (the security
//! control that precedes any invocation path).
//!
//! Crystallised skills land `trust:"untrusted"` and non-executable (see
//! [`crate::memory::l3_crystallise`]). This module adds the typed
//! [`SkillTrust`] read boundary and the pure [`evaluate_approval`] gate
//! an operator runs (via `kastellan-cli memory l3 approve`) before a skill
//! is promoted to `user_approved`. **Nothing here executes a skill** —
//! `UserApproved`/`Pinned` are inert until the invocation slice lands.
//!
//! See `docs/superpowers/specs/2026-05-31-l3-skill-approval-gate-design.md`.

use std::collections::BTreeSet;

use crate::cassandra::types::L3SkillCandidate;
use crate::memory::l3_crystallise::validate_l3_skill;

/// Trust level of a crystallised L3 skill, stored as the metadata
/// `trust` string. Forward-compat: `Pinned` is defined but no command
/// produces it in the gate slice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkillTrust {
    Untrusted,
    UserApproved,
    Pinned,
}

impl SkillTrust {
    /// Metadata-string form. Single source of truth for the literals
    /// written to / read from `metadata->>'trust'`.
    pub fn as_str(self) -> &'static str {
        match self {
            SkillTrust::Untrusted => "untrusted",
            SkillTrust::UserApproved => "user_approved",
            SkillTrust::Pinned => "pinned",
        }
    }

    /// TOTAL, fail-safe parse from a metadata string: any unknown or
    /// absent value maps to [`SkillTrust::Untrusted`]. An unrecognised
    /// trust marker must never read as trusted.
    pub fn from_metadata_str(s: &str) -> SkillTrust {
        match s {
            "user_approved" => SkillTrust::UserApproved,
            "pinned" => SkillTrust::Pinned,
            _ => SkillTrust::Untrusted,
        }
    }
}

/// Recursively collect every string leaf that begins with the secret-ref
/// prefix (`secret://`). Walks objects + arrays but NOT object keys —
/// only *values* can carry a baked-in secret. Mirrors the writer's
/// `collect_placeholders` walker shape.
///
/// The prefix match is intentionally looser than
/// [`crate::secrets::substitute`]'s strict `secret://<8-hex>` validation: a
/// *gate* must flag ANY `secret://`-prefixed leaf as suspicious regardless
/// of hex validity. Do NOT tighten this to the strict form — doing so would
/// let a malformed-but-secret-shaped ref slip past the gate (a bypass).
fn scan_secret_refs(v: &serde_json::Value, out: &mut Vec<String>) {
    match v {
        serde_json::Value::String(s) => {
            if s.starts_with(crate::secrets::REF_PREFIX) {
                out.push(s.clone());
            }
        }
        serde_json::Value::Array(a) => {
            for e in a {
                scan_secret_refs(e, out);
            }
        }
        serde_json::Value::Object(m) => {
            for e in m.values() {
                scan_secret_refs(e, out);
            }
        }
        _ => {}
    }
}

/// Extract the set of tool names from a `registry.loaded` audit payload
/// `{ "tools": [ {"name": "..."}, ... ] }`. A missing/`!array` `tools`
/// key, or entries without a string `name`, yield an empty set (which
/// the CLI maps to `NoRegistrySnapshot`).
pub fn extract_tool_names(payload: &serde_json::Value) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    if let Some(arr) = payload.get("tools").and_then(|t| t.as_array()) {
        for entry in arr {
            if let Some(name) = entry.get("name").and_then(|n| n.as_str()) {
                set.insert(name.to_string());
            }
        }
    }
    set
}

/// A single reason an approval was refused.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RejectReason {
    /// The stored template failed `validate_l3_skill` re-validation
    /// (e.g. hand-edited in SQL, or written by an older validator).
    StructuralInvalid(String),
    /// A step's parameters embed a baked-in `secret://` reference.
    SecretRefPresent { step: usize, found: String },
    /// A step names a tool the running daemon did not register.
    UnknownTool { tool: String },
    /// No `registry.loaded` snapshot exists, so tool existence could not
    /// be established. Constructed by the CLI orchestration, NOT by
    /// `evaluate_approval` (which only sees a `known_tools` set).
    NoRegistrySnapshot,
    /// A Python skill's source embeds a `secret://` reference at `offset`
    /// bytes. The opaque-code analogue of [`RejectReason::SecretRefPresent`]
    /// (which keys on a step index a Python skill has none of).
    CodeSecretRef { offset: usize, found: String },
}

impl std::fmt::Display for RejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RejectReason::StructuralInvalid(m) => {
                write!(f, "structural validation failed: {m}")
            }
            RejectReason::SecretRefPresent { step, found } => write!(
                f,
                "step {step} embeds a secret reference '{found}' \
                 (skills must not carry baked-in secrets)"
            ),
            RejectReason::UnknownTool { tool } => {
                write!(f, "tool '{tool}' is not registered by the running daemon")
            }
            RejectReason::NoRegistrySnapshot => write!(
                f,
                "no registry.loaded snapshot found; start the daemon once \
                 so the tool registry is recorded"
            ),
            RejectReason::CodeSecretRef { offset, found } => write!(
                f,
                "code embeds a secret reference '{found}' at byte offset {offset} \
                 (skills must not carry baked-in secrets)"
            ),
        }
    }
}

/// The gate's verdict.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApprovalDecision {
    Approve,
    Reject { reasons: Vec<RejectReason> },
}

/// Decide whether a stored skill template may be promoted to
/// `UserApproved`. **PURE** — no I/O. `known_tools` is the set of tool
/// names the live daemon registered (from the latest `registry.loaded`
/// snapshot); an empty set is fail-closed (every step tool is unknown).
///
/// Checks, collecting ALL reasons so the operator sees every problem:
/// 1. structural re-validation (short-circuits — later checks assume a
///    well-formed template);
/// 2. baked-in `secret://` refs in step parameters (one reason per
///    occurrence, with the step index);
/// 3. tool existence (one reason per distinct unknown tool).
pub fn evaluate_approval(
    template: &L3SkillCandidate,
    known_tools: &BTreeSet<String>,
) -> ApprovalDecision {
    let template = match validate_l3_skill(template) {
        Ok(norm) => norm,
        Err(e) => {
            return ApprovalDecision::Reject {
                reasons: vec![RejectReason::StructuralInvalid(e.to_string())],
            }
        }
    };

    let mut reasons = Vec::new();

    for (i, step) in template.steps.iter().enumerate() {
        let mut found = Vec::new();
        scan_secret_refs(&step.parameters, &mut found);
        for f in found {
            reasons.push(RejectReason::SecretRefPresent { step: i, found: f });
        }
    }

    let mut unknown_seen: BTreeSet<&str> = BTreeSet::new();
    for step in &template.steps {
        if !known_tools.contains(&step.tool) && unknown_seen.insert(step.tool.as_str()) {
            reasons.push(RejectReason::UnknownTool { tool: step.tool.clone() });
        }
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

    #[test]
    fn skilltrust_roundtrips_every_variant() {
        for t in [SkillTrust::Untrusted, SkillTrust::UserApproved, SkillTrust::Pinned] {
            assert_eq!(SkillTrust::from_metadata_str(t.as_str()), t);
        }
    }

    #[test]
    fn skilltrust_unknown_or_empty_is_untrusted() {
        assert_eq!(SkillTrust::from_metadata_str("bogus"), SkillTrust::Untrusted);
        assert_eq!(SkillTrust::from_metadata_str(""), SkillTrust::Untrusted);
        assert_eq!(SkillTrust::from_metadata_str("USER_APPROVED"), SkillTrust::Untrusted);
    }

    #[test]
    fn scan_secret_refs_finds_nested_in_object_and_array() {
        let v = serde_json::json!({
            "argv": ["cat", "secret://abc12345"],
            "nested": { "k": "secret://deadbeef" },
            "plain": "no ref here"
        });
        let mut out = Vec::new();
        scan_secret_refs(&v, &mut out);
        out.sort();
        assert_eq!(out, vec!["secret://abc12345".to_string(), "secret://deadbeef".to_string()]);
    }

    #[test]
    fn scan_secret_refs_finds_ref_in_array_of_objects() {
        let v = serde_json::json!({ "items": [{ "tok": "secret://abcd1234" }] });
        let mut out = Vec::new();
        scan_secret_refs(&v, &mut out);
        assert_eq!(out, vec!["secret://abcd1234".to_string()]);
    }

    #[test]
    fn scan_secret_refs_ignores_plain_and_object_keys() {
        // A `secret://`-named KEY must NOT be flagged (only string leaves).
        let v = serde_json::json!({ "secret://notavalue": "ok", "x": 42, "y": true });
        let mut out = Vec::new();
        scan_secret_refs(&v, &mut out);
        assert!(out.is_empty(), "got {out:?}");
    }

    #[test]
    fn extract_tool_names_happy() {
        let payload = serde_json::json!({
            "tools": [{"name": "shell-exec", "binary": "/x"}, {"name": "gliner-relex"}]
        });
        let got = extract_tool_names(&payload);
        assert!(got.contains("shell-exec") && got.contains("gliner-relex"));
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn extract_tool_names_handles_missing_malformed() {
        assert!(extract_tool_names(&serde_json::json!({})).is_empty());
        assert!(extract_tool_names(&serde_json::json!({"tools": "notarray"})).is_empty());
        assert!(extract_tool_names(&serde_json::json!({"tools": [{"binary": "/x"}]})).is_empty());
    }

    fn valid_template() -> L3SkillCandidate {
        use crate::cassandra::types::{L3Param, L3TemplateStep};
        L3SkillCandidate {
            name: "summarise_repo_readme".into(),
            description: "Read a repo README and summarise".into(),
            parameters: vec![L3Param { name: "repo_path".into(), description: "abs path".into() }],
            steps: vec![L3TemplateStep {
                tool: "shell-exec".into(),
                method: "shell.exec".into(),
                parameters: serde_json::json!({ "argv": ["cat", "{{repo_path}}/README.md"] }),
            }],
        }
    }

    fn tools(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn gate_approves_clean_skill_with_known_tool() {
        let d = evaluate_approval(&valid_template(), &tools(&["shell-exec"]));
        assert_eq!(d, ApprovalDecision::Approve);
    }

    #[test]
    fn gate_rejects_unknown_tool() {
        let d = evaluate_approval(&valid_template(), &tools(&["gliner-relex"]));
        assert_eq!(
            d,
            ApprovalDecision::Reject { reasons: vec![RejectReason::UnknownTool { tool: "shell-exec".into() }] }
        );
    }

    #[test]
    fn gate_empty_known_tools_rejects_every_tool() {
        let d = evaluate_approval(&valid_template(), &BTreeSet::new());
        assert!(matches!(d, ApprovalDecision::Reject { .. }));
    }

    #[test]
    fn gate_rejects_baked_in_secret_ref() {
        use crate::cassandra::types::{L3Param, L3TemplateStep};
        let t = L3SkillCandidate {
            name: "leaky".into(),
            description: "carries a secret".into(),
            parameters: vec![L3Param { name: "repo_path".into(), description: "p".into() }],
            steps: vec![L3TemplateStep {
                tool: "shell-exec".into(),
                method: "shell.exec".into(),
                parameters: serde_json::json!({ "argv": ["cat", "{{repo_path}}"], "tok": "secret://abc12345" }),
            }],
        };
        let d = evaluate_approval(&t, &tools(&["shell-exec"]));
        assert_eq!(
            d,
            ApprovalDecision::Reject {
                reasons: vec![RejectReason::SecretRefPresent { step: 0, found: "secret://abc12345".into() }]
            }
        );
    }

    #[test]
    fn gate_accumulates_secret_and_unknown_tool() {
        use crate::cassandra::types::{L3Param, L3TemplateStep};
        let t = L3SkillCandidate {
            name: "leaky_unknown".into(),
            description: "both problems".into(),
            parameters: vec![L3Param { name: "p".into(), description: "d".into() }],
            steps: vec![L3TemplateStep {
                tool: "ghost-tool".into(),
                method: "m.x".into(),
                parameters: serde_json::json!({ "a": "{{p}}", "tok": "secret://deadbeef" }),
            }],
        };
        let d = evaluate_approval(&t, &tools(&["shell-exec"]));
        match d {
            ApprovalDecision::Reject { reasons } => {
                assert!(reasons.contains(&RejectReason::SecretRefPresent { step: 0, found: "secret://deadbeef".into() }));
                assert!(reasons.contains(&RejectReason::UnknownTool { tool: "ghost-tool".into() }));
                assert_eq!(reasons.len(), 2);
            }
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    #[test]
    fn gate_unknown_tool_in_multiple_steps_yields_one_reason() {
        use crate::cassandra::types::{L3Param, L3TemplateStep};
        // One declared param `p` referenced as `{{p}}` in BOTH steps keeps
        // the closed-world structural check happy; both steps name the same
        // unknown tool, so the gate must dedupe to ONE UnknownTool reason.
        let t = L3SkillCandidate {
            name: "double_ghost".into(),
            description: "same unknown tool twice".into(),
            parameters: vec![L3Param { name: "p".into(), description: "d".into() }],
            steps: vec![
                L3TemplateStep {
                    tool: "ghost-tool".into(),
                    method: "m.a".into(),
                    parameters: serde_json::json!({ "a": "{{p}}" }),
                },
                L3TemplateStep {
                    tool: "ghost-tool".into(),
                    method: "m.b".into(),
                    parameters: serde_json::json!({ "b": "{{p}}" }),
                },
            ],
        };
        // Sanity: structurally valid so we exercise the UnknownTool path.
        assert!(validate_l3_skill(&t).is_ok());
        let d = evaluate_approval(&t, &tools(&["shell-exec"]));
        assert_eq!(
            d,
            ApprovalDecision::Reject {
                reasons: vec![RejectReason::UnknownTool { tool: "ghost-tool".into() }]
            }
        );
    }

    #[test]
    fn gate_structurally_invalid_short_circuits() {
        // A template the writer's validator rejects (empty name) → exactly
        // one StructuralInvalid reason, no secret/tool reasons appended.
        let mut t = valid_template();
        t.name = "".into();
        let d = evaluate_approval(&t, &BTreeSet::new());
        match d {
            ApprovalDecision::Reject { reasons } => {
                assert_eq!(reasons.len(), 1);
                assert!(matches!(reasons[0], RejectReason::StructuralInvalid(_)));
            }
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    #[test]
    fn reject_reason_renders_human_readable() {
        assert!(RejectReason::NoRegistrySnapshot.to_string().contains("registry"));
        assert!(RejectReason::UnknownTool { tool: "x".into() }.to_string().contains("not registered"));
    }
}
