//! Unit tests for the parent [`super`] `memory l3 run` module. Lifted verbatim
//! from the former inline `#[cfg(test)] mod tests` block (Item 9b over-cap
//! test-lift); `super::` paths resolve to the `run` module unchanged.

use super::{parse_run_argv, render_invoke_report, RunArgv};
use kastellan_core::memory::l3_invoke::InvokeReport;
use kastellan_core::scheduler::inner_loop::StepOutcome;

fn v(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

#[test]
fn build_params_empty_is_empty_object() {
    let v = super::build_params(None, &[]).unwrap();
    assert_eq!(v, serde_json::json!({}));
}

#[test]
fn build_params_from_param_tokens_are_string_values() {
    let v = super::build_params(None, &v(&["greeting=hi", "name=world"])).unwrap();
    assert_eq!(v, serde_json::json!({"greeting": "hi", "name": "world"}));
}

#[test]
fn build_params_json_base_merged_with_param_overrides() {
    let v = super::build_params(
        Some(r#"{"n": 5, "greeting": "old"}"#),
        &v(&["greeting=new"]),
    )
    .unwrap();
    assert_eq!(v, serde_json::json!({"n": 5, "greeting": "new"}));
}

#[test]
fn build_params_rejects_non_object_json() {
    assert!(super::build_params(Some("[1,2]"), &[]).is_err());
}

#[test]
fn build_params_rejects_malformed_token() {
    assert!(super::build_params(None, &v(&["noequals"])).is_err());
}

#[test]
fn parse_run_argv_collects_param_and_params_json() {
    let got = parse_run_argv(&v(&[
        "5", "--param", "a=b", "--params-json", r#"{"n":1}"#, "--execute",
    ]))
    .unwrap();
    assert_eq!(got.id, 5);
    assert_eq!(got.param_tokens, v(&["a=b"]));
    assert_eq!(got.params_json.as_deref(), Some(r#"{"n":1}"#));
    assert!(got.execute);
}

#[test]
fn parses_id_args_and_execute() {
    let got = parse_run_argv(&v(&["5", "--arg", "a=b", "--execute"])).unwrap();
    assert_eq!(got, RunArgv { id: 5, arg_tokens: v(&["a=b"]), param_tokens: vec![], params_json: None, execute: true });
}

#[test]
fn accepts_gnu_equals_arg_form_and_repeats() {
    let got = parse_run_argv(&v(&["7", "--arg=k=v", "--arg", "x=y"])).unwrap();
    assert_eq!(got.id, 7);
    assert_eq!(got.arg_tokens, v(&["k=v", "x=y"]));
    assert!(!got.execute, "no --execute/--yes => dry-run");
}

#[test]
fn yes_is_an_alias_for_execute() {
    let got = parse_run_argv(&v(&["3", "--yes"])).unwrap();
    assert!(got.execute);
}

#[test]
fn id_may_follow_flags() {
    let got = parse_run_argv(&v(&["--execute", "9"])).unwrap();
    assert_eq!(got, RunArgv { id: 9, arg_tokens: vec![], param_tokens: vec![], params_json: None, execute: true });
}

#[test]
fn missing_id_is_a_usage_error() {
    let err = parse_run_argv(&v(&["--execute"])).unwrap_err();
    assert!(err.contains("usage"), "got: {err}");
}

#[test]
fn empty_argv_is_a_usage_error() {
    let err = parse_run_argv(&[]).unwrap_err();
    assert!(err.contains("usage"), "got: {err}");
}

#[test]
fn dangling_arg_flag_is_rejected() {
    let err = parse_run_argv(&v(&["1", "--arg"])).unwrap_err();
    assert!(err.contains("--arg requires"), "got: {err}");
}

#[test]
fn non_numeric_id_is_rejected() {
    let err = parse_run_argv(&v(&["abc"])).unwrap_err();
    assert!(err.contains("invalid id"), "got: {err}");
}

#[test]
fn second_positional_is_rejected() {
    // A stray second bare token (e.g. a typo'd second id) must not be
    // silently swallowed.
    let err = parse_run_argv(&v(&["1", "2"])).unwrap_err();
    assert!(err.contains("unexpected argument '2'"), "got: {err}");
}

#[test]
fn unknown_flag_is_rejected() {
    let err = parse_run_argv(&v(&["1", "--bogus"])).unwrap_err();
    assert!(err.contains("unexpected argument '--bogus'"), "got: {err}");
}

#[test]
fn render_refused_is_nonzero_and_lists_reasons() {
    let (text, code) = render_invoke_report(
        5, "echo",
        &InvokeReport::Refused { reasons: vec!["tool x not in registry".into()] },
    );
    assert_eq!(code, 1);
    assert!(text.contains("REFUSED"));
    assert!(text.contains("tool x not in registry"));
}

#[test]
fn render_dry_run_is_zero() {
    let (text, code) = render_invoke_report(
        5, "echo", &InvokeReport::DryRun { steps: vec![] },
    );
    assert_eq!(code, 0);
    assert!(text.contains("dry-run"));
}

#[test]
fn render_executed_all_ok_is_zero() {
    let (_text, code) = render_invoke_report(
        5, "echo",
        &InvokeReport::Executed {
            outcomes: vec![StepOutcome::Ok(serde_json::json!({"ok": true}))],
            steps_total: 1,
        },
    );
    assert_eq!(code, 0);
}

#[test]
fn render_executed_with_error_is_nonzero() {
    let (text, code) = render_invoke_report(
        5, "echo",
        &InvokeReport::Executed {
            outcomes: vec![StepOutcome::Err { code: "BOOM".into(), detail: "nope".into() }],
            steps_total: 2,
        },
    );
    assert_eq!(code, 1);
    assert!(text.contains("BOOM"));
}
