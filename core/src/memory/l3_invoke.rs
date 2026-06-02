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

use std::collections::BTreeMap;

use crate::cassandra::types::L3SkillCandidate;

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

#[cfg(test)]
mod tests {
    use super::*;

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
