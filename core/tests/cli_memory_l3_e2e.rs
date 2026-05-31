//! Subprocess-level pin for `hhagent-cli memory l3 {list,remove}`.
//!
//! ## What this file pins
//!
//! Four independent scenarios, each bringing up its own per-test PG cluster
//! and spawning the real `hhagent-cli` binary as a subprocess:
//!
//! 1. **`cli_memory_l3_list_empty_then_populated`** — `memory l3 list` against
//!    an empty DB exits 0 with just the header; after seeding one skill via
//!    `crystallise_l3`, a second `list` exits 0 and contains a data row with
//!    `untrusted` and the skill name `summarise_repo_readme`.
//!
//! 2. **`cli_memory_l3_remove_existing`** — seed a skill, capture its
//!    `memory_id`, `memory l3 remove <id>` exits 0 with `removed id=N`, then
//!    a subsequent `list` shows no data row.
//!
//! 3. **`cli_memory_l3_remove_missing_id`** — `memory l3 remove 999999` against
//!    a DB with no such row exits 0 with `no row at layer 3 with id=999999`.
//!
//! 4. **`cli_memory_l3_remove_bad_arg`** — `memory l3 remove notanumber` exits
//!    2 and stderr contains `invalid id`.
//!
//! ## Skip semantics
//!
//! Each test short-circuits with a `[SKIP]` print when the host lacks
//! `pg_ctl` / a supervisor. Cross-platform (Linux + macOS).

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::process::Command;

use hhagent_core::memory::l3_crystallise::{crystallise_l3, L3Source, L3WriteOutcome};
use hhagent_core::cassandra::types::{L3SkillCandidate, L3Param, L3TemplateStep};
use hhagent_db::pool::connect_runtime_pool;
use hhagent_db::probe::run as probe_run;
use hhagent_tests_common::{
    bring_up_pg_cluster, cli_binary, current_username, pg_bin_dir_or_skip,
    skip_if_no_supervisor, unique_suffix,
};

// ---------------------------------------------------------------------------
// Fixture: a valid L3SkillCandidate (mirrors memory_l3_crystallise_e2e.rs)
// ---------------------------------------------------------------------------

fn valid_skill() -> L3SkillCandidate {
    L3SkillCandidate {
        name: "summarise_repo_readme".into(),
        description: "Read a repo README and summarise".into(),
        parameters: vec![L3Param {
            name: "repo_path".into(),
            description: "abs path".into(),
        }],
        steps: vec![L3TemplateStep {
            tool: "shell-exec".into(),
            method: "shell.exec".into(),
            parameters: serde_json::json!({ "argv": ["cat", "{{repo_path}}/README.md"] }),
        }],
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Build the env block the CLI subprocess needs to find PG via UDS.
///
/// Mirrors `cli_env` in `cli_memory_l1_e2e.rs` verbatim: the CLI's
/// `resolve_connect_spec` reads `HHAGENT_DATA_DIR` and derives the socket
/// path from there. `HOME` and `USER` are forwarded so the process can find
/// its home directory and so that audit-row `actor` fields resolve cleanly.
fn cli_env(data_dir: &std::path::Path) -> Vec<(String, String)> {
    let mut env = vec![
        ("HHAGENT_DATA_DIR".to_string(), data_dir.display().to_string()),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        env.push(("HOME".to_string(), home.to_string_lossy().into_owned()));
    }
    if let Some(user) = std::env::var_os("USER") {
        env.push(("USER".to_string(), user.to_string_lossy().into_owned()));
    } else {
        env.push(("USER".to_string(), current_username()));
    }
    env
}

// ---------------------------------------------------------------------------
// Scenario 1 — list: empty header, then populated data row
// ---------------------------------------------------------------------------

/// `hhagent-cli memory l3 list` must:
///   * exit 0 on an empty DB with only the header line,
///   * exit 0 after seeding one skill with a row containing `untrusted`
///     and the skill name `summarise_repo_readme`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_memory_l3_list_empty_then_populated() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "cml3-lst-d",
        "cml3-lst-l",
        &format!("hhagent-postgres-cli-memory-l3-list-{suffix}"),
    );

    probe_run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"test": "cli_memory_l3_list_e2e"}),
    )
    .await
    .expect("probe run");

    let pool = connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");

    let bin = cli_binary();
    let env = cli_env(&cluster.data_dir);

    // --- Empty list: only header -------------------------------------------
    let out_empty = Command::new(&bin)
        .args(["memory", "l3", "list"])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli memory l3 list (empty)");

    let stdout_empty = String::from_utf8_lossy(&out_empty.stdout).into_owned();
    let stderr_empty = String::from_utf8_lossy(&out_empty.stderr).into_owned();

    assert!(
        out_empty.status.success(),
        "empty list must exit 0; status={:?}\nstdout={stdout_empty}\nstderr={stderr_empty}",
        out_empty.status,
    );

    // Header columns must be present.
    assert!(
        stdout_empty.contains("ID") && stdout_empty.contains("CREATED_AT") && stdout_empty.contains("TRUST"),
        "stdout must contain table header columns ID / CREATED_AT / TRUST; got:\n{stdout_empty}",
    );

    // No data rows: neither `untrusted` nor `summarise_repo_readme` appears.
    assert!(
        !stdout_empty.contains("untrusted"),
        "empty list must not contain 'untrusted'; got:\n{stdout_empty}",
    );

    // --- Seed one skill directly against the test pool --------------------
    let outcome = crystallise_l3(
        &pool,
        &valid_skill(),
        L3Source::AgentRaised { task_id: 1 },
    )
    .await
    .expect("crystallise_l3");
    assert!(
        matches!(outcome, L3WriteOutcome::Inserted { .. }),
        "first crystallise_l3 must insert"
    );

    // --- Populated list: data row must appear ------------------------------
    let out_pop = Command::new(&bin)
        .args(["memory", "l3", "list"])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli memory l3 list (populated)");

    let stdout_pop = String::from_utf8_lossy(&out_pop.stdout).into_owned();
    let stderr_pop = String::from_utf8_lossy(&out_pop.stderr).into_owned();

    assert!(
        out_pop.status.success(),
        "populated list must exit 0; status={:?}\nstdout={stdout_pop}\nstderr={stderr_pop}",
        out_pop.status,
    );
    assert!(
        stdout_pop.contains("untrusted"),
        "populated list must contain 'untrusted'; got:\n{stdout_pop}",
    );
    assert!(
        stdout_pop.contains("summarise_repo_readme"),
        "populated list must contain skill name 'summarise_repo_readme'; got:\n{stdout_pop}",
    );

    drop(pool);
    drop(cluster);
}

// ---------------------------------------------------------------------------
// Scenario 2 — remove existing: exits 0 with `removed id=N`, row gone
// ---------------------------------------------------------------------------

/// `hhagent-cli memory l3 remove <id>` for an existing row must:
///   * exit 0,
///   * print `removed id=N` to stdout,
///   * leave the DB with no layer-3 rows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_memory_l3_remove_existing() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "cml3-rm-d",
        "cml3-rm-l",
        &format!("hhagent-postgres-cli-memory-l3-remove-{suffix}"),
    );

    probe_run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"test": "cli_memory_l3_remove_e2e"}),
    )
    .await
    .expect("probe run");

    let pool = connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");

    let bin = cli_binary();
    let env = cli_env(&cluster.data_dir);

    // --- Seed a skill and capture its memory_id ---------------------------
    let outcome = crystallise_l3(
        &pool,
        &valid_skill(),
        L3Source::AgentRaised { task_id: 1 },
    )
    .await
    .expect("crystallise_l3");
    let memory_id = outcome.memory_id();

    // --- Remove the seeded row --------------------------------------------
    let out_rm = Command::new(&bin)
        .args(["memory", "l3", "remove", &memory_id.to_string()])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli memory l3 remove");

    let stdout_rm = String::from_utf8_lossy(&out_rm.stdout).into_owned();
    let stderr_rm = String::from_utf8_lossy(&out_rm.stderr).into_owned();

    assert!(
        out_rm.status.success(),
        "remove must exit 0; status={:?}\nstdout={stdout_rm}\nstderr={stderr_rm}",
        out_rm.status,
    );
    assert!(
        stdout_rm.contains(&format!("removed id={memory_id}")),
        "remove stdout must contain 'removed id={memory_id}'; got: {stdout_rm:?}",
    );

    // --- Confirm the row is gone via list ---------------------------------
    let out_list = Command::new(&bin)
        .args(["memory", "l3", "list"])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli memory l3 list after remove");

    let stdout_list = String::from_utf8_lossy(&out_list.stdout).into_owned();

    assert!(
        out_list.status.success(),
        "list after remove must exit 0; stderr={}",
        String::from_utf8_lossy(&out_list.stderr),
    );
    assert!(
        !stdout_list.contains("summarise_repo_readme"),
        "list after remove must not contain the removed skill name; got:\n{stdout_list}",
    );

    drop(pool);
    drop(cluster);
}

// ---------------------------------------------------------------------------
// Scenario 3 — remove missing id: exits 0 with informative message
// ---------------------------------------------------------------------------

/// `hhagent-cli memory l3 remove 999999` against a DB with no such row must:
///   * exit 0,
///   * stdout contains `no row at layer 3 with id=999999`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_memory_l3_remove_missing_id() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "cml3-mis-d",
        "cml3-mis-l",
        &format!("hhagent-postgres-cli-memory-l3-missing-{suffix}"),
    );

    probe_run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"test": "cli_memory_l3_remove_missing_e2e"}),
    )
    .await
    .expect("probe run");

    let bin = cli_binary();
    let env = cli_env(&cluster.data_dir);

    // --- Remove a non-existent id -----------------------------------------
    let out = Command::new(&bin)
        .args(["memory", "l3", "remove", "999999"])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli memory l3 remove 999999");

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    assert!(
        out.status.success(),
        "remove of missing id must exit 0; status={:?}\nstdout={stdout}\nstderr={stderr}",
        out.status,
    );
    assert!(
        stdout.contains("no row at layer 3 with id=999999"),
        "stdout must contain 'no row at layer 3 with id=999999'; got: {stdout:?}",
    );

    drop(cluster);
}

// ---------------------------------------------------------------------------
// Scenario 4 — remove bad arg: exits 2 with `invalid id` on stderr
// ---------------------------------------------------------------------------

/// `hhagent-cli memory l3 remove notanumber` must:
///   * exit 2,
///   * stderr contains `invalid id`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_memory_l3_remove_bad_arg() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "cml3-bad-d",
        "cml3-bad-l",
        &format!("hhagent-postgres-cli-memory-l3-badarg-{suffix}"),
    );

    probe_run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"test": "cli_memory_l3_remove_badarg_e2e"}),
    )
    .await
    .expect("probe run");

    let bin = cli_binary();
    let env = cli_env(&cluster.data_dir);

    // --- Pass a non-integer id --------------------------------------------
    let out = Command::new(&bin)
        .args(["memory", "l3", "remove", "notanumber"])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli memory l3 remove notanumber");

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    assert_eq!(
        out.status.code(),
        Some(2),
        "remove with bad arg must exit 2; status={:?}\nstdout={stdout}\nstderr={stderr}",
        out.status,
    );
    assert!(
        stderr.contains("invalid id"),
        "stderr must contain 'invalid id'; got: {stderr:?}",
    );

    drop(cluster);
}
