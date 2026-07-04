//! Unit tests for observation capture (`slug_model`, `capture_filename`,
//! `parse_fixture_prompt`, `extract_plans_from_audit_rows`, and the on-disk
//! JSON schema helpers).
//!
//! Lifted verbatim from the parent module's inline `#[cfg(test)] mod tests`
//! block (Rust-2018 sibling-module pattern; precedents: `replay/tests.rs`,
//! `inner_loop/tests.rs`, `l0_seed/tests.rs`, `injection_guard/tests.rs`).
//! `use super::*` resolves to the parent `capture` module, so every item the
//! tests exercise is reachable exactly as before the lift.

use super::*;

// ---- slug_model ----

#[test]
fn slug_model_lowercases_ascii_input() {
    assert_eq!(slug_model("Qwen3-7B-Instruct"), "qwen3-7b-instruct");
}

#[test]
fn slug_model_normalises_colon_and_underscore_punctuation() {
    assert_eq!(slug_model("gemma4:26b-a4b-it-q8_0"), "gemma4-26b-a4b-it-q8-0");
}

#[test]
fn slug_model_collapses_runs_of_punctuation() {
    // ".:_-" all map to a single '-' between alphanum runs.
    assert_eq!(slug_model("model.with::lots__of---punct"), "model-with-lots-of-punct");
}

#[test]
fn slug_model_trims_leading_and_trailing_hyphens() {
    assert_eq!(slug_model(":foo:"), "foo");
    assert_eq!(slug_model("---foo---"), "foo");
}

#[test]
fn slug_model_returns_empty_string_for_punctuation_only_input() {
    // Pure helper; caller is responsible for upstream validation
    // (e.g. asserting that `llm_model` is non-empty in CaptureJson).
    assert_eq!(slug_model(":::"), "");
}

#[test]
fn slug_model_preserves_alphanumeric_unicode_as_lowercased_ascii_loss() {
    // Non-ASCII alphanumerics are treated as non-alphanumeric for
    // simplicity (filesystem-safe ASCII slug). Operators using
    // non-ASCII model ids will see them mapped to '-' runs.
    assert_eq!(slug_model("Mödel-é"), "m-del");
}

// ---- capture_filename ----

#[test]
fn capture_filename_shape_pin() {
    let fname = capture_filename("2026-05-13", "gemma4-26b-a4b-it-q8-0");
    assert_eq!(fname, "2026-05-13_gemma4-26b-a4b-it-q8-0.json");
    assert!(!fname.contains('/'));
    assert!(!fname.contains('\\'));
    assert!(fname.ends_with(".json"));
}

// ---- parse_fixture_prompt ----

#[test]
fn parse_fixture_prompt_happy_path() {
    let md = "# Plain echo control\n\nSay HELLO and nothing else.";
    let (summary, body) = parse_fixture_prompt(md).expect("parse");
    assert_eq!(summary, "Plain echo control");
    assert_eq!(body, "Say HELLO and nothing else.");
}

#[test]
fn parse_fixture_prompt_strips_multiple_blank_lines_after_h1() {
    let md = "# Summary\n\n\n\nBody line.";
    let (summary, body) = parse_fixture_prompt(md).expect("parse");
    assert_eq!(summary, "Summary");
    assert_eq!(body, "Body line.");
}

#[test]
fn parse_fixture_prompt_preserves_internal_blank_lines_in_body() {
    let md = "# Summary\n\nFirst para.\n\nSecond para.";
    let (_, body) = parse_fixture_prompt(md).expect("parse");
    assert_eq!(body, "First para.\n\nSecond para.");
}

#[test]
fn parse_fixture_prompt_preserves_h2_in_body() {
    let md = "# Summary\n\n## Subheading\n\nDetail.";
    let (_, body) = parse_fixture_prompt(md).expect("parse");
    assert_eq!(body, "## Subheading\n\nDetail.");
}

#[test]
fn parse_fixture_prompt_accepts_h1_with_no_space_after_hash() {
    // Edge case: `#FOO` (no space after the hash) is accepted as an
    // H1 with summary `FOO`. Pinned so the fallback branch in
    // parse_fixture_prompt does not silently rot.
    let md = "#FOO\n\nBody.";
    let (summary, body) = parse_fixture_prompt(md).expect("parse");
    assert_eq!(summary, "FOO");
    assert_eq!(body, "Body.");
}

#[test]
fn parse_fixture_prompt_does_not_treat_h2_as_h1() {
    // `## Subheading` is H2, not H1. Without an H1 line, parsing
    // must fail with MissingH1 — the no-space-after-hash branch is
    // gated on `!starts_with("##")`.
    let md = "## Subheading\n\nBody.";
    match parse_fixture_prompt(md) {
        Err(ParseError::MissingH1) => {}
        other => panic!("expected MissingH1, got {other:?}"),
    }
}

#[test]
fn parse_fixture_prompt_rejects_missing_h1() {
    let md = "No leading hash.";
    match parse_fixture_prompt(md) {
        Err(ParseError::MissingH1) => {}
        other => panic!("expected MissingH1, got {other:?}"),
    }
}

#[test]
fn parse_fixture_prompt_rejects_empty_body() {
    let md = "# Just the summary\n\n   \n\n";
    match parse_fixture_prompt(md) {
        Err(ParseError::EmptyBody) => {}
        other => panic!("expected EmptyBody, got {other:?}"),
    }
}

// ---- extract_plans_from_audit_rows ----

fn fake_audit_row(id: i64, actor: &str, action: &str, payload: serde_json::Value)
    -> CapturedAuditRow
{
    CapturedAuditRow {
        id,
        ts: "2026-05-13T00:00:00Z".into(),
        actor: actor.into(),
        action: action.into(),
        payload,
    }
}

fn fake_plan_payload(decision: &str, steps_len: usize, data_ceiling: &str)
    -> serde_json::Value
{
    let steps: Vec<serde_json::Value> = (0..steps_len)
        .map(|i| serde_json::json!({
            "tool": "shell-exec",
            "method": "shell.exec",
            "parameters": {"argv": ["/usr/bin/echo", format!("s{i}")]},
            "returns": "stdout",
            "done_when": "exit_code == 0",
            "classification": "Public",
        }))
        .collect();
    serde_json::json!({
        "plan": {
            "context": "ctx",
            "decision": decision,
            "rationale": "why",
            "steps": steps,
            "data_ceiling": data_ceiling,
        }
    })
}

#[test]
fn extract_plans_empty_input_returns_empty_vec() {
    let rows: Vec<CapturedAuditRow> = vec![];
    assert!(extract_plans_from_audit_rows(&rows).is_empty());
}

#[test]
fn extract_plans_one_plan_one_verdict() {
    let rows = vec![
        fake_audit_row(1, "agent", "plan.formulate", fake_plan_payload("act", 1, "Public")),
        fake_audit_row(2, "cassandra:chain", "verdict",
            serde_json::json!({"verdict": "Approve"})),
    ];
    let plans = extract_plans_from_audit_rows(&rows);
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].iter, 1);
    assert_eq!(plans[0].verdict_today, Some("Approve".to_string()));
    assert_eq!(plans[0].step_count, 1);
    assert_eq!(plans[0].data_ceiling, "Public");
}

#[test]
fn extract_plans_two_plans_two_verdicts_carry_iter_indices() {
    let rows = vec![
        fake_audit_row(1, "agent", "plan.formulate", fake_plan_payload("act", 2, "Personal")),
        fake_audit_row(2, "cassandra:chain", "verdict",
            serde_json::json!({"verdict": "Approve"})),
        fake_audit_row(3, "agent", "plan.formulate",
            fake_plan_payload("task_complete", 0, "Personal")),
        fake_audit_row(4, "cassandra:chain", "verdict",
            serde_json::json!({"verdict": "Approve"})),
    ];
    let plans = extract_plans_from_audit_rows(&rows);
    assert_eq!(plans.len(), 2);
    assert_eq!(plans[0].iter, 1);
    assert_eq!(plans[0].step_count, 2);
    assert_eq!(plans[1].iter, 2);
    assert_eq!(plans[1].step_count, 0);
    assert_eq!(plans[1].data_ceiling, "Personal");
}

#[test]
fn extract_plans_returns_none_when_verdict_row_missing() {
    // Schema-v2 (issue #47) bumps `verdict_today` from `String` to
    // `Option<String>`. Missing verdict → `None` (was silently
    // defaulted to `"Approve"` in v1, which lost the signal).
    let rows = vec![
        fake_audit_row(1, "agent", "plan.formulate", fake_plan_payload("act", 1, "Public")),
        // No following cassandra:chain/verdict row.
    ];
    let plans = extract_plans_from_audit_rows(&rows);
    assert_eq!(plans.len(), 1);
    assert!(
        plans[0].verdict_today.is_none(),
        "missing verdict row must yield None (schema-v2)"
    );
}

#[test]
fn extract_plans_some_approve_is_distinct_from_none() {
    // The whole point of the schema-v2 bump: `Some("Approve")` and
    // `None` are now distinct values. Same fixture pair as above,
    // with vs without the verdict row.
    let with_verdict = vec![
        fake_audit_row(1, "agent", "plan.formulate", fake_plan_payload("act", 1, "Public")),
        fake_audit_row(2, "cassandra:chain", "verdict",
            serde_json::json!({"verdict": "Approve"})),
    ];
    let without_verdict = vec![
        fake_audit_row(1, "agent", "plan.formulate", fake_plan_payload("act", 1, "Public")),
    ];
    let p_with = extract_plans_from_audit_rows(&with_verdict);
    let p_without = extract_plans_from_audit_rows(&without_verdict);
    assert_eq!(p_with[0].verdict_today, Some("Approve".to_string()));
    assert_eq!(p_without[0].verdict_today, None);
    assert_ne!(p_with[0].verdict_today, p_without[0].verdict_today);
}

#[test]
fn extract_plans_flags_truncated_source_row() {
    // A plan.formulate row whose payload is the `truncate_payload`
    // envelope (`{_truncated, sha256, len}`) has lost its `plan` key.
    // Schema-v3 (issue #62) surfaces that as `source_truncated: true`
    // so the null plan_json is attributable to truncation, not a real
    // zero-step plan or a pre-Slice-A capture.
    let rows = vec![
        fake_audit_row(
            1,
            "agent",
            "plan.formulate",
            serde_json::json!({"_truncated": true, "sha256": "ab".repeat(32), "len": 8192}),
        ),
        fake_audit_row(2, "cassandra:chain", "verdict",
            serde_json::json!({"verdict": "Approve"})),
    ];
    let plans = extract_plans_from_audit_rows(&rows);
    assert_eq!(plans.len(), 1);
    assert!(plans[0].source_truncated, "truncation envelope must set source_truncated");
    assert!(plans[0].plan_json.is_null());
    assert_eq!(plans[0].step_count, 0);
    // The verdict lookahead is unaffected by truncation of the plan row.
    assert_eq!(plans[0].verdict_today, Some("Approve".to_string()));
}

#[test]
fn extract_plans_truncated_is_distinct_from_pre_slice_a_null() {
    // Both a truncated row and a pre-Slice-A row yield `plan_json:
    // null`, but only the truncated one sets `source_truncated`. This
    // is the whole point of the schema-v3 bump (issue #62).
    let truncated = vec![fake_audit_row(
        1,
        "agent",
        "plan.formulate",
        serde_json::json!({"_truncated": true, "sha256": "cd".repeat(32), "len": 9000}),
    )];
    // Pre-Slice-A: a plan.formulate row with no `plan` key and no
    // `_truncated` marker.
    let pre_slice_a = vec![fake_audit_row(
        1,
        "agent",
        "plan.formulate",
        serde_json::json!({"task_id": 1, "plan_count": 1}),
    )];
    let p_trunc = extract_plans_from_audit_rows(&truncated);
    let p_pre = extract_plans_from_audit_rows(&pre_slice_a);
    assert!(p_trunc[0].plan_json.is_null());
    assert!(p_pre[0].plan_json.is_null());
    assert!(p_trunc[0].source_truncated);
    assert!(!p_pre[0].source_truncated);
    assert_ne!(p_trunc[0].source_truncated, p_pre[0].source_truncated);
}

/// `SCHEMA_VERSION` pin. Bumping requires a deliberate edit here
/// plus a migration note in the doc-comment.
#[test]
fn schema_version_is_three() {
    assert_eq!(SCHEMA_VERSION, 3);
}

// ---- write_capture_to_dir ----

fn sample_capture(fixture_id: &str, model: &str) -> CaptureJson {
    CaptureJson {
        schema_version: SCHEMA_VERSION,
        fixture_id: fixture_id.into(),
        fixture_summary: "summary".into(),
        captured_at: "2026-05-13T10:30:00Z".into(),
        llm_backend: "local".into(),
        llm_model: model.into(),
        llm_base_url: "http://127.0.0.1:11434/v1".into(),
        prompt: "p".into(),
        task_id: 1,
        task_state: "completed".into(),
        plan_iterations: 1,
        plans: vec![],
        audit_rows: vec![],
    }
}

#[test]
fn write_capture_to_dir_creates_parent_and_writes_file() {
    let tmp = tempfile::tempdir().unwrap();
    let cap = sample_capture("safe-001-echo-marker", "gemma4:26b-a4b-it-q8_0");
    let path = write_capture_to_dir(tmp.path(), &cap).expect("write");
    assert!(path.exists());
    // Expected filename: <date>_<model_slug>.json under
    // <out_dir>/<fixture_id>/.
    assert_eq!(
        path,
        tmp.path()
            .join("safe-001-echo-marker")
            .join("2026-05-13_gemma4-26b-a4b-it-q8-0.json")
    );
}

#[test]
fn write_capture_to_dir_round_trips_through_json() {
    let tmp = tempfile::tempdir().unwrap();
    let cap = sample_capture("safe-001-echo-marker", "gemma4:26b-a4b-it-q8_0");
    let path = write_capture_to_dir(tmp.path(), &cap).expect("write");
    let bytes = std::fs::read(&path).expect("read back");
    let parsed: CaptureJson = serde_json::from_slice(&bytes).expect("decode");
    assert_eq!(parsed, cap);
}

#[test]
fn write_capture_to_dir_refuses_to_overwrite_existing_baseline() {
    let tmp = tempfile::tempdir().unwrap();
    let cap = sample_capture("safe-001-echo-marker", "gemma4:26b-a4b-it-q8_0");
    let _first = write_capture_to_dir(tmp.path(), &cap).expect("first write");
    let err = write_capture_to_dir(tmp.path(), &cap)
        .expect_err("second write should refuse");
    assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
}

#[test]
fn write_capture_to_dir_rejects_short_captured_at() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cap = sample_capture("safe-001-echo-marker", "gemma4:26b-a4b-it-q8_0");
    cap.captured_at = "2026".into(); // shorter than 10 chars
    let err = write_capture_to_dir(tmp.path(), &cap).expect_err("must reject");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn write_capture_to_dir_rejects_punctuation_only_llm_model() {
    let tmp = tempfile::tempdir().unwrap();
    // Punctuation-only model id slugs to "" — write must refuse
    // rather than producing a filename that begins with "_" .
    let cap = sample_capture("safe-001-echo-marker", ":::");
    let err = write_capture_to_dir(tmp.path(), &cap).expect_err("must reject");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn write_capture_to_dir_rejects_path_traversal_in_fixture_id() {
    let tmp = tempfile::tempdir().unwrap();
    // `..` would escape the captures root; `/` would escape the
    // fixture directory; leading `.` would create a hidden dir;
    // NUL is rejected by the filesystem. All must be rejected up
    // front with InvalidInput.
    for bad in ["../escape", "a/b", "a\\b", ".hidden", "", "with\0nul"] {
        let cap = sample_capture(bad, "gemma4:26b-a4b-it-q8_0");
        let err = write_capture_to_dir(tmp.path(), &cap)
            .expect_err(&format!("must reject fixture_id={bad:?}"));
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::InvalidInput,
            "fixture_id={bad:?} must surface InvalidInput, got {err:?}"
        );
    }
}
