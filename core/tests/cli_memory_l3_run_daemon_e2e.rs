//! Real-daemon e2e for the issue #179 `memory l3 run` reroute (Opt 3).
//!
//! ## What this file pins — the #179 regression
//!
//! Pre-#179, `kastellan-cli memory l3 run <id>` rebuilt the tool registry
//! **in-process from the operator's environment**. Because the operator CLI
//! subprocess runs WITHOUT `KASTELLAN_SHELL_EXEC_BIN`, that rebuild produced a
//! registry that lacked `shell-exec`, so an otherwise-valid approved skill was
//! refused with "tool 'shell-exec' not in registry".
//!
//! Post-#179 the CLI submits an `l3_run` task on the `long` lane; the **daemon**
//! claims it and runs `invoke_l3` against ITS OWN live registry (which DOES have
//! `shell-exec`, registered from the daemon's own `KASTELLAN_SHELL_EXEC_BIN`).
//! The decisive property: the same operator env that used to fail now succeeds,
//! because execution moved into the daemon.
//!
//! Scenarios:
//!
//!  1. **`run_succeeds_against_daemon_registry_without_operator_env`** — the
//!     #179 pin. Daemon up (with `KASTELLAN_SHELL_EXEC_BIN`); CLI subprocess run
//!     with `--execute` and **no** `KASTELLAN_SHELL_EXEC_BIN`. Asserts exit 0 and
//!     stdout `"executed skill"`.
//!
//!  2. **`run_with_no_daemon_cancels_and_errors`** — PG only, NO daemon. CLI
//!     `run` should error ("daemon does not appear to be running"), exit
//!     non-zero, and leave the submitted task `cancelled`.
//!
//! ## Skip semantics
//!
//! Every daemon scenario short-circuits with a `[SKIP]` print when the host
//! lacks a supervisor, a sandbox backend, `pg_ctl`, or the workspace binaries.
//! On the DGX (live PG + bwrap) all three RUN. Cross-platform (Linux + macOS).

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::Path;

use kastellan_core::cassandra::types::{L3Param, L3SkillCandidate, L3TemplateStep};
use kastellan_core::memory::l3_crystallise::{crystallise_l3, L3Source};
use kastellan_tests_common::{
    assert_cli_failure, assert_cli_success, bring_up_daemon, bring_up_pg_cluster, cli_binary,
    cli_command, core_binary, current_username, pg_bin_dir_or_skip, seed_tool_allowlist,
    shell_exec_worker_binary, skip_if_no_supervisor, skip_if_sandbox_unavailable, spawn_inert_mock,
    unique_suffix, PgCluster,
};
#[cfg(target_os = "macos")]
use kastellan_tests_common::serial_lock;

#[cfg(target_os = "linux")]
const ECHO_PATH: &str = "/usr/bin/echo";
#[cfg(target_os = "macos")]
const ECHO_PATH: &str = "/bin/echo";

// ---------------------------------------------------------------------------
// Daemon bring-up: the inert mock LLM + `bring_up_daemon` live in
// `kastellan_tests_common::daemon` (shared with cli_memory_l3py_run_daemon_e2e).
// The decisive #179 env var — `KASTELLAN_SHELL_EXEC_BIN` — is registered on the
// *daemon* via `extra_env`; the operator CLI subprocess deliberately omits it.
// ---------------------------------------------------------------------------

/// The shell-exec worker registration the daemon (not the operator CLI) carries.
fn shell_exec_env() -> Vec<(String, String)> {
    vec![(
        "KASTELLAN_SHELL_EXEC_BIN".into(),
        shell_exec_worker_binary().to_string_lossy().into_owned(),
    )]
}

/// Build the per-test PG cluster.
fn cluster_for(suffix: &str) -> PgCluster {
    let bin_dir = pg_bin_dir_or_skip().expect("caller already short-circuited on missing pg");
    bring_up_pg_cluster(
        &bin_dir,
        "l3rd-d",
        "l3rd-l",
        &format!("kastellan-supervisor-test-pg-l3run-{suffix}"),
    )
}

/// An echo skill whose single shell-exec step is a real, allowlisted echo.
fn echo_skill() -> L3SkillCandidate {
    L3SkillCandidate {
        name: "echo_daemon".into(),
        description: "Echo a message via the daemon".into(),
        parameters: vec![L3Param {
            name: "msg".into(),
            description: "the message".into(),
        }],
        steps: vec![L3TemplateStep {
            tool: "shell-exec".into(),
            method: "shell.exec".into(),
            parameters: serde_json::json!({ "argv": [ECHO_PATH, "{{msg}}"] }),
        }],
    }
}

/// Seed → snapshot → approve an echo skill via the CLI; returns its memory id.
/// `approve` is a CLI+DB operation that reads the `registry.loaded` snapshot we
/// seed here — it needs no running daemon.
async fn seed_and_approve_echo_skill(pool: &sqlx::PgPool, data_dir: &Path, user: &str) -> i64 {
    let outcome = crystallise_l3(pool, &echo_skill(), L3Source::AgentRaised { task_id: 1 })
        .await
        .expect("crystallise_l3");
    let id = outcome.memory_id();

    // The `registry.loaded` snapshot the approval gate reads.
    seed_registry_loaded(pool, &["shell-exec"]).await;

    let approve = cli_command(data_dir, user)
        .args(["memory", "l3", "approve", &id.to_string()])
        .output()
        .expect("spawn cli memory l3 approve");
    assert!(
        approve.status.success(),
        "approve must succeed before run; stdout={}\nstderr={}",
        String::from_utf8_lossy(&approve.stdout),
        String::from_utf8_lossy(&approve.stderr),
    );
    id as i64
}

/// Seed a `registry.loaded` audit row naming `tool_names` so the approval
/// gate can verify tool existence.
async fn seed_registry_loaded(pool: &sqlx::PgPool, tool_names: &[&str]) {
    let tools: Vec<serde_json::Value> = tool_names
        .iter()
        .map(|n| serde_json::json!({ "name": n }))
        .collect();
    kastellan_db::audit::insert(
        pool,
        "core",
        kastellan_core::scheduler::audit::ACTION_REGISTRY_LOADED,
        serde_json::json!({ "tools": tools }),
    )
    .await
    .expect("seed registry.loaded");
}

/// Apply migrations + seed the shell-exec allowlist before the daemon boots
/// (build_tool_registry reads the allowlist from the DB at start).
async fn prepare_db(cluster: &PgCluster) -> sqlx::PgPool {
    kastellan_db::probe::run(
        &cluster.conn_spec,
        "test",
        "setup",
        serde_json::json!({"test": "cli_memory_l3_run_daemon_e2e"}),
    )
    .await
    .expect("probe run");
    let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");
    seed_tool_allowlist(&pool, "shell-exec", &[ECHO_PATH])
        .await
        .expect("seed shell-exec allowlist");
    pool
}

// ---------------------------------------------------------------------------
// Scenario 1 — the #179 pin: run succeeds against the daemon's own registry
// even though the operator CLI carries NO KASTELLAN_SHELL_EXEC_BIN.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_succeeds_against_daemon_registry_without_operator_env() {
    #[cfg(target_os = "macos")]
    let _serial = serial_lock();

    if skip_if_no_supervisor() {
        return;
    }
    if skip_if_sandbox_unavailable() {
        return;
    }
    if pg_bin_dir_or_skip().is_none() {
        return;
    }
    for (label, p) in &[
        ("kastellan", core_binary()),
        ("kastellan-cli", cli_binary()),
        ("kastellan-worker-shell-exec", shell_exec_worker_binary()),
    ] {
        if !p.exists() {
            eprintln!("\n[SKIP] {label} binary missing at {}\n", p.display());
            return;
        }
    }

    let suffix = unique_suffix();
    let user = current_username();
    let cluster = tokio::task::block_in_place(|| cluster_for(&suffix));

    let pool = prepare_db(&cluster).await;
    let id = seed_and_approve_echo_skill(&pool, &cluster.data_dir, &user).await;

    let mock = spawn_inert_mock().await;
    let (daemon, _daemon_guards) = bring_up_daemon(
        "l3run",
        &suffix,
        &cluster.data_dir,
        &mock.base_url,
        &user,
        shell_exec_env(),
    );

    // The operator CLI subprocess: NO KASTELLAN_SHELL_EXEC_BIN. Pre-#179 the
    // in-process rebuild refused here; post-#179 the daemon executes against
    // its own registry and this SUCCEEDS.
    let output = cli_command(&cluster.data_dir, &user)
        .args([
            "memory",
            "l3",
            "run",
            &id.to_string(),
            "--arg",
            "msg=hello-179",
            "--execute",
        ])
        .env("KASTELLAN_L3_RUN_GRACE_SECS", "30")
        // Bound Phase-2 (execution-wait) so a daemon that claims the task but
        // never NOTIFYs can't hang the suite for the 1800s default.
        .env("KASTELLAN_L3_RUN_TIMEOUT_SECS", "120")
        .output()
        .expect("spawn kastellan-cli memory l3 run --execute");

    let (stdout, stderr) =
        assert_cli_success(&output, &daemon, "run --execute (the #179 regression pin)");
    assert!(
        stdout.contains("executed skill"),
        "stdout must report 'executed skill'; got:\n{stdout}\n--- stderr ---\n{stderr}",
    );

    pool.close().await;
    drop(cluster);
}

// ---------------------------------------------------------------------------
// Scenario 2 — no daemon: the CLI cancels the submitted task and errors.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_with_no_daemon_cancels_and_errors() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(_bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };
    if !cli_binary().exists() {
        eprintln!("\n[SKIP] kastellan-cli binary missing\n");
        return;
    }

    let suffix = unique_suffix();
    let user = current_username();
    let cluster = tokio::task::block_in_place(|| cluster_for(&suffix));

    // NO daemon brought up. Approve still works — it's a CLI+DB op reading the
    // snapshot we seed.
    let pool = prepare_db(&cluster).await;
    let id = seed_and_approve_echo_skill(&pool, &cluster.data_dir, &user).await;

    // Dry-run is fine — there's no daemon to execute against anyway. A short
    // grace keeps the no-daemon detection fast.
    let output = cli_command(&cluster.data_dir, &user)
        .args(["memory", "l3", "run", &id.to_string()])
        .env("KASTELLAN_L3_RUN_GRACE_SECS", "1")
        .output()
        .expect("spawn kastellan-cli memory l3 run (no daemon)");

    let (_stdout, stderr) = assert_cli_failure(&output, "run with no daemon");
    assert!(
        stderr.contains("daemon does not appear to be running"),
        "stderr must explain the daemon is not running; got:\n{stderr}",
    );

    // The submitted l3_run task must be left `cancelled`. Exactly one task row
    // exists (the one this CLI submitted), so check its state.
    let tasks: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, state FROM tasks ORDER BY id")
            .fetch_all(&pool)
            .await
            .expect("select tasks");
    assert_eq!(
        tasks.len(),
        1,
        "expected exactly one (cancelled) task row; got {tasks:?}",
    );
    assert_eq!(
        tasks[0].1, "cancelled",
        "the submitted l3_run task must be cancelled; got {tasks:?}",
    );

    pool.close().await;
    drop(cluster);
}
