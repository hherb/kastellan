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
        L1WriteOutcome::Inserted { memory_id } => ("inserted", *memory_id),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use time::macros::datetime;

    fn keys(v: &Value) -> BTreeSet<String> {
        v.as_object()
            .expect("payload is a JSON object")
            .keys()
            .cloned()
            .collect()
    }

    // --- action_task_terminal -------------------------------------------

    #[test]
    fn action_task_terminal_concatenates_with_dot() {
        assert_eq!(action_task_terminal("completed"), "task.completed");
        assert_eq!(action_task_terminal("failed"), "task.failed");
        assert_eq!(action_task_terminal("cancelled"), "task.cancelled");
        assert_eq!(action_task_terminal("timed_out"), "task.timed_out");
        assert_eq!(action_task_terminal("blocked"), "task.blocked");
        assert_eq!(action_task_terminal("crashed"), "task.crashed");
    }

    #[test]
    fn action_task_terminal_uses_pinned_prefix_constant() {
        // Defends against drift if someone renames ACTION_TASK_PREFIX.
        assert!(action_task_terminal("x").starts_with(ACTION_TASK_PREFIX));
    }

    // --- build_lifecycle_payload ----------------------------------------

    #[test]
    fn build_lifecycle_payload_shape_pins_exact_key_set() {
        let p = build_lifecycle_payload(42, Lane::Fast, 3);
        let expected: BTreeSet<String> = ["task_id", "lane", "plan_count"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(keys(&p), expected);
    }

    #[test]
    fn build_lifecycle_payload_serialises_field_values() {
        let p = build_lifecycle_payload(7, Lane::Long, 12);
        assert_eq!(p["task_id"], 7);
        assert_eq!(p["lane"], "long");
        assert_eq!(p["plan_count"], 12);
    }

    #[test]
    fn build_lifecycle_payload_lane_as_sql_round_trip() {
        // `lane` is serialised via Lane::as_sql() — pinned so a future
        // change to the enum's serde tag (e.g. lower → PascalCase)
        // doesn't silently rename the audit-log field value.
        assert_eq!(
            build_lifecycle_payload(1, Lane::Fast, 0)["lane"],
            "fast"
        );
        assert_eq!(
            build_lifecycle_payload(1, Lane::Long, 0)["lane"],
            "long"
        );
    }

    // --- build_finalize_payload -----------------------------------------

    fn sample_stats() -> TaskFinalizeStats {
        TaskFinalizeStats {
            plan_count: 2,
            total_llm_calls: 2,
            total_dispatch_calls: 1,
            total_duration_ms: 5432,
            started_at: Some(datetime!(2026-05-12 10:00:00 UTC)),
            finished_at: datetime!(2026-05-12 10:00:05.432 UTC),
        }
    }

    #[test]
    fn build_finalize_payload_shape_pins_exact_key_set() {
        let p = build_finalize_payload(99, Lane::Fast, "completed", &sample_stats());
        let expected: BTreeSet<String> = [
            "task_id",
            "lane",
            "state",
            "plan_count",
            "total_llm_calls",
            "total_dispatch_calls",
            "total_duration_ms",
            "started_at",
            "finished_at",
            "provenance",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(keys(&p), expected);
    }

    /// `build_finalize_payload` hardcodes `provenance="runtime"` —
    /// this helper is the runtime scheduler's entry point. A future
    /// refactor that lifts the value out of the helper must update
    /// callers; the constant + this pin together make that explicit.
    /// Issue #50 schema-v2.
    #[test]
    fn build_finalize_payload_provenance_is_runtime() {
        let p = build_finalize_payload(1, Lane::Fast, "completed", &sample_stats());
        assert_eq!(p["provenance"], FINALIZE_PROVENANCE_RUNTIME);
    }

    #[test]
    fn build_finalize_payload_serialises_field_values() {
        let p = build_finalize_payload(99, Lane::Long, "failed", &sample_stats());
        assert_eq!(p["task_id"], 99);
        assert_eq!(p["lane"], "long");
        assert_eq!(p["state"], "failed");
        assert_eq!(p["plan_count"], 2);
        assert_eq!(p["total_llm_calls"], 2);
        assert_eq!(p["total_dispatch_calls"], 1);
        assert_eq!(p["total_duration_ms"], 5432);
    }

    #[test]
    fn build_finalize_payload_started_at_null_when_absent() {
        let mut s = sample_stats();
        s.started_at = None;
        let p = build_finalize_payload(1, Lane::Fast, "cancelled", &s);
        assert!(p["started_at"].is_null());
        // finished_at remains a string regardless.
        assert!(p["finished_at"].is_string());
    }

    #[test]
    fn build_finalize_payload_timestamps_are_rfc3339_strings() {
        let p = build_finalize_payload(1, Lane::Fast, "completed", &sample_stats());
        // Should round-trip via the same parser. The 'Z' suffix proves
        // the value is UTC and uses Rfc3339 — a naive Debug-print
        // would have different shape.
        let s = p["finished_at"].as_str().unwrap();
        let parsed = OffsetDateTime::parse(s, &Rfc3339).expect("rfc3339 round-trip");
        assert_eq!(parsed, sample_stats().finished_at);
    }

    // --- compute_duration_ms --------------------------------------------

    // --- build_crashed_finalize_payload --------------------------------
    //
    // Companion to `build_finalize_payload` for the startup
    // crash-recovery path. Same 10-key shape, but the two counters
    // (`total_llm_calls`, `total_dispatch_calls`) are JSON `null`
    // because they died with the previous daemon — null is the wire
    // signal "unknowable", distinct from `0` which would mean
    // "observed zero". `total_duration_ms` is `null` when `started_at`
    // is missing (can't compute) and a number otherwise. `state` is
    // hard-pinned to `"crashed"` so the helper can't be misused for
    // any other terminal state.

    #[test]
    fn build_crashed_finalize_payload_shape_pins_exact_key_set() {
        let p = build_crashed_finalize_payload(
            42,
            Lane::Fast,
            3,
            Some(datetime!(2026-05-12 10:00:00 UTC)),
            datetime!(2026-05-12 10:00:05.432 UTC),
        );
        let expected: BTreeSet<String> = [
            "task_id",
            "lane",
            "state",
            "plan_count",
            "total_llm_calls",
            "total_dispatch_calls",
            "total_duration_ms",
            "started_at",
            "finished_at",
            "provenance",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(keys(&p), expected);
    }

    /// `build_crashed_finalize_payload` hardcodes
    /// `provenance="crash_recovery"`. Issue #50 schema-v2.
    #[test]
    fn build_crashed_finalize_payload_provenance_is_crash_recovery() {
        let p = build_crashed_finalize_payload(
            1,
            Lane::Fast,
            0,
            None,
            datetime!(2026-05-12 10:00:00 UTC),
        );
        assert_eq!(p["provenance"], FINALIZE_PROVENANCE_CRASH_RECOVERY);
    }

    // --- build_producer_cancel_finalize_payload -------------------------
    //
    // Companion to `build_finalize_payload` for the producer-cancel
    // path (`hhagent-cli ask` cancelling a `pending` task that was
    // never claimed). Same 10-key shape; everything-known-constant
    // values hardcoded. Issue #50 schema-v2 added `provenance` so
    // observation queries no longer infer the path from
    // `actor + total_llm_calls + started_at` heuristics.

    #[test]
    fn build_producer_cancel_finalize_payload_shape_pins_exact_key_set() {
        let p = build_producer_cancel_finalize_payload(
            42,
            Lane::Fast,
            0,
            datetime!(2026-05-13 10:00:00 UTC),
        );
        let expected: BTreeSet<String> = [
            "task_id",
            "lane",
            "state",
            "plan_count",
            "total_llm_calls",
            "total_dispatch_calls",
            "total_duration_ms",
            "started_at",
            "finished_at",
            "provenance",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(keys(&p), expected);
    }

    #[test]
    fn build_producer_cancel_finalize_payload_state_is_always_cancelled() {
        let p = build_producer_cancel_finalize_payload(
            1,
            Lane::Long,
            7,
            datetime!(2026-05-13 10:00:00 UTC),
        );
        assert_eq!(p["state"], "cancelled");
    }

    #[test]
    fn build_producer_cancel_finalize_payload_counters_are_known_zero() {
        // Distinct from the crash-recovery path (JSON null = unknowable),
        // the producer-cancel path KNOWS the counters are zero because
        // the task never ran. Integer zero on the wire.
        let p = build_producer_cancel_finalize_payload(
            1,
            Lane::Fast,
            0,
            datetime!(2026-05-13 10:00:00 UTC),
        );
        assert_eq!(p["total_llm_calls"], 0);
        assert_eq!(p["total_dispatch_calls"], 0);
        assert_eq!(p["total_duration_ms"], 0);
    }

    #[test]
    fn build_producer_cancel_finalize_payload_started_at_is_always_null() {
        // The task never entered `running`, so `mark_cancelled` never
        // set `started_at`. JSON null is the wire signal "never claimed".
        let p = build_producer_cancel_finalize_payload(
            1,
            Lane::Fast,
            0,
            datetime!(2026-05-13 10:00:00 UTC),
        );
        assert!(p["started_at"].is_null());
    }

    #[test]
    fn build_producer_cancel_finalize_payload_provenance_is_producer_cancel_pending() {
        let p = build_producer_cancel_finalize_payload(
            1,
            Lane::Fast,
            0,
            datetime!(2026-05-13 10:00:00 UTC),
        );
        assert_eq!(
            p["provenance"],
            FINALIZE_PROVENANCE_PRODUCER_CANCEL_PENDING
        );
    }

    /// Provenance values are a closed set; the three helpers' outputs
    /// must be discriminable on this field alone. Pinned so a future
    /// addition (e.g. `"operator_fail"`) is a deliberate change.
    #[test]
    fn finalize_provenance_values_are_distinct() {
        assert_ne!(
            FINALIZE_PROVENANCE_RUNTIME,
            FINALIZE_PROVENANCE_CRASH_RECOVERY
        );
        assert_ne!(
            FINALIZE_PROVENANCE_RUNTIME,
            FINALIZE_PROVENANCE_PRODUCER_CANCEL_PENDING
        );
        assert_ne!(
            FINALIZE_PROVENANCE_CRASH_RECOVERY,
            FINALIZE_PROVENANCE_PRODUCER_CANCEL_PENDING
        );
    }

    #[test]
    fn build_crashed_finalize_payload_state_is_always_crashed() {
        // The helper is single-purpose: a crash-recovery sweep emits
        // `state="crashed"` regardless of caller intent. Caller errors
        // can't produce a wrong state-string.
        let finished = datetime!(2026-05-12 10:00:00 UTC);
        let p = build_crashed_finalize_payload(1, Lane::Fast, 0, None, finished);
        assert_eq!(p["state"], "crashed");
        let p2 = build_crashed_finalize_payload(2, Lane::Long, 99, Some(finished), finished);
        assert_eq!(p2["state"], "crashed");
    }

    #[test]
    fn build_crashed_finalize_payload_counters_are_json_null() {
        // The two aggregate counters were carried in the dead daemon's
        // memory and cannot be recovered. JSON `null` is the wire
        // signal "unknowable" — distinguishable from `0` (which the
        // runtime path emits to mean "observed zero").
        let p = build_crashed_finalize_payload(
            1,
            Lane::Fast,
            5,
            Some(datetime!(2026-05-12 10:00:00 UTC)),
            datetime!(2026-05-12 10:00:01 UTC),
        );
        assert!(
            p["total_llm_calls"].is_null(),
            "total_llm_calls must be JSON null for crashed tasks (got {:?})",
            p["total_llm_calls"]
        );
        assert!(
            p["total_dispatch_calls"].is_null(),
            "total_dispatch_calls must be JSON null for crashed tasks"
        );
    }

    #[test]
    fn build_crashed_finalize_payload_serialises_known_fields() {
        let finished = datetime!(2026-05-12 10:00:05.432 UTC);
        let p = build_crashed_finalize_payload(
            99,
            Lane::Long,
            7,
            Some(datetime!(2026-05-12 10:00:00 UTC)),
            finished,
        );
        assert_eq!(p["task_id"], 99);
        assert_eq!(p["lane"], "long");
        assert_eq!(p["plan_count"], 7);
        // finished_at always present; serialised as RFC 3339 string.
        let s = p["finished_at"].as_str().expect("finished_at is a string");
        let parsed = OffsetDateTime::parse(s, &Rfc3339).expect("rfc3339 round-trip");
        assert_eq!(parsed, finished);
    }

    #[test]
    fn build_crashed_finalize_payload_started_at_null_collapses_duration() {
        // If `started_at` is missing (CLI cancel raced the claim, then
        // a separate-daemon crash never recovered) the duration is
        // unknowable too — both go to null, in lockstep.
        let p = build_crashed_finalize_payload(
            1,
            Lane::Fast,
            0,
            None,
            datetime!(2026-05-12 10:00:00 UTC),
        );
        assert!(p["started_at"].is_null());
        assert!(p["total_duration_ms"].is_null());
    }

    #[test]
    fn build_crashed_finalize_payload_computes_duration_when_started_at_present() {
        let start = datetime!(2026-05-12 10:00:00 UTC);
        let finish = datetime!(2026-05-12 10:00:01.250 UTC);
        let p = build_crashed_finalize_payload(1, Lane::Fast, 0, Some(start), finish);
        assert_eq!(p["total_duration_ms"], 1250);
        assert!(p["started_at"].is_string());
    }

    // --- compute_duration_ms --------------------------------------------

    #[test]
    fn compute_duration_ms_happy_path() {
        let start = datetime!(2026-05-12 10:00:00 UTC);
        let finish = datetime!(2026-05-12 10:00:01.250 UTC);
        assert_eq!(compute_duration_ms(Some(start), finish), 1250);
    }

    #[test]
    fn compute_duration_ms_clamps_negative_to_zero() {
        // Should never happen in practice (started_at is a DB now(),
        // finished_at is a local now() always later) but cheap to
        // defend against clock skew.
        let start = datetime!(2026-05-12 10:00:01 UTC);
        let finish = datetime!(2026-05-12 10:00:00 UTC);
        assert_eq!(compute_duration_ms(Some(start), finish), 0);
    }

    #[test]
    fn compute_duration_ms_returns_zero_when_started_at_missing() {
        let finish = datetime!(2026-05-12 10:00:00 UTC);
        assert_eq!(compute_duration_ms(None, finish), 0);
    }

    // --- build_l1_write_payload -----------------------------------------

    #[test]
    fn build_l1_write_payload_operator_inserted_shape() {
        let payload = build_l1_write_payload(
            &L1WriteOutcome::Inserted { memory_id: 42 },
            &L1Source::Operator,
            "abc123",
        );
        assert_eq!(
            payload,
            json!({"source": "operator", "action": "inserted", "memory_id": 42, "body_sha256": "abc123"}),
        );
    }

    #[test]
    fn build_l1_write_payload_operator_skipped_duplicate_shape() {
        let payload = build_l1_write_payload(
            &L1WriteOutcome::SkippedDuplicate { memory_id: 7 },
            &L1Source::Operator,
            "def456",
        );
        assert_eq!(
            payload,
            json!({"source": "operator", "action": "skipped_duplicate", "memory_id": 7, "body_sha256": "def456"}),
        );
    }

    #[test]
    fn build_l1_write_payload_agent_raised_carries_task_id() {
        let payload = build_l1_write_payload(
            &L1WriteOutcome::Inserted { memory_id: 88 },
            &L1Source::AgentRaised { task_id: 123 },
            "abc123",
        );
        assert_eq!(
            payload,
            json!({"source": "agent_raised", "task_id": 123, "action": "inserted", "memory_id": 88, "body_sha256": "abc123"}),
        );
    }

    #[test]
    fn build_l1_write_payload_agent_raised_skipped_duplicate_shape() {
        let payload = build_l1_write_payload(
            &L1WriteOutcome::SkippedDuplicate { memory_id: 88 },
            &L1Source::AgentRaised { task_id: 99 },
            "ddd",
        );
        assert_eq!(
            payload,
            json!({"source": "agent_raised", "task_id": 99, "action": "skipped_duplicate", "memory_id": 88, "body_sha256": "ddd"}),
        );
    }

    #[test]
    fn l1_action_constants_are_distinct_and_stable() {
        // Stability check: these strings are wire contract. A future
        // rename would invalidate JSONB queries grouped on `action`.
        assert_eq!(ACTION_L1_ADDED, "l1.added");
        assert_eq!(ACTION_L1_REMOVED, "l1.removed");
        assert_eq!(ACTION_L1_PROMOTED, "l1.promoted");
    }
}
