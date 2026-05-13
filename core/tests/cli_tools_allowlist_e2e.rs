//! Subprocess-level pin for `hhagent-cli tools allowlist {add,remove,list}`.
//!
//! Each subtest runs the real CLI binary against a per-test PG cluster,
//! asserts the DB row state, the audit-row shape, and the CLI exit code
//! + stdout/stderr contract.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::collections::BTreeMap;
use std::process::Command;

use hhagent_db::pool::connect_runtime_pool;
use hhagent_db::probe::run as probe_run;
use hhagent_tests_common::{
    bring_up_pg_cluster, cli_binary, current_username, pg_bin_dir_or_skip,
    skip_if_no_supervisor, unique_suffix,
};
use sqlx::Row;

/// Build the env block the CLI subprocess needs to find PG via UDS.
/// The CLI's `resolve_connect_spec` reads `HHAGENT_DATA_DIR` and
/// builds the socket path from there.
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_tools_allowlist_add_remove_list_round_trip_writes_audit_rows() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };

    let suffix = unique_suffix();
    // bring_up_pg_cluster panics on failure — no Result to unwrap.
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "ta-cli-d",
        "ta-cli-l",
        &format!("hhagent-postgres-cli-tools-allowlist-e2e-{suffix}"),
    );

    // Apply migrations.
    probe_run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"test": "cli_tools_allowlist_e2e"}),
    )
    .await
    .expect("probe run");

    let pool = connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");

    let bin = cli_binary();
    let env = cli_env(&cluster.data_dir);

    // --- 1. `tools allowlist add` happy path ----------------------------
    let out = Command::new(&bin)
        .args(["tools", "allowlist", "add", "shell-exec", "/usr/bin/echo"])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli add");
    assert!(out.status.success(), "add exit: {:?}, stderr: {}",
        out.status, String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("added"), "stdout was: {stdout}");

    // DB row landed.
    let rows: Vec<(String,)> = sqlx::query_as("SELECT argv0 FROM tool_allowlists WHERE tool = $1 ORDER BY argv0")
        .bind("shell-exec")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(rows, vec![("/usr/bin/echo".to_string(),)]);

    // --- 2. Idempotent re-add ------------------------------------------
    let out2 = Command::new(&bin)
        .args(["tools", "allowlist", "add", "shell-exec", "/usr/bin/echo"])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli add #2");
    assert!(out2.status.success());
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert!(stdout2.contains("already present"), "stdout was: {stdout2}");

    // --- 3. `tools allowlist list` -------------------------------------
    let out_l = Command::new(&bin)
        .args(["tools", "allowlist", "list", "--tool", "shell-exec"])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli list");
    assert!(out_l.status.success());
    let stdout_l = String::from_utf8_lossy(&out_l.stdout);
    assert!(stdout_l.contains("shell-exec"));
    assert!(stdout_l.contains("/usr/bin/echo"));

    // --- 4. `tools allowlist remove` -----------------------------------
    let out_r = Command::new(&bin)
        .args(["tools", "allowlist", "remove", "shell-exec", "/usr/bin/echo"])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli remove");
    assert!(out_r.status.success());
    let stdout_r = String::from_utf8_lossy(&out_r.stdout);
    assert!(stdout_r.contains("removed"), "stdout was: {stdout_r}");
    let after: Vec<(String,)> = sqlx::query_as("SELECT argv0 FROM tool_allowlists WHERE tool = $1")
        .bind("shell-exec")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(after.is_empty());

    // --- 5. Idempotent re-remove ---------------------------------------
    let out_r2 = Command::new(&bin)
        .args(["tools", "allowlist", "remove", "shell-exec", "/usr/bin/echo"])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli remove #2");
    assert!(out_r2.status.success());
    let stdout_r2 = String::from_utf8_lossy(&out_r2.stdout);
    assert!(stdout_r2.contains("not present"), "stdout was: {stdout_r2}");

    // --- 6. Validation error: relative argv0 ---------------------------
    let out_bad = Command::new(&bin)
        .args(["tools", "allowlist", "add", "shell-exec", "echo"])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli add bad");
    assert_eq!(out_bad.status.code(), Some(2), "validation error exit");
    let stderr_bad = String::from_utf8_lossy(&out_bad.stderr);
    assert!(stderr_bad.to_lowercase().contains("absolute"),
        "stderr was: {stderr_bad}");

    // --- 7. Audit multiset --------------------------------------------
    // Expected: 1 cli/tools.allowlist.add + 1 cli/tools.allowlist.remove.
    // No row for the idempotent no-ops or the validation error.
    let audit_rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT actor, action FROM audit_log WHERE actor = 'cli' ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let mut counts: BTreeMap<(String, String), usize> = BTreeMap::new();
    for r in &audit_rows {
        *counts.entry((r.0.clone(), r.1.clone())).or_default() += 1;
    }
    assert_eq!(
        counts.get(&("cli".to_string(), "tools.allowlist.add".to_string())),
        Some(&1)
    );
    assert_eq!(
        counts.get(&("cli".to_string(), "tools.allowlist.remove".to_string())),
        Some(&1)
    );

    // Payload spot-check: the add row's payload is `{tool, argv0}`.
    let row = sqlx::query("SELECT payload FROM audit_log WHERE actor = 'cli' AND action = 'tools.allowlist.add' LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let payload: serde_json::Value = row.get("payload");
    assert_eq!(payload["tool"], "shell-exec");
    assert_eq!(payload["argv0"], "/usr/bin/echo");

    drop(pool);
    drop(cluster);
}
