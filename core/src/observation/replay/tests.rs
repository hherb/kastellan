//! Unit tests for observation replay (`VerdictSnapshot` / `ReplayBundle`
//! construction and the delta/full-snapshot logic).
//!
//! Lifted verbatim from the parent module's inline `#[cfg(test)] mod tests`
//! block (Rust-2018 sibling-module pattern; precedents: `tool_dispatch/tests.rs`,
//! `audit/tests.rs`, `launchd_agents/tests.rs`, `macos_container/tests.rs`).
//! `use super::*` resolves to the parent `replay` module, so every item the
//! tests exercise is reachable exactly as before the lift.

use super::*;
use crate::cassandra::types::Severity;

// ---- VerdictSnapshot::from_verdict ----

#[test]
fn verdict_snapshot_approve_has_no_detail() {
    let s = VerdictSnapshot::from_verdict(&Verdict::Approve);
    assert_eq!(s.kind, "approve");
    assert!(s.detail.is_none());
}

#[test]
fn verdict_snapshot_advisory_carries_message_as_detail_string() {
    let s = VerdictSnapshot::from_verdict(&Verdict::Advisory("careful".into()));
    assert_eq!(s.kind, "advisory");
    assert_eq!(s.detail, Some(serde_json::json!("careful")));
}

#[test]
fn verdict_snapshot_escalate_carries_concern_and_severity_object() {
    let s = VerdictSnapshot::from_verdict(&Verdict::Escalate(
        "high latency".into(),
        Severity::High,
    ));
    assert_eq!(s.kind, "escalate");
    assert_eq!(
        s.detail,
        Some(serde_json::json!({"concern": "high latency", "severity": "high"})),
    );
}

#[test]
fn verdict_snapshot_block_carries_reason_as_detail_string() {
    let s = VerdictSnapshot::from_verdict(&Verdict::Block("denied".into()));
    assert_eq!(s.kind, "block");
    assert_eq!(s.detail, Some(serde_json::json!("denied")));
}

#[test]
fn verdict_snapshot_constitutional_block_carries_principle_and_reason() {
    let s = VerdictSnapshot::from_verdict(&Verdict::ConstitutionalBlock {
        principle: 1,
        reason: "physical_harm".into(),
    });
    assert_eq!(s.kind, "constitutional_block");
    assert_eq!(
        s.detail,
        Some(serde_json::json!({"principle": 1, "reason": "physical_harm"})),
    );
}

#[test]
fn verdict_snapshot_round_trips_through_serde_json() {
    let s = VerdictSnapshot::from_verdict(&Verdict::ConstitutionalBlock {
        principle: 2,
        reason: "fraud".into(),
    });
    let j = serde_json::to_value(&s).expect("snapshot must serialise");
    let s2: VerdictSnapshot =
        serde_json::from_value(j).expect("snapshot must round-trip");
    assert_eq!(s, s2);
}

// ---- is_delta ----

#[test]
fn is_delta_false_when_both_approve() {
    assert!(!is_delta(Some("approve"), Some("approve")));
}

#[test]
fn is_delta_true_when_baseline_approve_new_block() {
    assert!(is_delta(Some("approve"), Some("block")));
}

#[test]
fn is_delta_true_when_baseline_approve_new_constitutional_block() {
    assert!(is_delta(Some("approve"), Some("constitutional_block")));
}

#[test]
fn is_delta_true_when_baseline_missing_new_not_approve() {
    // Baseline absent + new verdict is anything but approve = delta.
    // Operator wants to see "something fired where the capture
    // never observed a verdict."
    assert!(is_delta(None, Some("block")));
}

#[test]
fn is_delta_false_when_baseline_missing_new_approve() {
    // Baseline absent + new approve = not a delta. "Same default
    // posture" — nothing interesting to flag.
    assert!(!is_delta(None, Some("approve")));
}

#[test]
fn is_delta_false_when_new_missing_skipped() {
    // new = None means the plan was skipped (pre-Slice-A capture);
    // no comparison possible. Per spec: skipped plans are never deltas.
    assert!(!is_delta(Some("approve"), None));
    assert!(!is_delta(None, None));
}

// ---- format_report_table ----

fn dummy_result(fixture_id: &str, per_plan: Vec<ReplayedPlan>) -> ReplayResult {
    let n: u32 = per_plan.iter().filter(|p| p.skipped_reason.is_none()).count() as u32;
    let s: u32 = per_plan.iter().filter(|p| p.skipped_reason.is_some()).count() as u32;
    ReplayResult {
        fixture_id: fixture_id.into(),
        fixture_summary: format!("summary of {fixture_id}"),
        captured_at: "2026-05-15T00:00:00Z".into(),
        llm_model: "gemma4:26b".into(),
        plans_replayed: n,
        plans_skipped_missing_body: s,
        per_plan,
    }
}

fn approve_plan(iter: u32) -> ReplayedPlan {
    ReplayedPlan {
        iter,
        baseline_verdict: Some("approve".into()),
        new_verdict: Some(VerdictSnapshot {
            kind: "approve".into(),
            detail: None,
        }),
        is_delta: false,
        skipped_reason: None,
    }
}

fn cb_plan(iter: u32, principle: u8) -> ReplayedPlan {
    ReplayedPlan {
        iter,
        baseline_verdict: Some("approve".into()),
        new_verdict: Some(VerdictSnapshot {
            kind: "constitutional_block".into(),
            detail: Some(serde_json::json!({"principle": principle, "reason": "x"})),
        }),
        is_delta: true,
        skipped_reason: None,
    }
}

fn skipped_plan(iter: u32) -> ReplayedPlan {
    ReplayedPlan {
        iter,
        baseline_verdict: Some("approve".into()),
        new_verdict: None,
        is_delta: false,
        skipped_reason: Some("plan body missing".into()),
    }
}

#[test]
fn format_report_table_emits_header_and_one_row_per_plan() {
    let results = vec![dummy_result("f1", vec![approve_plan(1)])];
    let s = format_report_table(&results);
    assert!(s.contains("fixture"), "header row present");
    assert!(s.contains("iter"), "iter column present");
    assert!(s.contains("baseline"), "baseline column present");
    assert!(s.contains("new"), "new column present");
    assert!(s.contains("d?"), "delta column present");
    assert!(s.contains("f1"), "fixture id row present");
    assert!(s.contains("approve"), "verdict kind shown");
}

#[test]
fn format_report_table_marks_deltas_with_asterisk() {
    let results = vec![dummy_result("p1", vec![cb_plan(1, 1)])];
    let s = format_report_table(&results);
    // Delta marker: ASCII '*' (rendered in the d? column).
    assert!(s.contains("*"), "delta marker '*' must be present");
    // Constitutional block detail rendered with principle: "constitutional_block(p=1)".
    assert!(
        s.contains("constitutional_block(p=1)"),
        "constitutional_block detail must show principle index"
    );
}

#[test]
fn format_report_table_marks_skipped_with_dash() {
    let results = vec![dummy_result("ec", vec![skipped_plan(1)])];
    let s = format_report_table(&results);
    // Skipped marker: ASCII '-' (rendered in the d? column).
    assert!(s.contains("-"), "skipped marker '-' must be present");
    assert!(s.contains("[skipped"), "[skipped: ...] tag must be present");
}

#[test]
fn format_report_table_renders_multi_iter_fixture() {
    // Multi-iter case — 3 iterations, last one is a delta.
    let results = vec![dummy_result("ec", vec![
        approve_plan(1),
        approve_plan(2),
        cb_plan(3, 3),
    ])];
    let s = format_report_table(&results);
    // All three iter values appear.
    assert!(s.contains(" 1 "), "iter=1 present");
    assert!(s.contains(" 2 "), "iter=2 present");
    assert!(s.contains(" 3 "), "iter=3 present");
}

#[test]
fn format_report_table_aggregate_summary_line_counts_deltas_and_skipped() {
    let results = vec![
        dummy_result("f1", vec![approve_plan(1)]),
        dummy_result("f2", vec![cb_plan(1, 1)]),
        dummy_result("f3", vec![skipped_plan(1)]),
    ];
    let s = format_report_table(&results);
    // Aggregate summary line.
    assert!(s.contains("3 plans"), "total plans count");
    assert!(s.contains("3 fixtures"), "fixture count");
    assert!(s.contains("1 delta"), "delta count");
    assert!(s.contains("1 skipped"), "skipped count");
}

#[test]
fn format_report_table_empty_input_emits_only_header_and_zero_summary() {
    let s = format_report_table(&[]);
    assert!(s.contains("fixture"), "header row present even with empty input");
    assert!(
        s.contains("0 plans") || s.contains("0 fixtures"),
        "summary line must report zero counts; got:\n{s}"
    );
}

// ---- replay_capture ----

use std::sync::Arc;

use crate::cassandra::review::{ChainReviewStage, NoopReviewStage};
use crate::cassandra::types::{DataClass, Plan};
use crate::observation::capture::{CapturedAuditRow, CapturedPlan};

fn rich_plan_audit_row(id: i64, task_id: i64, plan_body: &Plan) -> CapturedAuditRow {
    // Mimics post-Slice-A agent/plan.formulate payload.
    CapturedAuditRow {
        id,
        ts: "2026-05-15T00:00:00Z".into(),
        actor: "agent".into(),
        action: "plan.formulate".into(),
        payload: serde_json::json!({
            "task_id": task_id,
            "plan_count": 1,
            "decision_kind": "task_complete",
            "plan_step_count": plan_body.steps.len(),
            "refused": serde_json::Value::Null,
            "plan": serde_json::to_value(plan_body).unwrap(),
            "classification_floor": "Public",
        }),
    }
}

fn verdict_audit_row(id: i64, task_id: i64, kind: &str) -> CapturedAuditRow {
    CapturedAuditRow {
        id,
        ts: "2026-05-15T00:00:01Z".into(),
        actor: "cassandra:chain".into(),
        action: "verdict".into(),
        payload: serde_json::json!({
            "task_id": task_id,
            "plan_count": 1,
            "verdict_kind": kind,
            "detail": serde_json::Value::Null,
            "latency_ms": 0,
        }),
    }
}

fn pre_slice_a_plan_audit_row(id: i64, task_id: i64) -> CapturedAuditRow {
    // Mimics pre-Slice-A — no `plan` key.
    CapturedAuditRow {
        id,
        ts: "2026-05-14T00:00:00Z".into(),
        actor: "agent".into(),
        action: "plan.formulate".into(),
        payload: serde_json::json!({
            "task_id": task_id,
            "plan_count": 1,
            "decision_kind": "task_complete",
            "plan_step_count": 0,
            "refused": serde_json::Value::Null,
        }),
    }
}

fn synthetic_capture(audit_rows: Vec<CapturedAuditRow>, plans: Vec<CapturedPlan>) -> CaptureJson {
    CaptureJson {
        // Models a *current* capture — track the live schema version.
        schema_version: crate::observation::capture::SCHEMA_VERSION,
        fixture_id: "test-fixture".into(),
        fixture_summary: "synthetic for replay_capture test".into(),
        captured_at: "2026-05-15T00:00:00Z".into(),
        llm_backend: "local".into(),
        llm_model: "gemma4:26b".into(),
        llm_base_url: "http://localhost:11434/v1".into(),
        prompt: "test prompt".into(),
        task_id: 1,
        task_state: "completed".into(),
        plan_iterations: plans.len() as u32,
        plans,
        audit_rows,
    }
}

fn terminal_plan() -> Plan {
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
        python_skill: None,
    }
}

#[tokio::test]
async fn replay_capture_against_noop_chain_yields_approve_no_delta() {
    let plan = terminal_plan();
    let audit_rows = vec![
        rich_plan_audit_row(1, 1, &plan),
        verdict_audit_row(2, 1, "approve"),
    ];
    let plans = vec![CapturedPlan {
        iter: 1,
        plan_json: serde_json::to_value(&plan).unwrap(),
        verdict_today: Some("approve".into()),
        step_count: 0,
        data_ceiling: "Public".into(),
        source_truncated: false,
    }];
    let capture = synthetic_capture(audit_rows, plans);
    let chain = ChainReviewStage::new(vec![Arc::new(NoopReviewStage)]);

    let result = replay_capture(&capture, &chain).await;
    assert_eq!(result.fixture_id, "test-fixture");
    assert_eq!(result.plans_replayed, 1);
    assert_eq!(result.plans_skipped_missing_body, 0);
    assert_eq!(result.per_plan.len(), 1);
    let p = &result.per_plan[0];
    assert_eq!(p.iter, 1);
    assert_eq!(p.baseline_verdict.as_deref(), Some("approve"));
    assert_eq!(p.new_verdict.as_ref().unwrap().kind, "approve");
    assert!(!p.is_delta);
    assert!(p.skipped_reason.is_none());
}

#[tokio::test]
async fn replay_capture_skips_when_plan_body_is_null() {
    // Pre-Slice-A capture shape — plan_json: null on the
    // CapturedPlan AND no `plan` key in the audit-row payload.
    let plans = vec![CapturedPlan {
        iter: 1,
        plan_json: serde_json::Value::Null,
        verdict_today: Some("approve".into()),
        step_count: 0,
        data_ceiling: "Public".into(),
        source_truncated: false,
    }];
    let audit_rows = vec![
        pre_slice_a_plan_audit_row(1, 1),
        verdict_audit_row(2, 1, "approve"),
    ];
    let capture = synthetic_capture(audit_rows, plans);
    let chain = ChainReviewStage::new(vec![Arc::new(NoopReviewStage)]);

    let result = replay_capture(&capture, &chain).await;
    assert_eq!(result.plans_replayed, 0);
    assert_eq!(result.plans_skipped_missing_body, 1);
    assert_eq!(result.per_plan.len(), 1);
    let p = &result.per_plan[0];
    assert!(p.new_verdict.is_none());
    assert!(p.skipped_reason.is_some(),
        "skipped_reason must be populated when plan_json is null");
    assert!(!p.is_delta);
    // Pre-Slice-A rows ARE recoverable by recapture — the reason must say so
    // and must NOT be the truncation message (see the sibling test below).
    assert!(p.skipped_reason.as_deref().unwrap().contains("recapture"));
}

#[tokio::test]
async fn replay_capture_truncated_row_gets_distinct_skip_reason() {
    // Schema-v3 (#62): a truncated source row also arrives with
    // plan_json: null, but `source_truncated: true` — and recapture CANNOT
    // recover it (the audit writer destroyed the payload). The skip reason
    // must be distinct from the pre-Slice-A "recapture" advice.
    let plans = vec![CapturedPlan {
        iter: 1,
        plan_json: serde_json::Value::Null,
        verdict_today: Some("approve".into()),
        step_count: 0,
        data_ceiling: "Public".into(),
        source_truncated: true,
    }];
    let audit_rows = vec![
        pre_slice_a_plan_audit_row(1, 1),
        verdict_audit_row(2, 1, "approve"),
    ];
    let capture = synthetic_capture(audit_rows, plans);
    let chain = ChainReviewStage::new(vec![Arc::new(NoopReviewStage)]);

    let result = replay_capture(&capture, &chain).await;
    assert_eq!(result.plans_replayed, 0);
    assert_eq!(result.plans_skipped_missing_body, 1);
    let reason = result.per_plan[0].skipped_reason.as_deref().unwrap();
    assert!(
        reason.contains("truncation") || reason.contains("elided"),
        "truncated row must name truncation, got: {reason}"
    );
    assert!(
        !reason.contains("recapture against current daemon"),
        "truncated row must not carry the (useless) recapture advice: {reason}"
    );
}
