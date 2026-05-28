//! End-to-end integration tests for opaque secret references (Item 31).
//!
//! Mirrors `injection_guard_e2e.rs` shape: per-test PG cluster via
//! tests_common, real shell-exec worker, real sandbox, real audit log.
//! Skip-as-pass on hosts without PG/supervisor/sandbox/worker; on this
//! Mac set `HHAGENT_PG_BIN_DIR` to run live.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::sync::Arc;
use std::time::Duration;

use hhagent_core::secrets::{
    MissingReason, RedeemFromVault, SubstituteError, Vault, VaultError,
};
use hhagent_core::tool_host::{dispatch, spawn_worker, WorkerSpec};
use hhagent_db::secrets::{MapKeyProvider, SecretsError, KEY_LEN};
use hhagent_tests_common::{
    backend, bring_up_pg_cluster, pg_bin_dir_or_skip, policy_for_shell_exec,
    shell_exec_worker_binary, skip_if_no_supervisor, skip_if_sandbox_unavailable, unique_suffix,
};
use serde_json::json;
use sqlx::Row;

// `/usr/bin/printf` exists on both Linux and macOS.
const PRINTF_PATH: &str = "/usr/bin/printf";

const TEST_KEY_ID: &str = "test-keyring";

fn test_key_provider() -> MapKeyProvider {
    MapKeyProvider::new(TEST_KEY_ID, [42u8; KEY_LEN])
}

/// Shared probe + pool setup for tests that bring up a PG cluster.
async fn probe_and_pool(conn_spec: &hhagent_db::conn::ConnectSpec) -> sqlx::PgPool {
    hhagent_db::probe::run(
        conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "secret-vault-e2e"}),
    )
    .await
    .expect("probe run");
    hhagent_db::pool::connect_runtime_pool(conn_spec)
        .await
        .expect("connect runtime pool")
}

// ── Test 1: materialize writes audit row and returns ref ──────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn materialize_writes_audit_row_and_returns_ref() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        &format!("svault-1-{suffix}"),
        &format!("svault-1-{suffix}-log"),
        &format!("hhagent-test-svault-1-{suffix}"),
    );
    let pool = probe_and_pool(&cluster.conn_spec).await;

    let kp = test_key_provider();
    hhagent_db::secrets::put(&pool, &kp, "test-secret-X", b"plaintext-XYZ", None)
        .await
        .expect("put");

    let vault = Vault::new();
    let secret_ref = vault
        .materialize(&pool, &kp, "test-secret-X", "test")
        .await
        .expect("materialize");

    assert!(
        secret_ref.as_str().starts_with("secret://"),
        "ref must begin with secret:// prefix, got {}",
        secret_ref.as_str()
    );
    assert_eq!(
        secret_ref.as_str().len(),
        "secret://".len() + 8,
        "ref must be 'secret://' + 8 hex chars"
    );

    let rows: Vec<sqlx::postgres::PgRow> = sqlx::query(
        "SELECT actor, action, payload FROM audit_log WHERE actor = 'policy' AND action = 'secret.materialized'",
    )
    .fetch_all(&pool)
    .await
    .expect("query");

    assert_eq!(rows.len(), 1, "exactly one secret.materialized row");

    let payload: serde_json::Value = rows[0].try_get("payload").expect("payload");
    assert_eq!(payload["name"], json!("test-secret-X"));
    assert_eq!(payload["ref_hash"], json!(secret_ref.ref_hash()));
    assert_eq!(payload["ttl_secs"], json!(3600));
    assert_eq!(payload["actor"], json!("test"));

    pool.close().await;
}

// ── Test 2: materialize fails when secret missing ─────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn materialize_fails_when_secret_missing() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        &format!("svault-2-{suffix}"),
        &format!("svault-2-{suffix}-log"),
        &format!("hhagent-test-svault-2-{suffix}"),
    );
    let pool = probe_and_pool(&cluster.conn_spec).await;
    let kp = test_key_provider();

    let vault = Vault::new();
    let err = vault
        .materialize(&pool, &kp, "no-such-secret", "test")
        .await
        .expect_err("must fail");

    match err {
        VaultError::Secrets(SecretsError::NotFound(name)) => {
            assert_eq!(name, "no-such-secret");
        }
        other => panic!("expected NotFound, got {other:?}"),
    }

    let row_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE actor = 'policy' AND action = 'secret.materialized'",
    )
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(row_count, 0, "no audit row written on materialize failure");

    pool.close().await;
}

// ── Test 3: redeem returns plaintext within TTL ───────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn redeem_returns_plaintext_within_ttl() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        &format!("svault-3-{suffix}"),
        &format!("svault-3-{suffix}-log"),
        &format!("hhagent-test-svault-3-{suffix}"),
    );
    let pool = probe_and_pool(&cluster.conn_spec).await;
    let kp = test_key_provider();
    hhagent_db::secrets::put(&pool, &kp, "X", b"plaintext-abc", None).await.unwrap();

    let vault = Vault::new();
    let secret_ref = vault.materialize(&pool, &kp, "X", "test").await.unwrap();

    use hhagent_core::secrets::RedeemResult;
    let result = <Vault as RedeemFromVault>::redeem(&vault, &secret_ref);
    match result {
        RedeemResult::Hit(z) => assert_eq!(z.as_slice(), b"plaintext-abc"),
        other => panic!("expected Hit, got {other:?}"),
    }

    pool.close().await;
}

// ── Test 4: redeem returns Expired past TTL ───────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn redeem_returns_expired_past_ttl() {
    if skip_if_no_supervisor() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        &format!("svault-4-{suffix}"),
        &format!("svault-4-{suffix}-log"),
        &format!("hhagent-test-svault-4-{suffix}"),
    );
    let pool = probe_and_pool(&cluster.conn_spec).await;
    let kp = test_key_provider();
    hhagent_db::secrets::put(&pool, &kp, "X", b"plaintext-exp", None).await.unwrap();

    let vault = Vault::with_ttl(Duration::from_millis(100));
    let secret_ref = vault.materialize(&pool, &kp, "X", "test").await.unwrap();

    tokio::time::sleep(Duration::from_millis(150)).await;

    use hhagent_core::secrets::RedeemResult;
    match <Vault as RedeemFromVault>::redeem(&vault, &secret_ref) {
        RedeemResult::Expired => (),
        other => panic!("expected Expired, got {other:?}"),
    }
    match <Vault as RedeemFromVault>::redeem(&vault, &secret_ref) {
        RedeemResult::NotFound => (),
        other => panic!("expected NotFound after lazy GC, got {other:?}"),
    }

    pool.close().await;
}

// ── Test 5: dispatch substitutes and writes redeemed row ──────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn dispatch_substitutes_and_writes_redeemed_row() {
    if skip_if_no_supervisor() { return; }
    if skip_if_sandbox_unavailable() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };
    let worker_bin = shell_exec_worker_binary();
    if !worker_bin.exists() {
        eprintln!("\n[SKIP] worker binary not built; run cargo build --workspace\n");
        return;
    }

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        &format!("svault-5-{suffix}"),
        &format!("svault-5-{suffix}-log"),
        &format!("hhagent-test-svault-5-{suffix}"),
    );
    let pool = probe_and_pool(&cluster.conn_spec).await;
    let kp = test_key_provider();

    // The plaintext we want the worker to receive — a unique marker so
    // the privacy-invariant test (test 7) can search the audit log for it.
    let marker = "SECRET_LEAK_MARKER_xyz789";
    hhagent_db::secrets::put(&pool, &kp, "marker-secret", marker.as_bytes(), None)
        .await
        .unwrap();

    let vault = Arc::new(Vault::new());
    let secret_ref = vault
        .materialize(&pool, &kp, "marker-secret", "test")
        .await
        .unwrap();

    // Build a shell-exec worker policy that allows /usr/bin/printf so
    // the worker can echo our substituted plaintext to stdout.
    let worker_str = worker_bin.to_string_lossy().into_owned();
    let policy = policy_for_shell_exec(&worker_bin, &[PRINTF_PATH]);
    let backend = backend();
    let spec = WorkerSpec {
        policy: &policy,
        program: &worker_str,
        args: &[],
        wall_clock_ms: Some(15_000),
    };
    let mut worker = spawn_worker(&*backend, &spec).expect("spawn shell-exec");

    let params = json!({
        "argv": [PRINTF_PATH, "%s\n", secret_ref.as_str()],
    });

    let result = dispatch(&pool, &*vault, &mut worker, "shell-exec", "shell.exec", params)
        .await
        .expect("dispatch");

    let stdout = result["stdout"].as_str().expect("stdout");
    assert!(
        stdout.contains(marker),
        "worker stdout should contain substituted plaintext: got {stdout:?}"
    );

    // Audit log: 1 materialize + 1 redeemed + 1 tool row (3 in addition
    // to the bring-up rows that probe::run writes).
    let materialize_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE actor='policy' AND action='secret.materialized'",
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(materialize_count, 1);

    let redeemed_rows: Vec<sqlx::postgres::PgRow> = sqlx::query(
        "SELECT payload FROM audit_log WHERE actor='policy' AND action='secret.redeemed'",
    ).fetch_all(&pool).await.unwrap();
    assert_eq!(redeemed_rows.len(), 1);
    let p: serde_json::Value = redeemed_rows[0].try_get("payload").unwrap();
    assert_eq!(p["tool"], json!("shell-exec"));
    assert_eq!(p["method"], json!("shell.exec"));
    assert_eq!(p["ref_hash"], json!(secret_ref.ref_hash()));

    let tool_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE actor='tool:shell-exec'",
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(tool_count, 1, "exactly one tool:shell-exec row");

    let _ = worker.close();
    pool.close().await;
}

// ── Test 6: dispatch fails closed on missing ref ──────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn dispatch_fails_closed_on_missing_ref() {
    if skip_if_no_supervisor() { return; }
    if skip_if_sandbox_unavailable() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };
    let worker_bin = shell_exec_worker_binary();
    if !worker_bin.exists() {
        eprintln!("\n[SKIP] worker binary not built; run cargo build --workspace\n");
        return;
    }

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        &format!("svault-6-{suffix}"),
        &format!("svault-6-{suffix}-log"),
        &format!("hhagent-test-svault-6-{suffix}"),
    );
    let pool = probe_and_pool(&cluster.conn_spec).await;

    // Empty vault — no refs materialized.
    let vault = Arc::new(Vault::new());

    let worker_str = worker_bin.to_string_lossy().into_owned();
    let policy = policy_for_shell_exec(&worker_bin, &[PRINTF_PATH]);
    let backend = backend();
    let spec = WorkerSpec {
        policy: &policy,
        program: &worker_str,
        args: &[],
        wall_clock_ms: Some(15_000),
    };
    let mut worker = spawn_worker(&*backend, &spec).unwrap();

    let synthetic_ref = "secret://00000000";
    let params = json!({"argv": [PRINTF_PATH, "%s\n", synthetic_ref]});

    let err = dispatch(&pool, &*vault, &mut worker, "shell-exec", "shell.exec", params)
        .await
        .expect_err("dispatch must fail");

    use hhagent_core::tool_host::ToolHostError;
    match err {
        ToolHostError::SecretRedemptionFailed(SubstituteError::MissingRef { reason, .. }) => {
            assert_eq!(reason, MissingReason::NotFound);
        }
        other => panic!("expected SecretRedemptionFailed(MissingRef(NotFound)), got {other:?}"),
    }

    // Exactly one row: redemption_failed. No tool:shell-exec row.
    let failed_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE actor='policy' AND action='secret.redemption_failed'",
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(failed_count, 1);

    let tool_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE actor='tool:shell-exec'",
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(tool_count, 0, "no tool row when fail-closed");

    let failed_payload: Vec<sqlx::postgres::PgRow> = sqlx::query(
        "SELECT payload FROM audit_log WHERE actor='policy' AND action='secret.redemption_failed'",
    ).fetch_all(&pool).await.unwrap();
    let p: serde_json::Value = failed_payload[0].try_get("payload").unwrap();
    assert_eq!(p["reason"], json!("not_found"));

    let _ = worker.close();
    pool.close().await;
}

// ── Test 7: policy rows contain no substring of redeemed plaintext ────────────

#[tokio::test(flavor = "multi_thread")]
async fn policy_rows_contain_no_substring_of_redeemed_plaintext() {
    // Privacy invariant. Mirrors injection-guard's
    // `policy_audit_row_contains_no_substring_of_blocked_body` pin
    // from commit 45627fd. The plaintext marker MUST NOT appear in
    // any `actor='policy'` row's serialized payload. Positive-
    // presence assertion: rows.is_empty() for secret.redeemed ALSO
    // fails — catches a regression where the chokepoint stops
    // emitting.
    if skip_if_no_supervisor() { return; }
    if skip_if_sandbox_unavailable() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };
    let worker_bin = shell_exec_worker_binary();
    if !worker_bin.exists() {
        eprintln!("\n[SKIP] worker binary not built; run cargo build --workspace\n");
        return;
    }

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        &format!("svault-7-{suffix}"),
        &format!("svault-7-{suffix}-log"),
        &format!("hhagent-test-svault-7-{suffix}"),
    );
    let pool = probe_and_pool(&cluster.conn_spec).await;
    let kp = test_key_provider();

    let marker = "SECRET_LEAK_MARKER_xyz789";
    hhagent_db::secrets::put(&pool, &kp, "marker-secret", marker.as_bytes(), None).await.unwrap();

    let vault = Arc::new(Vault::new());
    let secret_ref = vault.materialize(&pool, &kp, "marker-secret", "test").await.unwrap();

    let worker_str = worker_bin.to_string_lossy().into_owned();
    let policy = policy_for_shell_exec(&worker_bin, &[PRINTF_PATH]);
    let backend_obj = backend();
    let spec = WorkerSpec {
        policy: &policy,
        program: &worker_str,
        args: &[],
        wall_clock_ms: Some(15_000),
    };
    let mut worker = spawn_worker(&*backend_obj, &spec).unwrap();
    let params = json!({"argv": [PRINTF_PATH, "%s\n", secret_ref.as_str()]});
    let _ = dispatch(&pool, &*vault, &mut worker, "shell-exec", "shell.exec", params)
        .await
        .expect("dispatch");

    let policy_rows: Vec<sqlx::postgres::PgRow> = sqlx::query(
        "SELECT action, payload FROM audit_log WHERE actor='policy'",
    ).fetch_all(&pool).await.unwrap();

    let redeemed_only: Vec<&sqlx::postgres::PgRow> = policy_rows
        .iter()
        .filter(|r| {
            let action: String = r.try_get("action").unwrap_or_default();
            action == "secret.redeemed"
        })
        .collect();
    assert!(
        !redeemed_only.is_empty(),
        "positive-presence assertion: at least one secret.redeemed row must exist"
    );

    for row in &policy_rows {
        let p: serde_json::Value = row.try_get("payload").unwrap();
        let s = serde_json::to_string(&p).unwrap();
        assert!(
            !s.contains(marker),
            "privacy invariant violated — policy row payload contains the plaintext: {s}"
        );
    }

    let _ = worker.close();
    pool.close().await;
}

// ── Test 8: dispatch substitutes multiple refs in one params ──────────────────

#[tokio::test(flavor = "multi_thread")]
async fn dispatch_substitutes_multiple_refs_in_one_params() {
    if skip_if_no_supervisor() { return; }
    if skip_if_sandbox_unavailable() { return; }
    let Some(bin_dir) = pg_bin_dir_or_skip() else { return; };
    let worker_bin = shell_exec_worker_binary();
    if !worker_bin.exists() {
        eprintln!("\n[SKIP] worker binary not built; run cargo build --workspace\n");
        return;
    }

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        &format!("svault-8-{suffix}"),
        &format!("svault-8-{suffix}-log"),
        &format!("hhagent-test-svault-8-{suffix}"),
    );
    let pool = probe_and_pool(&cluster.conn_spec).await;
    let kp = test_key_provider();

    hhagent_db::secrets::put(&pool, &kp, "a", b"alpha", None).await.unwrap();
    hhagent_db::secrets::put(&pool, &kp, "b", b"bravo", None).await.unwrap();

    let vault = Arc::new(Vault::new());
    let ref_a = vault.materialize(&pool, &kp, "a", "test").await.unwrap();
    let ref_b = vault.materialize(&pool, &kp, "b", "test").await.unwrap();

    let worker_str = worker_bin.to_string_lossy().into_owned();
    let policy = policy_for_shell_exec(&worker_bin, &[PRINTF_PATH]);
    let backend_obj = backend();
    let spec = WorkerSpec {
        policy: &policy,
        program: &worker_str,
        args: &[],
        wall_clock_ms: Some(15_000),
    };
    let mut worker = spawn_worker(&*backend_obj, &spec).unwrap();

    let params = json!({"argv": [PRINTF_PATH, "%s/%s\n", ref_a.as_str(), ref_b.as_str()]});
    let result = dispatch(&pool, &*vault, &mut worker, "shell-exec", "shell.exec", params)
        .await
        .expect("dispatch");

    let stdout = result["stdout"].as_str().expect("stdout");
    assert!(stdout.contains("alpha/bravo"), "got stdout: {stdout:?}");

    let redeemed_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE actor='policy' AND action='secret.redeemed'",
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(redeemed_count, 2, "exactly two secret.redeemed rows for two distinct refs");

    let _ = worker.close();
    pool.close().await;
}
