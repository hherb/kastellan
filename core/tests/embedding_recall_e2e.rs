//! End-to-end test for `core::memory::embed_query` and the full
//! free-text-to-recall flow.
//!
//! Bring-up scaffolding + deterministic embedding seed now live in
//! `hhagent-tests-common` (issue #15). The mock LLM TCP listener
//! remains in-file because its `ServedRequest` shape varies by site
//! (path field here; absent in other mocks); folding it into the
//! shared crate would force a single shape on every consumer.
//!
//! Four cases:
//!
//!   1. Happy path — mock returns 1024-float vector; `embed_query`
//!      returns `Ok(Vec<f32>)` of length 1024.
//!   2. Audit row written — after `embed_query` returns Ok, the
//!      `audit_log` table has exactly one row with
//!      `actor='llm:router' action='embed'`, payload shape matching
//!      `build_embed_audit_payload` invariants.
//!   3. Dim mismatch — mock returns 512-float vector; `embed_query`
//!      returns `Err(MemoryError::EmbeddingDimMismatch)`; `audit_log`
//!      has only the probe bring-up row (no llm:router row).
//!   4. Full text-to-recall flow — seed 3 memories with deterministic
//!      embeddings; mock returns the embedding for memory A;
//!      `embed_query("alpha bravo charlie")` → recall(SEMANTIC_ONLY)
//!      → top-1 is memory A; one `actor='llm:router'` row in audit
//!      log.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::time::Duration;

use hhagent_core::memory::{embed_query, recall, MemoryError, RecallModes, RecallParams};
use hhagent_db::memories::{insert_memory, EMBEDDING_DIM};
use hhagent_llm_router::{Router, RouterConfig};
use hhagent_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor, text_to_embedding,
    unique_suffix,
};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

// ---- Mock LLM HTTP listener (site-specific shape) ---------------------

#[derive(Debug, Clone)]
struct ServedRequest {
    path: String,
    body: String,
}

/// Pin the wire shape of the embedding request the router sent: the path
/// must be `/embeddings`, and the JSON body must carry the expected
/// `model` plus a single-element `input` array containing `text`.
fn assert_embedding_request(served: &ServedRequest, text: &str) {
    assert_eq!(
        served.path, "/embeddings",
        "router must POST to /embeddings, got {:?}",
        served.path,
    );
    let body: serde_json::Value =
        serde_json::from_str(&served.body).expect("served request body is JSON");
    assert_eq!(
        body["model"], "embedding-test",
        "model mismatch in request body: {body}",
    );
    assert_eq!(
        body["input"],
        serde_json::json!([text]),
        "input must be a single-element array carrying the caller's text: {body}",
    );
}

#[derive(Debug, Clone)]
struct CannedResponse {
    status_line: &'static str,
    body: String,
}

impl CannedResponse {
    fn ok_json(body: impl Into<String>) -> Self {
        Self {
            status_line: "HTTP/1.1 200 OK",
            body: body.into(),
        }
    }
}

async fn spawn_one_shot_mock(canned: CannedResponse) -> (String, oneshot::Receiver<ServedRequest>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let port = listener.local_addr().unwrap().port();
    let base_url = format!("http://127.0.0.1:{port}");

    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let (mut sock, _peer) = match listener.accept().await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("mock accept failed: {e}");
                return;
            }
        };
        let mut buf = Vec::with_capacity(4096);
        let mut tmp = [0u8; 1024];
        loop {
            let n = sock.read(&mut tmp).await.expect("read socket");
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(headers_end) = find_double_crlf(&buf) {
                let header_str =
                    std::str::from_utf8(&buf[..headers_end]).expect("headers are utf-8");
                let content_length = header_content_length(header_str).unwrap_or(0);
                let body_start = headers_end + 4;
                let total_needed = body_start + content_length;
                if buf.len() >= total_needed {
                    let request_line =
                        header_str.lines().next().unwrap_or("").to_string();
                    let path = request_line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("")
                        .to_string();
                    let body = String::from_utf8(buf[body_start..total_needed].to_vec())
                        .expect("body is utf-8");
                    let _ = tx.send(ServedRequest { path, body });
                    let resp = format!(
                        "{status}\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
                        status = canned.status_line,
                        len = canned.body.len(),
                        body = canned.body,
                    );
                    sock.write_all(resp.as_bytes())
                        .await
                        .expect("write response");
                    sock.flush().await.expect("flush");
                    let _ = sock.shutdown().await;
                    break;
                }
            }
            if buf.len() > 1 << 20 {
                break;
            }
        }
    });
    (base_url, rx)
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    if buf.len() < 4 {
        return None;
    }
    for i in 0..(buf.len() - 3) {
        if &buf[i..i + 4] == b"\r\n\r\n" {
            return Some(i);
        }
    }
    None
}

fn header_content_length(headers: &str) -> Option<usize> {
    for line in headers.lines() {
        let mut parts = line.splitn(2, ':');
        let Some(name) = parts.next() else { continue };
        let Some(value) = parts.next() else { continue };
        if name.trim().eq_ignore_ascii_case("content-length") {
            return value.trim().parse().ok();
        }
    }
    None
}

fn build_router_pointing_at(base_url: &str) -> Router {
    let cfg = RouterConfig {
        local_url: base_url.to_string(),
        local_model: "local-default".into(),
        embedding_url: base_url.to_string(),
        embedding_model: "embedding-test".into(),
        frontier_url: None,
        frontier_model: None,
        timeout: Duration::from_secs(2),
    };
    Router::new(cfg).expect("build router")
}

async fn setup_pg(conn_spec: &hhagent_db::conn::ConnectSpec) -> sqlx::PgPool {
    hhagent_db::probe::run(
        conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "embed-recall"}),
    )
    .await
    .expect("probe run");

    hhagent_db::pool::connect_runtime_pool(conn_spec)
        .await
        .expect("connect runtime pool")
}

fn make_short_vec(n: usize) -> Vec<f32> {
    (0..n).map(|i| (i as f32) / (n as f32)).collect()
}

// ====================================================================
// Test 1 — happy path
// ====================================================================

#[test]
fn embed_query_returns_vec_of_expected_dim() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "embr-d",
        "embr-l",
        &format!("hhagent-supervisor-test-pg-embr-{suffix}"),
    );

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build tokio runtime");

    rt.block_on(async {
        let pool = setup_pg(&cluster.conn_spec).await;

        let emb_vec = text_to_embedding("hello");
        assert_eq!(emb_vec.len(), EMBEDDING_DIM);

        let canned = serde_json::json!({
            "data": [{"index": 0, "embedding": emb_vec}],
            "model": "embedding-test"
        });
        let (base_url, served_rx) =
            spawn_one_shot_mock(CannedResponse::ok_json(canned.to_string())).await;
        let router = build_router_pointing_at(&base_url);

        let result = embed_query(&pool, &router, "hello")
            .await
            .expect("embed_query ok");
        assert_eq!(
            result.len(),
            EMBEDDING_DIM,
            "embed_query must return a vector of length {EMBEDDING_DIM}, got {}",
            result.len()
        );

        let served = served_rx.await.expect("mock recorded request");
        assert_embedding_request(&served, "hello");

        pool.close().await;
    });
}

// ====================================================================
// Test 2 — audit row written with privacy-safe payload
// ====================================================================

#[test]
fn embed_query_writes_llm_router_audit_row() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "embr-d",
        "embr-l",
        &format!("hhagent-supervisor-test-pg-embr-{suffix}"),
    );

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build tokio runtime");

    rt.block_on(async {
        let pool = setup_pg(&cluster.conn_spec).await;

        let emb_vec = text_to_embedding("alpha bravo");
        let canned = serde_json::json!({
            "data": [{"index": 0, "embedding": emb_vec}],
            "model": "embedding-test"
        });
        let (base_url, served_rx) =
            spawn_one_shot_mock(CannedResponse::ok_json(canned.to_string())).await;
        let router = build_router_pointing_at(&base_url);

        embed_query(&pool, &router, "alpha bravo")
            .await
            .expect("embed_query ok");

        let served = served_rx.await.expect("mock recorded request");
        assert_embedding_request(&served, "alpha bravo");

        let rows: Vec<(String, String, serde_json::Value)> = sqlx::query_as(
            "SELECT actor, action, payload FROM audit_log \
             WHERE actor = 'llm:router' ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .expect("query audit_log");

        assert_eq!(rows.len(), 1, "exactly one llm:router row");
        let (actor, action, payload) = &rows[0];
        assert_eq!(actor, "llm:router");
        assert_eq!(action, "embed");
        assert_eq!(payload["model"], "embedding-test");
        assert_eq!(payload["n_texts"], 1);
        assert_eq!(payload["dim"], 1024);
        assert_eq!(payload["backend"], "local");
        assert!(
            payload["latency_ms"].is_u64(),
            "latency_ms must be a JSON u64: {payload:?}"
        );

        // Privacy invariants — the text and embedding must not be in the row.
        let payload_str = serde_json::to_string(payload).unwrap();
        assert!(!payload_str.contains("\"input\""), "input leaked: {payload_str}");
        assert!(!payload_str.contains("alpha"), "user text leaked: {payload_str}");
        assert!(
            !payload_str.contains("\"embedding\""),
            "embedding leaked: {payload_str}"
        );

        pool.close().await;
    });
}

// ====================================================================
// Test 3 — dim mismatch surfaces typed error; no audit row written
// ====================================================================

#[test]
fn embed_query_dim_mismatch_surfaces_typed_error_and_writes_no_audit_row() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "embr-d",
        "embr-l",
        &format!("hhagent-supervisor-test-pg-embr-{suffix}"),
    );

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build tokio runtime");

    rt.block_on(async {
        let pool = setup_pg(&cluster.conn_spec).await;

        // Mock returns a 512-float vector — wrong dim.
        let short_vec = make_short_vec(512);
        let canned = serde_json::json!({
            "data": [{"index": 0, "embedding": short_vec}],
            "model": "embedding-test"
        });
        let (base_url, served_rx) =
            spawn_one_shot_mock(CannedResponse::ok_json(canned.to_string())).await;
        let router = build_router_pointing_at(&base_url);

        let err = embed_query(&pool, &router, "hello")
            .await
            .expect_err("dim must mismatch");

        // The HTTP request was sent and answered before the dim-check
        // ran on the client; pin the wire shape regardless.
        let served = served_rx.await.expect("mock recorded request");
        assert_embedding_request(&served, "hello");
        match err {
            MemoryError::EmbeddingDimMismatch {
                expected,
                actual,
                model,
            } => {
                assert_eq!(expected, 1024);
                assert_eq!(actual, 512);
                assert_eq!(model, "embedding-test");
            }
            other => panic!("expected EmbeddingDimMismatch, got {other:?}"),
        }

        // No audit row for the failure (chokepoint precedent).
        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM audit_log WHERE actor = 'llm:router'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(n, 0, "failure must not write audit row");

        pool.close().await;
    });
}

// ====================================================================
// Test 4 — full text-to-recall flow
// ====================================================================

#[test]
fn full_text_to_recall_flow_uses_embed_query_then_recall() {
    if skip_if_no_supervisor() {
        return;
    }
    let bin_dir = match pg_bin_dir_or_skip() {
        Some(d) => d,
        None => return,
    };

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "embr-d",
        "embr-l",
        &format!("hhagent-supervisor-test-pg-embr-{suffix}"),
    );

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build tokio runtime");

    rt.block_on(async {
        let pool = setup_pg(&cluster.conn_spec).await;

        const BODY_A: &str = "alpha bravo charlie";
        const BODY_B: &str = "delta echo foxtrot";
        const BODY_C: &str = "golf hotel india";

        // Seed 3 memories with deterministic embeddings.
        let emb_a = text_to_embedding(BODY_A);
        insert_memory(&pool, BODY_A, &serde_json::json!({}), Some(&emb_a))
            .await
            .expect("insert A");
        let emb_b = text_to_embedding(BODY_B);
        insert_memory(&pool, BODY_B, &serde_json::json!({}), Some(&emb_b))
            .await
            .expect("insert B");
        let emb_c = text_to_embedding(BODY_C);
        insert_memory(&pool, BODY_C, &serde_json::json!({}), Some(&emb_c))
            .await
            .expect("insert C");

        // Mock returns the embedding for BODY_A — same SHA-256-seeded vector.
        let canned = serde_json::json!({
            "data": [{"index": 0, "embedding": emb_a.clone()}],
            "model": "embedding-test"
        });
        let (base_url, served_rx) =
            spawn_one_shot_mock(CannedResponse::ok_json(canned.to_string())).await;
        let router = build_router_pointing_at(&base_url);

        // embed_query the matching text.
        let emb = embed_query(&pool, &router, BODY_A).await.expect("embed");
        assert_eq!(emb.len(), EMBEDDING_DIM);

        let served = served_rx.await.expect("mock recorded request");
        assert_embedding_request(&served, BODY_A);

        // Plug into recall — semantic-only lane.
        let mems = recall(
            &pool,
            &RecallParams {
                query_text: None,
                query_embedding: Some(&emb),
                seed_entity_ids: None,
                k: 3,
                modes: RecallModes::SEMANTIC_ONLY,
            },
        )
        .await
        .expect("recall");
        assert!(!mems.is_empty(), "recall returned nothing");
        assert_eq!(mems[0].body, BODY_A, "top-1 must be A: {mems:?}");

        // Audit log has the llm:router row.
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM audit_log \
             WHERE actor = 'llm:router' AND action = 'embed'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(n, 1);

        pool.close().await;
    });
}
