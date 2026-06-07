//! Pure + async helpers for the audit rows the inner loop emits.
//!
//! Split out from `inner_loop.rs` (issue #81) so the wire-shape pins
//! live next to the payload builder. Composition unchanged: the inner
//! loop calls the `write_audit_*` functions inline; this module only
//! moves their definitions + tests, with no behavioural change.
//!
//! Module map:
//!   - [`build_plan_formulate_payload`] — pure builder for the
//!     `agent/plan.formulate` payload (unit-testable without a pool).
//!   - [`write_audit_plan_formulate`] — async writer that composes
//!     the builder with `hhagent_db::audit::insert`.
//!   - [`write_audit_verdict`] — async writer for
//!     `cassandra:chain/verdict` rows.
//!   - [`write_audit_plan_outcome`] — async writer for
//!     `scheduler/plan.outcome` rows (step execution summary).
//!
//! `pub(super)` visibility on the writers keeps them callable only from
//! sibling modules under `crate::scheduler` (today: only `inner_loop`).
//! The builder is `pub(crate)` so future cross-module consumers (e.g.
//! the observation/replay harness) can build a payload without going
//! through I/O.

use sqlx::PgPool;

use crate::cassandra::types::{DataClass, Plan, Verdict};

use super::agent::FormulationMeta;
use super::inner_loop::{ClassificationFloorSource, InnerLoopError, TaskContext};

/// Pure builder for the `agent/plan.formulate` audit-row payload.
///
/// Extracted from `write_audit_plan_formulate` so the wire shape is
/// unit-testable without a live Postgres pool. The shape pins
/// (in this file's `tests` module) defend against accidental drift —
/// 27 keys for non-`CliInferred` sources, 28 when `CliInferred` carries
/// matched signals.
///
/// Slice A (2026-05-15) added `plan` (full serialised Plan) +
/// `classification_floor` (task-level DataClass) so captures carry
/// everything the reviewer pipeline needs to be replayed offline —
/// see `core::observation::replay`.
///
/// Slice B (2026-05-16) added `classification_floor_source` (always)
/// and conditional `classification_floor_signals` (CliInferred only)
/// so audit consumers can trace how the floor was set.
///
/// Slice C (2026-05-16) added `system_prompt_sha256`, `l0_count`, and
/// `l1_count` so operators can detect L0/L1 drift across daemon restarts
/// and operator edits without grepping logs.
///
/// Slice D (2026-05-17) added `recalled_memory_ids`, `recall_count`,
/// and `recall_query_sha256` so the observation phase can audit which
/// memories the recall lane surfaced and detect drift across captures.
///
/// Slice E (2026-05-17) added `l1_insight` (the agent-raised L1 promotion
/// candidate from `Plan.l1_insight`, explicit JSON `null` when absent
/// — matches the `refused` precedent so JSONB `?` queries find every
/// row). The runner reads `InnerLoopResult.terminal_l1_insight` in
/// `drain_lane` and emits the `actor='scheduler' action='l1.promoted'`
/// row when the agent set a value and the plan reached `Outcome::Completed`.
///
/// Slice F (2026-05-19) added `graph_seed_entity_ids`, `graph_seed_count`,
/// and `graph_seed_source` so the observation phase can audit which
/// entity ids the gliner-relex extractor resolved for the graph lane
/// and which extraction path produced them.
///
/// Slice G (2026-05-31) added `l3_skill` (compact `{name, step_count,
/// param_count}` summary of the agent-raised L3 skill candidate on the
/// terminal plan, or explicit JSON `null` when absent — mirrors the
/// `l1_insight` / `refused` precedent). The full template lives in the
/// crystallised memories row, not here. This brought the default-source
/// key count to 25, and `CliInferred`+signals to 26.
///
/// Slice H (2026-06-01) added `skill_count` (the number of L3 skill rows
/// surfaced into the assembled prompt's `<skills>` block), mirroring
/// `l0_count`/`l1_count`. This brings the default-source key count to 26,
/// and `CliInferred`+signals to 27.
///
/// L3 autonomous-door (2026-06-04) added `invoke_skill` (compact `{name,
/// arg_count}` projection of the agent-emitted invoke directive on the
/// plan, or explicit JSON `null` when absent — mirrors the `l1_insight` /
/// `l3_skill` precedent). This brings the default-source key count to 27,
/// and `CliInferred`+signals to 28.
pub(crate) fn build_plan_formulate_payload(
    task_id: i64,
    plan_count: u32,
    classification_floor: DataClass,
    classification_floor_source: ClassificationFloorSource,
    classification_floor_signals: &[String],
    plan: &Plan,
    meta: &FormulationMeta,
) -> serde_json::Value {
    // Issue #23 (spec §3): "refused" takes precedence over the
    // is_terminal-derived "task_complete" so a refusal payload is
    // wire-distinguishable from a successful completion via the same
    // discriminator field — including the malformed-refusal-with-steps
    // shape the inner-loop short-circuit also honours.
    let decision_kind = if plan.is_refused() {
        crate::cassandra::types::DECISION_REFUSED
    } else if plan.is_terminal() {
        crate::cassandra::types::DECISION_TERMINAL
    } else {
        "act"
    };

    // Explicit JSON null (not key-absent) so downstream JSONB queries
    // can rely on `refused` always being present.
    let refused = plan.refused.as_ref()
        .map(|r| serde_json::json!({ "principle": r.principle, "reason": r.reason }))
        .unwrap_or(serde_json::Value::Null);

    // `plan` is the full Plan JSON. Together with `classification_floor`
    // this is what enables offline replay (Slice B / observation::replay).
    // Plans are typically <1 KiB; the audit-envelope SHA-256 truncation
    // at 4 KiB is the safety net for the rare oversized case.
    let plan_json = serde_json::to_value(plan)
        .expect("Plan serialisation cannot fail (no non-string keys, no NaN)");

    // PascalCase string via DataClass's #[serde(rename_all = "PascalCase")].
    let classification_floor_json = serde_json::to_value(classification_floor)
        .expect("DataClass serialisation cannot fail (closed enum, no payloads)");

    let mut obj = serde_json::Map::new();
    obj.insert("task_id".into(),         serde_json::json!(task_id));
    obj.insert("plan_count".into(),      serde_json::json!(plan_count));
    obj.insert("prompt_name".into(),     serde_json::json!(meta.prompt_name));
    obj.insert("prompt_sha256".into(),   serde_json::json!(meta.prompt_sha256));
    obj.insert("llm_model".into(),       serde_json::json!(meta.llm_model));
    obj.insert("llm_backend".into(),     serde_json::json!(meta.llm_backend));
    obj.insert("latency_ms".into(),      serde_json::json!(meta.latency_ms));
    obj.insert("retry_count".into(),     serde_json::json!(meta.retry_count));
    obj.insert("plan_step_count".into(), serde_json::json!(plan.steps.len()));
    obj.insert("decision_kind".into(),   serde_json::json!(decision_kind));
    obj.insert("refused".into(),         refused);
    // Slice E (l1-promotion-writer, 2026-05-18): the agent-raised L1
    // insight on the terminal plan. Explicit JSON null (not key-absent)
    // so JSONB queries `WHERE payload ? 'l1_insight'` find the row even
    // when the agent did not set an insight — mirrors the `refused` precedent.
    obj.insert(
        "l1_insight".into(),
        match &plan.l1_insight {
            Some(s) => serde_json::Value::String(s.clone()),
            None => serde_json::Value::Null,
        },
    );
    // Slice G (l3-skill-crystallisation, 2026-05-31): compact summary of
    // the agent-raised L3 skill candidate on the terminal plan. Explicit
    // JSON null (not key-absent) so `WHERE payload ? 'l3_skill'` finds
    // every row — mirrors the `l1_insight` / `refused` precedent. The
    // full template lives in the crystallised memories row, not here.
    obj.insert(
        "l3_skill".into(),
        match &plan.l3_skill {
            Some(s) => serde_json::json!({
                "name": s.name,
                "step_count": s.steps.len(),
                "param_count": s.parameters.len(),
            }),
            None => serde_json::Value::Null,
        },
    );
    // Compact `invoke_skill` projection — `{name, arg_count}` when the
    // agent emitted an invoke directive, explicit JSON null otherwise so
    // `WHERE payload ? 'invoke_skill'` finds every row (mirrors the
    // `l1_insight` / `l3_skill` precedent). The full directive (incl. arg
    // values) is NOT embedded here; arg names/values surface in the
    // separate `l3.invoked` envelope + per-step chokepoint rows.
    obj.insert(
        "invoke_skill".into(),
        match &plan.invoke_skill {
            Some(d) => serde_json::json!({ "name": d.name, "arg_count": d.args.len() }),
            None => serde_json::Value::Null,
        },
    );
    // Slice A:
    obj.insert("plan".into(),                 plan_json);
    obj.insert("classification_floor".into(), classification_floor_json);
    // Slice B (automatic floor inference, 2026-05-16):
    obj.insert(
        "classification_floor_source".into(),
        serde_json::json!(classification_floor_source.as_snake_str()),
    );
    // Slice C (prompt-assembler, 2026-05-16): drift detection for
    // L0/L1 across daemon restarts and operator edits. `prompt_sha256`
    // above is the BASE prompt only; `system_prompt_sha256` here is
    // the assembled prompt the model actually saw.
    obj.insert(
        "system_prompt_sha256".into(),
        serde_json::json!(meta.assembled_prompt_sha256),
    );
    obj.insert("l0_count".into(), serde_json::json!(meta.l0_count));
    obj.insert("l1_count".into(), serde_json::json!(meta.l1_count));
    // Slice H (l3-skill-recall-surfacing, 2026-06-01): the count of L3
    // skill rows surfaced into the `<skills>` block. Emitted alongside
    // l0/l1_count so operators can audit skill surfacing the same way.
    obj.insert("skill_count".into(), serde_json::json!(meta.skill_count));
    // Slice D (recall-lane wiring, 2026-05-17): the recall lane's
    // contribution to this iteration. recalled_memory_ids is the
    // RRF-fused id list capped by L_RECALL_CAP_BYTES; recall_count is
    // a cheap-to-query duplicate of its length; recall_query_sha256 is
    // a stable hash of the query text the agent embedded so the
    // observation phase can detect paraphrase vs. genuine drift.
    obj.insert(
        "recalled_memory_ids".into(),
        serde_json::json!(meta.recalled_memory_ids),
    );
    obj.insert("recall_count".into(), serde_json::json!(meta.recall_count));
    obj.insert(
        "recall_query_sha256".into(),
        serde_json::json!(meta.recall_query_sha256),
    );
    // Slice F (entity-extraction v2, 2026-05-19): the graph-lane seeds
    // the extractor resolved + which path produced them. `_source`
    // serializes as snake_case ("gliner_relex" / "none") — JSONB queries
    // filter via WHERE payload->>'graph_seed_source' = 'gliner_relex'.
    obj.insert(
        "graph_seed_entity_ids".into(),
        serde_json::json!(meta.graph_seed_entity_ids),
    );
    obj.insert("graph_seed_count".into(), serde_json::json!(meta.graph_seed_count));
    obj.insert(
        "graph_seed_source".into(),
        serde_json::to_value(meta.graph_seed_source).expect("SeedSource serializes"),
    );
    // Signals key only appears when source is CliInferred AND we have
    // signals. Other sources (Operator / AgentRaised / Default) omit
    // the key (saving JSON payload bytes and making the absence itself
    // a wire signal that no CLI inference was the load-bearing decision).
    if classification_floor_source == ClassificationFloorSource::CliInferred
        && !classification_floor_signals.is_empty()
    {
        obj.insert(
            "classification_floor_signals".into(),
            serde_json::json!(classification_floor_signals),
        );
    }
    serde_json::Value::Object(obj)
}

pub(super) async fn write_audit_plan_formulate(
    pool: &PgPool,
    ctx: &TaskContext,
    plan: &Plan,
    meta: &FormulationMeta,
) -> Result<(), InnerLoopError> {
    let payload = build_plan_formulate_payload(
        ctx.task_id,
        ctx.plan_count,
        ctx.classification_floor,
        ctx.classification_floor_source,
        &ctx.classification_floor_signals,
        plan,
        meta,
    );
    hhagent_db::audit::insert(pool, "agent", "plan.formulate", payload).await?;
    Ok(())
}

pub(super) async fn write_audit_verdict(
    pool: &PgPool,
    ctx: &TaskContext,
    verdict: &Verdict,
    latency_ms: u64,
) -> Result<(), InnerLoopError> {
    let (kind, detail) = match verdict {
        Verdict::Approve => ("approve", serde_json::Value::Null),
        Verdict::Advisory(c) => ("advisory", serde_json::json!(c)),
        Verdict::Escalate(c, s) => ("escalate", serde_json::json!({"concern": c, "severity": s})),
        Verdict::Block(r) => ("block", serde_json::json!(r)),
        Verdict::ConstitutionalBlock { principle, reason } =>
            ("constitutional_block", serde_json::json!({"principle": principle, "reason": reason})),
    };
    let payload = serde_json::json!({
        "task_id":      ctx.task_id,
        "plan_count":   ctx.plan_count,
        "verdict_kind": kind,
        "detail":       detail,
        "latency_ms":   latency_ms,
    });
    hhagent_db::audit::insert(pool, "cassandra:chain", "verdict", payload).await?;
    Ok(())
}

pub(super) async fn write_audit_plan_outcome(
    pool: &PgPool,
    ctx: &TaskContext,
    steps_executed: usize,
    steps_total: usize,
    any_err: bool,
) -> Result<(), InnerLoopError> {
    let payload = serde_json::json!({
        "task_id":         ctx.task_id,
        "plan_count":      ctx.plan_count,
        "terminal_kind":   if any_err { "err" } else { "ok" },
        "steps_executed":  steps_executed,
        "steps_total":     steps_total,
    });
    hhagent_db::audit::insert(pool, "scheduler", "plan.outcome", payload).await?;
    Ok(())
}

#[cfg(test)]
mod tests;
