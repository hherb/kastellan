//! `sift` (#922): one planner step that goes through every item of a
//! candidate list — reading each one if asked — keeps the items a cheap
//! per-item decision (the "sorter") says match, with the fields it
//! extracted, in the task's scrapbook, and tells the planner only the counts
//! and a small sample.
//!
//! * [`params`] — the call's parameters, scrapbook refs, the read allowlist.
//! * [`template`] — the candidate list, and each item's reads.
//! * [`config`] — the two operator knobs, checked at boot.
//! * [`sorter`] — the per-item decision, and what it is shown.
//! * [`table`] — the answer's table, rendered by code.
//! * [`router_sorter`] — the sorter on the local LLM.
//! * [`answer`] — an answer's `table`: the scrapbook rows it names, rendered.
//! * [`run`] — a sift in the notebook loop: one reviewed plan of reads, a
//!   sorter call per item, the scrapbook rows, the summary the planner sees.
//!
//! Design: `docs/superpowers/specs/2026-11-02-sift-and-scrapbook-design.md`.

pub mod config;
pub(crate) mod params;
pub mod router_sorter;
pub(crate) mod run;
pub mod sorter;
pub(crate) mod answer;
pub(crate) mod table;
pub(crate) mod template;

/// The reserved built-in tool name (K1): no worker manifest may claim it.
pub const SIFT_TOOL: &str = "sift";
/// The one method on [`SIFT_TOOL`].
pub const SIFT_METHOD: &str = "sift.run";

/// How a sift's plan of reads begins its `decision`, before the criterion:
/// what tells it apart from a plan the planner wrote.
pub(crate) const SIFT_READS_DECISION: &str = "sift reads: ";

/// Is `plan` a sift's reads ([`SIFT_READS_DECISION`]) or the record of a
/// sift itself (its one step is [`SIFT_TOOL`])? Neither is a result the
/// planner saw: a sift over a step skips them, and so does the repeat guard.
pub(crate) fn is_sift_plan(plan: &crate::cassandra::types::Plan) -> bool {
    plan.decision.starts_with(SIFT_READS_DECISION) || plan.steps.first().is_some_and(|s| s.tool == SIFT_TOOL)
}

/// May `ctx`'s task read scrapbook `r` (K11)? Its own, or one from a turn
/// this task loaded as its conversation — same channel, peer and
/// conversation, inside the window. Never any other task's.
pub(crate) fn may_read(ctx: &crate::scheduler::inner_loop::TaskContext, r: params::ScrapRef) -> bool {
    r.task_id == ctx.task_id || ctx.conversation_task_ids.as_ref().is_some_and(|ids| ids.contains(&r.task_id))
}

#[cfg(test)]
mod tests;
