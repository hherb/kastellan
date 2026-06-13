//! Subprocess-level pin for `kastellan-cli memory l3 {list,remove,approve,pin,revoke,run}`.
//!
//! ## What this file pins
//!
//! Ten independent scenarios, each bringing up its own per-test PG cluster
//! and spawning the real `kastellan-cli` binary as a subprocess:
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
//! 5. **`cli_memory_l3_approve_happy`** — seed a valid skill + a
//!    `registry.loaded` row naming its tool; approve exits 0 and a follow-up
//!    list shows `user_approved`.
//!
//! 6. **`cli_memory_l3_approve_rejects_secret_ref`** — a skill carrying a
//!    `secret://` ref is rejected: non-zero exit, trust stays `untrusted`.
//!
//! 7. **`cli_memory_l3_approve_fail_closed_no_snapshot`** — with no
//!    `registry.loaded` row, approve fails closed.
//!
//! 8. **`cli_memory_l3_revoke_after_approve`** — approve then revoke: trust
//!    cycles untrusted → user_approved → untrusted.
//!
//! 8b. **`cli_memory_l3_pin_happy`** — seed a valid skill + a `registry.loaded`
//!    row; approve it, then pin it: exit 0, stdout `pinned`, `metadata.trust ==
//!    "pinned"`, and an `l3.pinned` audit row exists.
//!
//! 8c. **`cli_memory_l3_pin_rejects_not_approved`** — seed a skill left
//!    `untrusted` (NOT approved); pin refuses (non-zero exit), trust stays
//!    `untrusted`, and an `l3.pin_rejected` audit row exists.
//!
//! ## Skip semantics
//!
//! Each test short-circuits with a `[SKIP]` print when the host lacks
//! `pg_ctl` / a supervisor. Cross-platform (Linux + macOS).

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::process::Command;

use kastellan_core::memory::l3_crystallise::{crystallise_l3, L3Source, L3WriteOutcome};
use kastellan_core::memory::l3py_crystallise::crystallise_python_skill;
use kastellan_core::cassandra::types::{L3SkillCandidate, L3Param, L3TemplateStep, PythonSkillCandidate};
use kastellan_db::pool::connect_runtime_pool;
use kastellan_db::probe::run as probe_run;
use kastellan_tests_common::{
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

/// A structurally-valid skill that ALSO carries a baked-in secret ref —
/// the writer accepts it (no secret scan); the approval gate must reject.
fn skill_with_secret_ref() -> L3SkillCandidate {
    L3SkillCandidate {
        name: "leaky_skill".into(),
        description: "carries a secret ref".into(),
        parameters: vec![L3Param { name: "repo_path".into(), description: "abs path".into() }],
        steps: vec![L3TemplateStep {
            tool: "shell-exec".into(),
            method: "shell.exec".into(),
            parameters: serde_json::json!({
                "argv": ["cat", "{{repo_path}}"],
                "token": "secret://abc12345"
            }),
        }],
    }
}

/// Seed a `registry.loaded` audit row naming `tool_names` so the CLI's
/// approval gate can verify tool existence.
async fn seed_registry_loaded(pool: &sqlx::PgPool, tool_names: &[&str]) {
    let tools: Vec<serde_json::Value> =
        tool_names.iter().map(|n| serde_json::json!({ "name": n })).collect();
    kastellan_db::audit::insert(
        pool,
        "core",
        kastellan_core::scheduler::audit::ACTION_REGISTRY_LOADED,
        serde_json::json!({ "tools": tools }),
    )
    .await
    .expect("seed registry.loaded");
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Build the env block the CLI subprocess needs to find PG via UDS.
///
/// Mirrors `cli_env` in `cli_memory_l1_e2e.rs` verbatim: the CLI's
/// `resolve_connect_spec` reads `KASTELLAN_DATA_DIR` and derives the socket
/// path from there. `HOME` and `USER` are forwarded so the process can find
/// its home directory and so that audit-row `actor` fields resolve cleanly.
fn cli_env(data_dir: &std::path::Path) -> Vec<(String, String)> {
    let mut env = vec![
        ("KASTELLAN_DATA_DIR".to_string(), data_dir.display().to_string()),
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

/// `kastellan-cli memory l3 list` must:
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
        &format!("kastellan-postgres-cli-memory-l3-list-{suffix}"),
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

/// `kastellan-cli memory l3 remove <id>` for an existing row must:
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
        &format!("kastellan-postgres-cli-memory-l3-remove-{suffix}"),
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

/// `kastellan-cli memory l3 remove 999999` against a DB with no such row must:
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
        &format!("kastellan-postgres-cli-memory-l3-missing-{suffix}"),
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

/// `kastellan-cli memory l3 remove notanumber` must:
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
        &format!("kastellan-postgres-cli-memory-l3-badarg-{suffix}"),
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

// ---------------------------------------------------------------------------
// Scenario 5 — approve happy path
// ---------------------------------------------------------------------------

/// Seed a valid skill + a registry.loaded row naming its tool; approve
/// exits 0 and a follow-up list shows `user_approved`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_memory_l3_approve_happy() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir, "cml3-app-d", "cml3-app-l",
        &format!("kastellan-postgres-cli-memory-l3-approve-{suffix}"),
    );
    probe_run(&cluster.conn_spec, "core", "startup",
        serde_json::json!({"test": "cli_memory_l3_approve_happy"})).await.expect("probe");
    let pool = connect_runtime_pool(&cluster.conn_spec).await.expect("pool");

    let outcome = crystallise_l3(&pool, &valid_skill(), L3Source::AgentRaised { task_id: 1 })
        .await.expect("crystallise_l3");
    let id = outcome.memory_id();
    seed_registry_loaded(&pool, &["shell-exec"]).await;

    let bin = cli_binary();
    let env = cli_env(&cluster.data_dir);

    let out = Command::new(&bin)
        .args(["memory", "l3", "approve", &id.to_string()])
        .env_clear().envs(env.clone()).output().expect("spawn approve");
    let so = String::from_utf8_lossy(&out.stdout).into_owned();
    let se = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "approve must exit 0; stdout={so}\nstderr={se}");
    assert!(so.contains("user_approved"), "approve stdout must confirm; got {so}");

    let list = Command::new(&bin).args(["memory", "l3", "list"])
        .env_clear().envs(env).output().expect("spawn list");
    let lo = String::from_utf8_lossy(&list.stdout).into_owned();
    assert!(lo.contains("user_approved"), "list must show user_approved; got {lo}");

    drop(pool); drop(cluster);
}

// ---------------------------------------------------------------------------
// Scenario 6 — approve rejected on a baked-in secret ref
// ---------------------------------------------------------------------------

/// A skill carrying a `secret://` ref is rejected: non-zero exit, trust
/// stays `untrusted`, an `l3.approve_rejected` audit row exists.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_memory_l3_approve_rejects_secret_ref() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir, "cml3-sec-d", "cml3-sec-l",
        &format!("kastellan-postgres-cli-memory-l3-secret-{suffix}"),
    );
    probe_run(&cluster.conn_spec, "core", "startup",
        serde_json::json!({"test": "cli_memory_l3_approve_rejects_secret_ref"})).await.expect("probe");
    let pool = connect_runtime_pool(&cluster.conn_spec).await.expect("pool");

    let outcome = crystallise_l3(&pool, &skill_with_secret_ref(), L3Source::AgentRaised { task_id: 1 })
        .await.expect("crystallise_l3");
    let id = outcome.memory_id();
    seed_registry_loaded(&pool, &["shell-exec"]).await; // tool IS known → only reason is the secret ref

    let bin = cli_binary();
    let env = cli_env(&cluster.data_dir);

    let out = Command::new(&bin)
        .args(["memory", "l3", "approve", &id.to_string()])
        .env_clear().envs(env).output().expect("spawn approve");
    let se = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(!out.status.success(), "approve must exit non-zero on a secret ref");
    assert!(se.contains("secret"), "stderr must explain the secret-ref reason; got {se}");

    // trust unchanged
    let trust: String = sqlx::query_scalar("SELECT metadata->>'trust' FROM memories WHERE id = $1")
        .bind(id).fetch_one(&pool).await.expect("fetch trust");
    assert_eq!(trust, "untrusted", "trust must NOT change on a rejected approval");

    // a rejection audit row exists
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE actor='cli' AND action='l3.approve_rejected'")
        .fetch_one(&pool).await.expect("count rejected rows");
    assert!(n >= 1, "expected an l3.approve_rejected audit row");

    drop(pool); drop(cluster);
}

// ---------------------------------------------------------------------------
// Scenario 7 — fail-closed when no registry snapshot
// ---------------------------------------------------------------------------

/// With NO registry.loaded row, approve fails closed (NoRegistrySnapshot)
/// and trust stays untrusted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_memory_l3_approve_fail_closed_no_snapshot() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir, "cml3-noc-d", "cml3-noc-l",
        &format!("kastellan-postgres-cli-memory-l3-nosnap-{suffix}"),
    );
    probe_run(&cluster.conn_spec, "core", "startup",
        serde_json::json!({"test": "cli_memory_l3_approve_fail_closed_no_snapshot"})).await.expect("probe");
    let pool = connect_runtime_pool(&cluster.conn_spec).await.expect("pool");

    let outcome = crystallise_l3(&pool, &valid_skill(), L3Source::AgentRaised { task_id: 1 })
        .await.expect("crystallise_l3");
    let id = outcome.memory_id();
    // NOTE: deliberately NOT seeding registry.loaded.

    let out = Command::new(cli_binary())
        .args(["memory", "l3", "approve", &id.to_string()])
        .env_clear().envs(cli_env(&cluster.data_dir)).output().expect("spawn approve");
    let se = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(!out.status.success(), "approve must fail closed with no snapshot");
    assert!(se.contains("registry"), "stderr must mention the missing registry snapshot; got {se}");

    let trust: String = sqlx::query_scalar("SELECT metadata->>'trust' FROM memories WHERE id = $1")
        .bind(id).fetch_one(&pool).await.expect("fetch trust");
    assert_eq!(trust, "untrusted");

    drop(pool); drop(cluster);
}

// ---------------------------------------------------------------------------
// Scenario 8 — revoke after approve
// ---------------------------------------------------------------------------

/// Approve then revoke: trust goes untrusted → user_approved → untrusted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_memory_l3_revoke_after_approve() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir, "cml3-rev-d", "cml3-rev-l",
        &format!("kastellan-postgres-cli-memory-l3-revoke-{suffix}"),
    );
    probe_run(&cluster.conn_spec, "core", "startup",
        serde_json::json!({"test": "cli_memory_l3_revoke_after_approve"})).await.expect("probe");
    let pool = connect_runtime_pool(&cluster.conn_spec).await.expect("pool");

    let outcome = crystallise_l3(&pool, &valid_skill(), L3Source::AgentRaised { task_id: 1 })
        .await.expect("crystallise_l3");
    let id = outcome.memory_id();
    seed_registry_loaded(&pool, &["shell-exec"]).await;

    let env = cli_env(&cluster.data_dir);
    let approve = Command::new(cli_binary())
        .args(["memory", "l3", "approve", &id.to_string()])
        .env_clear().envs(env.clone()).output().expect("spawn approve");
    assert!(approve.status.success(), "approve must succeed first");

    let revoke = Command::new(cli_binary())
        .args(["memory", "l3", "revoke", &id.to_string()])
        .env_clear().envs(env).output().expect("spawn revoke");
    let so = String::from_utf8_lossy(&revoke.stdout).into_owned();
    assert!(revoke.status.success(), "revoke must exit 0");
    assert!(so.contains("untrusted"), "revoke stdout must confirm; got {so}");

    let trust: String = sqlx::query_scalar("SELECT metadata->>'trust' FROM memories WHERE id = $1")
        .bind(id).fetch_one(&pool).await.expect("fetch trust");
    assert_eq!(trust, "untrusted");

    drop(pool); drop(cluster);
}

// ---------------------------------------------------------------------------
// Scenario 8b — pin happy path (user_approved → pinned)
// ---------------------------------------------------------------------------

/// Seed a valid skill + a registry.loaded row naming its tool; approve it
/// (trust → user_approved); then pin it. Pin must exit 0, stdout contains
/// `pinned`, the row's `metadata.trust == "pinned"`, and an `l3.pinned`
/// audit row exists.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_memory_l3_pin_happy() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir, "cml3-pin-d", "cml3-pin-l",
        &format!("kastellan-postgres-cli-memory-l3-pin-{suffix}"),
    );
    probe_run(&cluster.conn_spec, "core", "startup",
        serde_json::json!({"test": "cli_memory_l3_pin_happy"})).await.expect("probe");
    let pool = connect_runtime_pool(&cluster.conn_spec).await.expect("pool");

    let outcome = crystallise_l3(&pool, &valid_skill(), L3Source::AgentRaised { task_id: 1 })
        .await.expect("crystallise_l3");
    let id = outcome.memory_id();
    seed_registry_loaded(&pool, &["shell-exec"]).await;

    let bin = cli_binary();
    let env = cli_env(&cluster.data_dir);

    // --- approve first (precondition for pin) ----------------------------
    let approve = Command::new(&bin)
        .args(["memory", "l3", "approve", &id.to_string()])
        .env_clear().envs(env.clone()).output().expect("spawn approve");
    assert!(approve.status.success(), "approve must succeed first; stderr={}",
        String::from_utf8_lossy(&approve.stderr));

    // --- pin -------------------------------------------------------------
    let out = Command::new(&bin)
        .args(["memory", "l3", "pin", &id.to_string()])
        .env_clear().envs(env.clone()).output().expect("spawn pin");
    let so = String::from_utf8_lossy(&out.stdout).into_owned();
    let se = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "pin must exit 0; stdout={so}\nstderr={se}");
    assert!(so.contains("pinned"), "pin stdout must confirm 'pinned'; got {so}");

    // trust flipped to pinned
    let trust: String = sqlx::query_scalar("SELECT metadata->>'trust' FROM memories WHERE id = $1")
        .bind(id).fetch_one(&pool).await.expect("fetch trust");
    assert_eq!(trust, "pinned", "trust must be pinned after a successful pin");

    // an l3.pinned audit row exists
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE actor='cli' AND action='l3.pinned'")
        .fetch_one(&pool).await.expect("count pinned rows");
    assert!(n >= 1, "expected an l3.pinned audit row");

    drop(pool); drop(cluster);
}

// ---------------------------------------------------------------------------
// Scenario 8c — pin rejected: not user_approved (ladder enforcement)
// ---------------------------------------------------------------------------

/// Seed a skill but leave it `untrusted` (NOT approved); pin must refuse:
/// non-zero exit, trust unchanged (still `untrusted`), and an
/// `l3.pin_rejected` audit row exists.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_memory_l3_pin_rejects_not_approved() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir, "cml3-pnr-d", "cml3-pnr-l",
        &format!("kastellan-postgres-cli-memory-l3-pin-reject-{suffix}"),
    );
    probe_run(&cluster.conn_spec, "core", "startup",
        serde_json::json!({"test": "cli_memory_l3_pin_rejects_not_approved"})).await.expect("probe");
    let pool = connect_runtime_pool(&cluster.conn_spec).await.expect("pool");

    let outcome = crystallise_l3(&pool, &valid_skill(), L3Source::AgentRaised { task_id: 1 })
        .await.expect("crystallise_l3");
    let id = outcome.memory_id();
    // NOTE: deliberately NOT approving — the skill stays `untrusted`.

    let bin = cli_binary();
    let env = cli_env(&cluster.data_dir);

    let out = Command::new(&bin)
        .args(["memory", "l3", "pin", &id.to_string()])
        .env_clear().envs(env).output().expect("spawn pin");
    let se = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(!out.status.success(), "pin must refuse a non-approved skill; stderr={se}");

    // trust unchanged
    let trust: String = sqlx::query_scalar("SELECT metadata->>'trust' FROM memories WHERE id = $1")
        .bind(id).fetch_one(&pool).await.expect("fetch trust");
    assert_eq!(trust, "untrusted", "trust must NOT change on a rejected pin");

    // an l3.pin_rejected audit row exists
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE actor='cli' AND action='l3.pin_rejected'")
        .fetch_one(&pool).await.expect("count pin_rejected rows");
    assert!(n >= 1, "expected an l3.pin_rejected audit row");

    drop(pool); drop(cluster);
}

// ---------------------------------------------------------------------------
// Scenario 9 — Python skill approves WITHOUT a registry snapshot
// ---------------------------------------------------------------------------

/// Security-critical path: a `metadata.kind == "python"` skill MUST be
/// approvable via the pure `evaluate_python_approval` gate WITHOUT any
/// `registry.loaded` row in the audit log.  This is the inverse of
/// `cli_memory_l3_approve_fail_closed_no_snapshot` (a templated skill fails
/// closed without a snapshot); here a Python skill succeeds because it
/// dispatches no tools and the registry is irrelevant to its gate.
///
/// Steps:
///   1. Seed a Python skill via `crystallise_python_skill`.
///   2. Do NOT seed any `registry.loaded` audit row.
///   3. Run `memory l3 approve <id>` — must exit 0.
///   4. Re-query the DB — `metadata.trust` must be `user_approved`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_memory_l3_approve_python_skill_without_registry() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir, "cml3-pyp-d", "cml3-pyp-l",
        &format!("kastellan-postgres-cli-memory-l3-pyappr-{suffix}"),
    );
    probe_run(&cluster.conn_spec, "core", "startup",
        serde_json::json!({"test": "cli_memory_l3_approve_python_skill_without_registry"}))
        .await.expect("probe");
    let pool = connect_runtime_pool(&cluster.conn_spec).await.expect("pool");

    // --- Seed a Python skill -----------------------------------------------
    let cand = PythonSkillCandidate {
        name: "sum_stdin".into(),
        description: "Sum integers from stdin".into(),
        code: "import sys\nprint(sum(int(x) for x in sys.stdin))\n".into(),
    };
    let outcome = crystallise_python_skill(&pool, &cand, L3Source::AgentRaised { task_id: 1 })
        .await.expect("crystallise_python_skill");
    let id = outcome.memory_id();

    // NOTE: deliberately NOT seeding registry.loaded — this is the whole point.
    // A Python skill must succeed without any registry snapshot.

    let bin = cli_binary();
    let env = cli_env(&cluster.data_dir);

    // --- Approve the Python skill -----------------------------------------
    let out = Command::new(&bin)
        .args(["memory", "l3", "approve", &id.to_string()])
        .env_clear().envs(env.clone()).output().expect("spawn approve");
    let so = String::from_utf8_lossy(&out.stdout).into_owned();
    let se = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "Python skill approve must exit 0 without a registry snapshot; \
         stdout={so}\nstderr={se}",
    );
    assert!(so.contains("user_approved"), "approve stdout must confirm user_approved; got {so}");

    // --- Re-query the DB: trust must be user_approved ---------------------
    let trust: String = sqlx::query_scalar(
        "SELECT metadata->>'trust' FROM memories WHERE id = $1")
        .bind(id).fetch_one(&pool).await.expect("fetch trust");
    assert_eq!(trust, "user_approved",
        "metadata.trust must be user_approved after Python skill approval");

    drop(pool); drop(cluster);
}
