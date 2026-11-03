//! The notebook loop's audit rows (#922): a model text the planner-text
//! screen blocks (`policy / injection.blocked`, once, when it lands) and a
//! turn whose reply the loop set aside unreviewed (`agent / plan.discarded`).

use serde_json::json;
use sqlx::PgPool;

use super::reply::TickSpec;
use super::state::{NotebookState, StepSpec};
use crate::scheduler::agent::FormulationMeta;
use crate::scheduler::inner_loop::{notebook_block_audit_payload, screen_model_text, TaskContext};

/// Audit action for a turn whose reply the loop set aside unreviewed. Not a
/// `plan.formulate`: the observation capture pairs each of those with the
/// next verdict, and this reply has none.
pub(crate) const ACTION_PLAN_DISCARDED: &str = "plan.discarded";

/// Write a `policy / injection.blocked` row for each part of a tick the
/// planner-text screen blocks, once, when the tick lands — the render
/// withholds it on every turn after, and must not audit it every turn.
/// Best-effort, like the sink rows.
pub(crate) async fn audit_screened_tick(pool: &PgPool, task_id: i64, tick: &TickSpec) {
    let parts = [("note", tick.note.as_str()), ("summary", tick.summary.as_str())]
        .into_iter()
        .chain(tick.findings.iter().flat_map(|f| [("finding", f.text.as_str()), ("finding", f.source.as_str())]));
    for (part, text) in parts {
        audit_if_blocked(pool, task_id, tick.step, part, text).await;
    }
}

/// [`audit_screened_tick`] for a `steps` list. A replan re-sends steps it
/// did not change; those were audited when first written, so only a step
/// whose text is new is screened here.
pub(crate) async fn audit_screened_steps(pool: &PgPool, task_id: i64, before: &NotebookState, steps: &[StepSpec]) {
    for s in steps {
        let unchanged = before.steps.iter().any(|b| {
            b.spec.id == s.id && b.spec.purpose == s.purpose && b.spec.tool == s.tool && b.spec.done_when == s.done_when
        });
        if unchanged {
            continue;
        }
        for (part, text) in [("purpose", &s.purpose), ("tool", &s.tool), ("done_when", &s.done_when)] {
            audit_if_blocked(pool, task_id, s.id, part, text).await;
        }
    }
}

pub(crate) async fn audit_if_blocked(pool: &PgPool, task_id: i64, step: u32, part: &str, text: &str) {
    if let Some(block) = screen_model_text(text) {
        let payload = notebook_block_audit_payload(task_id, step, part, &block);
        if let Err(e) = kastellan_db::audit::insert(pool, "policy", "injection.blocked", payload).await {
            tracing::error!(task_id, error = %e, "notebook injection.blocked audit insert failed");
        }
    }
}

/// Write the `agent / plan.discarded` row for a turn whose reply was set
/// aside unreviewed (`reason`: `repeated_call`, `answer_rewritten`): the
/// model call is accounted for as a `plan.formulate` row accounts for one.
/// Best-effort, like the `plan.formulate_failed` rows.
pub(crate) async fn audit_discarded_turn(
    pool: &PgPool,
    ctx: &TaskContext,
    reason: &str,
    method: Option<&str>,
    meta: &FormulationMeta,
) {
    let payload = json!({
        "task_id": ctx.task_id,
        "plan_count": ctx.plan_count,
        "reason": reason,
        "method": method,
        "prompt_name": meta.prompt_name,
        "llm_model": meta.llm_model,
        "latency_ms": meta.latency_ms,
        "retry_count": meta.retry_count,
        "llm_usage": serde_json::to_value(&meta.usage)
            .expect("CompletionStats serialisation cannot fail (integers, a string, no maps)"),
    });
    if let Err(e) = kastellan_db::audit::insert(pool, "agent", ACTION_PLAN_DISCARDED, payload).await {
        tracing::error!(task_id = ctx.task_id, error = %e, "plan.discarded audit insert failed");
    }
}
