//! End-to-end test for `core::memory::embed_query` and the full
//! free-text-to-recall flow.
//!
//! Per-test Postgres cluster (8th duplication site; issue #15 tracks
//! the hoist). Per-test hand-rolled TCP mock for `/embeddings`.
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
//!
//! Skips silently with `[SKIP]` lines on hosts without Postgres or a
//! reachable supervisor; `cargo test -- --nocapture` to see them.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use hhagent_db::memories::{insert_memory, EMBEDDING_DIM};
use hhagent_db::{
    build_initdb_argv, build_postgresql_auto_conf, default_pg_bin_dir_candidates,
    default_socket_dir, find_pg_bin_dir, InitDbOptions, PgConfigOptions,
};
use hhagent_supervisor::specs::postgres_service_spec;
use hhagent_supervisor::{default_probe, default_supervisor, ServiceStatus, Supervisor};

use hhagent_core::memory::{embed_query, recall, MemoryError, RecallModes, RecallParams};
use hhagent_llm_router::{Router, RouterConfig};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

// ---- PG bring-up helpers (copied verbatim from memory_recall_e2e.rs;
//      8th duplication site; issue #15 tracks the hoist) ---------------

fn skip_if_no_supervisor() -> bool {
    match default_probe() {
        Ok(()) => false,
        Err(e) => {
            eprintln!("\n[SKIP] supervisor probe failed: {e}\n");
            true
        }
    }
}

fn pg_bin_dir_or_skip() -> Option<PathBuf> {
    match find_pg_bin_dir(&default_pg_bin_dir_candidates()) {
        Ok(dir) => Some(dir),
        Err(e) => {
            eprintln!("\n[SKIP] no Postgres install found: {e}\n");
            None
        }
    }
}

fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{}", std::process::id(), nanos)
}

fn unique_temp_root(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("hhagent-{}-{}", label, unique_suffix()))
}

fn current_username() -> String {
    if let Some(u) = std::env::var_os("USER") {
        let s = u.to_string_lossy().into_owned();
        if !s.is_empty() {
            return s;
        }
    }
    if let Ok(out) = Command::new("whoami").output() {
        if out.status.success() {
            return String::from_utf8_lossy(&out.stdout).trim().to_string();
        }
    }
    "hhagent".into()
}

struct ServiceGuard {
    sup: Box<dyn Supervisor>,
    name: String,
}
impl Drop for ServiceGuard {
    fn drop(&mut self) {
        let _ = self.sup.stop(&self.name);
        let _ = self.sup.uninstall(&self.name);
    }
}

struct PathGuard {
    path: PathBuf,
}
impl Drop for PathGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn wait_for_status<F: Fn(ServiceStatus) -> bool>(
    sup: &dyn Supervisor,
    name: &str,
    predicate: F,
    timeout: Duration,
) -> Result<ServiceStatus, String> {
    let start = Instant::now();
    let mut last = sup.status(name).map_err(|e| format!("status: {e}"))?;
    loop {
        if predicate(last) {
            return Ok(last);
        }
        if start.elapsed() > timeout {
            return Err(format!("timeout {:?}; last={last:?}", timeout));
        }
        std::thread::sleep(Duration::from_millis(50));
        last = sup.status(name).map_err(|e| format!("status: {e}"))?;
    }
}

fn wait_for_socket(socket_dir: &Path, timeout: Duration) -> Result<(), String> {
    let target = socket_dir.join(".s.PGSQL.5432");
    let start = Instant::now();
    loop {
        if target.exists() {
            return Ok(());
        }
        if start.elapsed() > timeout {
            return Err(format!("timeout {:?} waiting for {}", timeout, target.display()));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Bring up a per-test PG cluster (initdb + auto.conf + supervisor
/// install + start). Returns the connection spec and the cleanup
/// guards. Same shape as the helper in `audit_dispatch_e2e.rs` and
/// `memory_recall_e2e.rs` — issue #15 will eventually hoist this into a
/// shared `tests-common` dev-dep crate.
fn bring_up_pg_cluster(
    bin_dir: &Path,
    suffix: &str,
) -> (
    hhagent_db::conn::ConnectSpec,
    (ServiceGuard, PathGuard, PathGuard),
) {
    let postgres = bin_dir.join("postgres");
    let initdb = bin_dir.join("initdb");

    let data_root = unique_temp_root("embr-d");
    let data_guard = PathGuard {
        path: data_root.clone(),
    };
    let data_dir = data_root.join("data");
    let socket_dir = default_socket_dir(&data_dir);
    let log_dir = unique_temp_root("embr-l");
    std::fs::create_dir_all(&log_dir).expect("create log dir");
    let log_guard = PathGuard {
        path: log_dir.clone(),
    };

    let user = current_username();
    let argv = build_initdb_argv(
        &initdb,
        &InitDbOptions {
            data_dir: data_dir.clone(),
            username: user.clone(),
            ..InitDbOptions::default()
        },
    );
    let out = Command::new(&argv[0])
        .args(&argv[1..])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .output()
        .expect("spawn initdb");
    assert!(
        out.status.success(),
        "initdb failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    std::fs::create_dir(&socket_dir).expect("create socket dir");
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&socket_dir).unwrap().permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(&socket_dir, perms).unwrap();
    }
    std::fs::write(
        data_dir.join("postgresql.auto.conf"),
        build_postgresql_auto_conf(&PgConfigOptions {
            socket_dir: socket_dir.clone(),
            ..PgConfigOptions::default()
        }),
    )
    .expect("write postgresql.auto.conf");

    let mut spec = postgres_service_spec(&postgres, &data_dir, &log_dir);
    spec.name = format!("hhagent-supervisor-test-pg-embr-{suffix}");
    assert!(spec.name.len() <= 200);
    spec.stdout_log = Some(log_dir.join(format!("{}.out", spec.name)));
    spec.stderr_log = Some(log_dir.join(format!("{}.err", spec.name)));

    let sup = default_supervisor();
    let service_guard = ServiceGuard {
        sup: default_supervisor(),
        name: spec.name.clone(),
    };
    sup.install(&spec).expect("install pg");
    sup.start(&spec.name).expect("start pg");
    wait_for_status(
        sup.as_ref(),
        &spec.name,
        |s| s == ServiceStatus::Active,
        Duration::from_secs(15),
    )
    .expect("pg active");
    wait_for_socket(&socket_dir, Duration::from_secs(15)).expect("pg socket");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        sup.status(&spec.name).unwrap(),
        ServiceStatus::Active,
        "pg flap"
    );

    let conn_spec = hhagent_db::conn::ConnectSpec {
        socket_dir: socket_dir.clone(),
        user: user.clone(),
        database: hhagent_db::conn::DEFAULT_APPLICATION_DB.to_string(),
    };
    (conn_spec, (service_guard, data_guard, log_guard))
}

/// Deterministic, dependency-free embedding stub for tests.
///
/// Hashes the input text with SHA-256 to produce a 32-byte seed, then
/// runs an xorshift64 PRNG to fill 1024 floats in `[-1, 1]`, and
/// finally L2-normalises so the cosine-similarity calculation is
/// numerically clean.
///
/// Copied verbatim from `memory_recall_e2e.rs`; issue #15 tracks the
/// workspace-level hoist.
fn text_to_embedding(text: &str) -> Vec<f32> {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(text.as_bytes());
    let mut seed: u64 = 0;
    for (i, b) in digest[..8].iter().enumerate() {
        seed |= (*b as u64) << (i * 8);
    }
    if seed == 0 {
        seed = 1;
    }

    let mut state = seed;
    let mut v: Vec<f32> = Vec::with_capacity(EMBEDDING_DIM);
    for _ in 0..EMBEDDING_DIM {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let bits = (state >> 40) as u32;
        let unit = (bits as f32) / ((1u32 << 24) as f32);
        v.push(unit * 2.0 - 1.0);
    }

    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
    v
}

// ---- Mock helpers (copied verbatim from
//      llm-router/tests/embedding_backend_e2e.rs; issue #15) -----------

#[derive(Debug, Clone)]
struct ServedRequest {
    path: String,
    body: String,
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

async fn spawn_one_shot_mock(
    canned: CannedResponse,
) -> (String, oneshot::Receiver<ServedRequest>) {
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
                let header_str = std::str::from_utf8(&buf[..headers_end])
                    .expect("headers are utf-8");
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

// ---- Local router-builder helper -------------------------------------

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

// ---- Shared async PG setup helper -----------------------------------

/// Run probe (applies migrations + writes bring-up row), then connect
/// the runtime pool. Returns pool.
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

// ---- Build a canned 512-float vector for dim-mismatch tests ----------

fn make_short_vec(n: usize) -> Vec<f32> {
    (0..n).map(|i| (i as f32) / (n as f32)).collect()
}

// ====================================================================
// Test 1 — happy path: embed_query returns Vec<f32> of EMBEDDING_DIM
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
    let (conn_spec, _guards) = bring_up_pg_cluster(&bin_dir, &suffix);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build tokio runtime");

    rt.block_on(async {
        let pool = setup_pg(&conn_spec).await;

        let emb_vec = text_to_embedding("hello");
        assert_eq!(emb_vec.len(), EMBEDDING_DIM);

        let canned = serde_json::json!({
            "data": [{"index": 0, "embedding": emb_vec}],
            "model": "embedding-test"
        });
        let (base_url, _served) =
            spawn_one_shot_mock(CannedResponse::ok_json(canned.to_string())).await;
        let router = build_router_pointing_at(&base_url);

        let result = embed_query(&pool, &router, "hello").await.expect("embed_query ok");
        assert_eq!(result.len(), EMBEDDING_DIM,
            "embed_query must return a vector of length {EMBEDDING_DIM}, got {}",
            result.len()
        );

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
    let (conn_spec, _guards) = bring_up_pg_cluster(&bin_dir, &suffix);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build tokio runtime");

    rt.block_on(async {
        let pool = setup_pg(&conn_spec).await;

        let emb_vec = text_to_embedding("alpha bravo");
        let canned = serde_json::json!({
            "data": [{"index": 0, "embedding": emb_vec}],
            "model": "embedding-test"
        });
        let (base_url, _served) =
            spawn_one_shot_mock(CannedResponse::ok_json(canned.to_string())).await;
        let router = build_router_pointing_at(&base_url);

        embed_query(&pool, &router, "alpha bravo").await.expect("embed_query ok");

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
        assert!(payload["latency_ms"].is_u64(),
            "latency_ms must be a JSON u64: {payload:?}");

        // Privacy invariants — the text and embedding must not be in the row.
        let payload_str = serde_json::to_string(payload).unwrap();
        assert!(!payload_str.contains("\"input\""), "input leaked: {payload_str}");
        assert!(!payload_str.contains("alpha"), "user text leaked: {payload_str}");
        assert!(!payload_str.contains("\"embedding\""), "embedding leaked: {payload_str}");

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
    let (conn_spec, _guards) = bring_up_pg_cluster(&bin_dir, &suffix);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build tokio runtime");

    rt.block_on(async {
        let pool = setup_pg(&conn_spec).await;

        // Mock returns a 512-float vector — wrong dim.
        let short_vec = make_short_vec(512);
        let canned = serde_json::json!({
            "data": [{"index": 0, "embedding": short_vec}],
            "model": "embedding-test"
        });
        let (base_url, _served) =
            spawn_one_shot_mock(CannedResponse::ok_json(canned.to_string())).await;
        let router = build_router_pointing_at(&base_url);

        let err = embed_query(&pool, &router, "hello")
            .await
            .expect_err("dim must mismatch");
        match err {
            MemoryError::EmbeddingDimMismatch { expected, actual, model } => {
                assert_eq!(expected, 1024);
                assert_eq!(actual, 512);
                assert_eq!(model, "embedding-test");
            }
            other => panic!("expected EmbeddingDimMismatch, got {other:?}"),
        }

        // No audit row for the failure (chokepoint precedent).
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM audit_log WHERE actor = 'llm:router'",
        )
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
    let (conn_spec, _guards) = bring_up_pg_cluster(&bin_dir, &suffix);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build tokio runtime");

    rt.block_on(async {
        let pool = setup_pg(&conn_spec).await;

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
        let (base_url, _served) =
            spawn_one_shot_mock(CannedResponse::ok_json(canned.to_string())).await;
        let router = build_router_pointing_at(&base_url);

        // embed_query the matching text.
        let emb = embed_query(&pool, &router, BODY_A).await.expect("embed");
        assert_eq!(emb.len(), EMBEDDING_DIM);

        // Plug into recall — semantic-only lane.
        let mems = recall(
            &pool,
            &RecallParams {
                query_text: None,
                query_embedding: Some(&emb),
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
