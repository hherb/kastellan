//! End-to-end smoke for [`kastellan_core::recall_assembly::PgRecallBuilder`].
//!
//! Each scenario brings up its own per-test Postgres cluster + a
//! hand-rolled `tokio::net::TcpListener` mock for `/embeddings` (same
//! pattern as `embedding_recall_e2e.rs`).
//!
//! Skips silently with `[SKIP]` lines on hosts without Postgres or a
//! reachable supervisor; `cargo test -- --nocapture` to see them.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::sync::Arc;
use std::time::Duration;

use kastellan_core::recall_assembly::{PgRecallBuilder, RecallBuilder};
use kastellan_db::memories::insert_memory;
use kastellan_llm_router::{Router, RouterConfig};
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, text_to_embedding,
    unique_suffix,
};

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime")
}

/// Spawn a `tokio::net::TcpListener` that responds to every
/// `/embeddings` POST with a fixed embedding vector.
async fn spawn_mock_embedding_listener(vec: Vec<f32>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr").to_string();
    let url = format!("http://{addr}");

    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let body = serde_json::json!({
                "object": "list",
                "data": [{"object": "embedding", "index": 0, "embedding": vec}],
                "model": "test-embedding-model",
                "usage": {"prompt_tokens": 1, "total_tokens": 1},
            });
            let payload = body.to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                payload.len(),
                payload,
            );
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf).await;
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });

    (url, handle)
}

#[test]
fn pg_recall_builder_round_trips_against_seeded_pool_and_mock_embedding() {
    if skip_if_no_supervisor() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "rae-d",
        "rae-l",
        &format!("kastellan-supervisor-test-pg-rae-{suffix}"),
    );

    rt().block_on(async {
        kastellan_db::probe::run(
            &cluster.conn_spec,
            "core",
            "startup",
            serde_json::json!({"purpose": "recall-assembly-e2e"}),
        )
        .await
        .expect("probe");

        let pool = kastellan_db::pool::connect_runtime_pool(&cluster.conn_spec)
            .await
            .expect("pool");

        // Seed 3 memories with deterministic embeddings. The query
        // embedding (computed below from the same `text_to_embedding`
        // helper) matches the second memory's text, so semantic+lexical
        // fusion should rank it first.
        let texts = ["alpha bravo charlie", "delta echo foxtrot", "golf hotel india"];
        let mut seeded_ids: Vec<i64> = Vec::new();
        for t in texts {
            let emb = text_to_embedding(t);
            let id = insert_memory(&pool, t, &serde_json::json!({}), Some(&emb))
                .await
                .expect("insert memory");
            seeded_ids.push(id);
        }

        // Query embedding = the second seeded memory's embedding (so
        // it will rank top-1 in the semantic lane); lexical lane will
        // also hit because the query string carries the exact body words.
        let query = "delta echo foxtrot";
        let query_emb = text_to_embedding(query);

        // Start mock embedding listener that returns the same vector.
        let (mock_url, _handle) = spawn_mock_embedding_listener(query_emb.clone()).await;

        // Build a Router pointed at the mock.
        let router_cfg = RouterConfig {
            local_url: mock_url.clone(),
            local_model: "test-model".into(),
            frontier_url: None,
            frontier_model: None,
            embedding_url: mock_url,
            embedding_model: "test-embedding-model".into(),
            timeout: Duration::from_millis(5000),
        };
        let router = Arc::new(Router::new(router_cfg).expect("Router::new"));

        let builder = PgRecallBuilder::new(pool.clone(), router);
        let recalled = builder.build(query).await.expect("recall builder");

        // The seeded second memory should be top-1 in fused order.
        assert!(!recalled.ids.is_empty(), "recall must return at least one row");
        assert_eq!(
            recalled.ids[0],
            seeded_ids[1],
            "seeded memory matching query must rank #1 (got ids={:?}, expected top-1={})",
            recalled.ids,
            seeded_ids[1]
        );
        assert_eq!(recalled.bodies[0], "delta echo foxtrot");
        // Pin the exact SHA-256 of the query text so a bug that
        // swapped in the empty-string sentinel (sha256_hex(b"")) or
        // any other input would fail loudly. SHA-256("delta echo foxtrot")
        // computed via sha2 = e9faf828bb31edc70d10e2bf9ac4715c83f37c3ff0b3d6cab9c2db6c2b8e8eee.
        // Keep the length pin as a redundant guard.
        assert_eq!(recalled.query_sha256.len(), 64,
                   "query_sha256 must be 64 hex chars");
        let mut hasher = sha2::Sha256::new();
        use sha2::Digest;
        hasher.update(b"delta echo foxtrot");
        let expected = format!("{:x}", hasher.finalize());
        assert_eq!(recalled.query_sha256, expected,
                   "query_sha256 must be SHA-256 of the exact query text");

        pool.close().await;
    });
}
