//! Operator approval gate for crystallised L3 skills (the security
//! control that precedes any invocation path).
//!
//! Crystallised skills land `trust:"untrusted"` and non-executable (see
//! [`crate::memory::l3_crystallise`]). This module adds the typed
//! [`SkillTrust`] read boundary and the pure [`evaluate_approval`] gate
//! an operator runs (via `hhagent-cli memory l3 approve`) before a skill
//! is promoted to `user_approved`. **Nothing here executes a skill** —
//! `UserApproved`/`Pinned` are inert until the invocation slice lands.
//!
//! See `docs/superpowers/specs/2026-05-31-l3-skill-approval-gate-design.md`.

use std::collections::BTreeSet;

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
// Used by `evaluate_approval` (Task 3); only the tests reference it until
// the gate lands, so suppress the transient dead-code warning.
#[cfg_attr(not(test), allow(dead_code))]
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
}
