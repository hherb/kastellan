//! Integration tests for the `kastellan-cli observation replay`
//! subcommand. Spawns the binary as a subprocess against a per-test
//! tempdir of hand-crafted captures.

use std::process::Command;

use tempfile::TempDir;

use kastellan_core::cassandra::types::{DataClass, Plan};
use kastellan_core::observation::capture::{CaptureJson, CapturedAuditRow, CapturedPlan};
use kastellan_tests_common::cli_binary;

fn approve_capture() -> CaptureJson {
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
        invoke_skill: None,
        python_skill: None,
    };
    let plan_value = serde_json::to_value(&plan).unwrap();
    CaptureJson {
        schema_version: 2,
        fixture_id: "cli-approve-baseline".into(),
        fixture_summary: "synthetic approve baseline for CLI test".into(),
        captured_at: "2026-05-15T11:00:00Z".into(),
        llm_backend: "local".into(),
        llm_model: "gemma4:26b".into(),
        llm_base_url: "http://localhost:11434/v1".into(),
        prompt: "synthetic prompt".into(),
        task_id: 300,
        task_state: "completed".into(),
        plan_iterations: 1,
        plans: vec![CapturedPlan {
            iter: 1,
            plan_json: plan_value.clone(),
            verdict_today: Some("approve".into()),
            step_count: 0,
            data_ceiling: "Public".into(),
            source_truncated: false,
        }],
        audit_rows: vec![CapturedAuditRow {
            id: 1,
            ts: "2026-05-15T11:00:01Z".into(),
            actor: "agent".into(),
            action: "plan.formulate".into(),
            payload: serde_json::json!({
                "task_id": 300,
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

fn write_capture(root: &std::path::Path, capture: &CaptureJson) {
    let dir = root.join(&capture.fixture_id);
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("2026-05-15_synthetic.json");
    let bytes = serde_json::to_vec_pretty(capture).unwrap();
    std::fs::write(p, bytes).unwrap();
}

#[test]
fn cli_observation_replay_happy_path() {
    let tempdir = TempDir::new().unwrap();
    write_capture(tempdir.path(), &approve_capture());

    let bin = cli_binary();
    if !bin.exists() {
        eprintln!("[SKIP] kastellan-cli binary not built; run `cargo build` first");
        return;
    }

    let out = Command::new(&bin)
        .arg("observation")
        .arg("replay")
        .arg("--captures-dir")
        .arg(tempdir.path())
        .output()
        .expect("spawn kastellan-cli observation replay");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(out.status.success(), "exit ok; stderr = {stderr}");
    assert!(stdout.contains("cli-approve-baseline"),
        "fixture id row must appear; stdout = {stdout}");
    assert!(stdout.contains("approve"),
        "verdict kind must appear; stdout = {stdout}");
    assert!(stdout.contains("1 plans across 1 fixtures"),
        "summary line must appear; stdout = {stdout}");
}

#[test]
fn cli_observation_replay_rejects_unknown_flag() {
    let tempdir = TempDir::new().unwrap();
    let bin = cli_binary();
    if !bin.exists() {
        eprintln!("[SKIP] kastellan-cli binary not built");
        return;
    }

    let out = Command::new(&bin)
        .arg("observation")
        .arg("replay")
        .arg("--captures-dir")
        .arg(tempdir.path())
        .arg("--bogus")
        .output()
        .expect("spawn");

    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn cli_observation_replay_empty_dir_exits_zero() {
    let tempdir = TempDir::new().unwrap();
    let bin = cli_binary();
    if !bin.exists() {
        eprintln!("[SKIP] kastellan-cli binary not built");
        return;
    }

    let out = Command::new(&bin)
        .arg("observation")
        .arg("replay")
        .arg("--captures-dir")
        .arg(tempdir.path())
        .output()
        .expect("spawn");

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success());
    assert!(stdout.contains("no captures found"),
        "empty-dir message must appear; stdout = {stdout}");
}
