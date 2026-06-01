//! L3 skill recall surfacing — the `<skills>` planner block.
//!
//! Mirrors the L1 insight-index loader ([`crate::memory::layers`]) one
//! layer over: a query-independent load of operator-approved L3 skills
//! that the prompt assembler concatenates into every system prompt.
//!
//! ## Surfacing, not invocation
//!
//! This module makes approved skills *visible* to the planner (name +
//! description + parameter manifest). It does NOT execute them and does
//! NOT expose their step templates — surfacing summarises a capability,
//! it is not an execution recipe. Invocation is a later slice.
//!
//! ## Trust is the load-bearing gate
//!
//! Only `user_approved` / `pinned` rows surface ([`is_surfaceable`]).
//! An `untrusted` skill — or any row whose trust marker is corrupted or
//! absent (the fail-safe
//! [`crate::memory::l3_approval::SkillTrust::from_metadata_str`]
//! downgrades it to `Untrusted`) — never reaches the planner.

use crate::cassandra::types::{L3Param, L3SkillCandidate};
use crate::memory::l3_approval::SkillTrust;

/// A trust-gated L3 skill projected to exactly what the planner sees:
/// name, description, and the parameter manifest.
///
/// Steps are deliberately absent — surfacing summarises a capability,
/// it does not expose the execution recipe (that is an invocation
/// concern). Encoding the omission in the type makes "we do not surface
/// steps" a compile-time fact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfacedSkill {
    pub name: String,
    pub description: String,
    pub params: Vec<L3Param>,
}

/// Project a stored L3 row's `metadata.template` into a [`SurfacedSkill`].
///
/// PURE + fail-safe: a row whose `metadata` lacks a `template` key, or
/// whose `template` is `null` or otherwise does not deserialise into an
/// [`L3SkillCandidate`], yields `None` and is silently skipped by the
/// loader. A malformed skill must never crash prompt assembly or
/// surface garbage.
pub fn parse_surfaced_skill(metadata: &serde_json::Value) -> Option<SurfacedSkill> {
    let template = metadata.get("template")?;
    let cand: L3SkillCandidate = serde_json::from_value(template.clone()).ok()?;
    Some(SurfacedSkill {
        name: cand.name,
        description: cand.description,
        params: cand.parameters,
    })
}

/// PURE trust gate: only operator-approved or pinned skills surface to
/// the planner. The single source of truth for "is this skill allowed
/// in the prompt." Reuses the gate slice's fail-safe trust parse so an
/// unknown/absent marker reads `Untrusted` ⇒ never surfaced.
pub fn is_surfaceable(trust: SkillTrust) -> bool {
    matches!(trust, SkillTrust::UserApproved | SkillTrust::Pinned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn template_meta(name: &str, desc: &str, params: serde_json::Value) -> serde_json::Value {
        json!({
            "trust": "user_approved",
            "template": {
                "name": name,
                "description": desc,
                "parameters": params,
                "steps": [
                    { "tool": "shell-exec", "method": "shell.exec",
                      "parameters": { "argv": ["echo", "{{x}}"] } }
                ]
            }
        })
    }

    #[test]
    fn parse_well_formed_projects_name_desc_params() {
        let meta = template_meta(
            "summarise_repo_readme",
            "Read a repo's README and return a short summary.",
            json!([{ "name": "repo_path", "description": "absolute path to the repo" }]),
        );
        let s = parse_surfaced_skill(&meta).expect("well-formed template parses");
        assert_eq!(s.name, "summarise_repo_readme");
        assert_eq!(s.description, "Read a repo's README and return a short summary.");
        assert_eq!(s.params.len(), 1);
        assert_eq!(s.params[0].name, "repo_path");
        assert_eq!(s.params[0].description, "absolute path to the repo");
    }

    #[test]
    fn parse_zero_param_skill_yields_empty_params() {
        let meta = template_meta("run_tests", "Run the suite.", json!([]));
        let s = parse_surfaced_skill(&meta).expect("zero-param template parses");
        assert!(s.params.is_empty());
    }

    #[test]
    fn parse_missing_template_key_is_none() {
        let meta = json!({ "trust": "user_approved", "source": "agent_raised" });
        assert!(parse_surfaced_skill(&meta).is_none());
    }

    #[test]
    fn parse_template_null_is_none() {
        // `template` key present but null — a state direct SQL could produce.
        let meta = json!({ "trust": "user_approved", "template": null });
        assert!(parse_surfaced_skill(&meta).is_none());
    }

    #[test]
    fn parse_undeserialisable_template_is_none() {
        // `parameters` is a string, not an array of L3Param → from_value fails.
        let meta = json!({
            "template": { "name": "x", "description": "y", "parameters": "nope", "steps": [] }
        });
        assert!(parse_surfaced_skill(&meta).is_none());
    }

    #[test]
    fn is_surfaceable_only_approved_and_pinned() {
        assert!(is_surfaceable(SkillTrust::UserApproved));
        assert!(is_surfaceable(SkillTrust::Pinned));
        assert!(!is_surfaceable(SkillTrust::Untrusted));
    }
}
