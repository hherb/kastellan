//! End-to-end: agent core spawns the `web-fetch` worker under the platform
//! sandbox and round-trips a `web.fetch` call through `tool_host::dispatch`.
//!
//! Hermetic test (`host_outside_allowlist_is_denied`): a non-allowlisted URL
//! is refused by the worker's own allowlist check before any network egress,
//! so it needs no server — it verifies the worker runs under the real
//! net-enabled sandbox and the wire contract holds.
//!
//! Ignored test (`real_fetch_extracts_readable_text`): a real HTTPS GET against
//! an allowlisted public host. Run manually with `--ignored`; it also validates
//! that DNS + TLS work inside the `--unshare-all` (Linux) / Seatbelt (macOS)
//! jail, which the hermetic test cannot.
//!
//! `[SKIP]`s cleanly when PG, the supervisor, the worker binary, or a working
//! sandbox is missing — same posture as `shell_exec_e2e.rs`.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::PathBuf;

use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch, spawn_worker, WorkerSpec};
use kastellan_core::workers::web_fetch::web_fetch_entry;
use kastellan_protocol::codes;
use kastellan_tests_common::{
    backend, bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor,
    skip_if_sandbox_unavailable, unique_suffix, workspace_target_binary, PgCluster,
};

async fn probe_and_pool(conn_spec: &kastellan_db::conn::ConnectSpec) -> sqlx::PgPool {
    kastellan_db::probe::run(
        conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "web-fetch-e2e"}),
    )
    .await
    .expect("probe run");
    kastellan_db::pool::connect_runtime_pool(conn_spec)
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
    allowlist: Vec<String>,
}

fn ready_or_skip(allowlist: &[&str]) -> Option<TestEnv> {
    if skip_if_no_supervisor() {
        return None;
    }
    if skip_if_sandbox_unavailable() {
        return None;
    }
    let bin_dir = pg_bin_dir_or_skip()?;
    let worker_path = workspace_target_binary("kastellan-worker-web-fetch");
    if !worker_path.exists() {
        eprintln!("\n[SKIP] web-fetch worker binary not built; run cargo build --workspace\n");
        return None;
    }

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "wf-d",
        "wf-l",
        &format!("kastellan-supervisor-test-pg-webfetch-{suffix}"),
    );

    Some(TestEnv {
        cluster,
        worker_path,
        allowlist: allowlist.iter().map(|s| s.to_string()).collect(),
    })
}

#[test]
fn host_outside_allowlist_is_denied() {
    // Allowlist a host we will NOT request, so the request is denied before
    // any egress — hermetic, no server required.
    let env = match ready_or_skip(&["en.wikipedia.org"]) {
        Some(e) => e,
        None => return,
    };
    dispatch_runtime().block_on(async {
        let pool = probe_and_pool(&env.cluster.conn_spec).await;
        let policy = web_fetch_entry(env.worker_path.clone(), &env.allowlist).policy;
        let backend = backend();
        let worker_str = env.worker_path.to_string_lossy().into_owned();
        let spec = WorkerSpec {
            policy: &policy,
            program: &worker_str,
            args: &[],
            wall_clock_ms: None,
        };
        let mut sworker = spawn_worker(&*backend, &spec).expect("spawn web-fetch under sandbox");

        let err = dispatch(
            &pool,
            &Vault::new(),
            &mut sworker,
            "web-fetch",
            "web.fetch",
            serde_json::json!({"url": "https://not-allowlisted.example/"}),
        )
        .await
        .expect_err("non-allowlisted host must be denied");

        let msg = format!("{err}");
        assert!(
            msg.contains(&format!("{}", codes::POLICY_DENIED)),
            "expected POLICY_DENIED ({}), got: {msg}",
            codes::POLICY_DENIED
        );

        let _ = sworker.close();
        pool.close().await;
    });
}

#[test]
#[ignore = "hits the real network; validates DNS+TLS inside the sandbox jail"]
fn real_fetch_extracts_readable_text() {
    let env = match ready_or_skip(&["example.com"]) {
        Some(e) => e,
        None => return,
    };
    dispatch_runtime().block_on(async {
        let pool = probe_and_pool(&env.cluster.conn_spec).await;
        let policy = web_fetch_entry(env.worker_path.clone(), &env.allowlist).policy;
        let backend = backend();
        let worker_str = env.worker_path.to_string_lossy().into_owned();
        let spec = WorkerSpec {
            policy: &policy,
            program: &worker_str,
            args: &[],
            wall_clock_ms: None,
        };
        let mut sworker = spawn_worker(&*backend, &spec).expect("spawn web-fetch under sandbox");

        let result = dispatch(
            &pool,
            &Vault::new(),
            &mut sworker,
            "web-fetch",
            "web.fetch",
            serde_json::json!({"url": "https://example.com/"}),
        )
        .await
        .expect("web.fetch round trip (network + DNS in jail)");

        assert_eq!(result["status"], 200);
        let text = result["text"].as_str().unwrap_or("");
        assert!(
            text.to_lowercase().contains("example"),
            "expected readable text to mention 'example', got: {text}"
        );

        let _ = sworker.close();
        pool.close().await;
    });
}

/// Live spot-check for issue #142: a real HuggingFace model file carrying
/// ChatML control tokens (`<|im_start|>`) must NOT be injection-blocked
/// when fetched through `web-fetch`, because the dispatch chokepoint uses
/// the Relaxed guard profile for that worker. Confirms the committed
/// fixtures in `injection_guard_fixtures.rs` match real-world content.
/// Run manually with `--ignored`.
#[test]
#[ignore = "hits the real network; validates the Relaxed profile against a real model card"]
fn real_modelcard_with_chat_template_is_not_blocked() {
    let env = match ready_or_skip(&["huggingface.co"]) {
        Some(e) => e,
        None => return,
    };
    dispatch_runtime().block_on(async {
        let pool = probe_and_pool(&env.cluster.conn_spec).await;
        let policy = web_fetch_entry(env.worker_path.clone(), &env.allowlist).policy;
        let backend = backend();
        let worker_str = env.worker_path.to_string_lossy().into_owned();
        let spec = WorkerSpec {
            policy: &policy,
            program: &worker_str,
            args: &[],
            wall_clock_ms: None,
        };
        let mut sworker = spawn_worker(&*backend, &spec).expect("spawn web-fetch under sandbox");

        // A raw tokenizer config reliably contains `<|im_start|>`.
        let result = dispatch(
            &pool,
            &Vault::new(),
            &mut sworker,
            "web-fetch",
            "web.fetch",
            serde_json::json!({
                "url": "https://huggingface.co/Qwen/Qwen2.5-0.5B-Instruct/raw/main/tokenizer_config.json"
            }),
        )
        .await
        .expect("web.fetch round trip (network + DNS in jail)");

        // Relaxed profile: the result is the real body, NOT the redacted
        // injection placeholder.
        assert!(
            result.get("injection_blocked").is_none(),
            "Relaxed profile must not block a real model card; got: {result}"
        );
        let text = result["text"].as_str().unwrap_or("");
        assert!(
            text.contains("<|im_start|>"),
            "expected the fetched config to carry the ChatML token, got: {text}"
        );

        let _ = sworker.close();
        pool.close().await;
    });
}
