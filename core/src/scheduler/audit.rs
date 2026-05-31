//! Audit-row helpers for scheduler-emitted rows.
//!
//! Centralises the wire-level contract for the `actor = "scheduler"`
//! rows the lane runner writes around each task's lifecycle. Pure
//! functions — no I/O, no clock, no global state — so payload shape
//! is unit-testable without spinning up Postgres.
//!
//! Spec source: `docs/superpowers/specs/2026-05-10-scheduler-design.md`
//! §7 ("Instrumentation"). Two row families live here:
//!
//! * **Lifecycle transition** — `actor="scheduler"`, `action="task.<state>"`,
//!   payload `{task_id, lane, plan_count}`. One row per transition the
//!   scheduler **observes** (`running` after claim; the terminal state
//!   after finalize). The `<state>` segment is the *destination* state
//!   so an audit grep on `action LIKE 'task.%'` is the lifecycle stream.
//!
//! * **Task finalize summary** — `actor="scheduler"`,
//!   `action="task.finalize"`, payload
//!   `{task_id, lane, state, plan_count, total_llm_calls,
//!     total_dispatch_calls, total_duration_ms, started_at, finished_at}`.
//!   One row per terminal task. The aggregate fields are the
//!   convenience pre-rollup observation-phase SQL would otherwise
//!   compute from many rows.
//!
//! The dispatcher's `step.unknown_tool` / `step.spawn_failed` rows
//! (in [`super::tool_dispatch`]) reuse [`SCHEDULER_AUDIT_ACTOR`] but
//! belong to a different family (step-level short-circuits, not
//! task-level transitions) and carry a different payload shape.
//!
//! # Caveat for observation-phase SQL: audit row vs `tasks.state`
//!
//! Both row families record what the scheduler **observed**, not what
//! the DB UPDATE achieved. The most common case where these diverge is
//! a race between the inner loop and a producer-side cancel:
//!
//! 1. Inner loop finishes with `Outcome::Completed` (or any other
//!    terminal outcome — `Failed`, `TimedOut`, `Blocked`).
//! 2. Before the lane runner's `tasks::finalize` UPDATE fires, a CLI
//!    cancel has already set `state = 'cancelled'`.
//! 3. `tasks::finalize` is a no-op (`WHERE state = 'running'` no
//!    longer matches).
//! 4. The scheduler still writes `scheduler/task.completed` +
//!    `scheduler/task.finalize` rows because *it* saw the task
//!    complete, even though `tasks.state` is now `'cancelled'`.
//!
//! Practical consequences for observation-phase queries:
//!
//! * Don't compute counts of e.g. "completed tasks" by joining
//!   `audit_log.action = 'task.completed'` against `tasks.state =
//!   'completed'` — either source alone is internally consistent, but
//!   the two won't always agree.
//! * The `task.finalize` payload's `state` field reflects the
//!   scheduler's observation (the inner loop's `Outcome`), not the
//!   post-UPDATE DB state.
//! * To detect divergence after the fact, filter for tasks where the
//!   `task.<state>` audit row's state segment doesn't match
//!   `tasks.state`; this is the population of races where a producer
//!   cancel beat the scheduler's finalize.
//!
//! The same posture applies to crash recovery: the
//! [`super::crash_recovery::sweep_and_audit`] startup helper emits one
//! `scheduler/task.crashed` row per recovered task. That row records
//! the sweep's *intent* — `tasks::sweep_crashed`'s UPDATE returns the
//! recovered rows via `RETURNING`, so the audit row reflects rows the
//! sweep actually flipped. (A producer-side `mark_cancelled` racing
//! the sweep is rejected at the DB layer because the sweep already
//! transitioned the row out of `running`, so this concrete race does
//! not produce divergence.)

use crate::memory::l1_promote::{L1Source, L1WriteOutcome};
use crate::memory::l3_crystallise::{L3Source, L3WriteOutcome};
use hhagent_db::tasks::Lane;
use serde_json::{json, Value};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// Logical actor string used for every scheduler-emitted audit row.
/// Reused by [`super::tool_dispatch`] for its short-circuit rows so
/// consumers can `WHERE actor = 'scheduler'` to capture both families.
pub const SCHEDULER_AUDIT_ACTOR: &str = "scheduler";

/// Action string for `actor='core'` audit rows emitted at daemon
/// bring-up, summarising which tools were registered and the SHA-256
/// of each tool's loaded allowlist. Cross-restart drift detection.
pub const ACTION_REGISTRY_LOADED: &str = "registry.loaded";

/// Action label written by the L0 seed loader at daemon startup.
///
/// One row per startup when the L0 rules file is present. The
/// payload carries `rules_loaded`, `new_rows_written`,
/// `unchanged_skipped`, `source_path`, and `source_sha256` —
/// operator-visible breadcrumb that the loader ran, plus
/// cross-restart drift detection via the file hash.
pub const ACTION_L0_SEEDED: &str = "l0.seeded";

/// Action string for `actor='cli' action='l1.added'` audit rows.
/// Emitted by `cli_audit::l1_add_and_audit` after a successful
/// `hhagent-cli memory l1 add` call. The payload is built by
/// [`build_l1_write_payload`].
pub const ACTION_L1_ADDED: &str = "l1.added";

/// Action string for `actor='cli' action='l1.removed'` audit rows.
/// Emitted by `cli_audit::l1_remove_and_audit` after a successful
/// `hhagent-cli memory l1 remove`. Payload: `{memory_id, deleted}`.
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

/// `action` value written when the lane runner claims a `pending` task
/// and transitions it to `running`. Fires exactly once per `claim_one`
/// success.
pub const ACTION_TASK_RUNNING: &str = "task.running";

/// `action` value for the per-task summary row. Fires once per
/// finalised task, regardless of which terminal state was reached.
/// Carries the aggregate counters observation-phase SQL needs.
pub const ACTION_TASK_FINALIZE: &str = "task.finalize";

/// `action` value for the producer-side row written by `hhagent-cli ask`
/// after `tasks::insert_pending` succeeds. Distinct from the scheduler's
/// own `task.running` row that fires later on claim — paired with
/// [`crate::cli_audit::CLI_AUDIT_ACTOR`] so observation queries grouping
/// by `(actor, action)` can separate submit-time intent from
/// scheduler-time observation. Carries the same lifecycle payload shape
/// (`{task_id, lane, plan_count}`) the rest of the `task.<state>` family
/// uses; `plan_count` is always 0 at submit by definition but is
/// included for shape parity so consumers don't need a special case.
pub const ACTION_TASK_SUBMITTED: &str = "task.submitted";

/// `prefix` of the per-terminal-state lifecycle row's `action`.
/// Full action is built via [`action_task_terminal`] so the writer
/// and any reader can't drift on the separator/format.
pub const ACTION_TASK_PREFIX: &str = "task.";

/// Action string for `actor='cli'` audit rows emitted when an operator
/// adds one allowlist entry via `hhagent-cli tools allowlist add`.
pub const ACTION_TOOLS_ALLOWLIST_ADD: &str = "tools.allowlist.add";

/// Action string for `actor='cli'` audit rows emitted when an operator
/// removes one allowlist entry via `hhagent-cli tools allowlist remove`.
pub const ACTION_TOOLS_ALLOWLIST_REMOVE: &str = "tools.allowlist.remove";

/// `actor='cli' action='entities.approved'` — operator flipped a
/// quarantined entity to approved. Payload: {entity_id, kind, name}.
pub const ACTION_ENTITIES_APPROVED: &str = "entities.approved";

/// `actor='cli' action='entities.rejected'` — operator deleted a
/// quarantined entity. Payload:
/// {entity_id, kind, name, mentions_dropped}. The `mentions_dropped`
/// field is the number of `memory_entities` rows cascaded by the FK.
pub const ACTION_ENTITIES_REJECTED: &str = "entities.rejected";

/// `actor='cli' action='entities.merged'` — operator consolidated near-
/// duplicate entities. Payload: {kept_id, kept_kind, kept_name,
/// dropped_ids, links_retargeted, links_dropped_as_duplicate}.
pub const ACTION_ENTITIES_MERGED: &str = "entities.merged";

/// `actor='cli' action='entity_kinds.add'` — operator added a new
/// entity-kind label via `hhagent-cli entities kinds add`. Payload:
/// `{kind, description}` where `description` is `null` when omitted.
/// Emitted only on a real INSERT (`Ok(true)`); idempotent re-adds and
/// validation errors write no row. Symmetric to
/// [`ACTION_RELATION_KINDS_ADD`].
pub const ACTION_ENTITY_KINDS_ADD: &str = "entity_kinds.add";

/// `actor='cli' action='entity_kinds.remove'` — operator removed an
/// entity-kind label via `hhagent-cli entities kinds remove`. Payload:
/// `{kind}`. Emitted only on a real DELETE (`Ok(true)`); idempotent
/// no-ops, validation errors, and the explicit
/// `RemovalOfUndefinedRejected` write no row. Symmetric to
/// [`ACTION_RELATION_KINDS_REMOVE`].
pub const ACTION_ENTITY_KINDS_REMOVE: &str = "entity_kinds.remove";

/// `actor='cli' action='relation_kinds.add'` — operator added a new
/// relation-kind label via `hhagent-cli relations kinds add`. Payload:
/// `{kind, description}` where `description` is `null` when omitted.
/// Emitted only on a real INSERT (`Ok(true)`); idempotent re-adds and
/// validation errors write no row. Symmetric to
/// [`ACTION_TOOLS_ALLOWLIST_ADD`].
pub const ACTION_RELATION_KINDS_ADD: &str = "relation_kinds.add";

/// `actor='cli' action='relation_kinds.remove'` — operator removed a
/// relation-kind label via `hhagent-cli relations kinds remove`.
/// Payload: `{kind}`. Emitted only on a real DELETE (`Ok(true)`);
/// idempotent no-ops, validation errors, and the explicit
/// `RemovalOfUndefinedRejected` write no row. Symmetric to
/// [`ACTION_TOOLS_ALLOWLIST_REMOVE`].
pub const ACTION_RELATION_KINDS_REMOVE: &str = "relation_kinds.remove";

/// Value of the `provenance` field in a `task.finalize` payload emitted
/// from the scheduler's runtime path (the lane runner observed the task
/// end-to-end). Counters are facts; `started_at` is always present.
pub const FINALIZE_PROVENANCE_RUNTIME: &str = "runtime";

/// Value of the `provenance` field in a `task.finalize` payload emitted
/// from the startup crash-recovery sweep. Counters are JSON `null`
/// because the dead daemon's in-memory counters were lost.
pub const FINALIZE_PROVENANCE_CRASH_RECOVERY: &str = "crash_recovery";

/// Value of the `provenance` field in a `task.finalize` payload emitted
/// when a producer (`hhagent-cli`) cancels a `pending` task that was
/// never claimed. Counters are zero by construction; `started_at` is
/// always JSON `null`.
pub const FINALIZE_PROVENANCE_PRODUCER_CANCEL_PENDING: &str = "producer_cancel_pending";

/// Build the `action` string for a terminal-state lifecycle row.
/// Centralises the `"task." + state` format so a future rename can't
/// drift between the writer and any reader. Example: `"failed"` →
/// `"task.failed"`.
///
/// Accepts the same set of state strings the `tasks.state` CHECK
/// constraint allows (`completed`, `failed`, `cancelled`, `timed_out`,
/// `blocked`, `crashed`); does not enforce — bad inputs produce bad
/// audit rows, which is loud (you'd see them in `audit tail`) but not
/// a correctness hazard.
pub fn action_task_terminal(state: &str) -> String {
    format!("{ACTION_TASK_PREFIX}{state}")
}

/// Aggregate counters carried in the `task.finalize` payload.
///
/// `total_llm_calls` is just `plan_count` today (one formulator call
/// per plan iteration), but the field is named per-spec so a future
/// formulator that retries internally can populate it distinctly.
/// `total_dispatch_calls` is incremented by the inner loop on every
/// `StepDispatcher::dispatch_step` call.
///
/// `total_duration_ms` is `finished_at - started_at` clamped to 0
/// (a negative duration would mean clock skew between `claim_one`'s
/// `now()` and the local `OffsetDateTime::now_utc()` — unlikely on a
/// UDS-local Postgres but cheap to defend against).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskFinalizeStats {
    pub plan_count: i32,
    pub total_llm_calls: u32,
    pub total_dispatch_calls: u32,
    pub total_duration_ms: u64,
    pub started_at: Option<OffsetDateTime>,
    pub finished_at: OffsetDateTime,
}

/// Build the JSON payload for a lifecycle transition row
/// (`actor = "scheduler"`, `action = "task.<state>"`). Shape pinned by
/// `build_lifecycle_payload_shape_*` unit tests so adding a field is a
/// deliberate audit-contract change.
pub fn build_lifecycle_payload(task_id: i64, lane: Lane, plan_count: i32) -> Value {
    json!({
        "task_id":    task_id,
        "lane":       lane.as_sql(),
        "plan_count": plan_count,
    })
}

/// Build the JSON payload for the per-task summary row
/// (`actor = "scheduler"`, `action = "task.finalize"`). Shape pinned by
/// `build_finalize_payload_shape_*` unit tests.
///
/// `started_at` is optional because a task can in principle be
/// finalised before `claim_one` set it (e.g. if a CLI cancel races a
/// claim attempt). The wire representation is `null` in that case;
/// `total_duration_ms` falls back to 0.
///
/// The `provenance` field is hard-pinned to
/// [`FINALIZE_PROVENANCE_RUNTIME`] — this helper is the runtime
/// scheduler's entry point. Crash-recovery and producer-cancel paths
/// use [`build_crashed_finalize_payload`] and
/// [`build_producer_cancel_finalize_payload`] respectively, each
/// carrying its own provenance value. Issue #50 schema-v2.
pub fn build_finalize_payload(
    task_id: i64,
    lane: Lane,
    state: &str,
    stats: &TaskFinalizeStats,
) -> Value {
    json!({
        "task_id":              task_id,
        "lane":                 lane.as_sql(),
        "state":                state,
        "plan_count":           stats.plan_count,
        "total_llm_calls":      stats.total_llm_calls,
        "total_dispatch_calls": stats.total_dispatch_calls,
        "total_duration_ms":    stats.total_duration_ms,
        "started_at":           stats.started_at.map(format_rfc3339),
        "finished_at":          format_rfc3339(stats.finished_at),
        "provenance":           FINALIZE_PROVENANCE_RUNTIME,
    })
}

/// Build the JSON payload for the `task.finalize` summary row of a
/// **crashed** task (one recovered by the startup sweep).
///
/// Same 10-key shape as [`build_finalize_payload`] so observation-phase
/// queries that filter on `action = 'task.finalize'` see a uniform
/// projection. Two fields differ:
///
/// * `total_llm_calls` and `total_dispatch_calls` are JSON `null`
///   because the dead daemon's in-memory counters were lost. `null` is
///   the wire signal "unknowable" — distinguishable from `0` (which the
///   runtime path emits to mean "observed zero").
/// * `total_duration_ms` is `null` when `started_at` is `None`
///   (the duration is unknowable without a start time). When
///   `started_at` is present, it's the wall-clock distance from
///   `started_at` to `finished_at` via [`compute_duration_ms`], same
///   as the runtime path.
///
/// `state` is hard-pinned to `"crashed"` — the helper is single-purpose
/// for the startup-sweep path, so a misuse can't produce a wrong
/// state-string. Pinned by `build_crashed_finalize_payload_state_is_*`
/// unit tests.
pub fn build_crashed_finalize_payload(
    task_id: i64,
    lane: Lane,
    plan_count: i32,
    started_at: Option<OffsetDateTime>,
    finished_at: OffsetDateTime,
) -> Value {
    let duration_ms: Option<u64> = started_at.map(|s| compute_duration_ms(Some(s), finished_at));
    json!({
        "task_id":              task_id,
        "lane":                 lane.as_sql(),
        "state":                "crashed",
        "plan_count":           plan_count,
        "total_llm_calls":      Value::Null,
        "total_dispatch_calls": Value::Null,
        "total_duration_ms":    duration_ms,
        "started_at":           started_at.map(format_rfc3339),
        "finished_at":          format_rfc3339(finished_at),
        "provenance":           FINALIZE_PROVENANCE_CRASH_RECOVERY,
    })
}

/// Build the JSON payload for the `task.finalize` summary row emitted
/// when a producer (`hhagent-cli`) cancels a `pending` task that was
/// never claimed by any scheduler lane runner.
///
/// Same 10-key shape as [`build_finalize_payload`] so observation-phase
/// queries that filter on `action = 'task.finalize'` see a uniform
/// projection. Hardcoded fields:
///
/// * `state` = `"cancelled"` — the task entered the `cancelled` terminal
///   state directly from `pending`, bypassing every runtime counter.
/// * `total_llm_calls` / `total_dispatch_calls` / `total_duration_ms`
///   = `0` — the task ran zero plan iterations and zero step
///   dispatches before being cancelled, so the values are known
///   zeros (distinguishable from the crash-recovery path's JSON-null
///   "unknowable").
/// * `started_at` = JSON `null` — `mark_cancelled` never sets
///   `started_at` because the task never entered `running`.
/// * `provenance` = [`FINALIZE_PROVENANCE_PRODUCER_CANCEL_PENDING`].
///
/// Issue #50 schema-v2 introduces the explicit `provenance` signal so
/// observation queries no longer have to discriminate via the
/// `actor='cli' + total_llm_calls=0 + started_at=null` heuristic.
pub fn build_producer_cancel_finalize_payload(
    task_id: i64,
    lane: Lane,
    plan_count: i32,
    finished_at: OffsetDateTime,
) -> Value {
    json!({
        "task_id":              task_id,
        "lane":                 lane.as_sql(),
        "state":                "cancelled",
        "plan_count":           plan_count,
        "total_llm_calls":      0,
        "total_dispatch_calls": 0,
        "total_duration_ms":    0,
        "started_at":           Value::Null,
        "finished_at":          format_rfc3339(finished_at),
        "provenance":           FINALIZE_PROVENANCE_PRODUCER_CANCEL_PENDING,
    })
}

/// RFC 3339 string for an `OffsetDateTime`. Falls back to the empty
/// string if `time` rejects the value — operationally impossible for
/// `OffsetDateTime::now_utc()`-shaped inputs, but the empty string is
/// loud in `audit tail` output where a panic would be silent.
fn format_rfc3339(ts: OffsetDateTime) -> String {
    ts.format(&Rfc3339).unwrap_or_default()
}

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

/// Build the wire-stable payload for `actor='cli' action='entities.approved'`.
/// Keys: {entity_id, kind, name} (3 keys, BTreeSet-pinned in tests).
pub fn build_entities_approved_payload(
    entity_id: i64,
    kind: &str,
    name: &str,
) -> serde_json::Value {
    serde_json::json!({
        "entity_id": entity_id,
        "kind":      kind,
        "name":      name,
    })
}

/// Build the wire-stable payload for `actor='cli' action='entities.rejected'`.
/// Keys: {entity_id, kind, name, mentions_dropped} (4 keys).
pub fn build_entities_rejected_payload(
    entity_id: i64,
    kind: &str,
    name: &str,
    mentions_dropped: i64,
) -> serde_json::Value {
    serde_json::json!({
        "entity_id":        entity_id,
        "kind":             kind,
        "name":             name,
        "mentions_dropped": mentions_dropped,
    })
}

/// Build the wire-stable payload for `actor='cli' action='entities.merged'`.
/// Keys: {kept_id, kept_kind, kept_name, dropped_ids, links_retargeted,
/// links_dropped_as_duplicate} (6 keys).
pub fn build_entities_merged_payload(
    kept_id: i64,
    kept_kind: &str,
    kept_name: &str,
    dropped_ids: &[i64],
    links_retargeted: i64,
    links_dropped_as_duplicate: i64,
) -> serde_json::Value {
    serde_json::json!({
        "kept_id":                     kept_id,
        "kept_kind":                   kept_kind,
        "kept_name":                   kept_name,
        "dropped_ids":                 dropped_ids,
        "links_retargeted":            links_retargeted,
        "links_dropped_as_duplicate":  links_dropped_as_duplicate,
    })
}

/// Compute `total_duration_ms` for the finalize payload, clamping
/// negative or huge values (clock skew, missing `started_at`) to 0.
/// Pure helper, separately testable.
pub fn compute_duration_ms(
    started_at: Option<OffsetDateTime>,
    finished_at: OffsetDateTime,
) -> u64 {
    let Some(start) = started_at else { return 0 };
    let delta = finished_at - start;
    let millis = delta.whole_milliseconds();
    if millis <= 0 {
        0
    } else {
        u64::try_from(millis).unwrap_or(u64::MAX)
    }
}

/// The `extractor:gliner-relex` summary-row payload (v2 Entity Extraction)
/// lives in the sibling `extract_entities` module; re-exported here so the
/// public paths `scheduler::audit::{ACTION_EXTRACT_ENTITIES, build_…}` hold.
mod extract_entities;
pub use extract_entities::{build_extract_entities_payload, ACTION_EXTRACT_ENTITIES};

#[cfg(test)]
mod tests;
