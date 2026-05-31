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
/// 25 keys for non-`CliInferred` sources, 26 when `CliInferred` carries
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
/// crystallised memories row, not here. This brings the default-source
/// key count to 25, and `CliInferred`+signals to 26.
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
mod tests {
    use super::*;
    use crate::cassandra::types::{DataClass, Plan, PlannedStep};

    /// Canonical text-only terminal Plan; tests vary single fields via
    /// `..make_text_plan()` struct-update syntax.
    fn make_text_plan() -> Plan {
        Plan {
            context: "".into(),
            decision: "task_complete".into(),
            rationale: "".into(),
            steps: vec![],
            result: Some(serde_json::json!({"kind": "text", "body": "ok"})),
            data_ceiling: DataClass::Public,
            refused: None,
            floor_request: None,
            l1_insight: None,
            l3_skill: None,
        }
    }

    /// Canonical baseline meta with empty recall; tests vary the
    /// recall/sha256/count fields via `..make_default_meta()`.
    fn make_default_meta() -> FormulationMeta {
        FormulationMeta {
            prompt_name: "agent_planner".into(),
            prompt_sha256: "p1".into(),
            llm_model: "lm".into(),
            llm_backend: "local".into(),
            latency_ms: 1,
            retry_count: 0,
            assembled_prompt_sha256: "ax".into(),
            l0_count: 0,
            l1_count: 0,
            recalled_memory_ids: Vec::new(),
            recall_count: 0,
            recall_query_sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
            graph_seed_entity_ids: Vec::new(),
            graph_seed_count: 0,
            graph_seed_source: crate::entity_extraction::SeedSource::None,
        }
    }

    #[test]
    fn build_plan_formulate_payload_carries_full_plan_and_classification_floor() {
        let plan = Plan {
            context: "ctx".into(),
            decision: "act".into(),
            rationale: "r".into(),
            steps: vec![PlannedStep {
                tool: "shell-exec".into(),
                method: "shell.exec".into(),
                parameters: serde_json::json!({"argv": ["/bin/echo", "hi"]}),
                returns: "stdout".into(),
                done_when: "echoed".into(),
                classification: DataClass::Public,
            }],
            result: None,
            data_ceiling: DataClass::Personal,
            refused: None,
            floor_request: None,
            l1_insight: None,
            l3_skill: None,
        };
        let meta = FormulationMeta {
            prompt_sha256: "deadbeef".into(),
            llm_model: "gemma4:26b".into(),
            latency_ms: 42,
            assembled_prompt_sha256: "cafebabe".into(),
            l0_count: 7,
            l1_count: 3,
            ..make_default_meta()
        };
        let payload = build_plan_formulate_payload(
            7, 1, DataClass::ClinicalConfidential,
            ClassificationFloorSource::Default, &[], &plan, &meta,
        );

        // Full Plan JSON round-trips byte-for-byte.
        let plan_back: Plan = serde_json::from_value(payload["plan"].clone())
            .expect("plan key must deserialise back into a Plan");
        assert_eq!(plan_back, plan, "plan payload field must round-trip");

        // Task-level classification_floor stringified PascalCase.
        assert_eq!(payload["classification_floor"], "ClinicalConfidential");

        assert_eq!(payload["task_id"], 7);
        assert_eq!(payload["plan_count"], 1);
        assert_eq!(payload["decision_kind"], "act");
        assert_eq!(payload["plan_step_count"], 1);
        assert!(payload["refused"].is_null());

        // Slice C value round-trips catch a "wrong field" bug — e.g. a
        // refactor wiring meta.prompt_sha256 (the base prompt) into the
        // system_prompt_sha256 key instead of meta.assembled_prompt_sha256
        // (the assembled prompt the model actually saw).
        assert_eq!(payload["system_prompt_sha256"], "cafebabe");
        assert_eq!(payload["l0_count"], 7u64);
        assert_eq!(payload["l1_count"], 3u64);
    }

    #[test]
    fn build_plan_formulate_payload_pins_twenty_five_keys_for_default_source() {
        // Slice D (2026-05-17, recall-lane wiring) bumped the
        // default-source key count from 17 to 20 by adding
        // recalled_memory_ids, recall_count, recall_query_sha256.
        // Slice E (2026-05-18, l1-promotion-writer) bumped to 21 by
        // adding l1_insight.
        // Slice F (2026-05-19, entity-extraction v2) bumps to 24 by
        // adding graph_seed_entity_ids, graph_seed_count, graph_seed_source.
        // Slice G (2026-05-31, l3-skill-crystallisation) bumps to 25 by
        // adding l3_skill.
        let meta = FormulationMeta {
            recalled_memory_ids: vec![100, 200],
            recall_count: 2,
            recall_query_sha256: "f".repeat(64),
            ..make_default_meta()
        };
        let payload = build_plan_formulate_payload(
            1, 1, DataClass::Public, ClassificationFloorSource::Default,
            &[], &make_text_plan(), &meta,
        );
        let obj = payload.as_object().expect("payload object");
        let got: std::collections::BTreeSet<&str> =
            obj.keys().map(|s| s.as_str()).collect();
        let expected: std::collections::BTreeSet<&str> = [
            "task_id", "plan_count", "prompt_name", "prompt_sha256",
            "llm_model", "llm_backend", "latency_ms", "retry_count",
            "plan_step_count", "decision_kind", "refused",
            "plan", "classification_floor", "classification_floor_source",
            "system_prompt_sha256", "l0_count", "l1_count",
            "recalled_memory_ids", "recall_count", "recall_query_sha256",
            "l1_insight", "l3_skill",
            "graph_seed_entity_ids", "graph_seed_count", "graph_seed_source",
        ].into_iter().collect();
        assert_eq!(got, expected,
            "default-source payload must carry exactly 25 keys; diff:\n\
             missing = {:?}\nextra = {:?}",
            expected.difference(&got).collect::<Vec<_>>(),
            got.difference(&expected).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn build_plan_formulate_payload_cli_inferred_source_has_26_keys_with_signals() {
        let payload = build_plan_formulate_payload(
            1, 1, DataClass::ClinicalConfidential,
            ClassificationFloorSource::CliInferred,
            &["patient".to_string(), "pathology".to_string()],
            &make_text_plan(), &make_default_meta(),
        );
        let obj = payload.as_object().expect("payload object");
        assert_eq!(obj.len(), 26,
            "cli_inferred + signals must carry 26 keys (25 default + signals); got {} keys: {:?}",
            obj.len(), obj.keys().collect::<Vec<_>>(),
        );
        assert_eq!(
            payload["classification_floor_signals"],
            serde_json::json!(["patient", "pathology"]),
            "all signals (not just the first) must pass through to the audit payload",
        );
    }

    #[test]
    fn build_plan_formulate_payload_recall_keys_round_trip_through_meta() {
        let meta = FormulationMeta {
            recalled_memory_ids: vec![42, 99, 7],
            recall_count: 3,
            recall_query_sha256: "deadbeef".repeat(8),  // 64 hex chars
            ..make_default_meta()
        };
        let payload = build_plan_formulate_payload(
            1, 1, DataClass::Public, ClassificationFloorSource::Default,
            &[], &make_text_plan(), &meta,
        );
        assert_eq!(payload["recalled_memory_ids"], serde_json::json!([42, 99, 7]));
        assert_eq!(payload["recall_count"], 3u64);
        assert_eq!(payload["recall_query_sha256"], serde_json::json!("deadbeef".repeat(8)));
    }

    #[test]
    fn build_plan_formulate_payload_graph_seed_keys_round_trip_through_meta() {
        // Slice F (2026-05-19, entity-extraction v2): the three
        // graph_seed_* keys must round-trip through the meta struct
        // and serialize SeedSource as snake_case.
        let meta = FormulationMeta {
            graph_seed_entity_ids: vec![11, 22, 33],
            graph_seed_count: 3,
            graph_seed_source: crate::entity_extraction::SeedSource::GlinerRelex,
            ..make_default_meta()
        };
        let payload = build_plan_formulate_payload(
            1, 1, DataClass::Public, ClassificationFloorSource::Default,
            &[], &make_text_plan(), &meta,
        );
        assert_eq!(payload["graph_seed_entity_ids"], serde_json::json!([11, 22, 33]));
        assert_eq!(payload["graph_seed_count"], 3u64);
        assert_eq!(payload["graph_seed_source"], serde_json::json!("gliner_relex"));
    }

    #[test]
    fn build_plan_formulate_payload_graph_seed_source_serializes_none_as_snake_case() {
        // Default meta has SeedSource::None — must serialize as "none",
        // matching the snake_case rename_all on the enum.
        let payload = build_plan_formulate_payload(
            1, 1, DataClass::Public, ClassificationFloorSource::Default,
            &[], &make_text_plan(), &make_default_meta(),
        );
        assert_eq!(payload["graph_seed_source"], serde_json::json!("none"));
        assert_eq!(payload["graph_seed_entity_ids"], serde_json::json!([] as [i64; 0]));
        assert_eq!(payload["graph_seed_count"], 0u64);
    }

    #[test]
    fn build_plan_formulate_payload_recall_query_sha256_is_64_hex_chars_in_empty_default() {
        // When recall degraded (or returned no rows), the sha256 of the
        // empty string still satisfies the 64-hex-char contract.
        // Observation phase SQL can pin the format without a special case.
        let payload = build_plan_formulate_payload(
            1, 1, DataClass::Public, ClassificationFloorSource::Default,
            &[], &make_text_plan(), &make_default_meta(),
        );
        let sha = payload["recall_query_sha256"].as_str().expect("string");
        assert_eq!(sha.len(), 64, "recall_query_sha256 must always be 64 chars; got {sha}");
        assert!(sha.chars().all(|c| c.is_ascii_hexdigit()),
                "recall_query_sha256 must be hex; got {sha}");
    }

    #[test]
    fn build_plan_formulate_payload_default_source_omits_signals_key() {
        let payload = build_plan_formulate_payload(
            1, 1, DataClass::Public, ClassificationFloorSource::Default,
            &[], &make_text_plan(), &make_default_meta(),
        );
        let obj = payload.as_object().expect("payload is an object");
        assert_eq!(obj.len(), 25);
        assert_eq!(obj["classification_floor_source"], serde_json::Value::String("default".into()));
        assert!(obj.get("classification_floor_signals").is_none(),
            "signals key must be ABSENT when source is not cli_inferred");
    }

    #[test]
    fn build_plan_formulate_payload_agent_raised_source_omits_signals() {
        // After an agent raise, signals are cleared — they only explain
        // the original CLI inference, not the elevated floor.
        let plan = Plan {
            floor_request: Some(DataClass::ClinicalConfidential),
            data_ceiling: DataClass::ClinicalConfidential,
            ..make_text_plan()
        };
        let payload = build_plan_formulate_payload(
            1, 1, DataClass::ClinicalConfidential,
            ClassificationFloorSource::AgentRaised,
            &[],  // empty: signals are cleared on raise
            &plan, &make_default_meta(),
        );
        let obj = payload.as_object().expect("payload is an object");
        assert_eq!(obj.len(), 25,
            "agent_raised should have 25 keys (no signals); got: {:?}", obj.keys().collect::<Vec<_>>());
        assert_eq!(obj["classification_floor_source"], serde_json::Value::String("agent_raised".into()));
        assert!(obj.get("classification_floor_signals").is_none());
    }

    #[test]
    fn plan_formulate_payload_carries_l1_insight_when_set() {
        let plan = Plan {
            l1_insight: Some("learned X".into()),
            ..make_text_plan()
        };
        let payload = build_plan_formulate_payload(
            1, 1, DataClass::Public, ClassificationFloorSource::Default,
            &[], &plan, &make_default_meta(),
        );
        assert_eq!(
            payload.get("l1_insight").expect("l1_insight key must be present"),
            &serde_json::Value::String("learned X".into()),
        );
    }

    #[test]
    fn plan_formulate_payload_carries_explicit_null_l1_insight_when_unset() {
        // The key must be present-but-null when the agent does not set it,
        // so JSONB queries `WHERE payload ? 'l1_insight'` find the row.
        let payload = build_plan_formulate_payload(
            1, 1, DataClass::Public, ClassificationFloorSource::Default,
            &[], &make_text_plan(), &make_default_meta(),
        );
        assert_eq!(
            payload.get("l1_insight").expect("l1_insight key must be present even when None"),
            &serde_json::Value::Null,
        );
    }

    #[test]
    fn build_plan_formulate_payload_l3_skill_compact_shape() {
        use crate::cassandra::types::{L3Param, L3SkillCandidate, L3TemplateStep};
        let mut plan = make_text_plan();
        plan.l3_skill = Some(L3SkillCandidate {
            name: "summarise_repo_readme".into(),
            description: "d".into(),
            parameters: vec![L3Param { name: "repo_path".into(), description: "p".into() }],
            steps: vec![L3TemplateStep {
                tool: "shell-exec".into(), method: "shell.exec".into(),
                parameters: serde_json::json!({ "argv": ["cat", "{{repo_path}}"] }),
            }],
        });
        let payload = build_plan_formulate_payload(
            1, 1, DataClass::Public, ClassificationFloorSource::Default,
            &[], &plan, &make_default_meta(),
        );
        assert_eq!(payload["l3_skill"], serde_json::json!({
            "name": "summarise_repo_readme", "step_count": 1, "param_count": 1
        }));

        // None case: explicit JSON null, not key-absent.
        let none_payload = build_plan_formulate_payload(
            1, 1, DataClass::Public, ClassificationFloorSource::Default,
            &[], &make_text_plan(), &make_default_meta(),
        );
        assert_eq!(none_payload["l3_skill"], serde_json::Value::Null);
        assert!(none_payload.as_object().unwrap().contains_key("l3_skill"));
    }
}
