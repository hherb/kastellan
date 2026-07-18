//! Live-PG pin for the migration-0021 union-branch CHECK on `tool_allowlists`.
//!
//! Uses **raw SQL** INSERTs — deliberately bypassing the Rust validators in
//! `db::tool_allowlists` — so this exercises the SQL-layer backstop on its own.
//! That is the layer that defends against a caller holding direct INSERT on the
//! table (the runtime role does) rather than going through `add()`.
//!
//! Confirms both entry kinds are storable after `0021` (the pre-`0021` CHECK
//! rejected every domain row, which is why domain allowlists were
//! unpopulatable) and that the #459 residual-#3 port-bearing row is refused.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use kastellan_db::pool::connect_runtime_pool;
use kastellan_db::probe::run as probe_run;
use kastellan_tests_common::{bring_up_pg_cluster, pg_bin_dir_or_skip, unique_suffix};

/// Insert one row with no Rust-side validation, so only the SQL CHECK judges it.
async fn raw_insert(pool: &sqlx::PgPool, tool: &str, argv0: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO tool_allowlists (tool, argv0, created_by)
         VALUES ($1, $2, 'test') ON CONFLICT (tool, argv0) DO NOTHING",
    )
    .bind(tool)
    .bind(argv0)
    .execute(pool)
    .await
    .map(|_| ())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migration_0021_check_accepts_both_kinds_and_rejects_malformed() {
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };
    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "ta-chk-d",
        "ta-chk-l",
        &format!("kastellan-postgres-tool-allowlists-check-e2e-{suffix}"),
    );
    probe_run(
        &cluster.conn_spec,
        "core",
        "startup",
        serde_json::json!({"test": "tool_allowlists_check_e2e"}),
    )
    .await
    .expect("probe run");
    let pool = connect_runtime_pool(&cluster.conn_spec)
        .await
        .expect("runtime pool");

    // Accepted: argv0 exec path, bare domain, wildcard, IPv4, bracketed IPv6.
    for ok in [
        "/bin/echo",
        "example.org",
        ".example.org",
        "203.0.113.5",
        "[::1]",
        "[2606:4700:4700::1111]",
    ] {
        raw_insert(&pool, "web-fetch", ok)
            .await
            .unwrap_or_else(|e| panic!("{ok} should satisfy the 0021 CHECK: {e}"));
    }

    // Rejected by the CHECK: embedded port (#459 residual #3), scheme, path,
    // userinfo, unbracketed IPv6, '..' segment, empty.
    for bad in [
        "localhost:8888",
        "http://example.org",
        "example.org/search",
        "user@example.org",
        "::1",
        "../etc/passwd",
        "",
    ] {
        assert!(
            raw_insert(&pool, "web-fetch", bad).await.is_err(),
            "{bad:?} should violate the 0021 CHECK"
        );
    }
}
