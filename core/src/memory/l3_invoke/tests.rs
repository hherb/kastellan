use super::*;
use crate::cassandra::types::L3Param;

fn skill_one_param() -> L3SkillCandidate {
    L3SkillCandidate {
        name: "summarise_repo".into(),
        description: "Read a repo README".into(),
        parameters: vec![L3Param { name: "repo_path".into(), description: "abs path".into() }],
        steps: vec![L3TemplateStep {
            tool: "shell-exec".into(),
            method: "shell.exec".into(),
            parameters: serde_json::json!({ "argv": ["cat", "{{repo_path}}/README.md"] }),
        }],
    }
}

#[test]
fn substitute_happy_interpolates_embedded_placeholder() {
    let args = parse_args(&["repo_path=/tmp/r".into()]).unwrap();
    let steps = substitute_template(&skill_one_param(), &args).unwrap();
    assert_eq!(steps[0].parameters["argv"][1], "/tmp/r/README.md");
}

#[test]
fn substitute_zero_param_skill_with_no_args() {
    let s = L3SkillCandidate {
        name: "run_tests".into(),
        description: "run suite".into(),
        parameters: vec![],
        steps: vec![L3TemplateStep {
            tool: "shell-exec".into(),
            method: "shell.exec".into(),
            parameters: serde_json::json!({ "argv": ["make", "test"] }),
        }],
    };
    let steps = substitute_template(&s, &BTreeMap::new()).unwrap();
    assert_eq!(steps[0].parameters["argv"][0], "make");
}

#[test]
fn substitute_rejects_missing_arg() {
    let err = substitute_template(&skill_one_param(), &BTreeMap::new()).unwrap_err();
    assert!(matches!(err, InvokeError::MissingArgs(_)));
}

#[test]
fn substitute_rejects_unknown_arg() {
    let args = parse_args(&["repo_path=/x".into(), "extra=1".into()]).unwrap();
    let err = substitute_template(&skill_one_param(), &args).unwrap_err();
    assert!(matches!(err, InvokeError::UnknownArgs(_)));
}

#[test]
fn substitute_rejects_value_with_newline() {
    let args = parse_args(&["repo_path=/x".into()]).unwrap();
    let mut args = args;
    args.insert("repo_path".into(), "a\nb".into());
    let err = substitute_template(&skill_one_param(), &args).unwrap_err();
    assert_eq!(err, InvokeError::BadArgValue { name: "repo_path".into() });
}

#[test]
fn substitute_rejects_oversized_value() {
    let big = "x".repeat(L3_ARG_MAX_VALUE_BYTES + 1);
    let mut args = BTreeMap::new();
    args.insert("repo_path".into(), big);
    let err = substitute_template(&skill_one_param(), &args).unwrap_err();
    assert!(matches!(err, InvokeError::ArgValueTooLong { .. }));
}

#[test]
fn substitute_rejects_value_containing_brace_sequence() {
    // A value legally containing `{{x}}` must be rejected up front (BadArgValue),
    // NOT silently interpolated and then mis-flagged as an unsubstituted
    // placeholder. Single-brace values stay valid (covered by the happy tests).
    let mut args = BTreeMap::new();
    args.insert("repo_path".into(), "/data/{{x}}/out".into());
    let err = substitute_template(&skill_one_param(), &args).unwrap_err();
    assert_eq!(err, InvokeError::BadArgValue { name: "repo_path".into() });
}

#[test]
fn parse_args_happy_multi() {
    let got = parse_args(&["repo_path=/tmp/x".into(), "depth=2".into()]).unwrap();
    assert_eq!(got["repo_path"], "/tmp/x");
    assert_eq!(got["depth"], "2");
}

#[test]
fn parse_args_value_may_contain_equals() {
    let got = parse_args(&["query=a=b=c".into()]).unwrap();
    assert_eq!(got["query"], "a=b=c");
}

#[test]
fn parse_args_rejects_missing_equals() {
    assert_eq!(
        parse_args(&["noequals".into()]),
        Err(InvokeError::MalformedArg("noequals".into()))
    );
}

#[test]
fn parse_args_rejects_non_snake_name() {
    assert_eq!(
        parse_args(&["Repo=/x".into()]),
        Err(InvokeError::BadArgName("Repo".into()))
    );
}

#[test]
fn parse_args_rejects_duplicate() {
    assert_eq!(
        parse_args(&["a=1".into(), "a=2".into()]),
        Err(InvokeError::DuplicateArg("a".into()))
    );
}

use crate::memory::l3_surface::is_surfaceable;

#[test]
fn is_runnable_only_approved_and_pinned() {
    assert!(is_runnable(SkillTrust::UserApproved));
    assert!(is_runnable(SkillTrust::Pinned));
    assert!(!is_runnable(SkillTrust::Untrusted));
}

#[test]
fn is_runnable_matches_is_surfaceable() {
    // The two gates have identical membership; pin them in sync so a future
    // change to one is caught.
    for t in [SkillTrust::Untrusted, SkillTrust::UserApproved, SkillTrust::Pinned] {
        assert_eq!(is_runnable(t), is_surfaceable(t));
    }
}

fn tools(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| s.to_string()).collect()
}

#[test]
fn prepare_rejects_untrusted_trust() {
    let args = parse_args(&["repo_path=/x".into()]).unwrap();
    let r = prepare_invocation(&skill_one_param(), SkillTrust::Untrusted, &args, &tools(&["shell-exec"]));
    match r {
        Err(InvokeRefusal { reasons }) => assert!(reasons.iter().any(|s| s.contains("trust"))),
        Ok(_) => panic!("untrusted must refuse"),
    }
}

#[test]
fn prepare_rejects_unknown_tool_via_live_gate() {
    let args = parse_args(&["repo_path=/x".into()]).unwrap();
    // approved trust, but the live registry lacks shell-exec
    let r = prepare_invocation(&skill_one_param(), SkillTrust::UserApproved, &args, &tools(&["gliner-relex"]));
    match r {
        Err(InvokeRefusal { reasons }) => assert!(reasons.iter().any(|s| s.contains("shell-exec"))),
        Ok(_) => panic!("unknown tool must refuse"),
    }
}

#[test]
fn prepare_happy_returns_concrete_steps() {
    let args = parse_args(&["repo_path=/tmp/r".into()]).unwrap();
    let steps = prepare_invocation(&skill_one_param(), SkillTrust::UserApproved, &args, &tools(&["shell-exec"]))
        .expect("clean approved skill with known tool");
    assert_eq!(steps[0].parameters["argv"][1], "/tmp/r/README.md");
}

#[test]
fn prepare_propagates_substitution_error_as_refusal() {
    // missing arg → refusal (not a panic); refusal must name the missing param
    let refusal = prepare_invocation(&skill_one_param(), SkillTrust::UserApproved, &BTreeMap::new(), &tools(&["shell-exec"]))
        .unwrap_err();
    assert!(
        refusal.reasons.iter().any(|s| s.contains("repo_path")),
        "refusal should name the missing arg; got {:?}", refusal.reasons
    );
}

#[test]
fn planned_step_from_l3_carries_tool_method_params() {
    let ts = L3TemplateStep {
        tool: "shell-exec".into(),
        method: "shell.exec".into(),
        parameters: serde_json::json!({ "argv": ["echo", "hi"] }),
    };
    let ps = planned_step_from_l3(&ts);
    assert_eq!(ps.tool, "shell-exec");
    assert_eq!(ps.method, "shell.exec");
    assert_eq!(ps.parameters["argv"][1], "hi");
}

use crate::cassandra::types::PlannedStep as PS;

struct ScriptedDispatcher {
    // outcomes returned in order; calls record the tool seen
    outcomes: std::sync::Mutex<std::collections::VecDeque<StepOutcome>>,
    seen: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl StepDispatcher for ScriptedDispatcher {
    async fn dispatch_step(&self, step: &PS) -> StepOutcome {
        self.seen.lock().unwrap().push(step.tool.clone());
        self.outcomes
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(StepOutcome::Ok(serde_json::json!(null)))
    }
}

fn two_steps() -> Vec<L3TemplateStep> {
    vec![
        L3TemplateStep { tool: "a".into(), method: "m".into(), parameters: serde_json::json!({}) },
        L3TemplateStep { tool: "b".into(), method: "m".into(), parameters: serde_json::json!({}) },
    ]
}

#[tokio::test]
async fn run_steps_executes_all_when_ok() {
    let d = ScriptedDispatcher {
        outcomes: std::sync::Mutex::new(
            vec![StepOutcome::Ok(serde_json::json!(1)), StepOutcome::Ok(serde_json::json!(2))].into(),
        ),
        seen: std::sync::Mutex::new(vec![]),
    };
    let outcomes = run_steps(&d, &two_steps()).await;
    assert_eq!(outcomes.len(), 2);
    assert_eq!(*d.seen.lock().unwrap(), vec!["a", "b"]);
}

#[tokio::test]
async fn run_steps_stops_at_first_error() {
    let d = ScriptedDispatcher {
        outcomes: std::sync::Mutex::new(
            vec![StepOutcome::Err { code: "X".into(), detail: "boom".into() }].into(),
        ),
        seen: std::sync::Mutex::new(vec![]),
    };
    let outcomes = run_steps(&d, &two_steps()).await;
    assert_eq!(outcomes.len(), 1, "must stop after the failing first step");
    assert_eq!(*d.seen.lock().unwrap(), vec!["a"], "second step never dispatched");
}
