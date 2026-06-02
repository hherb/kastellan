//! Operator-triggered execution of an approved L3 skill (the invocation
//! "DOOR"). Pure parsing + substitution + a pure decision
//! ([`prepare_invocation`]) reusing the approval gate against the *live*
//! tool set, plus the async [`invoke_l3`] orchestration that drives the
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

use crate::cassandra::types::{L3SkillCandidate, L3TemplateStep};

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
    #[error("argument '{name}' value contains a newline or control character")]
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

/// `true` iff any `{{ident}}` placeholder remains in a string leaf.
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
/// parameter names. Each value must be free of newlines/control chars and
/// within [`L3_ARG_MAX_VALUE_BYTES`]. Asserts no `{{…}}` survives.
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
        if value.bytes().any(|b| b < 0x20) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cassandra::types::{L3Param, L3TemplateStep};

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
}
