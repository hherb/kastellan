//! Subprocess-level pin for `hhagent-cli entities {list,show,approve,reject,merge}`.
//!
//! ## What this file pins
//!
//! Six independent scenarios, each bringing up its own per-test PG cluster
//! and spawning the real `hhagent-cli` binary as a subprocess:
//!
//! 1. **`cli_entities_list_shows_quarantined_rows`** — after seeding two
//!    quarantined entities, `entities list` exits 0, prints the header row
//!    (`ID`, `KIND`, `NAME`, `QUARANTINE`), and contains both entity names
//!    and a `TRUE` quarantine column.
//!
//! 2. **`cli_entities_show_prints_entity_detail_and_linked_memories`** —
//!    after seeding one entity + one linked memory, `entities show <id>` exits
//!    0 and prints the entity detail block including name, kind, quarantine
//!    state, and the linked memory preview.
//!
//! 3. **`cli_entities_approve_writes_audit_row`** — after seeding one
//!    quarantined entity, `entities approve <id>` exits 0, an
//!    `actor='cli' action='entities.approved'` audit row exists with the
//!    correct entity_id, and the entity's quarantine flag is flipped to FALSE.
//!
//! 4. **`cli_entities_reject_writes_audit_row_with_mentions_dropped`** —
//!    after seeding one entity + one linked memory, `entities reject <id>`
//!    exits 0 and an `actor='cli' action='entities.rejected'` audit row
//!    exists with the correct entity name and mentions_dropped count.
//!
//! 5. **`cli_entities_merge_writes_audit_row`** — after seeding three
//!    entities (keep, drop-A, drop-B), `entities merge --keep K --drop A,B`
//!    exits 0, an `actor='cli' action='entities.merged'` audit row exists
//!    with the correct kept_id, and the dropped entities are gone.
//!
//! 6. **`cli_entities_bad_args_exit_code_two`** — four parse-error sub-cases
//!    (missing ids, missing --keep, unknown sub, bad --state value) all exit
//!    with code 2 and print usage information on stderr.
//!
//! ## Skip semantics
//!
//! Each test short-circuits with a `[SKIP]` print when the host lacks
//! `pg_ctl` / a supervisor. Cross-platform (Linux + macOS). Test 6 skips
//! only if the binary cannot be found; PG is not needed for parse errors.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::process::Command;

use hhagent_db::pool::connect_runtime_pool;
use hhagent_db::probe::run as probe_run;
use hhagent_tests_common::{
    bring_up_pg_cluster, cli_binary, current_username, pg_bin_dir_or_skip,
    skip_if_no_supervisor, unique_suffix,
};

// ---------------------------------------------------------------------------
// Shared helper
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

/// Seed one quarantined entity directly via SQL. Returns the entity id.
/// Uses `person` as the kind (seeded by migration 0015).
async fn seed_quarantined_entity(
    pool: &sqlx::PgPool,
    name: &str,
) -> i64 {
    let name_norm = name.to_ascii_lowercase();
    sqlx::query_scalar(
        "INSERT INTO entities (kind, name, name_norm, quarantine) \
         VALUES ('person', $1, $2, TRUE) \
         RETURNING id",
    )
    .bind(name)
    .bind(&name_norm)
    .fetch_one(pool)
    .await
    .expect("seed_quarantined_entity: INSERT failed")
}

/// Seed one L2 memory row. Returns the memory id.
async fn seed_memory(pool: &sqlx::PgPool, body: &str) -> i64 {
    hhagent_db::memories::insert_memory(
        pool,
        body,
        &serde_json::json!({"source": "test"}),
        None,
    )
    .await
    .expect("seed_memory: insert_memory failed")
}

/// Link a memory to an entity via `memory_entities`.
async fn link(pool: &sqlx::PgPool, memory_id: i64, entity_id: i64) {
    hhagent_db::memories::link_memory_to_entities(pool, memory_id, &[entity_id])
        .await
        .expect("link: link_memory_to_entities failed");
}

// ---------------------------------------------------------------------------
// Test 1 — list shows quarantined rows + header
// ---------------------------------------------------------------------------

/// `hhagent-cli entities list` must:
///   * exit 0,
///   * print the fixed-width table header (ID, KIND, NAME, QUARANTINE, MENTIONS),
///   * contain both seeded entity names,
///   * contain "TRUE" for the quarantine column.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_entities_list_shows_quarantined_rows() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "cent-lst-d",
        "cent-lst-l",
        &format!("hhagent-postgres-cli-ent-list-{suffix}"),
    );

    probe_run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"test": "cli_entities_list_e2e"}),
    )
    .await
    .expect("probe run");

    let pool = connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");

    seed_quarantined_entity(&pool, "Alice Wonder").await;
    seed_quarantined_entity(&pool, "Sydney Smythe").await;

    let bin = cli_binary();
    let env = cli_env(&cluster.data_dir);

    let out = Command::new(&bin)
        .args(["entities", "list"])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli entities list");

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    assert!(
        out.status.success(),
        "entities list must exit 0; status={:?}\nstdout={stdout}\nstderr={stderr}",
        out.status,
    );

    // Header columns must be present.
    assert!(
        stdout.contains("ID") && stdout.contains("KIND") && stdout.contains("NAME")
            && stdout.contains("QUARANTINE"),
        "stdout must contain table header (ID / KIND / NAME / QUARANTINE); got:\n{stdout}",
    );

    // Both entity names must appear.
    assert!(
        stdout.contains("Alice Wonder"),
        "stdout must contain 'Alice Wonder'; got:\n{stdout}",
    );
    assert!(
        stdout.contains("Sydney Smythe"),
        "stdout must contain 'Sydney Smythe'; got:\n{stdout}",
    );

    // Quarantine column must show TRUE for these quarantined entities.
    assert!(
        stdout.contains("TRUE"),
        "stdout must contain 'TRUE' quarantine column; got:\n{stdout}",
    );

    drop(pool);
    drop(cluster);
}

// ---------------------------------------------------------------------------
// Test 2 — show prints entity detail + linked memories
// ---------------------------------------------------------------------------

/// `hhagent-cli entities show <id>` must:
///   * exit 0,
///   * print `kind:          person`,
///   * print `quarantine:    TRUE`,
///   * print the entity name,
///   * print the linked memory body preview.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_entities_show_prints_entity_detail_and_linked_memories() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "cent-shw-d",
        "cent-shw-l",
        &format!("hhagent-postgres-cli-ent-show-{suffix}"),
    );

    probe_run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"test": "cli_entities_show_e2e"}),
    )
    .await
    .expect("probe run");

    let pool = connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");

    let entity_id = seed_quarantined_entity(&pool, "Showme Smith").await;
    let memory_id = seed_memory(&pool, "showme body example").await;
    link(&pool, memory_id, entity_id).await;

    let bin = cli_binary();
    let env = cli_env(&cluster.data_dir);

    let out = Command::new(&bin)
        .args(["entities", "show", &entity_id.to_string()])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli entities show");

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    assert!(
        out.status.success(),
        "entities show must exit 0; status={:?}\nstdout={stdout}\nstderr={stderr}",
        out.status,
    );

    assert!(
        stdout.contains("Showme Smith"),
        "stdout must contain entity name 'Showme Smith'; got:\n{stdout}",
    );
    assert!(
        stdout.contains("kind:          person"),
        "stdout must contain 'kind:          person'; got:\n{stdout}",
    );
    assert!(
        stdout.contains("quarantine:    TRUE"),
        "stdout must contain 'quarantine:    TRUE'; got:\n{stdout}",
    );
    assert!(
        stdout.contains("showme body example"),
        "stdout must contain linked memory body 'showme body example'; got:\n{stdout}",
    );

    drop(pool);
    drop(cluster);
}

// ---------------------------------------------------------------------------
// Test 3 — approve writes audit row + flips quarantine
// ---------------------------------------------------------------------------

/// `hhagent-cli entities approve <id>` must:
///   * exit 0,
///   * write exactly one `actor='cli' action='entities.approved'` audit row
///     with `payload->>'entity_id' = '<id>'`,
///   * flip `entities.quarantine` to FALSE for that id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_entities_approve_writes_audit_row() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "cent-app-d",
        "cent-app-l",
        &format!("hhagent-postgres-cli-ent-approve-{suffix}"),
    );

    probe_run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"test": "cli_entities_approve_e2e"}),
    )
    .await
    .expect("probe run");

    let pool = connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");

    let entity_id = seed_quarantined_entity(&pool, "Approve Smith").await;

    let bin = cli_binary();
    let env = cli_env(&cluster.data_dir);

    let out = Command::new(&bin)
        .args(["entities", "approve", &entity_id.to_string()])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli entities approve");

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    assert!(
        out.status.success(),
        "entities approve must exit 0; status={:?}\nstdout={stdout}\nstderr={stderr}",
        out.status,
    );

    // One entities.approved audit row must exist with the correct entity_id.
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log \
         WHERE actor = 'cli' AND action = 'entities.approved' \
           AND payload->>'entity_id' = $1::TEXT",
    )
    .bind(entity_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("count entities.approved audit rows");

    assert_eq!(
        count, 1,
        "expected exactly 1 cli/entities.approved audit row for entity_id={entity_id}; got {count}",
    );

    // The entity's quarantine flag must now be FALSE.
    let quarantine: bool = sqlx::query_scalar(
        "SELECT quarantine FROM entities WHERE id = $1",
    )
    .bind(entity_id)
    .fetch_one(&pool)
    .await
    .expect("fetch quarantine after approve");

    assert!(
        !quarantine,
        "entities.quarantine must be FALSE after approve; got TRUE for id={entity_id}",
    );

    drop(pool);
    drop(cluster);
}

// ---------------------------------------------------------------------------
// Test 4 — reject writes audit row with mentions_dropped
// ---------------------------------------------------------------------------

/// `hhagent-cli entities reject <id>` must:
///   * exit 0,
///   * write exactly one `actor='cli' action='entities.rejected'` audit row
///     with `payload->>'name' = 'Reject Smith'` and
///     `payload->>'mentions_dropped' = '1'`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_entities_reject_writes_audit_row_with_mentions_dropped() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "cent-rej-d",
        "cent-rej-l",
        &format!("hhagent-postgres-cli-ent-reject-{suffix}"),
    );

    probe_run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"test": "cli_entities_reject_e2e"}),
    )
    .await
    .expect("probe run");

    let pool = connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");

    let entity_id = seed_quarantined_entity(&pool, "Reject Smith").await;
    let memory_id = seed_memory(&pool, "reject body example").await;
    link(&pool, memory_id, entity_id).await;

    let bin = cli_binary();
    let env = cli_env(&cluster.data_dir);

    let out = Command::new(&bin)
        .args(["entities", "reject", &entity_id.to_string()])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli entities reject");

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    assert!(
        out.status.success(),
        "entities reject must exit 0; status={:?}\nstdout={stdout}\nstderr={stderr}",
        out.status,
    );

    // One entities.rejected audit row must exist with the correct name.
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log \
         WHERE actor = 'cli' AND action = 'entities.rejected' \
           AND payload->>'name' = 'Reject Smith' \
           AND payload->>'mentions_dropped' = '1'",
    )
    .fetch_one(&pool)
    .await
    .expect("count entities.rejected audit rows");

    assert_eq!(
        count, 1,
        "expected exactly 1 cli/entities.rejected audit row for 'Reject Smith' with mentions_dropped=1; got {count}",
    );

    drop(pool);
    drop(cluster);
}

// ---------------------------------------------------------------------------
// Test 5 — merge writes audit row + drops source rows
// ---------------------------------------------------------------------------

/// `hhagent-cli entities merge --keep K --drop A,B` must:
///   * exit 0,
///   * write exactly one `actor='cli' action='entities.merged'` audit row
///     with `(payload->>'kept_id')::BIGINT = K`,
///   * delete the dropped entity rows (count = 0 for A and B).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_entities_merge_writes_audit_row() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "cent-mrg-d",
        "cent-mrg-l",
        &format!("hhagent-postgres-cli-ent-merge-{suffix}"),
    );

    probe_run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"test": "cli_entities_merge_e2e"}),
    )
    .await
    .expect("probe run");

    let pool = connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");

    // Seed three entities of the same kind so merge passes the kind-mismatch check.
    let keep_id = seed_quarantined_entity(&pool, "Merge Keep").await;
    let drop_a  = seed_quarantined_entity(&pool, "Merge Drop A").await;
    let drop_b  = seed_quarantined_entity(&pool, "Merge Drop B").await;

    let bin = cli_binary();
    let env = cli_env(&cluster.data_dir);

    let drop_arg = format!("{drop_a},{drop_b}");
    let out = Command::new(&bin)
        .args([
            "entities", "merge",
            "--keep", &keep_id.to_string(),
            "--drop", &drop_arg,
        ])
        .env_clear()
        .envs(env.clone())
        .output()
        .expect("spawn cli entities merge");

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    assert!(
        out.status.success(),
        "entities merge must exit 0; status={:?}\nstdout={stdout}\nstderr={stderr}",
        out.status,
    );

    // One entities.merged audit row must exist with the correct kept_id.
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log \
         WHERE actor = 'cli' AND action = 'entities.merged' \
           AND (payload->>'kept_id')::BIGINT = $1",
    )
    .bind(keep_id)
    .fetch_one(&pool)
    .await
    .expect("count entities.merged audit rows");

    assert_eq!(
        count, 1,
        "expected exactly 1 cli/entities.merged audit row with kept_id={keep_id}; got {count}",
    );

    // Dropped entities must no longer exist.
    let dropped_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM entities WHERE id = ANY($1::BIGINT[])",
    )
    .bind(&[drop_a, drop_b])
    .fetch_one(&pool)
    .await
    .expect("count remaining dropped entity rows");

    assert_eq!(
        dropped_count, 0,
        "dropped entities ({drop_a}, {drop_b}) must be gone after merge; found {dropped_count}",
    );

    drop(pool);
    drop(cluster);
}

// ---------------------------------------------------------------------------
// Test 6 — bad args exit with code 2
// ---------------------------------------------------------------------------

/// Parse-error sub-cases all exit with code 2 and print usage on stderr.
/// No PG cluster needed — these short-circuit before connecting.
///
/// Sub-cases:
///   a. `entities approve` (no ids) → exit 2 + stderr contains "usage:"
///   b. `entities merge --drop 1,2` (no --keep) → exit 2
///   c. `entities wat` (unknown sub) → exit 2 + stderr contains "unknown action"
///   d. `entities list --state bogus` → exit 2
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_entities_bad_args_exit_code_two() {
    let bin = cli_binary();
    if !bin.exists() {
        eprintln!("[SKIP] cli_entities_bad_args_exit_code_two: hhagent-cli binary not built at {}", bin.display());
        return;
    }

    // Sub-case a: `entities approve` with no ids.
    let out_a = Command::new(&bin)
        .args(["entities", "approve"])
        .env_clear()
        .env("HHAGENT_DATA_DIR", "/nonexistent-hhagent-test-path")
        .env("HOME", std::env::var_os("HOME").unwrap_or_default())
        .output()
        .expect("spawn cli entities approve (no ids)");

    let stderr_a = String::from_utf8_lossy(&out_a.stderr).into_owned();
    assert_eq!(
        out_a.status.code(),
        Some(2),
        "approve with no ids must exit 2; got {:?}\nstderr={stderr_a}",
        out_a.status,
    );
    assert!(
        stderr_a.to_ascii_lowercase().contains("usage:"),
        "approve with no ids must print usage on stderr; got: {stderr_a}",
    );

    // Sub-case b: `entities merge --drop 1,2` without --keep.
    let out_b = Command::new(&bin)
        .args(["entities", "merge", "--drop", "1,2"])
        .env_clear()
        .env("HHAGENT_DATA_DIR", "/nonexistent-hhagent-test-path")
        .env("HOME", std::env::var_os("HOME").unwrap_or_default())
        .output()
        .expect("spawn cli entities merge (no --keep)");

    let stderr_b = String::from_utf8_lossy(&out_b.stderr).into_owned();
    assert_eq!(
        out_b.status.code(),
        Some(2),
        "merge without --keep must exit 2; got {:?}\nstderr={stderr_b}",
        out_b.status,
    );

    // Sub-case c: `entities wat` unknown sub-action.
    let out_c = Command::new(&bin)
        .args(["entities", "wat"])
        .env_clear()
        .env("HHAGENT_DATA_DIR", "/nonexistent-hhagent-test-path")
        .env("HOME", std::env::var_os("HOME").unwrap_or_default())
        .output()
        .expect("spawn cli entities wat");

    let stderr_c = String::from_utf8_lossy(&out_c.stderr).into_owned();
    assert_eq!(
        out_c.status.code(),
        Some(2),
        "entities wat must exit 2; got {:?}\nstderr={stderr_c}",
        out_c.status,
    );
    assert!(
        stderr_c.contains("unknown action"),
        "entities wat must print 'unknown action' on stderr; got: {stderr_c}",
    );

    // Sub-case d: `entities list --state bogus`.
    let out_d = Command::new(&bin)
        .args(["entities", "list", "--state", "bogus"])
        .env_clear()
        .env("HHAGENT_DATA_DIR", "/nonexistent-hhagent-test-path")
        .env("HOME", std::env::var_os("HOME").unwrap_or_default())
        .output()
        .expect("spawn cli entities list (bad --state)");

    let stderr_d = String::from_utf8_lossy(&out_d.stderr).into_owned();
    assert_eq!(
        out_d.status.code(),
        Some(2),
        "entities list --state bogus must exit 2; got {:?}\nstderr={stderr_d}",
        out_d.status,
    );
}
