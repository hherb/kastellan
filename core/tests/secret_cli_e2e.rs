//! Integration tests for the `kastellan-cli secret` logic
//! (`kastellan_core::secrets::admin`). Mirrors `secret_vault_e2e.rs`:
//! per-test PG cluster via tests_common, real audit_log, MapKeyProvider
//! (no real OS keyring). Skip-as-pass on hosts without PG; on this Mac
//! set `KASTELLAN_PG_BIN_DIR` to run live.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use kastellan_core::secrets::admin::{remove_secret, store_secret, Outcome};
use kastellan_core::secrets::Vault;
use kastellan_db::secrets::{MapKeyProvider, KEY_LEN};
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, unique_suffix,
};
use sqlx::Row;

const TEST_KEY_ID: &str = "test-keyring";

fn test_key_provider() -> MapKeyProvider {
    MapKeyProvider::new(TEST_KEY_ID, [42u8; KEY_LEN])
}

async fn probe_and_pool(spec: &kastellan_db::conn::ConnectSpec) -> sqlx::PgPool {
    kastellan_db::probe::run(
        spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "secret-cli-e2e"}),
    )
    .await
    .expect("probe run");
    kastellan_db::pool::connect_runtime_pool(spec)
        .await
        .expect("connect runtime pool")
}

#[tokio::test(flavor = "multi_thread")]
async fn store_list_delete_roundtrip_with_clean_audit() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "seccli-1",
        "seccli-1-log",
        &format!("kastellan-test-seccli-1-{suffix}"),
    );
    let pool = probe_and_pool(&cluster.conn_spec).await;
    let kp = test_key_provider();

    // 1. store -> Created; round-trips through db::secrets::get + Vault.
    let o = store_secret(&pool, &kp, "matrix_pw", b"hunter2-token")
        .await
        .expect("store");
    assert_eq!(o, Outcome::Created);
    let got = kastellan_db::secrets::get(&pool, &kp, "matrix_pw", None)
        .await
        .expect("get");
    assert_eq!(got.as_slice(), b"hunter2-token");
    let r = Vault::new()
        .materialize(&pool, &kp, "matrix_pw", "test")
        .await
        .expect("materialize");
    assert!(r.as_str().starts_with("secret://"));

    // 2. store same name again -> Updated.
    let o2 = store_secret(&pool, &kp, "matrix_pw", b"hunter2-rotated")
        .await
        .expect("store2");
    assert_eq!(o2, Outcome::Updated);

    // 3. list includes it with a non-empty key_id.
    let rows = kastellan_db::secrets::list(&pool).await.expect("list");
    assert!(rows.iter().any(|s| s.name == "matrix_pw" && !s.key_id.is_empty()));

    // 4. every secret.put audit row is metadata-only (name + key_id, NO plaintext).
    let put_rows = sqlx::query(
        "SELECT payload::text AS p FROM audit_log WHERE actor='cli' AND action='secret.put'",
    )
    .fetch_all(&pool)
    .await
    .expect("audit put query");
    assert_eq!(put_rows.len(), 2, "one secret.put per store");
    for row in &put_rows {
        let p: String = row.try_get("p").unwrap();
        assert!(p.contains("matrix_pw"), "payload names the secret");
        assert!(p.contains(TEST_KEY_ID), "payload carries key_id");
        assert!(!p.contains("hunter2"), "payload MUST NOT contain plaintext");
    }

    // 5. delete -> true, then gone, then false; one secret.deleted row.
    assert!(remove_secret(&pool, "matrix_pw").await.expect("rm"));
    assert!(kastellan_db::secrets::get(&pool, &kp, "matrix_pw", None)
        .await
        .is_err());
    assert!(!remove_secret(&pool, "matrix_pw").await.expect("rm2"));
    let del_rows = sqlx::query(
        "SELECT payload::text AS p FROM audit_log WHERE actor='cli' AND action='secret.deleted'",
    )
    .fetch_all(&pool)
    .await
    .expect("audit del query");
    assert_eq!(del_rows.len(), 1);
}
