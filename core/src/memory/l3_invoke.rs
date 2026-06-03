//! Operator-triggered execution of an approved L3 skill (the invocation
//! "DOOR"). Pure parsing + substitution + a pure decision
//! ([`prepare_invocation`]) reusing the approval gate against the *live*
//! tool set, plus the async `invoke_l3` orchestration that drives the
//! existing [`crate::scheduler::tool_dispatch::ToolHostStepDispatcher`].
//!
//! Only `user_approved` / `pinned` skills run ([`is_runnable`]); dry-run is
//! the default (the CLI passes `execute = false`). There is NO agent-
//! autonomous invocation here and NO CASSANDRA review on the operator path
//! (the reviewer polices agent-formulated plans; an operator running their
//! own approved skill with explicit args is an authorised action).
//!
//! See `docs/superpowers/specs/2026-06-02-l3-skill-invocation-design.md`.

use std::collections::{BTreeMap, BTreeSet};

use crate::cassandra::types::{DataClass, L3SkillCandidate, L3TemplateStep, PlannedStep};
use crate::memory::l3_approval::{evaluate_approval, ApprovalDecision, SkillTrust};
use crate::scheduler::inner_loop::{StepDispatcher, StepOutcome};

/// Max bytes for a single operator-supplied argument value. A value is
/// just a tool argument (shell-exec does no shell interpretation and
/// argv[0] stays operator-allowlisted), but keeping it bounded + clean
/// mirrors the template guards.
pub const L3_ARG_MAX_VALUE_BYTES: usize = 1024;

/// Errors from the pure invocation front-end (arg parse + substitution).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InvokeError {
    #[error("argument '{0}' is not of the form name=value")]
    MalformedArg(String),
    #[error("argument name '{0}' is not snake_case")]
    BadArgName(String),
    #[error("duplicate argument '{0}'")]
    DuplicateArg(String),
    #[error("missing value for declared parameter(s): {0}")]
    MissingArgs(String),
    #[error("unknown argument(s) not declared by the skill: {0}")]
    UnknownArgs(String),
    #[error("argument '{name}' value contains a newline, control character, or '{{{{' / '}}}}' sequence")]
    BadArgValue { name: String },
    #[error("argument '{name}' value exceeds {max} bytes ({got})")]
    ArgValueTooLong { name: String, max: usize, got: usize },
    #[error("placeholder '{{{{{0}}}}}' survived substitution (internal error)")]
    UnsubstitutedPlaceholder(String),
}

/// `true` iff `s` is a strict snake_case identifier (`[a-z][a-z0-9_]*`).
/// Mirrors `l3_crystallise::is_snake_ident` (kept local to avoid widening
/// that module's visibility).
fn is_snake_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Parse `name=value` tokens (the CLI strips the `--arg` flag) into a map.
/// Splits on the FIRST `=` so values may contain `=`. Rejects a token with
/// no `=`, a non-snake_case name, or a duplicate name.
pub fn parse_args(tokens: &[String]) -> Result<BTreeMap<String, String>, InvokeError> {
    let mut map = BTreeMap::new();
    for tok in tokens {
        let (name, value) = tok
            .split_once('=')
            .ok_or_else(|| InvokeError::MalformedArg(tok.clone()))?;
        if !is_snake_ident(name) {
            return Err(InvokeError::BadArgName(name.to_string()));
        }
        if map.insert(name.to_string(), value.to_string()).is_some() {
            return Err(InvokeError::DuplicateArg(name.to_string()));
        }
    }
    Ok(map)
}

/// Replace every `{{name}}` occurrence inside a single string with the
/// supplied value. `args` is guaranteed complete by the caller's arity
/// check, so a `{{name}}` whose name is absent is left intact and caught
/// by the post-condition scan. Mirrors the writer's `scan_placeholders`
/// byte walk.
fn interpolate(s: &str, args: &BTreeMap<String, String>) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'{' && bytes[i + 1] == b'{' {
            // find closing }}
            let start = i + 2;
            let mut j = start;
            while j + 1 < bytes.len() && !(bytes[j] == b'}' && bytes[j + 1] == b'}') {
                j += 1;
            }
            if j + 1 < bytes.len() && bytes[j] == b'}' && bytes[j + 1] == b'}' {
                let name = &s[start..j];
                if let Some(v) = args.get(name) {
                    out.push_str(v);
                    i = j + 2;
                    continue;
                }
            }
        }
        // not a (resolvable) placeholder start — copy one char
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Recursively interpolate every string leaf of a JSON value.
fn interpolate_value(v: &serde_json::Value, args: &BTreeMap<String, String>) -> serde_json::Value {
    match v {
        serde_json::Value::String(s) => serde_json::Value::String(interpolate(s, args)),
        serde_json::Value::Array(a) => {
            serde_json::Value::Array(a.iter().map(|e| interpolate_value(e, args)).collect())
        }
        serde_json::Value::Object(m) => {
            let mut out = serde_json::Map::new();
            for (k, val) in m {
                out.insert(k.clone(), interpolate_value(val, args));
            }
            serde_json::Value::Object(out)
        }
        other => other.clone(),
    }
}

/// Returns the name of the first `{{name}}` placeholder still present in any
/// string leaf, or `None` if none remain. (A degenerate `{{}}` yields
/// `Some("")`, but empty names are impossible in a validated template.)
fn has_placeholder(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => {
            let b = s.as_bytes();
            let mut i = 0;
            while i + 1 < b.len() {
                if b[i] == b'{' && b[i + 1] == b'{' {
                    let start = i + 2;
                    let mut j = start;
                    while j + 1 < b.len() && !(b[j] == b'}' && b[j + 1] == b'}') {
                        j += 1;
                    }
                    if j + 1 < b.len() {
                        return Some(s[start..j].to_string());
                    }
                }
                i += 1;
            }
            None
        }
        serde_json::Value::Array(a) => a.iter().find_map(has_placeholder),
        serde_json::Value::Object(m) => m.values().find_map(has_placeholder),
        _ => None,
    }
}

/// Substitute operator-supplied args into a stored skill template,
/// producing concrete (placeholder-free) steps.
///
/// Closed-world: the supplied arg names must EXACTLY equal the declared
/// parameter names. Each value must be free of newlines/control chars, must
/// not contain the `{{`/`}}` template-brace sequences, and must be within
/// [`L3_ARG_MAX_VALUE_BYTES`]. Asserts no `{{…}}` survives.
pub fn substitute_template(
    template: &L3SkillCandidate,
    args: &BTreeMap<String, String>,
) -> Result<Vec<L3TemplateStep>, InvokeError> {
    let declared: BTreeSet<&str> = template.parameters.iter().map(|p| p.name.as_str()).collect();
    let supplied: BTreeSet<&str> = args.keys().map(|s| s.as_str()).collect();

    let missing: Vec<&str> = declared.difference(&supplied).copied().collect();
    if !missing.is_empty() {
        return Err(InvokeError::MissingArgs(missing.join(", ")));
    }
    let unknown: Vec<&str> = supplied.difference(&declared).copied().collect();
    if !unknown.is_empty() {
        return Err(InvokeError::UnknownArgs(unknown.join(", ")));
    }

    for (name, value) in args {
        if value.len() > L3_ARG_MAX_VALUE_BYTES {
            return Err(InvokeError::ArgValueTooLong {
                name: name.clone(),
                max: L3_ARG_MAX_VALUE_BYTES,
                got: value.len(),
            });
        }
        // Reject control chars AND the template-brace sequences `{{`/`}}`.
        // The brace check is load-bearing: without it, a value that legitimately
        // contained `{{x}}` would, after interpolation, look like a surviving
        // placeholder and trip the spurious `UnsubstitutedPlaceholder` post-condition
        // below. Rejecting only the two-char sequences (not single braces) keeps
        // single-brace values like `{"json":true}` valid.
        if value.bytes().any(|b| b < 0x20) || value.contains("{{") || value.contains("}}") {
            return Err(InvokeError::BadArgValue { name: name.clone() });
        }
    }

    let mut out = Vec::with_capacity(template.steps.len());
    for step in &template.steps {
        let parameters = interpolate_value(&step.parameters, args);
        if let Some(name) = has_placeholder(&parameters) {
            return Err(InvokeError::UnsubstitutedPlaceholder(name));
        }
        out.push(L3TemplateStep {
            tool: step.tool.clone(),
            method: step.method.clone(),
            parameters,
        });
    }
    Ok(out)
}

/// A refusal to invoke, carrying every human-readable reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvokeRefusal {
    pub reasons: Vec<String>,
}

/// PURE trust gate: only `user_approved` / `pinned` skills run. Identical
/// membership to [`crate::memory::l3_surface::is_surfaceable`] (pinned in
/// sync by a test) — a skill the planner may *see* is exactly a skill the
/// operator may *run*.
pub fn is_runnable(trust: SkillTrust) -> bool {
    matches!(trust, SkillTrust::UserApproved | SkillTrust::Pinned)
}

/// Synthesize a [`PlannedStep`] from a concrete (substituted) template
/// step. `returns` / `done_when` are empty and `classification` is set to
/// the most conservative class — all three are UNUSED on the operator-run
/// path: `ToolHostStepDispatcher::dispatch_step` reads only
/// `tool` / `method` / `parameters`. The conservative `classification`
/// is defensive in case a future reader inspects it.
pub fn planned_step_from_l3(step: &L3TemplateStep) -> PlannedStep {
    PlannedStep {
        tool: step.tool.clone(),
        method: step.method.clone(),
        parameters: step.parameters.clone(),
        returns: String::new(),
        done_when: String::new(),
        classification: DataClass::Secret,
    }
}

/// PURE decision: may this stored skill run with these args against this
/// live tool set, and if so, what are the concrete steps?
///
/// 1. trust must be runnable ([`is_runnable`]);
/// 2. re-run the approval gate ([`evaluate_approval`]) against `live_tools`
///    — the TOCTOU close (structural re-validation + `secret://` re-scan +
///    every tool must exist in the registry as it is now);
/// 3. substitute args into the template ([`substitute_template`]).
///
/// On any failure returns an [`InvokeRefusal`] collecting the reason(s).
pub fn prepare_invocation(
    template: &L3SkillCandidate,
    stored_trust: SkillTrust,
    args: &BTreeMap<String, String>,
    live_tools: &BTreeSet<String>,
) -> Result<Vec<L3TemplateStep>, InvokeRefusal> {
    if !is_runnable(stored_trust) {
        return Err(InvokeRefusal {
            reasons: vec![format!(
                "skill trust '{}' is not runnable (only user_approved / pinned)",
                stored_trust.as_str()
            )],
        });
    }

    // Re-validate the STORED template against the live registry first
    // (structural + secret-ref + tool existence). This guards against a
    // skill approved against a now-stale snapshot, and short-circuits on a
    // structurally broken template before substitution.
    match evaluate_approval(template, live_tools) {
        ApprovalDecision::Approve => {}
        ApprovalDecision::Reject { reasons } => {
            return Err(InvokeRefusal {
                reasons: reasons.iter().map(|r| r.to_string()).collect(),
            });
        }
    }

    // Substitution can still fail on operator-arg problems (missing /
    // unknown / bad value) — surface those as refusal reasons too.
    substitute_template(template, args).map_err(|e| InvokeRefusal { reasons: vec![e.to_string()] })
}

/// Dispatch each concrete step through the injected [`StepDispatcher`],
/// collecting outcomes and stopping at the first [`StepOutcome::Err`]
/// (mirrors `inner_loop::run_to_terminal`). No audit / DB here — the
/// per-step chokepoint rows are written inside `dispatch_step`; the
/// envelope rows are the caller's job.
pub async fn run_steps(
    dispatcher: &dyn StepDispatcher,
    steps: &[L3TemplateStep],
) -> Vec<StepOutcome> {
    let mut outcomes = Vec::with_capacity(steps.len());
    for step in steps {
        let ps = planned_step_from_l3(step);
        let outcome = dispatcher.dispatch_step(&ps).await;
        let is_err = outcome.is_err();
        outcomes.push(outcome);
        if is_err {
            break;
        }
    }
    outcomes
}

#[cfg(test)]
mod tests {
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
}
