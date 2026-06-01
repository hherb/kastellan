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
use hhagent_db::memories::{load_layer, MemoryLayer};
use hhagent_db::DbError;
use sqlx::PgPool;

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

/// Default upper bound on the number of L3 skills surfaced into a
/// prompt. Tighter than L1's 32 because approved skills are
/// operator-gated and therefore few; a smaller list keeps the
/// `<skills>` block scannable.
pub const L3_SKILLS_CAP_ROWS: usize = 16;

/// Default upper bound on the cumulative *rendered* byte length of the
/// surfaced skills. Matches L1's 4 KiB "fits in context unconditionally"
/// budget. Bounds actual prompt bytes because the accumulator measures
/// [`render_skill_entry`] output, which is exactly what the assembler
/// emits.
pub const L3_SKILLS_CAP_BYTES: usize = 4096;

/// Render a single skill into its `<skills>`-block lines:
///
/// ```text
/// - <name>: <description>
///   params: <p0.name> (<p0.description>), <p1.name> (<p1.description>)
/// ```
///
/// The `params:` line is omitted entirely for a zero-parameter skill.
/// PURE; the cap accumulator and the assembler both call this so the
/// byte budget and the emitted prompt never diverge.
pub fn render_skill_entry(skill: &SurfacedSkill) -> String {
    let mut out = String::new();
    out.push_str("- ");
    out.push_str(&skill.name);
    out.push_str(": ");
    out.push_str(&skill.description);
    out.push('\n');
    if !skill.params.is_empty() {
        out.push_str("  params: ");
        let rendered: Vec<String> = skill
            .params
            .iter()
            .map(|p| format!("{} ({})", p.name, p.description))
            .collect();
        out.push_str(&rendered.join(", "));
        out.push('\n');
    }
    out
}

/// Apply the row + rendered-byte caps to a trust-filtered, parsed skill
/// list (newest-first on input). PURE.
///
/// Row cap first, then a byte-accumulate loop over [`render_skill_entry`]
/// length: pushing the next entry stops once it would make the
/// cumulative rendered length *strictly exceed* `cap_bytes` (inclusive
/// boundary — an entry that fills the budget exactly still fits), mirroring
/// [`crate::memory::layers::load_l1`]. `cap_rows == 0` or `cap_bytes == 0`
/// returns empty.
///
/// Unlike `load_l1`, a single over-budget entry is dropped *silently* (no
/// `tracing::warn!`): L3 skills are operator-gated, so an oversized
/// name+description is caught at approval time, not surfacing time — and
/// this stays a pure function with no logging dependency.
pub fn cap_surfaced(
    skills: Vec<SurfacedSkill>,
    cap_rows: usize,
    cap_bytes: usize,
) -> Vec<SurfacedSkill> {
    if cap_rows == 0 || cap_bytes == 0 {
        return Vec::new();
    }
    let mut acc: Vec<SurfacedSkill> = Vec::with_capacity(cap_rows.min(skills.len()));
    let mut bytes_used: usize = 0;
    for skill in skills {
        // `>=` (not `==`) keeps this a guard even if the loop body is
        // ever reordered — it can never overshoot the row cap.
        if acc.len() >= cap_rows {
            break;
        }
        let entry_bytes = render_skill_entry(&skill).len();
        // saturating_add: if a (future, pathological) entry length wrapped
        // usize on accumulation, the sum saturates to "definitely over the
        // cap" — the safe direction (mirrors `layers::load_l1`).
        if bytes_used.saturating_add(entry_bytes) > cap_bytes {
            break;
        }
        bytes_used += entry_bytes;
        acc.push(skill);
    }
    acc
}

/// Load operator-approved/pinned L3 skills for the planner prompt.
///
/// Fetches every L3 row (newest-first, same as
/// [`crate::memory::l3_crystallise::list_l3`]), drops any whose trust is
/// not surfaceable, parses each surviving row's `metadata.template`
/// (malformed rows skipped fail-safe via [`parse_surfaced_skill`]), then
/// applies the row + rendered-byte caps. Fetch-all-then-filter is correct
/// here: the trust filter runs after the fetch, so a capped fetch could
/// starve the row cap when newer rows are untrusted; operator-gated volume
/// is low, so this is cheap.
///
/// Returns `Ok(vec![])` when no approved skill exists — the expected state
/// until an operator approves one. Not an error.
pub async fn load_l3_skills_for_prompt(
    pool: &PgPool,
    cap_rows: usize,
    cap_bytes: usize,
) -> Result<Vec<SurfacedSkill>, DbError> {
    if cap_rows == 0 || cap_bytes == 0 {
        return Ok(Vec::new());
    }
    let rows = load_layer(pool, MemoryLayer::Skill, usize::MAX).await?;
    let surfaced: Vec<SurfacedSkill> = rows
        .into_iter()
        .filter(|row| {
            let trust = row
                .metadata
                .get("trust")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            is_surfaceable(SkillTrust::from_metadata_str(trust))
        })
        .filter_map(|row| parse_surfaced_skill(&row.metadata))
        .collect();
    Ok(cap_surfaced(surfaced, cap_rows, cap_bytes))
}

/// Convenience wrapper pinning the published caps. Prefer this from the
/// prompt assembler (mirrors [`crate::memory::layers::load_l1_default`]).
pub async fn load_l3_skills_default(pool: &PgPool) -> Result<Vec<SurfacedSkill>, DbError> {
    load_l3_skills_for_prompt(pool, L3_SKILLS_CAP_ROWS, L3_SKILLS_CAP_BYTES).await
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

    fn skill(name: &str, desc: &str, params: &[(&str, &str)]) -> SurfacedSkill {
        SurfacedSkill {
            name: name.into(),
            description: desc.into(),
            params: params
                .iter()
                .map(|(n, d)| L3Param { name: (*n).into(), description: (*d).into() })
                .collect(),
        }
    }

    #[test]
    fn render_entry_with_params() {
        let s = skill("foo", "does foo.", &[("x", "the x"), ("y", "the y")]);
        assert_eq!(render_skill_entry(&s), "- foo: does foo.\n  params: x (the x), y (the y)\n");
    }

    #[test]
    fn render_entry_zero_params_omits_params_line() {
        let s = skill("bar", "does bar.", &[]);
        assert_eq!(render_skill_entry(&s), "- bar: does bar.\n");
    }

    #[test]
    fn cap_surfaced_honours_row_cap() {
        let skills = vec![skill("a", "a.", &[]), skill("b", "b.", &[]), skill("c", "c.", &[])];
        let capped = cap_surfaced(skills, 2, 4096);
        assert_eq!(capped.len(), 2);
        assert_eq!(capped[0].name, "a");
        assert_eq!(capped[1].name, "b");
    }

    #[test]
    fn cap_surfaced_honours_byte_cap() {
        // cap_bytes set to exactly one entry's rendered length admits one.
        let one = render_skill_entry(&skill("a", "a.", &[])).len();
        let skills = vec![skill("a", "a.", &[]), skill("b", "b.", &[])];
        let capped = cap_surfaced(skills, 16, one);
        assert_eq!(capped.len(), 1);
    }

    #[test]
    fn cap_surfaced_single_oversized_entry_returns_empty() {
        // The first (and only) entry already exceeds the byte budget alone:
        // bytes_used(0) + entry_bytes > cap_bytes ⇒ break before any push.
        let s = skill("a", "a.", &[]);
        let entry_len = render_skill_entry(&s).len();
        let capped = cap_surfaced(vec![s], 16, entry_len - 1);
        assert!(capped.is_empty(), "over-budget single entry must not sneak in");
    }

    #[test]
    fn cap_surfaced_zero_caps_return_empty() {
        let skills = vec![skill("a", "a.", &[])];
        assert!(cap_surfaced(skills.clone(), 0, 4096).is_empty());
        assert!(cap_surfaced(skills, 16, 0).is_empty());
    }

    #[test]
    fn caps_pinned_to_documented_defaults() {
        assert_eq!(L3_SKILLS_CAP_ROWS, 16);
        assert_eq!(L3_SKILLS_CAP_BYTES, 4096);
    }
}
