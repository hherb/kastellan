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

use hhagent_db::tasks::Lane;
use serde_json::{json, Value};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// Logical actor string used for every scheduler-emitted audit row.
/// Reused by [`super::tool_dispatch`] for its short-circuit rows so
/// consumers can `WHERE actor = 'scheduler'` to capture both families.
pub const SCHEDULER_AUDIT_ACTOR: &str = "scheduler";

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
    })
}

/// Build the JSON payload for the `task.finalize` summary row of a
/// **crashed** task (one recovered by the startup sweep).
///
/// Same 9-key shape as [`build_finalize_payload`] so observation-phase
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
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(keys(&p), expected);
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
    // crash-recovery path. Same 9-key shape, but the two counters
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
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(keys(&p), expected);
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
}
