//! Unit tests for the inner-loop audit-row builders.
//!
//! Lifted verbatim from the parent module's inline `#[cfg(test)] mod tests`
//! (item 9b over-cap test-lift). Production logic lives in the parent
//! `inner_loop_audit.rs`; this file is `mod tests;` from there and is only
//! compiled under `#[cfg(test)]`.

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
        invoke_skill: None,
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
        skill_count: 0,
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
        invoke_skill: None,
    };
    let meta = FormulationMeta {
        prompt_sha256: "deadbeef".into(),
        llm_model: "gemma4:26b".into(),
        latency_ms: 42,
        assembled_prompt_sha256: "cafebabe".into(),
        l0_count: 7,
        l1_count: 3,
        skill_count: 5,
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
    assert_eq!(payload["skill_count"], 5u64);
}

#[test]
fn build_plan_formulate_payload_pins_twenty_seven_keys_for_default_source() {
    // Slice D (2026-05-17, recall-lane wiring) bumped the
    // default-source key count from 17 to 20 by adding
    // recalled_memory_ids, recall_count, recall_query_sha256.
    // Slice E (2026-05-18, l1-promotion-writer) bumped to 21 by
    // adding l1_insight.
    // Slice F (2026-05-19, entity-extraction v2) bumps to 24 by
    // adding graph_seed_entity_ids, graph_seed_count, graph_seed_source.
    // Slice G (2026-05-31, l3-skill-crystallisation) bumps to 25 by
    // adding l3_skill.
    // Slice H (2026-06-01, l3-skill-recall-surfacing) bumps to 26 by
    // adding skill_count.
    // L3 autonomous-door (2026-06-04) bumps to 27 by adding invoke_skill.
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
        "system_prompt_sha256", "l0_count", "l1_count", "skill_count",
        "recalled_memory_ids", "recall_count", "recall_query_sha256",
        "l1_insight", "l3_skill", "invoke_skill",
        "graph_seed_entity_ids", "graph_seed_count", "graph_seed_source",
    ].into_iter().collect();
    assert_eq!(got, expected,
        "default-source payload must carry exactly 27 keys; diff:\n\
         missing = {:?}\nextra = {:?}",
        expected.difference(&got).collect::<Vec<_>>(),
        got.difference(&expected).collect::<Vec<_>>(),
    );
}

#[test]
fn build_plan_formulate_payload_cli_inferred_source_has_28_keys_with_signals() {
    let payload = build_plan_formulate_payload(
        1, 1, DataClass::ClinicalConfidential,
        ClassificationFloorSource::CliInferred,
        &["patient".to_string(), "pathology".to_string()],
        &make_text_plan(), &make_default_meta(),
    );
    let obj = payload.as_object().expect("payload object");
    assert_eq!(obj.len(), 28,
        "cli_inferred + signals must carry 28 keys (27 default + signals); got {} keys: {:?}",
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
    assert_eq!(obj.len(), 27);
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
    assert_eq!(obj.len(), 27,
        "agent_raised should have 27 keys (no signals); got: {:?}", obj.keys().collect::<Vec<_>>());
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

#[test]
fn build_plan_formulate_payload_invoke_skill_compact_shape() {
    // The present-case projection: invoke_skill => {name, arg_count}.
    use crate::cassandra::types::InvokeDirective;
    use std::collections::BTreeMap;
    let mut args = BTreeMap::new();
    args.insert("repo_path".to_string(), "/tmp/repo".to_string());
    args.insert("max_lines".to_string(), "40".to_string());
    let mut plan = make_text_plan();
    plan.invoke_skill = Some(InvokeDirective {
        name: "summarise_repo_readme".into(),
        args,
    });
    let payload = build_plan_formulate_payload(
        1, 1, DataClass::Public, ClassificationFloorSource::Default,
        &[], &plan, &make_default_meta(),
    );
    assert_eq!(payload["invoke_skill"]["name"], "summarise_repo_readme");
    assert_eq!(payload["invoke_skill"]["arg_count"], 2);
}
