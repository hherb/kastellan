//! End-to-end: agent core spawns the `web-search` worker under the platform
//! sandbox and round-trips a `web.search` call through `tool_host::dispatch`.
//!
//! Hermetic test (`endpoint_off_allowlist_fails_closed`): the configured
//! endpoint host is NOT on the worker's allowlist, so the worker refuses at
//! startup (fail-closed `from_env`) and the dispatch errors before any network
//! egress — no server required.
//!
//! Ignored test (`real_search_against_searxng`): a real query against a live
//! SearxNG instance. Run manually with `--ignored` and
//! `HHAGENT_WEB_SEARCH_ENDPOINT` set; also validates DNS/TLS (or loopback)
//! inside the sandbox jail.
//!
//! `[SKIP]`s cleanly when PG, the supervisor, the worker binary, or a working
//! sandbox is missing — same posture as `web_fetch_e2e.rs`.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::PathBuf;

use hhagent_core::secrets::Vault;
use hhagent_core::tool_host::{dispatch, spawn_worker, WorkerSpec};
use hhagent_core::workers::web_search::web_search_entry;
use hhagent_tests_common::{
    backend, bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor,
    skip_if_sandbox_unavailable, unique_suffix, workspace_target_binary, PgCluster,
};

async fn probe_and_pool(conn_spec: &hhagent_db::conn::ConnectSpec) -> sqlx::PgPool {
    hhagent_db::probe::run(
        conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "web-search-e2e"}),
    )
    .await
    .expect("probe run");
    hhagent_db::pool::connect_runtime_pool(conn_spec)
        .await
        .expect("connect runtime pool")
}

fn dispatch_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build multi-threaded tokio runtime")
}

struct TestEnv {
    cluster: PgCluster,
    worker_path: PathBuf,
    endpoint: String,
    allowlist: Vec<String>,
}

fn ready_or_skip(endpoint: &str, allowlist: &[&str]) -> Option<TestEnv> {
    if skip_if_no_supervisor() {
        return None;
    }
    if skip_if_sandbox_unavailable() {
        return None;
    }
    let bin_dir = pg_bin_dir_or_skip()?;
    let worker_path = workspace_target_binary("hhagent-worker-web-search");
    if !worker_path.exists() {
        eprintln!("\n[SKIP] web-search worker binary not built; run cargo build --workspace\n");
        return None;
    }

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "ws-d",
        "ws-l",
        &format!("hhagent-supervisor-test-pg-websearch-{suffix}"),
    );

    Some(TestEnv {
        cluster,
        worker_path,
        endpoint: endpoint.to_string(),
        allowlist: allowlist.iter().map(|s| s.to_string()).collect(),
    })
}

#[test]
fn endpoint_off_allowlist_fails_closed() {
    // Endpoint host NOT on the allowlist → worker refuses at startup. Hermetic.
    let env = match ready_or_skip("https://searx.example.org/search", &["other.example.org"]) {
        Some(e) => e,
        None => return,
    };
    dispatch_runtime().block_on(async {
        let pool = probe_and_pool(&env.cluster.conn_spec).await;
        let policy = web_search_entry(env.worker_path.clone(), &env.endpoint, &env.allowlist).policy;
        let backend = backend();
        let worker_str = env.worker_path.to_string_lossy().into_owned();
        let spec = WorkerSpec {
            policy: &policy,
            program: &worker_str,
            args: &[],
            wall_clock_ms: None,
        };

        // The worker exits non-zero at startup (fail-closed from_env), so either
        // spawn yields a worker whose first dispatch errors, or dispatch surfaces
        // the broken pipe — both are errors. Assert the round trip does NOT
        // succeed.
        let spawned = spawn_worker(&*backend, &spec);
        if let Ok(mut sworker) = spawned {
            let result = dispatch(
                &pool,
                &Vault::new(),
                &mut sworker,
                "web-search",
                "web.search",
                serde_json::json!({"query": "anything"}),
            )
            .await;
            assert!(
                result.is_err(),
                "expected dispatch to fail (worker fails closed on off-allowlist endpoint), got: {result:?}"
            );
            let _ = sworker.close();
        }
        pool.close().await;
    });
}

#[test]
#[ignore = "hits a live SearxNG; set HHAGENT_WEB_SEARCH_ENDPOINT; validates DNS/TLS/loopback in jail"]
fn real_search_against_searxng() {
    let endpoint = std::env::var("HHAGENT_WEB_SEARCH_ENDPOINT")
        .unwrap_or_else(|_| "http://127.0.0.1:8888/search".to_string());
    // Allowlist the endpoint host so the worker accepts it.
    let host = url_host(&endpoint);
    let env = match ready_or_skip(&endpoint, &[&host]) {
        Some(e) => e,
        None => return,
    };
    dispatch_runtime().block_on(async {
        let pool = probe_and_pool(&env.cluster.conn_spec).await;
        let policy = web_search_entry(env.worker_path.clone(), &env.endpoint, &env.allowlist).policy;
        let backend = backend();
        let worker_str = env.worker_path.to_string_lossy().into_owned();
        let spec = WorkerSpec {
            policy: &policy,
            program: &worker_str,
            args: &[],
            wall_clock_ms: None,
        };
        let mut sworker = spawn_worker(&*backend, &spec).expect("spawn web-search under sandbox");

        let result = dispatch(
            &pool,
            &Vault::new(),
            &mut sworker,
            "web-search",
            "web.search",
            serde_json::json!({"query": "rust programming language", "count": 5}),
        )
        .await
        .expect("web.search round trip (network + DNS in jail)");

        let results = result["results"].as_array().expect("results array");
        assert!(!results.is_empty(), "expected at least one hit");
        assert!(results[0]["url"].as_str().unwrap_or("").starts_with("http"));

        let _ = sworker.close();
        pool.close().await;
    });
}

/// Extract the host from a URL string for the ignored test's allowlist.
fn url_host(endpoint: &str) -> String {
    url::Url::parse(endpoint)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_else(|| "127.0.0.1".to_string())
}
