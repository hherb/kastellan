//! Unit tests for the parent [`super`] audit-payload helpers.
//!
//! Lifted out of `scheduler/audit.rs` (sibling test-module split) to
//! keep the production file under the 500-LOC cap. The `use super::*`
//! below resolves to the parent `audit` module, reaching the
//! `build_*_payload` builders, `compute_duration_ms`,
//! `action_task_terminal`, and the `ACTION_*` / `SCHEDULER_AUDIT_ACTOR`
//! / `FINALIZE_PROVENANCE_*` consts (including the `extract_entities`
//! items re-exported by the parent). The `extract_entities` payload has
//! its own co-located tests in `audit/extract_entities.rs`.

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
        &L1WriteOutcome::Inserted { memory_id: 42, link_outcome: None },
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
        &L1WriteOutcome::Inserted { memory_id: 88, link_outcome: None },
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

// --- entities.{approved,rejected,merged} ----------------------------

#[test]
fn action_entities_approved_string_is_pinned() {
    assert_eq!(ACTION_ENTITIES_APPROVED, "entities.approved");
}

#[test]
fn action_entities_rejected_string_is_pinned() {
    assert_eq!(ACTION_ENTITIES_REJECTED, "entities.rejected");
}

#[test]
fn action_entities_merged_string_is_pinned() {
    assert_eq!(ACTION_ENTITIES_MERGED, "entities.merged");
}

// --- entity_kinds.{add,remove} --------------------------------------

/// Wire-stable contract for log-consumers. A rename would silently
/// break downstream observability filters.
#[test]
fn action_entity_kinds_add_string_is_pinned() {
    assert_eq!(ACTION_ENTITY_KINDS_ADD, "entity_kinds.add");
}

#[test]
fn action_entity_kinds_remove_string_is_pinned() {
    assert_eq!(ACTION_ENTITY_KINDS_REMOVE, "entity_kinds.remove");
}

// --- relation_kinds.{add,remove} ------------------------------------

/// Wire-stable contract for log-consumers. A rename would silently
/// break downstream observability filters.
#[test]
fn action_relation_kinds_add_string_is_pinned() {
    assert_eq!(ACTION_RELATION_KINDS_ADD, "relation_kinds.add");
}

#[test]
fn action_relation_kinds_remove_string_is_pinned() {
    assert_eq!(ACTION_RELATION_KINDS_REMOVE, "relation_kinds.remove");
}

#[test]
fn build_entities_approved_payload_has_exact_three_keys() {
    use std::collections::BTreeSet;
    let v = build_entities_approved_payload(7, "person", "Dr Smith");
    let keys: BTreeSet<&str> = v.as_object().unwrap().keys().map(|s| s.as_str()).collect();
    let expected: BTreeSet<&str> = ["entity_id", "kind", "name"].iter().copied().collect();
    assert_eq!(keys, expected);
    assert_eq!(v["entity_id"], 7);
    assert_eq!(v["kind"], "person");
    assert_eq!(v["name"], "Dr Smith");
}

#[test]
fn build_entities_rejected_payload_has_exact_four_keys() {
    use std::collections::BTreeSet;
    let v = build_entities_rejected_payload(7, "person", "Dr Smith", 3);
    let keys: BTreeSet<&str> = v.as_object().unwrap().keys().map(|s| s.as_str()).collect();
    let expected: BTreeSet<&str> = ["entity_id", "kind", "name", "mentions_dropped"]
        .iter().copied().collect();
    assert_eq!(keys, expected);
    assert_eq!(v["mentions_dropped"], 3);
}

#[test]
fn build_entities_merged_payload_has_exact_six_keys() {
    use std::collections::BTreeSet;
    let v = build_entities_merged_payload(1, "person", "Smith", &[2, 3], 4, 1);
    let keys: BTreeSet<&str> = v.as_object().unwrap().keys().map(|s| s.as_str()).collect();
    let expected: BTreeSet<&str> = [
        "kept_id", "kept_kind", "kept_name",
        "dropped_ids", "links_retargeted", "links_dropped_as_duplicate",
    ].iter().copied().collect();
    assert_eq!(keys, expected);
    assert_eq!(v["dropped_ids"].as_array().unwrap().len(), 2);
    assert_eq!(v["links_retargeted"], 4);
    assert_eq!(v["links_dropped_as_duplicate"], 1);
}

// --- build_l3_write_payload -----------------------------------------

#[test]
fn build_l3_write_payload_inserted_agent_raised() {
    use crate::memory::l3_crystallise::{L3Source, L3WriteOutcome};
    let p = build_l3_write_payload(
        &L3WriteOutcome::Inserted { memory_id: 11 },
        &L3Source::AgentRaised { task_id: 42 },
        "summarise_repo_readme",
        "abc123",
    );
    let o = p.as_object().expect("object");
    assert_eq!(o.get("source").unwrap(), "agent_raised");
    assert_eq!(o.get("task_id").unwrap(), 42);
    assert_eq!(o.get("skill_name").unwrap(), "summarise_repo_readme");
    assert_eq!(o.get("action").unwrap(), "inserted");
    assert_eq!(o.get("memory_id").unwrap(), 11);
    assert_eq!(o.get("body_sha256").unwrap(), "abc123");
    assert_eq!(o.len(), 6, "exactly 6 payload keys");
}

#[test]
fn build_l3_write_payload_skipped_duplicate() {
    use crate::memory::l3_crystallise::{L3Source, L3WriteOutcome};
    let p = build_l3_write_payload(
        &L3WriteOutcome::SkippedDuplicate { memory_id: 9 },
        &L3Source::AgentRaised { task_id: 1 },
        "n", "s",
    );
    assert_eq!(p.get("action").unwrap(), "skipped_duplicate");
    assert_eq!(p.get("memory_id").unwrap(), 9);
}

// --- l3 approve / revoke payload builders ---------------------------

#[test]
fn l3_approved_payload_shape() {
    let p = build_l3_approved_payload(7, "summarise_repo_readme", "abcd", &["shell-exec".to_string()]);
    assert_eq!(p["memory_id"], 7);
    assert_eq!(p["skill_name"], "summarise_repo_readme");
    assert_eq!(p["body_sha256"], "abcd");
    assert_eq!(p["tools"][0], "shell-exec");
}

#[test]
fn l3_approve_rejected_payload_includes_reasons_and_optionals() {
    let p = build_l3_approve_rejected_payload(
        9, Some("leaky"), Some("ff00"), &["tool 'x' is not registered".to_string()],
    );
    assert_eq!(p["memory_id"], 9);
    assert_eq!(p["skill_name"], "leaky");
    assert_eq!(p["body_sha256"], "ff00");
    assert_eq!(p["reasons"][0], "tool 'x' is not registered");

    // Optionals omitted when None.
    let p2 = build_l3_approve_rejected_payload(9, None, None, &["x".to_string()]);
    assert!(p2.get("skill_name").is_none());
    assert!(p2.get("body_sha256").is_none());
    assert_eq!(p2["reasons"][0], "x");
}

#[test]
fn l3_revoked_payload_shape() {
    let p = build_l3_revoked_payload(3, true);
    assert_eq!(p["memory_id"], 3);
    assert_eq!(p["updated"], true);
}

// --- l3 invocation payload builders ----------------------------------

#[test]
fn build_l3_invoked_payload_shape() {
    let p = build_l3_invoked_payload(7, "summarise_repo", "abc123", &["repo_path".into()], 2);
    assert_eq!(p["memory_id"], 7);
    assert_eq!(p["skill_name"], "summarise_repo");
    assert_eq!(p["body_sha256"], "abc123");
    assert_eq!(p["arg_names"][0], "repo_path");
    assert_eq!(p["step_count"], 2);
}

#[test]
fn build_l3_invoke_outcome_payload_shape() {
    let p = build_l3_invoke_outcome_payload(7, "summarise_repo", 1, 2, true);
    assert_eq!(p["memory_id"], 7);
    assert_eq!(p["skill_name"], "summarise_repo");
    assert_eq!(p["steps_executed"], 1);
    assert_eq!(p["steps_total"], 2);
    assert_eq!(p["any_err"], true);
}

#[test]
fn build_l3_invoke_rejected_payload_shape() {
    let p = build_l3_invoke_rejected_payload(7, Some("leaky"), Some("sha9"), &["bad tool".into()]);
    assert_eq!(p["memory_id"], 7);
    assert_eq!(p["skill_name"], "leaky");
    assert_eq!(p["body_sha256"], "sha9");
    assert_eq!(p["reasons"][0], "bad tool");
}

#[test]
fn build_l3_invoke_rejected_payload_omits_optional_when_none() {
    let p = build_l3_invoke_rejected_payload(7, None, None, &["r".into()]);
    assert!(p.get("skill_name").is_none());
    assert!(p.get("body_sha256").is_none());
    assert_eq!(p["memory_id"], 7);
}
