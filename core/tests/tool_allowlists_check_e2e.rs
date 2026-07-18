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
async fn raw_insert(
    pool: &sqlx::PgPool,
    tool: &str,
    kind: &str,
    argv0: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO tool_allowlists (tool, argv0, kind, created_by)
         VALUES ($1, $2, $3, 'test') ON CONFLICT (tool, argv0) DO NOTHING",
    )
    .bind(tool)
    .bind(argv0)
    .bind(kind)
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

    // domain-kind accepts: bare domain, wildcard, IPv4, bracketed IPv6.
    for ok in [
        "example.org",
        ".example.org",
        "203.0.113.5",
        "[::1]",
        "[2606:4700:4700::1111]",
    ] {
        raw_insert(&pool, "web-fetch", "domain", ok)
            .await
            .unwrap_or_else(|e| panic!("domain {ok} should satisfy the 0021 CHECK: {e}"));
    }

    // domain-kind rejects: embedded port (#459 residual #3), scheme, path,
    // userinfo, unbracketed IPv6, '..' segment, empty, and an absolute path.
    for bad in [
        "localhost:8888",
        "http://example.org",
        "example.org/search",
        "user@example.org",
        "::1",
        "../etc/passwd",
        "",
        "/bin/echo",
    ] {
        assert!(
            raw_insert(&pool, "web-fetch", "domain", bad).await.is_err(),
            "domain {bad:?} should violate the 0021 CHECK"
        );
    }

    // argv0-kind keeps the 0009 guarantee: absolute paths only. A relative
    // `echo` must still be refused even though it is a valid *domain* shape —
    // this is exactly what a kind-blind union CHECK would have let through.
    raw_insert(&pool, "shell-exec", "argv0", "/usr/bin/echo")
        .await
        .expect("absolute argv0 should satisfy the 0021 CHECK");
    for bad in ["echo", "usr/bin/echo", "example.org", "/usr/bin/../bin/echo"] {
        assert!(
            raw_insert(&pool, "shell-exec", "argv0", bad).await.is_err(),
            "argv0 {bad:?} should violate the 0021 CHECK"
        );
    }

    // The kind column itself is constrained to the two known values.
    assert!(
        raw_insert(&pool, "web-fetch", "banana", "example.net")
            .await
            .is_err(),
        "an unknown kind should violate the 0021 CHECK"
    );
}
