//! Integration tests for `core::observation::replay`.
//!
//! Pure offline tests — no PG, no LLM, no daemon. The harness reads
//! capture files from a per-test scratch dir; the test owns those
//! files end-to-end so the production captures under
//! `tests/observation/captures/` are not touched.

use std::sync::Arc;

use tempfile::TempDir;

use hhagent_core::cassandra::review::{ChainReviewStage, NoopReviewStage};
use hhagent_core::cassandra::types::{DataClass, Plan};
use hhagent_core::observation::capture::{CaptureJson, CapturedAuditRow, CapturedPlan};
use hhagent_core::observation::replay::{load_captures_from_dir, replay_capture};

fn approve_baseline_capture() -> CaptureJson {
    let plan = Plan {
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
    };
    let plan_value = serde_json::to_value(&plan).unwrap();
    CaptureJson {
        schema_version: 2,
        fixture_id: "t1-approve-baseline-with-plan-body".into(),
        fixture_summary: "synthetic approve baseline".into(),
        captured_at: "2026-05-15T10:00:00Z".into(),
        llm_backend: "local".into(),
        llm_model: "gemma4:26b".into(),
        llm_base_url: "http://localhost:11434/v1".into(),
        prompt: "synthetic prompt".into(),
        task_id: 100,
        task_state: "completed".into(),
        plan_iterations: 1,
        plans: vec![CapturedPlan {
            iter: 1,
            plan_json: plan_value.clone(),
            verdict_today: Some("approve".into()),
            step_count: 0,
            data_ceiling: "Public".into(),
        }],
        audit_rows: vec![CapturedAuditRow {
            id: 1,
            ts: "2026-05-15T10:00:01Z".into(),
            actor: "agent".into(),
            action: "plan.formulate".into(),
            payload: serde_json::json!({
                "task_id": 100,
                "plan_count": 1,
                "decision_kind": "task_complete",
                "plan_step_count": 0,
                "refused": serde_json::Value::Null,
                "plan": plan_value,
                "classification_floor": "Public",
            }),
        }],
    }
}

fn pre_slice_a_capture() -> CaptureJson {
    CaptureJson {
        schema_version: 2,
        fixture_id: "t2-missing-plan-body".into(),
        fixture_summary: "synthetic pre-Slice-A capture".into(),
        captured_at: "2026-05-14T10:00:00Z".into(),
        llm_backend: "local".into(),
        llm_model: "gemma4:26b".into(),
        llm_base_url: "http://localhost:11434/v1".into(),
        prompt: "pre-Slice-A synthetic prompt".into(),
        task_id: 200,
        task_state: "completed".into(),
        plan_iterations: 1,
        plans: vec![CapturedPlan {
            iter: 1,
            plan_json: serde_json::Value::Null,
            verdict_today: Some("approve".into()),
            step_count: 0,
            data_ceiling: "Public".into(),
        }],
        audit_rows: vec![CapturedAuditRow {
            id: 1,
            ts: "2026-05-14T10:00:01Z".into(),
            actor: "agent".into(),
            action: "plan.formulate".into(),
            payload: serde_json::json!({
                "task_id": 200,
                "plan_count": 1,
                "decision_kind": "task_complete",
                "plan_step_count": 0,
                "refused": serde_json::Value::Null,
                // No `plan` key — pre-Slice-A.
            }),
        }],
    }
}

fn write_synthetic_capture(root: &std::path::Path, capture: &CaptureJson) {
    let fixture_dir = root.join(&capture.fixture_id);
    std::fs::create_dir_all(&fixture_dir).unwrap();
    let fname = format!("{}_synthetic.json", &capture.captured_at[..10]);
    let path = fixture_dir.join(fname);
    let bytes = serde_json::to_vec_pretty(capture).unwrap();
    std::fs::write(path, bytes).unwrap();
}

#[tokio::test]
async fn replay_against_approve_baseline_yields_no_delta() {
    let tempdir = TempDir::new().expect("tempdir");
    let capture = approve_baseline_capture();
    write_synthetic_capture(tempdir.path(), &capture);

    let loaded = load_captures_from_dir(tempdir.path())
        .expect("load synthetic captures");
    assert_eq!(loaded.len(), 1);

    let chain = ChainReviewStage::new(vec![Arc::new(NoopReviewStage)]);
    let result = replay_capture(&loaded[0].capture, &chain).await;

    assert_eq!(result.fixture_id, "t1-approve-baseline-with-plan-body");
    assert_eq!(result.plans_replayed, 1);
    assert_eq!(result.plans_skipped_missing_body, 0);
    assert_eq!(result.per_plan.len(), 1);
    assert!(!result.per_plan[0].is_delta);
    assert_eq!(
        result.per_plan[0].new_verdict.as_ref().unwrap().kind,
        "approve",
    );
}

#[tokio::test]
async fn replay_against_pre_slice_a_capture_skips_with_reason() {
    let tempdir = TempDir::new().expect("tempdir");
    let capture = pre_slice_a_capture();
    write_synthetic_capture(tempdir.path(), &capture);

    let loaded = load_captures_from_dir(tempdir.path())
        .expect("load synthetic captures");
    assert_eq!(loaded.len(), 1);

    let chain = ChainReviewStage::new(vec![Arc::new(NoopReviewStage)]);
    let result = replay_capture(&loaded[0].capture, &chain).await;

    assert_eq!(result.fixture_id, "t2-missing-plan-body");
    assert_eq!(result.plans_replayed, 0);
    assert_eq!(result.plans_skipped_missing_body, 1);
    assert!(result.per_plan[0].new_verdict.is_none());
    assert!(result.per_plan[0].skipped_reason.is_some());
}
