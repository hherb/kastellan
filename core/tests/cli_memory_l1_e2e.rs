//! Subprocess-level pin for `hhagent-cli memory l1 {add,list,remove}`.
//!
//! ## What this file pins
//!
//! Three independent scenarios, each bringing up its own per-test PG cluster
//! and spawning the real `hhagent-cli` binary as a subprocess:
//!
//! 1. **`cli_memory_l1_add_writes_row_and_audit`** — `memory l1 add` inserts
//!    a row, emits `inserted id=N` on stdout, and writes one
//!    `actor='cli' action='l1.added'` audit row.
//!
//! 2. **`cli_memory_l1_list_shows_added_rows`** — after three `add` calls,
//!    `memory l1 list` prints the fixed-width table header (`ID`, `CREATED_AT`,
//!    `BODY`) and contains each of the three added body strings.
//!
//! 3. **`cli_memory_l1_remove_deletes_specified_id`** — `memory l1 add` then
//!    `memory l1 remove <id>` emits `removed id=N` and the row count in
//!    `memories WHERE layer = 1` drops to zero.
//!
//! ## Skip semantics
//!
//! Each test short-circuits with a `[SKIP]` print when the host lacks
//! `pg_ctl` / a supervisor. Cross-platform (Linux + macOS).

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::process::Command;

use hhagent_db::pool::connect_runtime_pool;
use hhagent_db::probe::run as probe_run;
use hhagent_tests_common::{
    bring_up_pg_cluster, cli_binary, current_username, pg_bin_dir_or_skip,
    skip_if_no_supervisor, unique_suffix,
};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Build the env block the CLI subprocess needs to find PG via UDS.
///
/// Mirrors `cli_env` in `cli_tools_allowlist_e2e.rs` verbatim: the CLI's
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
// Scenario 1 — add writes a DB row and an audit row
// ---------------------------------------------------------------------------

/// `hhagent-cli memory l1 add <body>` must:
///   * exit 0,
///   * print `inserted id=N` to stdout,
///   * write exactly one `actor='cli' action='l1.added'` row to `audit_log`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_memory_l1_add_writes_row_and_audit() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "cml1-add-d",
        "cml1-add-l",
        &format!("hhagent-postgres-cli-memory-l1-add-{suffix}"),
    );

    probe_run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"test": "cli_memory_l1_add_e2e"}),
    )
    .await
    .expect("probe run");

    let pool = connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");

    let bin = cli_binary();
    let env = cli_env(&cluster.data_dir);

    // --- Invoke `memory l1 add` ----------------------------------------
    let out = Command::new(&bin)
        .args(["memory", "l1", "add", "shell-exec /bin/ls works"])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli memory l1 add");

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    assert!(
        out.status.success(),
        "add must exit 0; status={:?}\nstdout={stdout}\nstderr={stderr}",
        out.status,
    );

    // stdout must start with `inserted id=` followed by a digit.
    assert!(
        stdout.starts_with("inserted id="),
        "stdout must start with 'inserted id='; got: {stdout:?}",
    );
    let id_part = stdout.trim_start_matches("inserted id=").trim();
    let _id: i64 = id_part
        .split_whitespace()
        .next()
        .unwrap_or("")
        .parse()
        .expect("id after 'inserted id=' must be a valid i64");

    // --- Audit row -------------------------------------------------------
    // Exactly one `actor='cli' action='l1.added'` row must exist.
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE actor = 'cli' AND action = 'l1.added'",
    )
    .fetch_one(&pool)
    .await
    .expect("count l1.added audit rows");

    assert_eq!(
        count, 1,
        "expected exactly 1 cli/l1.added audit row after add; got {count}",
    );

    drop(pool);
    drop(cluster);
}

// ---------------------------------------------------------------------------
// Scenario 2 — list shows all added rows
// ---------------------------------------------------------------------------

/// After three `add` calls, `hhagent-cli memory l1 list` must:
///   * exit 0,
///   * print the fixed-width table header (`ID`, `CREATED_AT`, `BODY`),
///   * contain each of the three body strings.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_memory_l1_list_shows_added_rows() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "cml1-lst-d",
        "cml1-lst-l",
        &format!("hhagent-postgres-cli-memory-l1-list-{suffix}"),
    );

    probe_run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"test": "cli_memory_l1_list_e2e"}),
    )
    .await
    .expect("probe run");

    let bin = cli_binary();
    let env = cli_env(&cluster.data_dir);

    // --- Add three rows --------------------------------------------------
    for body in &["alpha", "beta", "gamma"] {
        let out = Command::new(&bin)
            .args(["memory", "l1", "add", body])
            .env_clear()
            .envs(env.clone())
            .output()
            .expect("spawn cli memory l1 add");
        assert!(
            out.status.success(),
            "add '{body}' must exit 0; stderr={}",
            String::from_utf8_lossy(&out.stderr),
        );
    }

    // --- List -------------------------------------------------------------
    let out_l = Command::new(&bin)
        .args(["memory", "l1", "list"])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli memory l1 list");

    let stdout_l = String::from_utf8_lossy(&out_l.stdout).into_owned();
    let stderr_l = String::from_utf8_lossy(&out_l.stderr).into_owned();

    assert!(
        out_l.status.success(),
        "list must exit 0; status={:?}\nstdout={stdout_l}\nstderr={stderr_l}",
        out_l.status,
    );

    // Header: the CLI uses `println!("{:<8}  {:<32}  {}", "ID", "CREATED_AT", "BODY")`
    assert!(
        stdout_l.contains("ID") && stdout_l.contains("CREATED_AT") && stdout_l.contains("BODY"),
        "stdout must contain table header columns ID / CREATED_AT / BODY; got:\n{stdout_l}",
    );

    // All three body strings must appear.
    for body in &["alpha", "beta", "gamma"] {
        assert!(
            stdout_l.contains(body),
            "list stdout must contain body {body:?}; got:\n{stdout_l}",
        );
    }

    drop(cluster);
}

// ---------------------------------------------------------------------------
// Scenario 3 — remove deletes the specified row
// ---------------------------------------------------------------------------

/// `hhagent-cli memory l1 remove <id>` must:
///   * exit 0,
///   * print `removed id=N` to stdout,
///   * leave `COUNT(*) FROM memories WHERE layer = 1` at zero.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_memory_l1_remove_deletes_specified_id() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "cml1-rm-d",
        "cml1-rm-l",
        &format!("hhagent-postgres-cli-memory-l1-remove-{suffix}"),
    );

    probe_run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"test": "cli_memory_l1_remove_e2e"}),
    )
    .await
    .expect("probe run");

    let pool = connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");

    let bin = cli_binary();
    let env = cli_env(&cluster.data_dir);

    // --- Add a row and parse the returned id ------------------------------
    let out_add = Command::new(&bin)
        .args(["memory", "l1", "add", "to-remove"])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli memory l1 add");

    let stdout_add = String::from_utf8_lossy(&out_add.stdout).into_owned();
    assert!(
        out_add.status.success(),
        "add must exit 0; stderr={}",
        String::from_utf8_lossy(&out_add.stderr),
    );
    assert!(
        stdout_add.starts_with("inserted id="),
        "add stdout must start with 'inserted id='; got: {stdout_add:?}",
    );

    // Parse `inserted id=N` → i64
    let id: i64 = stdout_add
        .trim_start_matches("inserted id=")
        .trim()
        .split_whitespace()
        .next()
        .unwrap_or("")
        .parse()
        .expect("id in 'inserted id=N' must be a valid i64");

    // --- Remove the row ---------------------------------------------------
    let out_rm = Command::new(&bin)
        .args(["memory", "l1", "remove", &id.to_string()])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli memory l1 remove");

    let stdout_rm = String::from_utf8_lossy(&out_rm.stdout).into_owned();
    let stderr_rm = String::from_utf8_lossy(&out_rm.stderr).into_owned();

    assert!(
        out_rm.status.success(),
        "remove must exit 0; status={:?}\nstdout={stdout_rm}\nstderr={stderr_rm}",
        out_rm.status,
    );
    assert!(
        stdout_rm.contains(&format!("removed id={id}")),
        "remove stdout must contain 'removed id={id}'; got: {stdout_rm:?}",
    );

    // --- Confirm DB row count is now zero --------------------------------
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM memories WHERE layer = 1",
    )
    .fetch_one(&pool)
    .await
    .expect("count memories layer=1");

    assert_eq!(
        count, 0,
        "after remove, memories WHERE layer=1 must be empty; got {count} row(s)",
    );

    drop(pool);
    drop(cluster);
}
