//! Subprocess-level pin for `hhagent-cli relations kinds {add,remove,list}`.
//!
//! Each subtest runs the real CLI binary against a per-test PG cluster,
//! asserts the DB row state, the audit-row shape, and the CLI exit
//! code + stdout/stderr contract. Mirrors the shape of
//! [`cli_tools_allowlist_e2e`].
//!
//! Key invariants pinned end-to-end:
//!   * `add` happy-path: exit 0 + "added <kind>" stdout + DB row landed,
//!     plus one `cli/relation_kinds.add` audit row with `{kind, description}`
//!     payload (`description: null` when omitted).
//!   * `add` idempotency: re-add prints "already present" and writes
//!     no new audit row (the operator intent did not materialise).
//!   * `add --description "<text>"`: description is persisted as TEXT
//!     and echoed back in the `list` output.
//!   * `remove`: exit 0 + "removed <kind>" + one `cli/relation_kinds.remove`
//!     audit row carrying `{kind}` (no description in the remove
//!     payload — the deleted row's description is gone by the time we
//!     audit, and re-printing the operator's add-time description
//!     would conflate roles).
//!   * `remove undefined`: exit 2 + clear "fallback" stderr; DB row
//!     intact; no audit row.
//!   * `list`: exit 0 + header line + at least the 19 seed rows; an
//!     operator-added kind shows up with its description.
//!   * Validation: bad kind (NUL byte / oversize) exits 2.

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

/// Same shape as `cli_tools_allowlist_e2e::cli_env`. The CLI's
/// `resolve_connect_spec` reads `HHAGENT_DATA_DIR` and builds the
/// socket path from there. Peer auth keys off `$USER`; the cluster's
/// bootstrap role IS the OS user, so `$USER` must reach the
/// subprocess intact.
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
async fn cli_relations_kinds_add_remove_list_round_trip_writes_audit_rows() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "rk-cli-d",
        "rk-cli-l",
        &format!("hhagent-postgres-cli-relations-kinds-e2e-{suffix}"),
    );

    // Apply migrations (including 0017 which seeds relation_kinds).
    probe_run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"test": "cli_relations_kinds_e2e"}),
    )
    .await
    .expect("probe run");

    // Runtime pool for DB-state and audit-log inspection. The runtime
    // role has SELECT on `relation_kinds` + INSERT/SELECT on
    // `audit_log` — exactly what these assertions need. (The CLI
    // itself uses admin-pool internally for the writes; we don't
    // share a pool with it.)
    let pool = connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");

    let bin = cli_binary();
    let env = cli_env(&cluster.data_dir);

    // --- 1. `relations kinds add` happy path (no description) ----------
    let out = Command::new(&bin)
        .args(["relations", "kinds", "add", "supervises"])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli add");
    assert!(
        out.status.success(),
        "add exit: {:?}, stderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("added"), "stdout was: {stdout}");

    // DB row landed.
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM relation_kinds WHERE kind = $1")
            .bind("supervises")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1, "supervises must be present after add");

    // --- 2. Idempotent re-add ------------------------------------------
    let out2 = Command::new(&bin)
        .args(["relations", "kinds", "add", "supervises"])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli add #2");
    assert!(out2.status.success());
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert!(stdout2.contains("already present"), "stdout was: {stdout2}");

    // --- 3. `add` with --description -----------------------------------
    let out3 = Command::new(&bin)
        .args([
            "relations",
            "kinds",
            "add",
            "mentions",
            "--description",
            "reference relation: doc mentions entity",
        ])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli add with desc");
    assert!(out3.status.success());
    let stdout3 = String::from_utf8_lossy(&out3.stdout);
    assert!(stdout3.contains("added"));

    let desc: Option<String> =
        sqlx::query_scalar("SELECT description FROM relation_kinds WHERE kind = $1")
            .bind("mentions")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        desc.as_deref(),
        Some("reference relation: doc mentions entity"),
        "description must round-trip through CLI -> DB intact",
    );

    // --- 4. `list` shows the new kinds + headers ----------------------
    let out_l = Command::new(&bin)
        .args(["relations", "kinds", "list"])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli list");
    assert!(out_l.status.success(), "list exit: {:?}", out_l.status);
    let stdout_l = String::from_utf8_lossy(&out_l.stdout);
    assert!(stdout_l.contains("KIND"), "header missing: {stdout_l}");
    assert!(stdout_l.contains("DESCRIPTION"), "header missing: {stdout_l}");
    assert!(
        stdout_l.contains("supervises"),
        "operator-added kind missing from list: {stdout_l}",
    );
    assert!(
        stdout_l.contains("mentions"),
        "operator-added kind missing from list: {stdout_l}",
    );
    assert!(
        stdout_l.contains("reference relation: doc mentions entity"),
        "operator description missing from list: {stdout_l}",
    );
    // The seed list MUST also still be visible.
    assert!(stdout_l.contains("treats"), "seed kind missing: {stdout_l}");
    assert!(
        stdout_l.contains("undefined"),
        "undefined sentinel missing: {stdout_l}",
    );

    // --- 5. `remove undefined` is rejected with exit 2 -----------------
    let out_u = Command::new(&bin)
        .args(["relations", "kinds", "remove", "undefined"])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli remove undefined");
    assert_eq!(
        out_u.status.code(),
        Some(2),
        "remove undefined must exit 2; stderr: {}",
        String::from_utf8_lossy(&out_u.stderr),
    );
    let stderr_u = String::from_utf8_lossy(&out_u.stderr);
    // The error message includes "fallback" by construction (see
    // `RelationKindError::RemovalOfUndefinedRejected`'s Display).
    assert!(
        stderr_u.to_lowercase().contains("fallback"),
        "stderr must mention 'fallback'; got: {stderr_u}",
    );
    // The undefined row must still be present in the DB.
    let still_there: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM relation_kinds WHERE kind = 'undefined'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(still_there, 1, "undefined must survive rejected remove");

    // --- 6. `remove supervises` happy path -----------------------------
    let out_r = Command::new(&bin)
        .args(["relations", "kinds", "remove", "supervises"])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli remove");
    assert!(
        out_r.status.success(),
        "remove exit: {:?}, stderr: {}",
        out_r.status,
        String::from_utf8_lossy(&out_r.stderr),
    );
    let stdout_r = String::from_utf8_lossy(&out_r.stdout);
    assert!(stdout_r.contains("removed"), "stdout was: {stdout_r}");
    let after: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM relation_kinds WHERE kind = $1")
            .bind("supervises")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(after, 0);

    // --- 7. Idempotent re-remove ---------------------------------------
    let out_r2 = Command::new(&bin)
        .args(["relations", "kinds", "remove", "supervises"])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli remove #2");
    assert!(out_r2.status.success());
    let stdout_r2 = String::from_utf8_lossy(&out_r2.stdout);
    assert!(stdout_r2.contains("not present"), "stdout was: {stdout_r2}");

    // --- 8. Validation error: oversize kind (>64 bytes) ---------------
    let big_kind = "a".repeat(100);
    let out_bad = Command::new(&bin)
        .args(["relations", "kinds", "add", &big_kind])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli add oversize");
    assert_eq!(
        out_bad.status.code(),
        Some(2),
        "oversize kind must exit 2; stderr: {}",
        String::from_utf8_lossy(&out_bad.stderr),
    );

    // --- 8b. Validation error: oversize description -------------------
    // Issue [#111](https://github.com/hherb/hhagent/issues/111) item 3:
    // a description larger than `MAX_RELATION_KIND_DESCRIPTION_LEN`
    // (2048 bytes) is rejected at the DB layer and surfaces as exit 2
    // from the CLI. 2049 bytes is exactly one byte over the cap; the
    // rejection diagnostic carries the offending byte length.
    let big_desc = "x".repeat(2049);
    let out_bad_desc = Command::new(&bin)
        .args([
            "relations",
            "kinds",
            "add",
            "valid_kind_with_big_desc",
            "--description",
            &big_desc,
        ])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli add oversize description");
    assert_eq!(
        out_bad_desc.status.code(),
        Some(2),
        "oversize description must exit 2; stderr: {}",
        String::from_utf8_lossy(&out_bad_desc.stderr),
    );
    let stderr_bad_desc = String::from_utf8_lossy(&out_bad_desc.stderr);
    assert!(
        stderr_bad_desc.contains("2049"),
        "oversize-description stderr must echo the byte count; got: {stderr_bad_desc}",
    );
    assert!(
        stderr_bad_desc.contains("cap is 2048"),
        "oversize-description stderr must echo the cap; got: {stderr_bad_desc}",
    );

    // --- 9. Audit multiset --------------------------------------------
    // Expected: 2 cli/relation_kinds.add ('supervises' + 'mentions')
    //         + 1 cli/relation_kinds.remove ('supervises').
    // The remove-undefined rejection, idempotent re-add of 'supervises',
    // idempotent re-remove of 'supervises', oversize-kind validation,
    // and oversize-description validation all write NO audit row.
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
        counts.get(&("cli".to_string(), "relation_kinds.add".to_string())),
        Some(&2),
        "expected 2 add audit rows; full multiset: {counts:?}",
    );
    assert_eq!(
        counts.get(&("cli".to_string(), "relation_kinds.remove".to_string())),
        Some(&1),
        "expected 1 remove audit row; full multiset: {counts:?}",
    );

    // --- 10. Payload spot-check ---------------------------------------
    // The first add (no description) must serialize description as null.
    let row_supervises = sqlx::query(
        "SELECT payload FROM audit_log
         WHERE actor = 'cli' AND action = 'relation_kinds.add'
           AND payload->>'kind' = 'supervises'
         LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let payload_supervises: serde_json::Value = row_supervises.get("payload");
    assert_eq!(payload_supervises["kind"], "supervises");
    assert!(
        payload_supervises["description"].is_null(),
        "no-description add must serialize as JSON null: {payload_supervises}",
    );

    // The second add (with description) must persist it in the payload.
    let row_mentions = sqlx::query(
        "SELECT payload FROM audit_log
         WHERE actor = 'cli' AND action = 'relation_kinds.add'
           AND payload->>'kind' = 'mentions'
         LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let payload_mentions: serde_json::Value = row_mentions.get("payload");
    assert_eq!(payload_mentions["kind"], "mentions");
    assert_eq!(
        payload_mentions["description"],
        "reference relation: doc mentions entity"
    );

    // The remove row's payload is `{kind}` only (no description).
    let row_rm = sqlx::query(
        "SELECT payload FROM audit_log
         WHERE actor = 'cli' AND action = 'relation_kinds.remove'
         LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let payload_rm: serde_json::Value = row_rm.get("payload");
    assert_eq!(payload_rm["kind"], "supervises");
    assert!(
        payload_rm.get("description").is_none(),
        "remove payload must not carry a description field: {payload_rm}",
    );

    drop(pool);
    drop(cluster);
}
