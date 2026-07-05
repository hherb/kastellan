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
//! Three further row families are defined in sibling modules and
//! re-exported here so every public path stays `scheduler::audit::…`:
//! L1/L3 memory-layer rows (`memory_rows`) and operator entity-review +
//! kind-registry rows (`entity_rows`) — both split out 2026-07-05 for
//! the 500-LOC cap — plus the earlier `extractor:gliner-relex` summary
//! row (`extract_entities`, split during the v2 Entity Extraction arc).
//!
//! A handful of standalone action strings also remain in this file
//! outside the two families above (action strings only, no co-located
//! payload builders): the daemon bring-up rows [`ACTION_REGISTRY_LOADED`]
//! and [`ACTION_L0_SEEDED`], and the operator tools-allowlist rows
//! [`ACTION_TOOLS_ALLOWLIST_ADD`] / [`ACTION_TOOLS_ALLOWLIST_REMOVE`]
//! (whose symmetric `entity_kinds.*` / `relation_kinds.*` siblings live
//! in `entity_rows`).
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

use kastellan_db::tasks::Lane;
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

/// `action` value written when the lane runner claims a `pending` task
/// and transitions it to `running`. Fires exactly once per `claim_one`
/// success.
pub const ACTION_TASK_RUNNING: &str = "task.running";

/// `action` value for the per-task summary row. Fires once per
/// finalised task, regardless of which terminal state was reached.
/// Carries the aggregate counters observation-phase SQL needs.
pub const ACTION_TASK_FINALIZE: &str = "task.finalize";

/// `action` value for the producer-side row written by `kastellan-cli ask`
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
/// adds one allowlist entry via `kastellan-cli tools allowlist add`.
pub const ACTION_TOOLS_ALLOWLIST_ADD: &str = "tools.allowlist.add";

/// Action string for `actor='cli'` audit rows emitted when an operator
/// removes one allowlist entry via `kastellan-cli tools allowlist remove`.
pub const ACTION_TOOLS_ALLOWLIST_REMOVE: &str = "tools.allowlist.remove";

/// Value of the `provenance` field in a `task.finalize` payload emitted
/// from the scheduler's runtime path (the lane runner observed the task
/// end-to-end). Counters are facts; `started_at` is always present.
pub const FINALIZE_PROVENANCE_RUNTIME: &str = "runtime";

/// Value of the `provenance` field in a `task.finalize` payload emitted
/// from the startup crash-recovery sweep. Counters are JSON `null`
/// because the dead daemon's in-memory counters were lost.
pub const FINALIZE_PROVENANCE_CRASH_RECOVERY: &str = "crash_recovery";

/// Value of the `provenance` field in a `task.finalize` payload emitted
/// when a producer (`kastellan-cli`) cancels a `pending` task that was
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
/// when a producer (`kastellan-cli`) cancels a `pending` task that was
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

/// L1/L3 memory-layer rows (`l1.*` / `l3.*` constants + payload
/// builders); see the module doc above for the split/re-export rationale.
mod memory_rows;
pub use memory_rows::{
    build_l1_write_payload, build_l3_approve_rejected_payload, build_l3_approved_payload,
    build_l3_invoke_outcome_payload, build_l3_invoke_rejected_agent_payload,
    build_l3_invoke_rejected_payload, build_l3_invoked_payload, build_l3_pin_rejected_payload,
    build_l3_pinned_payload, build_l3_revoked_payload, build_l3_write_payload, ACTION_L1_ADDED,
    ACTION_L1_PROMOTED, ACTION_L1_REMOVED, ACTION_L3_APPROVED, ACTION_L3_APPROVE_REJECTED,
    ACTION_L3_CRYSTALLISED, ACTION_L3_INVOKED, ACTION_L3_INVOKE_OUTCOME,
    ACTION_L3_INVOKE_REJECTED, ACTION_L3_PINNED, ACTION_L3_PIN_REJECTED, ACTION_L3_REMOVED,
    ACTION_L3_REVOKED,
};

/// Operator entity-review + entity/relation-kind rows (`entities.*`,
/// `entity_kinds.*`, `relation_kinds.*`); same split/re-export rationale.
mod entity_rows;
pub use entity_rows::{
    build_entities_approved_payload, build_entities_merged_payload,
    build_entities_rejected_payload, ACTION_ENTITIES_APPROVED, ACTION_ENTITIES_MERGED,
    ACTION_ENTITIES_REJECTED, ACTION_ENTITY_KINDS_ADD, ACTION_ENTITY_KINDS_REMOVE,
    ACTION_RELATION_KINDS_ADD, ACTION_RELATION_KINDS_REMOVE,
};

#[cfg(test)]
mod tests;
