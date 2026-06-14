//! Classification-floor provenance for the inner loop.
//!
//! A task's `classification_floor` can be set four ways (operator flag, CLI
//! keyword inference, an agent mid-task raise, or the default). [`ClassificationFloorSource`]
//! records *which*, and [`apply_floor_raise`] is the one place the agent-raise
//! path is taken. Both are re-exported from [`super`] so existing paths
//! (`scheduler::inner_loop::ClassificationFloorSource`) keep resolving.

use serde::{Deserialize, Serialize};

use crate::cassandra::types::Plan;

use super::TaskContext;

/// Provenance of the current `classification_floor` value.
///
/// Carried in [`TaskContext`] and emitted into the
/// `agent/plan.formulate` audit-row payload so operators can trace
/// any DP-blocked plan back to how the floor was set.
///
/// Wire form (lowercase snake_case via serde) matches the
/// operator-visible audit-log token — renaming any branch is an
/// audit-trail contract break. Mirrors the `as_pascal_str` shape on
/// `DataClass`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClassificationFloorSource {
    /// Operator explicitly passed `--classification-floor X`.
    Operator,
    /// CLI keyword classifier elevated above Public.
    CliInferred,
    /// Agent raised the floor mid-task via `Plan.floor_request`.
    AgentRaised,
    /// No inference matched and no operator flag was set.
    Default,
}

impl ClassificationFloorSource {
    /// Canonical lowercase snake_case string, identical to the serde wire
    /// form. Used by audit-log payload emitters so the rendered tag is a
    /// formal contract instead of relying on the de-facto stability of
    /// `Debug`. Renaming any branch is an audit-trail contract break.
    pub fn as_snake_str(self) -> &'static str {
        match self {
            ClassificationFloorSource::Operator    => "operator",
            ClassificationFloorSource::CliInferred => "cli_inferred",
            ClassificationFloorSource::AgentRaised => "agent_raised",
            ClassificationFloorSource::Default     => "default",
        }
    }
}

/// Apply `plan.floor_request` to `ctx` if it raises the current floor.
/// Pure side-effect on `ctx`. Returns true iff `ctx` was mutated.
///
/// Never lowers the floor: a `floor_request` whose rank is ≤ the
/// current floor is a no-op (pinned by
/// `agent_floor_request_lower_than_producer_is_ignored`).
///
/// On a successful raise, also flips
/// `ctx.classification_floor_source` to `AgentRaised` and clears
/// `ctx.classification_floor_signals` (the signals explained the
/// original CLI inference, not the elevated floor).
pub(super) fn apply_floor_raise(ctx: &mut TaskContext, plan: &Plan) -> bool {
    if let Some(req) = plan.floor_request {
        if req.rank() > ctx.classification_floor.rank() {
            ctx.classification_floor = req;
            ctx.classification_floor_source = ClassificationFloorSource::AgentRaised;
            ctx.classification_floor_signals.clear();
            return true;
        }
    }
    false
}
