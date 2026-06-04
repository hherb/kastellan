//! Agent-path L3 invocation: the stricter pinned-only gate, the pure
//! [`expand_for_agent`] expansion (re-validate + substitute against the
//! daemon's *live* tool set, classify at the invoking plan's data ceiling),
//! and the [`load_pinned_skill_by_name`] resolver the inner loop calls when
//! the agent emits an `invoke_skill` directive.
//!
//! Distinct from the operator path ([`super::operator`]): the agent may
//! invoke ONLY `pinned` skills, and the expanded steps still flow through
//! the unchanged CASSANDRA review → sandboxed-dispatch → audit pipeline.
//!
//! See `docs/superpowers/specs/2026-06-04-l3-skill-autonomous-door-design.md`.

use std::collections::{BTreeMap, BTreeSet};

use sqlx::PgPool;

use hhagent_db::memories::{load_layer_by_trust, MemoryLayer};

use crate::cassandra::types::{DataClass, L3SkillCandidate, PlannedStep};
use crate::memory::l3_approval::SkillTrust;

use super::pure::{
    is_autonomously_invocable, planned_step_from_l3_with_class, prepare_invocation, InvokeRefusal,
};

/// PURE agent-path expansion: gate on the stricter
/// [`is_autonomously_invocable`] (pinned only), re-validate + substitute
/// via [`prepare_invocation`] against the daemon's live tool set, and
/// synthesize concrete [`PlannedStep`]s whose `classification` is the
/// invoking plan's `data_ceiling` (so deterministic-policy I2/I3 hold and
/// governance reduces to the I1 check on the plan the agent declared).
///
/// On any failure returns an [`InvokeRefusal`] collecting the reason(s) —
/// the inner loop audits it (`l3.invoke_rejected`) and feeds it back so
/// the agent replans.
pub fn expand_for_agent(
    template: &L3SkillCandidate,
    stored_trust: SkillTrust,
    args: &BTreeMap<String, String>,
    live_tools: &BTreeSet<String>,
    data_ceiling: DataClass,
) -> Result<Vec<PlannedStep>, InvokeRefusal> {
    if !is_autonomously_invocable(stored_trust) {
        return Err(InvokeRefusal {
            reasons: vec![format!(
                "skill trust '{}' is not autonomously invocable (agent may invoke only pinned skills)",
                stored_trust.as_str()
            )],
        });
    }
    let concrete = prepare_invocation(template, stored_trust, args, live_tools)?;
    Ok(concrete
        .into_iter()
        .map(|s| planned_step_from_l3_with_class(&s, data_ceiling))
        .collect())
}

/// A pinned L3 skill resolved by name, ready for agent-path expansion.
#[derive(Debug, Clone)]
pub struct PinnedSkill {
    pub memory_id: i64,
    pub template: L3SkillCandidate,
    pub body_sha256: String,
}

/// Load the newest `pinned` L3 skill whose `template.name == name`.
///
/// Trust is filtered in SQL (`load_layer_by_trust(Skill, ["pinned"], …)`);
/// a defensive [`is_autonomously_invocable`] re-check runs over the result
/// so a future SQL/Rust divergence fails safe. Newest-wins resolves the
/// unlikely same-name case (matches surfacing's newest-first order).
/// `Ok(None)` when no pinned skill of that name exists — the inner loop
/// turns that into an "unknown or non-pinned skill" refusal.
pub async fn load_pinned_skill_by_name(
    pool: &PgPool,
    name: &str,
) -> Result<Option<PinnedSkill>, hhagent_db::DbError> {
    // Caps the TOTAL pinned rows scanned (newest-first), not same-name
    // collisions: a pinned skill older than the 64 newest pinned rows
    // would not resolve and would surface to the agent as "unknown skill".
    // Acceptable — pinning is a deliberate, rare human action; 64 distinct
    // pinned skills is a generous ceiling.
    const SCAN_CAP: usize = 64;
    let rows = load_layer_by_trust(pool, MemoryLayer::Skill, &["pinned"], SCAN_CAP).await?;
    for row in rows {
        let trust = row
            .metadata
            .get("trust")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !is_autonomously_invocable(SkillTrust::from_metadata_str(trust)) {
            continue; // defense-in-depth; SQL already excluded these
        }
        let template: L3SkillCandidate = match row
            .metadata
            .get("template")
            .cloned()
            .and_then(|t| serde_json::from_value(t).ok())
        {
            Some(t) => t,
            None => continue, // unparseable template — skip fail-safe
        };
        if template.name != name {
            continue;
        }
        let body_sha256 = row
            .metadata
            .get("body_sha256")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        return Ok(Some(PinnedSkill { memory_id: row.id, template, body_sha256 }));
    }
    Ok(None)
}
