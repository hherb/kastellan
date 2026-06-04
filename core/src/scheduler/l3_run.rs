//! Daemon-side handling of operator-submitted `l3_run` tasks (issue #179).
//!
//! `hhagent-cli memory l3 run <id>` no longer executes in-process. It enqueues
//! a `tasks` row whose payload `kind == "l3_run"`; the scheduler claims it on a
//! lane loop and routes it here. We load the L3 skill row and call the existing
//! [`crate::memory::l3_invoke::invoke_l3`] with the daemon's live dispatcher —
//! so execution uses the daemon's single `ToolRegistry`, eliminating the
//! operator-env divergence the in-process rebuild suffered (#179 Opt 3).

use std::collections::BTreeMap;

use serde_json::Value;
use sqlx::PgPool;

use crate::cassandra::types::L3SkillCandidate;
use crate::memory::l3_approval::SkillTrust;
use crate::memory::l3_invoke::{invoke_l3, InvokeReport};
use crate::scheduler::inner_loop::StepDispatcher;
use hhagent_db::memories::{fetch_by_ids, MemoryLayer};

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
}
