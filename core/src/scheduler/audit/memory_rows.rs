//! L1/L3 memory-layer audit-row payload builders.
//!
//! Split out of the parent `scheduler/audit.rs` (500-LOC cap) — same
//! prod-split shape as `runner/{audit_rows,task_exec}.rs`: the parent
//! keeps the task-lifecycle family it was written for and re-exports
//! everything here via `pub use`, so the public paths
//! `scheduler::audit::{ACTION_L1_*, ACTION_L3_*, build_l1_*, build_l3_*}`
//! are unchanged. Function bodies and doc comments are verbatim moves.
//!
//! Two sub-families live here:
//!
//! * **L1 insight rows** — `l1.added` (operator CLI), `l1.removed`
//!   (operator CLI), `l1.promoted` (agent-raised, written by
//!   `runner::drain_lane`). Payloads built by [`build_l1_write_payload`].
//! * **L3 skill rows** — the crystallise → approve → pin → invoke trust
//!   arc (`l3.*`). Rejection rows are audited deliberately: an operator
//!   attempting to approve/pin/run a non-runnable or `secret://`-carrying
//!   skill is a security-relevant event.

use crate::memory::l1_promote::{L1Source, L1WriteOutcome};
use crate::memory::l3_crystallise::{L3Source, L3WriteOutcome};
use serde_json::Value;

/// Action string for `actor='cli' action='l1.added'` audit rows.
/// Emitted by `cli_audit::l1_add_and_audit` after a successful
/// `kastellan-cli memory l1 add` call. The payload is built by
/// [`build_l1_write_payload`].
pub const ACTION_L1_ADDED: &str = "l1.added";

/// Action string for `actor='cli' action='l1.removed'` audit rows.
/// Emitted by `cli_audit::l1_remove_and_audit` after a successful
/// `kastellan-cli memory l1 remove`. Payload: `{memory_id, deleted}`.
pub const ACTION_L1_REMOVED: &str = "l1.removed";

/// Action string for `actor='scheduler' action='l1.promoted'` audit
/// rows. Emitted by `runner::drain_lane` when the terminal plan
/// carried `l1_insight` and the inner loop reached `Outcome::Completed`.
/// The payload is built by [`build_l1_write_payload`].
pub const ACTION_L1_PROMOTED: &str = "l1.promoted";

/// Action verb for the agent-raised L3 crystallisation row written by
/// `runner::drain_lane`. Payload built by [`build_l3_write_payload`].
pub const ACTION_L3_CRYSTALLISED: &str = "l3.crystallised";
/// Action verb for the operator `memory l3 remove` audit row.
pub const ACTION_L3_REMOVED: &str = "l3.removed";
/// Action verb for the operator `memory l3 approve` success row.
pub const ACTION_L3_APPROVED: &str = "l3.approved";
/// Action verb for the operator `memory l3 approve` rejection row (the
/// gate refused). Audited because an operator attempting to approve a
/// skill carrying a `secret://` ref is a security-relevant event.
pub const ACTION_L3_APPROVE_REJECTED: &str = "l3.approve_rejected";
/// Action verb for the operator `memory l3 revoke` row (trust → untrusted).
pub const ACTION_L3_REVOKED: &str = "l3.revoked";
/// Action verb for the start-of-execution row written by `memory l3 run
/// --execute`. Payload built by [`build_l3_invoked_payload`].
pub const ACTION_L3_INVOKED: &str = "l3.invoked";
/// Action verb for the end-of-execution summary row. Payload built by
/// [`build_l3_invoke_outcome_payload`].
pub const ACTION_L3_INVOKE_OUTCOME: &str = "l3.invoke_outcome";
/// Action verb for a refused run attempt (trust gate or live re-validation
/// rejected), written before any dispatch. Audited because attempting to
/// run a non-runnable / now-invalid skill is a security-relevant event.
/// Payload built by [`build_l3_invoke_rejected_payload`].
pub const ACTION_L3_INVOKE_REJECTED: &str = "l3.invoke_rejected";
/// Action verb for the operator `memory l3 pin` success row (trust
/// `user_approved` → `pinned`, granting agent-autonomous invocability).
pub const ACTION_L3_PINNED: &str = "l3.pinned";
/// Action verb for a refused `memory l3 pin` (not yet approved / gate
/// rejected / no registry snapshot). Trust unchanged; audited as a
/// security-relevant attempt.
pub const ACTION_L3_PIN_REJECTED: &str = "l3.pin_rejected";

/// Build the payload for `l1.added` (operator) and `l1.promoted`
/// (agent-raised) audit rows. Single helper so both paths land
/// byte-identical rows on the common keys.
///
/// Operator shape: `{source: "operator", action, memory_id, body_sha256}` (4 keys).
/// Agent-raised shape: `{source: "agent_raised", task_id, action, memory_id, body_sha256}` (5 keys).
pub fn build_l1_write_payload(
    outcome: &L1WriteOutcome,
    source: &L1Source,
    body_sha256: &str,
) -> Value {
    let mut obj = serde_json::Map::new();
    match source {
        L1Source::Operator => {
            obj.insert("source".into(), Value::String("operator".into()));
        }
        L1Source::AgentRaised { task_id } => {
            obj.insert("source".into(), Value::String("agent_raised".into()));
            obj.insert(
                "task_id".into(),
                Value::Number(serde_json::Number::from(*task_id)),
            );
        }
    }
    let (action_str, memory_id) = match outcome {
        L1WriteOutcome::Inserted { memory_id, .. } => ("inserted", *memory_id),
        L1WriteOutcome::SkippedDuplicate { memory_id } => ("skipped_duplicate", *memory_id),
    };
    obj.insert("action".into(), Value::String(action_str.into()));
    obj.insert(
        "memory_id".into(),
        Value::Number(serde_json::Number::from(memory_id)),
    );
    obj.insert(
        "body_sha256".into(),
        Value::String(body_sha256.into()),
    );
    Value::Object(obj)
}

/// Build the payload for the `l3.crystallised` audit row. Shape:
/// `{source: "agent_raised", task_id, skill_name, action, memory_id, body_sha256}` (6 keys).
pub fn build_l3_write_payload(
    outcome: &L3WriteOutcome,
    source: &L3Source,
    skill_name: &str,
    body_sha256: &str,
) -> Value {
    let mut obj = serde_json::Map::new();
    match source {
        L3Source::AgentRaised { task_id } => {
            obj.insert("source".into(), Value::String("agent_raised".into()));
            obj.insert("task_id".into(), Value::Number(serde_json::Number::from(*task_id)));
        }
    }
    let (action_str, memory_id) = match outcome {
        L3WriteOutcome::Inserted { memory_id } => ("inserted", *memory_id),
        L3WriteOutcome::SkippedDuplicate { memory_id } => ("skipped_duplicate", *memory_id),
    };
    obj.insert("skill_name".into(), Value::String(skill_name.into()));
    obj.insert("action".into(), Value::String(action_str.into()));
    obj.insert("memory_id".into(), Value::Number(serde_json::Number::from(memory_id)));
    obj.insert("body_sha256".into(), Value::String(body_sha256.into()));
    Value::Object(obj)
}

/// Payload for an `l3.approved` row. `tools` is the template's distinct
/// step tools the gate verified against the registry snapshot.
pub fn build_l3_approved_payload(
    memory_id: i64,
    skill_name: &str,
    body_sha256: &str,
    tools: &[String],
) -> Value {
    serde_json::json!({
        "memory_id": memory_id,
        "skill_name": skill_name,
        "body_sha256": body_sha256,
        "tools": tools,
    })
}

/// Payload for an `l3.approve_rejected` row. `skill_name`/`body_sha256`
/// are omitted when the row/template could not be parsed.
pub fn build_l3_approve_rejected_payload(
    memory_id: i64,
    skill_name: Option<&str>,
    body_sha256: Option<&str>,
    reasons: &[String],
) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("memory_id".into(), Value::Number(serde_json::Number::from(memory_id)));
    if let Some(n) = skill_name {
        obj.insert("skill_name".into(), Value::String(n.into()));
    }
    if let Some(s) = body_sha256 {
        obj.insert("body_sha256".into(), Value::String(s.into()));
    }
    obj.insert(
        "reasons".into(),
        Value::Array(reasons.iter().map(|r| Value::String(r.clone())).collect()),
    );
    Value::Object(obj)
}

/// Payload for an `l3.revoked` row.
pub fn build_l3_revoked_payload(memory_id: i64, updated: bool) -> Value {
    serde_json::json!({ "memory_id": memory_id, "updated": updated })
}

/// Payload for the `l3.invoked` row. Carries arg *names* only (not
/// values); substituted values land in the per-step chokepoint rows where
/// secret-refs stay opaque.
pub fn build_l3_invoked_payload(
    memory_id: i64,
    skill_name: &str,
    body_sha256: &str,
    arg_names: &[String],
    step_count: usize,
) -> Value {
    serde_json::json!({
        "memory_id": memory_id,
        "skill_name": skill_name,
        "body_sha256": body_sha256,
        "arg_names": arg_names,
        "step_count": step_count,
    })
}

/// Payload for the `l3.invoke_outcome` row (mirrors `plan.outcome`).
/// Shape: `{memory_id, skill_name, steps_executed, steps_total, any_err}`.
pub fn build_l3_invoke_outcome_payload(
    memory_id: i64,
    skill_name: &str,
    steps_executed: usize,
    steps_total: usize,
    any_err: bool,
) -> Value {
    serde_json::json!({
        "memory_id": memory_id,
        "skill_name": skill_name,
        "steps_executed": steps_executed,
        "steps_total": steps_total,
        "any_err": any_err,
    })
}

/// Payload for the `l3.invoke_rejected` row. Written by `invoke_l3` after
/// a trust-gate or live-re-validation refusal, before any dispatch. The
/// only caller always holds a successfully-parsed template (→ `skill_name`)
/// and the stored row's `body_sha256`, so both are **required** here
/// (unlike `build_l3_approve_rejected_payload`, whose no-parse path can
/// have neither). Shape: `{memory_id, skill_name, body_sha256, reasons}`.
pub fn build_l3_invoke_rejected_payload(
    memory_id: i64,
    skill_name: &str,
    body_sha256: &str,
    reasons: &[String],
) -> Value {
    serde_json::json!({
        "memory_id": memory_id,
        "skill_name": skill_name,
        "body_sha256": body_sha256,
        "reasons": reasons,
    })
}

/// Payload for the `l3.pinned` row (operator pinned an approved skill).
pub fn build_l3_pinned_payload(memory_id: i64, skill_name: &str, body_sha256: &str) -> Value {
    serde_json::json!({
        "memory_id": memory_id,
        "skill_name": skill_name,
        "body_sha256": body_sha256,
    })
}

/// Payload for the `l3.pin_rejected` row. `skill_name` is `None` when the
/// stored template did not parse (the only no-name pin-reject path).
pub fn build_l3_pin_rejected_payload(
    memory_id: i64,
    skill_name: Option<&str>,
    reasons: &[String],
) -> Value {
    serde_json::json!({
        "memory_id": memory_id,
        "skill_name": skill_name,
        "reasons": reasons,
    })
}

/// Agent-path variant of [`build_l3_invoke_rejected_payload`]: `memory_id`
/// and `body_sha256` are `Option` because an unknown / non-pinned skill
/// name refusal happens before any row is loaded. `skill_name` (the
/// directive's requested name) is always known.
pub fn build_l3_invoke_rejected_agent_payload(
    skill_name: &str,
    memory_id: Option<i64>,
    body_sha256: Option<&str>,
    reasons: &[String],
) -> Value {
    serde_json::json!({
        "memory_id": memory_id,
        "skill_name": skill_name,
        "body_sha256": body_sha256,
        "reasons": reasons,
    })
}
