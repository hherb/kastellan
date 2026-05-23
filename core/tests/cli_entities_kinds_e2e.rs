//! Subprocess-level pin for `hhagent-cli entities kinds {add,remove,list}`.
//!
//! Mirror of [`cli_relations_kinds_e2e`]. Same shape: per-test PG
//! cluster + spawn the real CLI binary; assert DB row state + audit
//! multiset + payload spot-check + exit-code contract.
//!
//! The two suites are byte-symmetric except for which table they
//! touch (`entity_kinds` here, `relation_kinds` there) and the seed-
//! row counts (20 entity seeds vs 19 relation seeds). Keeping both
//! suites in the tree is deliberate even though they're nearly
//! mechanical mirrors: a future contributor lifting the
//! `db::pool::connect_admin_pool` plumbing must satisfy both, and
//! a regression in one suite without the other would signal that
//! the symmetry has broken.

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

/// Same shape as `cli_relations_kinds_e2e::cli_env`.
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
async fn cli_entities_kinds_add_remove_list_round_trip_writes_audit_rows() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "ek-cli-d",
        "ek-cli-l",
        &format!("hhagent-postgres-cli-entities-kinds-e2e-{suffix}"),
    );

    // Apply migrations (including 0015 which seeds entity_kinds + 0016
    // which carves the REVOKE shape).
    probe_run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"test": "cli_entities_kinds_e2e"}),
    )
    .await
    .expect("probe run");

    // Runtime pool for DB-state and audit-log inspection.
    let pool = connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");

    let bin = cli_binary();
    let env = cli_env(&cluster.data_dir);

    // --- 1. `entities kinds add` happy path (no description) ----------
    let out = Command::new(&bin)
        .args(["entities", "kinds", "add", "research_subject"])
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

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM entity_kinds WHERE kind = $1")
        .bind("research_subject")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);

    // --- 2. Idempotent re-add ------------------------------------------
    let out2 = Command::new(&bin)
        .args(["entities", "kinds", "add", "research_subject"])
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
            "entities",
            "kinds",
            "add",
            "site_visit",
            "--description",
            "field visit to an external site",
        ])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli add with desc");
    assert!(out3.status.success());
    let stdout3 = String::from_utf8_lossy(&out3.stdout);
    assert!(stdout3.contains("added"));

    let desc: Option<String> =
        sqlx::query_scalar("SELECT description FROM entity_kinds WHERE kind = $1")
            .bind("site_visit")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        desc.as_deref(),
        Some("field visit to an external site"),
        "description must round-trip through CLI -> DB intact",
    );

    // --- 4. `list` shows the new kinds + headers ----------------------
    let out_l = Command::new(&bin)
        .args(["entities", "kinds", "list"])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli list");
    assert!(out_l.status.success(), "list exit: {:?}", out_l.status);
    let stdout_l = String::from_utf8_lossy(&out_l.stdout);
    assert!(stdout_l.contains("KIND"), "header missing: {stdout_l}");
    assert!(stdout_l.contains("DESCRIPTION"), "header missing: {stdout_l}");
    assert!(
        stdout_l.contains("research_subject"),
        "operator-added kind missing from list: {stdout_l}",
    );
    assert!(
        stdout_l.contains("site_visit"),
        "operator-added kind missing from list: {stdout_l}",
    );
    assert!(
        stdout_l.contains("field visit to an external site"),
        "operator description missing from list: {stdout_l}",
    );
    // Seed list must still be visible (one canonical-seed sample).
    assert!(stdout_l.contains("person"), "seed kind missing: {stdout_l}");
    assert!(
        stdout_l.contains("undefined"),
        "undefined sentinel missing: {stdout_l}",
    );

    // --- 5. `remove undefined` is rejected with exit 2 -----------------
    let out_u = Command::new(&bin)
        .args(["entities", "kinds", "remove", "undefined"])
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
    assert!(
        stderr_u.to_lowercase().contains("fallback"),
        "stderr must mention 'fallback'; got: {stderr_u}",
    );
    let still_there: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM entity_kinds WHERE kind = 'undefined'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(still_there, 1);

    // --- 6. `remove research_subject` happy path ----------------------
    let out_r = Command::new(&bin)
        .args(["entities", "kinds", "remove", "research_subject"])
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
    let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM entity_kinds WHERE kind = $1")
        .bind("research_subject")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(after, 0);

    // --- 7. Idempotent re-remove ---------------------------------------
    let out_r2 = Command::new(&bin)
        .args(["entities", "kinds", "remove", "research_subject"])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli remove #2");
    assert!(out_r2.status.success());
    let stdout_r2 = String::from_utf8_lossy(&out_r2.stdout);
    assert!(stdout_r2.contains("not present"), "stdout was: {stdout_r2}");

    // --- 8. Validation error: oversize kind ---------------------------
    let big_kind = "a".repeat(100);
    let out_bad = Command::new(&bin)
        .args(["entities", "kinds", "add", &big_kind])
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
    // a description larger than `MAX_ENTITY_KIND_DESCRIPTION_LEN` is
    // rejected at the DB layer and surfaces as exit 2 from the CLI.
    // 2049 bytes is exactly one byte over the cap; the rejection
    // diagnostic carries the offending byte length so the operator
    // sees how far over they were.
    let big_desc = "x".repeat(2049);
    let out_bad_desc = Command::new(&bin)
        .args([
            "entities",
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
        counts.get(&("cli".to_string(), "entity_kinds.add".to_string())),
        Some(&2),
        "expected 2 add audit rows; full multiset: {counts:?}",
    );
    assert_eq!(
        counts.get(&("cli".to_string(), "entity_kinds.remove".to_string())),
        Some(&1),
        "expected 1 remove audit row; full multiset: {counts:?}",
    );

    // --- 10. Payload spot-check ---------------------------------------
    let row_subject = sqlx::query(
        "SELECT payload FROM audit_log
         WHERE actor = 'cli' AND action = 'entity_kinds.add'
           AND payload->>'kind' = 'research_subject'
         LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let payload_subject: serde_json::Value = row_subject.get("payload");
    assert_eq!(payload_subject["kind"], "research_subject");
    assert!(
        payload_subject["description"].is_null(),
        "no-description add must serialize as JSON null: {payload_subject}",
    );

    let row_site = sqlx::query(
        "SELECT payload FROM audit_log
         WHERE actor = 'cli' AND action = 'entity_kinds.add'
           AND payload->>'kind' = 'site_visit'
         LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let payload_site: serde_json::Value = row_site.get("payload");
    assert_eq!(payload_site["kind"], "site_visit");
    assert_eq!(
        payload_site["description"],
        "field visit to an external site"
    );

    let row_rm = sqlx::query(
        "SELECT payload FROM audit_log
         WHERE actor = 'cli' AND action = 'entity_kinds.remove'
         LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let payload_rm: serde_json::Value = row_rm.get("payload");
    assert_eq!(payload_rm["kind"], "research_subject");
    assert!(
        payload_rm.get("description").is_none(),
        "remove payload must not carry a description field: {payload_rm}",
    );

    drop(pool);
    drop(cluster);
}
