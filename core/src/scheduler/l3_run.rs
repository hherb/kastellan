//! Daemon-side handling of operator-submitted `l3_run` tasks (issue #179).
//!
//! `kastellan-cli memory l3 run <id>` no longer executes in-process. It enqueues
//! a `tasks` row whose payload `kind == "l3_run"`; the scheduler claims it on a
//! lane loop and routes it here. We load the L3 skill row and call the existing
//! [`crate::memory::l3_invoke::invoke_l3`] with the daemon's live dispatcher —
//! so execution uses the daemon's single `ToolRegistry`, eliminating the
//! operator-env divergence the in-process rebuild suffered (#179 Opt 3).

use std::collections::BTreeMap;

use serde_json::Value;
use sqlx::PgPool;

use crate::cassandra::types::{L3SkillCandidate, PythonSkillCandidate};
use crate::memory::l3_approval::SkillTrust;
use crate::memory::l3_invoke::{invoke_l3, InvokeReport};
use crate::memory::l3py_invoke::invoke_python_skill;
use crate::scheduler::inner_loop::StepDispatcher;
use kastellan_db::memories::{fetch_by_ids, MemoryLayer};

/// The `kind` discriminator written by the CLI into `tasks.payload`.
pub const L3_RUN_KIND: &str = "l3_run";

/// Parsed `l3_run` task payload.
#[derive(Debug, PartialEq, Eq)]
pub struct L3RunRequest {
    pub memory_id: i64,
    pub args: BTreeMap<String, String>,
    pub execute: bool,
}

/// True iff this task payload is an `l3_run` directive.
pub fn is_l3_run_payload(payload: &Value) -> bool {
    payload.get("kind").and_then(|v| v.as_str()) == Some(L3_RUN_KIND)
}

/// Parse an `l3_run` payload. Returns a human-readable error string on any
/// shape violation (the caller turns it into an `InvokeReport::Refused`).
pub fn parse_l3_run_payload(payload: &Value) -> Result<L3RunRequest, String> {
    if !is_l3_run_payload(payload) {
        return Err("payload kind is not 'l3_run'".to_string());
    }
    let memory_id = payload
        .get("memory_id")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| "l3_run payload missing integer 'memory_id'".to_string())?;
    let execute = payload.get("execute").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut args = BTreeMap::new();
    if let Some(obj) = payload.get("args") {
        let map = obj
            .as_object()
            .ok_or_else(|| "l3_run payload 'args' is not an object".to_string())?;
        for (k, v) in map {
            let s = v
                .as_str()
                .ok_or_else(|| format!("l3_run arg '{k}' is not a string"))?;
            args.insert(k.clone(), s.to_string());
        }
    }
    Ok(L3RunRequest { memory_id, args, execute })
}

/// True iff a stored layer-3 row's `metadata` describes a Python skill
/// (`kind == "python"`). Absent `kind` ⇒ templated skill (back-compat).
pub fn is_python_skill_metadata(metadata: &Value) -> bool {
    metadata.get("kind").and_then(|v| v.as_str()) == Some("python")
}

/// Parse a Python skill's `{name, description, code}` out of `metadata.python`.
/// Returns `None` (fail-safe) if the payload is missing or malformed — the
/// caller turns that into an `InvokeReport::Refused`.
pub fn parse_python_candidate(metadata: &Value) -> Option<PythonSkillCandidate> {
    let p = metadata.get("python")?;
    serde_json::from_value(p.clone()).ok()
}

/// Execute an operator-submitted `l3_run` task against the daemon's live
/// dispatcher. Pure-failure cases (bad payload, missing/wrong-layer/unparseable
/// skill) are surfaced as `InvokeReport::Refused` so the CLI renders a refusal
/// rather than a task crash. Dispatch + audit are delegated to `invoke_l3`,
/// which audits with `actor='cli'` — preserving operator provenance even though
/// the steps physically run inside the daemon.
pub async fn run_l3_run_task(
    pool: &PgPool,
    dispatcher: &dyn StepDispatcher,
    payload: &Value,
) -> InvokeReport {
    let req = match parse_l3_run_payload(payload) {
        Ok(r) => r,
        Err(e) => return InvokeReport::Refused { reasons: vec![e] },
    };

    let row = match fetch_by_ids(pool, &[req.memory_id]).await {
        Ok(mut v) => v.pop(),
        Err(e) => {
            return InvokeReport::Refused {
                reasons: vec![format!("loading skill id={}: {e}", req.memory_id)],
            }
        }
    };
    let row = match row {
        Some(r) if r.layer == MemoryLayer::Skill => r,
        _ => {
            return InvokeReport::Refused {
                reasons: vec![format!("no layer-3 skill with id={}", req.memory_id)],
            }
        }
    };
    // Python-skill branch: dispatch one `python.exec` step. A Python skill
    // dispatches no tools, so there is no live-tool re-validation; the gate is
    // trust + structural re-validate + SHA re-hash (see l3py_invoke). If
    // python-exec is NOT registered in the daemon, the single dispatch fails
    // closed with a clear tool-not-found error surfaced in the outcome — never
    // a silent no-op.
    if is_python_skill_metadata(&row.metadata) {
        let candidate = match parse_python_candidate(&row.metadata) {
            Some(c) => c,
            None => {
                return InvokeReport::Refused {
                    reasons: vec![format!(
                        "python skill id={} has no parseable metadata.python",
                        req.memory_id
                    )],
                }
            }
        };
        let trust = SkillTrust::from_metadata_str(
            row.metadata.get("trust").and_then(|v| v.as_str()).unwrap_or(""),
        );
        let body_sha256 =
            row.metadata.get("body_sha256").and_then(|v| v.as_str()).unwrap_or("");
        return invoke_python_skill(
            pool, req.memory_id, dispatcher, &candidate, trust, body_sha256,
            &serde_json::json!({}), req.execute,
        )
        .await;
    }

    let template: L3SkillCandidate = match row
        .metadata
        .get("template")
        .cloned()
        .and_then(|t| serde_json::from_value(t).ok())
    {
        Some(t) => t,
        None => {
            return InvokeReport::Refused {
                reasons: vec![format!("skill id={} has no parseable template", req.memory_id)],
            }
        }
    };
    let trust = SkillTrust::from_metadata_str(
        row.metadata.get("trust").and_then(|v| v.as_str()).unwrap_or(""),
    );
    let body_sha256 = row.metadata.get("body_sha256").and_then(|v| v.as_str()).unwrap_or("");

    // The daemon's dispatcher exposes its live registry's tool names — the
    // authoritative set, with no operator-env rebuild (this is the #179 fix).
    let live_tools = dispatcher.known_tools();

    invoke_l3(
        pool,
        req.memory_id,
        dispatcher,
        &template,
        trust,
        body_sha256,
        &req.args,
        &live_tools,
        req.execute,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_l3_run_kind() {
        assert!(is_l3_run_payload(&serde_json::json!({"kind": "l3_run"})));
        assert!(!is_l3_run_payload(&serde_json::json!({"kind": "ask"})));
        assert!(!is_l3_run_payload(&serde_json::json!({})));
    }

    #[test]
    fn parses_full_payload() {
        let p = serde_json::json!({
            "kind": "l3_run", "memory_id": 42,
            "args": {"name": "world"}, "execute": true
        });
        let got = parse_l3_run_payload(&p).unwrap();
        assert_eq!(got.memory_id, 42);
        assert_eq!(got.args.get("name").map(String::as_str), Some("world"));
        assert!(got.execute);
    }

    #[test]
    fn execute_defaults_false_and_args_optional() {
        let p = serde_json::json!({"kind": "l3_run", "memory_id": 7});
        let got = parse_l3_run_payload(&p).unwrap();
        assert!(!got.execute);
        assert!(got.args.is_empty());
    }

    #[test]
    fn rejects_missing_memory_id() {
        let p = serde_json::json!({"kind": "l3_run", "execute": true});
        assert!(parse_l3_run_payload(&p).is_err());
    }

    #[test]
    fn rejects_non_string_arg_value() {
        let p = serde_json::json!({"kind": "l3_run", "memory_id": 1, "args": {"n": 5}});
        assert!(parse_l3_run_payload(&p).unwrap_err().contains("not a string"));
    }

    #[test]
    fn rejects_wrong_kind() {
        let p = serde_json::json!({"kind": "ask", "memory_id": 1});
        assert!(parse_l3_run_payload(&p).is_err());
    }

    #[test]
    fn rejects_non_object_args() {
        let p = serde_json::json!({"kind": "l3_run", "memory_id": 1, "args": "flat"});
        assert!(parse_l3_run_payload(&p).unwrap_err().contains("not an object"));
    }

    #[test]
    fn detects_python_kind_row() {
        let meta = serde_json::json!({
            "kind": "python", "trust": "user_approved", "body_sha256": "abc",
            "python": {"name": "say_hi", "description": "d", "code": "print(1)\n"}
        });
        assert!(is_python_skill_metadata(&meta));
        let templated = serde_json::json!({"template": {"name": "x"}});
        assert!(!is_python_skill_metadata(&templated));
        assert!(!is_python_skill_metadata(&serde_json::json!({})));
        // A row carrying BOTH kind:"python" and a stray `template` key resolves
        // as Python — the kind check fires first, so the templated branch is
        // never consulted (documents the precedence; not a real stored shape).
        let both = serde_json::json!({
            "kind": "python", "template": {"name": "x"},
            "python": {"name": "p", "description": "d", "code": "pass\n"}
        });
        assert!(is_python_skill_metadata(&both));
    }

    #[test]
    fn parses_python_candidate_from_metadata() {
        let meta = serde_json::json!({
            "kind": "python",
            "python": {"name": "say_hi", "description": "d", "code": "print(1)\n"}
        });
        let c = parse_python_candidate(&meta).expect("parse");
        assert_eq!(c.name, "say_hi");
        assert_eq!(c.code, "print(1)\n");
    }

    #[test]
    fn missing_python_payload_is_none() {
        assert!(parse_python_candidate(&serde_json::json!({"kind": "python"})).is_none());
        // malformed (missing fields) → None, fail-safe
        assert!(parse_python_candidate(&serde_json::json!({"python": {"name": "x"}})).is_none());
    }
}
