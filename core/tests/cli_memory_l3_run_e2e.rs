//! Live-PG e2e for operator-triggered L3 skill invocation via `invoke_l3`.
//!
//! ## What this file pins
//!
//! Five independent scenarios each spinning up their own per-test PG cluster
//! and calling `invoke_l3` directly (typed assertions on `InvokeReport`):
//!
//!  A. **Dry-run** — `execute=false` returns `DryRun{steps}` with substituted
//!     args; NO audit rows for l3.invoked / l3.invoke_outcome / tool:shell-exec.
//!
//!  B. **Execute happy path** — `execute=true`, approved skill, `ECHO_PATH`
//!     allowlisted → `Executed{outcomes, steps_total:1}`, single `Ok`, correct
//!     audit trail (l3.invoked + tool:shell-exec/shell.exec + l3.invoke_outcome).
//!
//!  C. **Untrusted refuses** — skill left at `untrusted` trust → `Refused`;
//!     one `l3.invoke_rejected` row; NO l3.invoked / tool:shell-exec rows.
//!
//!  D. **Unknown tool refuses (live re-validation)** — approved skill whose
//!     step names `ghost-tool`, absent from the registry → `Refused` with the
//!     tool name in a reason; one `l3.invoke_rejected` row; NO l3.invoked rows.
//!
//!  E. **Stop at first error** — two-step approved skill, step-1 `CAT_PATH`
//!     not allowlisted (→ POLICY_DENIED), step-2 `ECHO_PATH` allowlisted.
//!     Registry has shell-exec (gate passes), dispatch fails on step-1.
//!     Assert: `Executed{outcomes len=1, steps_total=2}`, `any_err=true`; the
//!     `l3.invoke_outcome` audit payload shows `steps_executed=1 steps_total=2
//!     any_err=true`; only ONE `tool:shell-exec` chokepoint row.
//!
//! ## Skip semantics
//!
//! Every scenario short-circuits with a `[SKIP]` print when the host lacks
//! `pg_ctl`, a reachable supervisor, the worker binary, or a sandbox backend.
//! Cross-platform (Linux + macOS).
//!
//! NOTE (post-#179): the operator CLI no longer calls `invoke_l3` in-process —
//! `memory l3 run` submits an `l3_run` task that the daemon executes via the
//! same `invoke_l3` entry point. These scenarios therefore exercise the
//! **daemon-side** execution machinery directly (no CLI subprocess). The
//! end-to-end CLI→daemon path (including the #179 divergence fix) is covered by
//! `cli_memory_l3_run_daemon_e2e.rs`.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use hhagent_core::cassandra::types::{L3Param, L3SkillCandidate, L3TemplateStep};
use hhagent_core::memory::l3_approval::SkillTrust;
use hhagent_core::memory::l3_crystallise::{crystallise_l3, L3Source, L3WriteOutcome};
use hhagent_core::memory::l3_invoke::{invoke_l3, InvokeReport};
use hhagent_core::scheduler::inner_loop::StepOutcome;
use hhagent_core::scheduler::{shell_exec_entry, ToolHostStepDispatcher, ToolRegistry};
use hhagent_core::secrets::Vault;
use hhagent_core::worker_lifecycle::CompositeLifecycle;
use hhagent_db::memories::set_skill_trust;
use hhagent_db::pool::connect_runtime_pool;
use hhagent_db::probe::run as probe_run;
use hhagent_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, shell_exec_worker_binary, skip_if_no_supervisor,
    skip_if_sandbox_unavailable, unique_suffix,
};

// ---------------------------------------------------------------------------
// Platform constants
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
const ECHO_PATH: &str = "/usr/bin/echo";
#[cfg(target_os = "macos")]
const ECHO_PATH: &str = "/bin/echo";

#[cfg(target_os = "linux")]
const CAT_PATH: &str = "/usr/bin/cat";
#[cfg(target_os = "macos")]
const CAT_PATH: &str = "/bin/cat";

// ---------------------------------------------------------------------------
// Per-scenario bring-up helper
// ---------------------------------------------------------------------------

/// Bring up a Postgres cluster for a single scenario. Returns `None` and
/// prints a `[SKIP]` line when the host is missing PG, a supervisor, the
/// worker binary, or a sandbox backend.
async fn bring_up_for_scenario(
    data_label: &str,
    log_label: &str,
    service_suffix: &str,
) -> Option<(sqlx::PgPool, hhagent_tests_common::PgCluster)> {
    if skip_if_no_supervisor() {
        return None;
    }
    if skip_if_sandbox_unavailable() {
        return None;
    }
    let bin_dir = pg_bin_dir_or_skip()?;

    let worker = shell_exec_worker_binary();
    if !worker.exists() {
        eprintln!("\n[SKIP] worker binary not built; run `cargo build --workspace`\n");
        return None;
    }

    let suffix = unique_suffix();
    let service_name = format!("hhagent-postgres-l3-run-{service_suffix}-{suffix}");

    let cluster = tokio::task::block_in_place(|| {
        bring_up_pg_cluster(&bin_dir, data_label, log_label, &service_name)
    });

    probe_run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"test": service_suffix}),
    )
    .await
    .expect("probe_run: migration or DB probe failed");

    let pool = connect_runtime_pool(&cluster.conn_spec).await.ok()?;
    Some((pool, cluster))
}

// ---------------------------------------------------------------------------
// Helpers to build the dispatcher
// ---------------------------------------------------------------------------

/// Build a `ToolHostStepDispatcher` wrapping `shell-exec` with `allowlist`.
fn make_dispatcher(
    pool: sqlx::PgPool,
    allowlist: &[String],
) -> ToolHostStepDispatcher {
    let worker = shell_exec_worker_binary();
    let mut registry = ToolRegistry::new();
    registry.insert("shell-exec", shell_exec_entry(worker, allowlist));
    let registry = Arc::new(registry);

    let sandboxes = Arc::new(hhagent_sandbox::SandboxBackends::default_for_current_os());
    let lifecycle: Arc<dyn hhagent_core::worker_lifecycle::WorkerLifecycleManager> =
        Arc::new(CompositeLifecycle::new(Arc::clone(&sandboxes)));
    let vault = Arc::new(Vault::new());

    ToolHostStepDispatcher::new(pool, vault, lifecycle, registry,
        std::sync::Arc::new(hhagent_core::handoff::HandoffCache::new()),
    )
}

/// The live tool-name set for these tests: only shell-exec is registered.
/// Must stay in sync with the registry built in make_dispatcher.
fn shell_exec_live_tools() -> BTreeSet<String> {
    let mut s = BTreeSet::new();
    s.insert("shell-exec".to_string());
    s
}

// ---------------------------------------------------------------------------
// Fixture: simple one-step echo skill
// ---------------------------------------------------------------------------

fn echo_skill(msg_value: &str) -> L3SkillCandidate {
    L3SkillCandidate {
        name: "echo_msg".into(),
        description: "Echo a message".into(),
        parameters: vec![L3Param { name: "msg".into(), description: "the message".into() }],
        steps: vec![L3TemplateStep {
            tool: "shell-exec".into(),
            method: "shell.exec".into(),
            parameters: serde_json::json!({ "argv": [ECHO_PATH, msg_value] }),
        }],
    }
}

fn echo_skill_template() -> L3SkillCandidate {
    echo_skill("{{msg}}")
}

// ---------------------------------------------------------------------------
// Scenario A — dry-run: preview spawns nothing, writes no audit row
// ---------------------------------------------------------------------------

/// Dry-run (execute=false) must:
///   * return `InvokeReport::DryRun` with the substituted concrete step;
///   * write ZERO `l3.invoked`, `l3.invoke_outcome`, or `tool:shell-exec` rows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dry_run_preview_spawns_nothing_writes_no_audit_row() {
    let Some((pool, _cluster)) = bring_up_for_scenario("l3r-a-d", "l3r-a-l", "a-dryrun").await
    else {
        return;
    };

    // Seed and approve the skill.
    let outcome = crystallise_l3(&pool, &echo_skill_template(), L3Source::AgentRaised { task_id: 1 })
        .await
        .expect("crystallise_l3");
    let memory_id = outcome.memory_id();
    set_skill_trust(&pool, memory_id, "user_approved")
        .await
        .expect("set_skill_trust");

    let dispatcher = make_dispatcher(pool.clone(), &[ECHO_PATH.to_string()]);
    let live_tools = shell_exec_live_tools();
    let mut args = BTreeMap::new();
    args.insert("msg".to_string(), "hello".to_string());

    let report = invoke_l3(
        &pool,
        memory_id,
        &dispatcher,
        &echo_skill_template(),
        SkillTrust::UserApproved,
        "",
        &args,
        &live_tools,
        false, // dry-run
    )
    .await;

    // Assert DryRun with the substituted step.
    let steps = match report {
        InvokeReport::DryRun { steps } => steps,
        other => panic!("expected DryRun, got {other:?}"),
    };
    assert_eq!(steps.len(), 1, "expected one step in dry-run");
    let argv = &steps[0].parameters["argv"];
    assert_eq!(argv[0], ECHO_PATH, "argv[0] must be echo path");
    assert_eq!(argv[1], "hello", "argv[1] must be substituted value");

    // No l3.* / tool:* rows (approved dry-run writes none of them).
    let disallowed_rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT actor, action FROM audit_log
         WHERE action LIKE 'l3.%'
            OR actor LIKE 'tool:%'",
    )
    .fetch_all(&pool)
    .await
    .expect("fetch disallowed rows");
    assert!(
        disallowed_rows.is_empty(),
        "dry-run must not write l3.*/tool rows; found: {disallowed_rows:?}",
    );

    pool.close().await;
}

// ---------------------------------------------------------------------------
// Scenario B — execute: round-trips through the real sandbox
// ---------------------------------------------------------------------------

/// `execute=true` with an approved `ECHO_PATH` skill must:
///   * return `InvokeReport::Executed` with one `Ok` outcome;
///   * write exactly one `cli/l3.invoked`, one `tool:shell-exec/shell.exec`,
///     and one `cli/l3.invoke_outcome` row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b_execute_round_trips_through_real_sandbox() {
    let Some((pool, _cluster)) = bring_up_for_scenario("l3r-b-d", "l3r-b-l", "b-exec").await
    else {
        return;
    };

    let outcome = crystallise_l3(&pool, &echo_skill_template(), L3Source::AgentRaised { task_id: 1 })
        .await
        .expect("crystallise_l3");
    let memory_id = outcome.memory_id();
    set_skill_trust(&pool, memory_id, "user_approved")
        .await
        .expect("set_skill_trust");

    let dispatcher = make_dispatcher(pool.clone(), &[ECHO_PATH.to_string()]);
    let live_tools = shell_exec_live_tools();
    let mut args = BTreeMap::new();
    args.insert("msg".to_string(), "sandbox-test".to_string());

    let report = invoke_l3(
        &pool,
        memory_id,
        &dispatcher,
        &echo_skill_template(),
        SkillTrust::UserApproved,
        "",
        &args,
        &live_tools,
        true, // execute
    )
    .await;

    // Assert Executed with one Ok outcome.
    let (outcomes, steps_total) = match report {
        InvokeReport::Executed { outcomes, steps_total } => (outcomes, steps_total),
        other => panic!("expected Executed, got {other:?}"),
    };
    assert_eq!(steps_total, 1, "steps_total must be 1");
    assert_eq!(outcomes.len(), 1, "must have exactly one outcome");
    match &outcomes[0] {
        StepOutcome::Ok(v) => {
            let exit_code = v["exit_code"].as_i64().unwrap_or(-1);
            assert_eq!(exit_code, 0, "echo must exit 0; got value={v}");
        }
        StepOutcome::Err { code, detail } => {
            panic!("expected Ok, got Err code={code} detail={detail}");
        }
    }

    // Audit trail: 1 × l3.invoked, 1 × tool:shell-exec/shell.exec, 1 × l3.invoke_outcome.
    // (Row 0 = startup; order is deterministic by id.)
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT actor, action FROM audit_log ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .expect("fetch audit rows");

    let invoked_count = rows.iter().filter(|(a, act)| a == "cli" && act == "l3.invoked").count();
    let tool_count = rows.iter().filter(|(a, act)| a == "tool:shell-exec" && act == "shell.exec").count();
    let outcome_count = rows.iter().filter(|(a, act)| a == "cli" && act == "l3.invoke_outcome").count();

    assert_eq!(invoked_count, 1, "must have exactly 1 cli/l3.invoked row; rows={rows:?}");
    assert_eq!(tool_count, 1, "must have exactly 1 tool:shell-exec/shell.exec row; rows={rows:?}");
    assert_eq!(outcome_count, 1, "must have exactly 1 cli/l3.invoke_outcome row; rows={rows:?}");

    // Payload assertions: fetch the rows that carry l3 envelope payloads.
    let payload_rows: Vec<(String, String, serde_json::Value)> = sqlx::query_as(
        "SELECT actor, action, payload FROM audit_log
         WHERE action IN ('l3.invoked', 'l3.invoke_outcome')
         ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .expect("fetch payload rows for scenario B");

    // l3.invoked row.
    let invoked_row = payload_rows
        .iter()
        .find(|(_, act, _)| act == "l3.invoked")
        .expect("l3.invoked row must exist");
    let invoked_payload = &invoked_row.2;
    assert_eq!(
        invoked_payload["memory_id"].as_i64(),
        Some(memory_id as i64),
        "l3.invoked payload.memory_id must match seeded skill; payload={invoked_payload}",
    );
    assert_eq!(
        invoked_payload["step_count"].as_i64(),
        Some(1),
        "l3.invoked payload.step_count must be 1; payload={invoked_payload}",
    );

    // l3.invoke_outcome row.
    let outcome_row = payload_rows
        .iter()
        .find(|(_, act, _)| act == "l3.invoke_outcome")
        .expect("l3.invoke_outcome row must exist");
    let outcome_payload = &outcome_row.2;
    assert_eq!(
        outcome_payload["memory_id"].as_i64(),
        Some(memory_id as i64),
        "l3.invoke_outcome payload.memory_id must match seeded skill; payload={outcome_payload}",
    );
    assert_eq!(
        outcome_payload["steps_executed"].as_i64(),
        Some(1),
        "l3.invoke_outcome payload.steps_executed must be 1; payload={outcome_payload}",
    );
    assert_eq!(
        outcome_payload["steps_total"].as_i64(),
        Some(1),
        "l3.invoke_outcome payload.steps_total must be 1; payload={outcome_payload}",
    );
    assert_eq!(
        outcome_payload["any_err"].as_bool(),
        Some(false),
        "l3.invoke_outcome payload.any_err must be false; payload={outcome_payload}",
    );

    pool.close().await;
}

// ---------------------------------------------------------------------------
// Scenario C — untrusted skill refuses
// ---------------------------------------------------------------------------

/// An `untrusted` skill must:
///   * return `InvokeReport::Refused` with a reason mentioning trust;
///   * write exactly one `cli/l3.invoke_rejected` row;
///   * write NO `l3.invoked` or `tool:shell-exec` rows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn c_untrusted_skill_refuses() {
    let Some((pool, _cluster)) = bring_up_for_scenario("l3r-c-d", "l3r-c-l", "c-untrusted").await
    else {
        return;
    };

    // Seed but do NOT approve — stays untrusted.
    let outcome = crystallise_l3(&pool, &echo_skill_template(), L3Source::AgentRaised { task_id: 1 })
        .await
        .expect("crystallise_l3");
    let memory_id = outcome.memory_id();
    // Deliberately left untrusted.

    let dispatcher = make_dispatcher(pool.clone(), &[ECHO_PATH.to_string()]);
    let live_tools = shell_exec_live_tools();
    let mut args = BTreeMap::new();
    args.insert("msg".to_string(), "should-not-run".to_string());

    let report = invoke_l3(
        &pool,
        memory_id,
        &dispatcher,
        &echo_skill_template(),
        SkillTrust::Untrusted, // untrusted
        "",
        &args,
        &live_tools,
        true,
    )
    .await;

    // Assert Refused with a trust-related reason.
    let reasons = match report {
        InvokeReport::Refused { reasons } => reasons,
        other => panic!("expected Refused, got {other:?}"),
    };
    assert!(
        reasons.iter().any(|r| r.contains("trust")),
        "refusal must mention 'trust'; got {reasons:?}",
    );

    // Audit: one l3.invoke_rejected, NO l3.invoked / tool:shell-exec.
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT actor, action FROM audit_log ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .expect("fetch audit rows");

    let rejected_count = rows.iter().filter(|(a, act)| a == "cli" && act == "l3.invoke_rejected").count();
    let invoked_count = rows.iter().filter(|(_, act)| act == "l3.invoked").count();
    let tool_count = rows.iter().filter(|(a, _)| a.starts_with("tool:")).count();

    assert_eq!(rejected_count, 1, "must have exactly 1 l3.invoke_rejected row; rows={rows:?}");
    assert_eq!(invoked_count, 0, "must have 0 l3.invoked rows; rows={rows:?}");
    assert_eq!(tool_count, 0, "must have 0 tool:* rows; rows={rows:?}");

    // Payload assertions on the rejected row.
    let rejected_payload_rows: Vec<(String, String, serde_json::Value)> = sqlx::query_as(
        "SELECT actor, action, payload FROM audit_log
         WHERE action = 'l3.invoke_rejected'
         ORDER BY id
         LIMIT 1",
    )
    .fetch_all(&pool)
    .await
    .expect("fetch rejected payload row for scenario C");
    assert_eq!(rejected_payload_rows.len(), 1, "expected one l3.invoke_rejected payload row");
    let rej_payload = &rejected_payload_rows[0].2;
    assert_eq!(
        rej_payload["memory_id"].as_i64(),
        Some(memory_id as i64),
        "l3.invoke_rejected payload.memory_id must match seeded skill; payload={rej_payload}",
    );
    let rej_reasons = rej_payload["reasons"].as_array()
        .expect("l3.invoke_rejected payload.reasons must be an array");
    assert!(!rej_reasons.is_empty(), "l3.invoke_rejected payload.reasons must be non-empty; payload={rej_payload}");

    pool.close().await;
}

// ---------------------------------------------------------------------------
// Scenario D — tool not in the live registry refuses (live re-validation)
// ---------------------------------------------------------------------------

/// An approved skill whose step names `ghost-tool` (absent from the registry)
/// must:
///   * return `InvokeReport::Refused` with a reason naming `ghost-tool`;
///   * write one `l3.invoke_rejected` row;
///   * write NO `l3.invoked` / `tool:*` rows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn d_unknown_tool_refuses_via_live_revalidation() {
    let Some((pool, _cluster)) = bring_up_for_scenario("l3r-d-d", "l3r-d-l", "d-notool").await
    else {
        return;
    };

    // Skill whose step names a tool not in the registry.
    let ghost_skill = L3SkillCandidate {
        name: "ghost_skill".into(),
        description: "Uses a non-existent tool".into(),
        parameters: vec![L3Param { name: "x".into(), description: "param".into() }],
        steps: vec![L3TemplateStep {
            tool: "ghost-tool".into(),
            method: "ghost.do".into(),
            parameters: serde_json::json!({ "value": "{{x}}" }),
        }],
    };

    let outcome = crystallise_l3(&pool, &ghost_skill, L3Source::AgentRaised { task_id: 1 })
        .await
        .expect("crystallise_l3");
    let memory_id = outcome.memory_id();
    set_skill_trust(&pool, memory_id, "user_approved")
        .await
        .expect("set_skill_trust");

    // Registry has ONLY shell-exec — ghost-tool is absent.
    let dispatcher = make_dispatcher(pool.clone(), &[ECHO_PATH.to_string()]);
    // live_tools only contains shell-exec; ghost-tool is not present.
    let live_tools = shell_exec_live_tools();

    let mut args = BTreeMap::new();
    args.insert("x".to_string(), "test".to_string());

    let report = invoke_l3(
        &pool,
        memory_id,
        &dispatcher,
        &ghost_skill,
        SkillTrust::UserApproved,
        "",
        &args,
        &live_tools,
        true,
    )
    .await;

    // Assert Refused with ghost-tool mentioned.
    let reasons = match report {
        InvokeReport::Refused { reasons } => reasons,
        other => panic!("expected Refused, got {other:?}"),
    };
    assert!(
        reasons.iter().any(|r| r.contains("ghost-tool")),
        "refusal must name 'ghost-tool'; got {reasons:?}",
    );

    // Audit: one l3.invoke_rejected, NO l3.invoked / tool:* rows.
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT actor, action FROM audit_log ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .expect("fetch audit rows");

    let rejected_count = rows.iter().filter(|(a, act)| a == "cli" && act == "l3.invoke_rejected").count();
    let invoked_count = rows.iter().filter(|(_, act)| act == "l3.invoked").count();
    let tool_count = rows.iter().filter(|(a, _)| a.starts_with("tool:")).count();

    assert_eq!(rejected_count, 1, "must have exactly 1 l3.invoke_rejected row; rows={rows:?}");
    assert_eq!(invoked_count, 0, "must have 0 l3.invoked rows; rows={rows:?}");
    assert_eq!(tool_count, 0, "must have 0 tool:* rows; rows={rows:?}");

    // Payload assertions on the rejected row.
    let rejected_payload_rows: Vec<(String, String, serde_json::Value)> = sqlx::query_as(
        "SELECT actor, action, payload FROM audit_log
         WHERE action = 'l3.invoke_rejected'
         ORDER BY id
         LIMIT 1",
    )
    .fetch_all(&pool)
    .await
    .expect("fetch rejected payload row for scenario D");
    assert_eq!(rejected_payload_rows.len(), 1, "expected one l3.invoke_rejected payload row");
    let rej_payload = &rejected_payload_rows[0].2;
    assert_eq!(
        rej_payload["memory_id"].as_i64(),
        Some(memory_id as i64),
        "l3.invoke_rejected payload.memory_id must match seeded skill; payload={rej_payload}",
    );
    let rej_reasons = rej_payload["reasons"].as_array()
        .expect("l3.invoke_rejected payload.reasons must be an array");
    assert!(!rej_reasons.is_empty(), "l3.invoke_rejected payload.reasons must be non-empty; payload={rej_payload}");

    pool.close().await;
}

// ---------------------------------------------------------------------------
// Scenario E — stop at first error
// ---------------------------------------------------------------------------

/// Two-step skill: step-1 `CAT_PATH` (NOT allowlisted → POLICY_DENIED),
/// step-2 `ECHO_PATH` (allowlisted). Must:
///   * return `Executed{outcomes len=1, steps_total=2}`;
///   * the single outcome is `Err` with `POLICY_DENIED`;
///   * `l3.invoke_outcome` audit payload: `steps_executed=1`, `steps_total=2`,
///     `any_err=true`;
///   * exactly ONE `tool:shell-exec` chokepoint row (step-2 never dispatched).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e_stop_at_first_error() {
    let Some((pool, _cluster)) = bring_up_for_scenario("l3r-e-d", "l3r-e-l", "e-stopp").await
    else {
        return;
    };

    // Two-step skill: step-1 uses CAT_PATH (not allowlisted), step-2 uses ECHO_PATH.
    let two_step_skill = L3SkillCandidate {
        name: "two_step_skill".into(),
        description: "Two steps; first should fail".into(),
        parameters: vec![L3Param { name: "p".into(), description: "path param".into() }],
        steps: vec![
            L3TemplateStep {
                tool: "shell-exec".into(),
                method: "shell.exec".into(),
                // CAT_PATH with p — NOT in the allowlist → POLICY_DENIED at dispatch
                parameters: serde_json::json!({ "argv": [CAT_PATH, "{{p}}"] }),
            },
            L3TemplateStep {
                tool: "shell-exec".into(),
                method: "shell.exec".into(),
                // ECHO_PATH with p — allowlisted, but step-2 must never run
                parameters: serde_json::json!({ "argv": [ECHO_PATH, "{{p}}"] }),
            },
        ],
    };

    let L3WriteOutcome::Inserted { memory_id } =
        crystallise_l3(&pool, &two_step_skill, L3Source::AgentRaised { task_id: 1 })
            .await
            .expect("crystallise_l3")
    else {
        panic!("expected Inserted outcome");
    };
    set_skill_trust(&pool, memory_id, "user_approved")
        .await
        .expect("set_skill_trust");

    // Registry: only ECHO_PATH is allowlisted — CAT_PATH will be POLICY_DENIED.
    // Both steps use shell-exec (→ passes the live gate); the allowlist is a
    // dispatch-time check inside the worker, not a pre-dispatch gate check.
    let dispatcher = make_dispatcher(pool.clone(), &[ECHO_PATH.to_string()]);
    let live_tools = shell_exec_live_tools();

    let mut args = BTreeMap::new();
    args.insert("p".to_string(), "/tmp/test-file".to_string());

    let report = invoke_l3(
        &pool,
        memory_id,
        &dispatcher,
        &two_step_skill,
        SkillTrust::UserApproved,
        "",
        &args,
        &live_tools,
        true,
    )
    .await;

    // Assert Executed with one outcome (stopped at first error).
    let (outcomes, steps_total) = match report {
        InvokeReport::Executed { outcomes, steps_total } => (outcomes, steps_total),
        other => panic!("expected Executed, got {other:?}"),
    };
    assert_eq!(steps_total, 2, "steps_total must reflect all declared steps");
    assert_eq!(outcomes.len(), 1, "must have exactly 1 outcome (stopped at error)");
    match &outcomes[0] {
        StepOutcome::Err { code, .. } => {
            assert_eq!(code, "POLICY_DENIED", "step-1 must be POLICY_DENIED, got {code}");
        }
        StepOutcome::Ok(v) => {
            panic!("expected step-1 to fail (POLICY_DENIED), got Ok: {v}");
        }
    }

    // Audit: one l3.invoked, one tool:shell-exec/shell.exec, one l3.invoke_outcome.
    let rows: Vec<(String, String, serde_json::Value)> = sqlx::query_as(
        "SELECT actor, action, payload FROM audit_log ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .expect("fetch audit rows");

    let invoked_count = rows.iter().filter(|(a, act, _)| a == "cli" && act == "l3.invoked").count();
    // Exactly ONE tool chokepoint row: step-1 dispatched (and denied), step-2 never reached.
    let tool_count = rows.iter().filter(|(a, act, _)| a == "tool:shell-exec" && act == "shell.exec").count();
    let outcome_rows: Vec<_> = rows.iter().filter(|(a, act, _)| a == "cli" && act == "l3.invoke_outcome").collect();

    assert_eq!(invoked_count, 1, "must have 1 l3.invoked row; rows={rows:?}");
    assert_eq!(tool_count, 1, "must have exactly 1 tool:shell-exec/shell.exec row (step-2 never dispatched); rows={rows:?}");
    assert_eq!(outcome_rows.len(), 1, "must have 1 l3.invoke_outcome row; rows={rows:?}");

    // Check the invoke_outcome payload for steps_executed=1, steps_total=2, any_err=true.
    let payload = &outcome_rows[0].2;
    let steps_executed = payload["steps_executed"].as_i64().unwrap_or(-1);
    let payload_steps_total = payload["steps_total"].as_i64().unwrap_or(-1);
    let any_err = payload["any_err"].as_bool().unwrap_or(false);
    assert_eq!(steps_executed, 1, "invoke_outcome.steps_executed must be 1; payload={payload}");
    assert_eq!(payload_steps_total, 2, "invoke_outcome.steps_total must be 2; payload={payload}");
    assert!(any_err, "invoke_outcome.any_err must be true; payload={payload}");

    pool.close().await;
}
