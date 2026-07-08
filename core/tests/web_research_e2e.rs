//! End-to-end: agent core spawns the `web-research` worker under the platform
//! sandbox and round-trips a `web.research` call through `tool_host::dispatch`.
//!
//! `web.research` is a *composite* worker: one call runs SearxNG search →
//! filter the hits to the operator content allowlist → fetch the top-N
//! allowlisted pages → chunk → BM25-rank passages. So unlike its `web-fetch` /
//! `web-search` siblings the live test exercises search *and* fetch *and* rank
//! in a single dispatch — the one real coverage gap the hermetic `FakeGet`
//! unit tests in `workers/web-research` cannot close (no DNS/TLS in the jail,
//! no real HTML→passage extraction).
//!
//! Hermetic test (`endpoint_off_allowlist_fails_closed`): the configured
//! endpoint host is NOT on the worker's allowlist, so the worker refuses at
//! startup (fail-closed `from_env`) and the dispatch errors before any network
//! egress — no server required. Mirrors `web_search_e2e.rs`.
//!
//! Ignored test (`real_research_against_searxng`): a real query against a live
//! SearxNG instance with a real content host (`en.wikipedia.org`) allowlisted.
//! Run manually with `--ignored`; stand up SearxNG first via
//! `scripts/web-search/setup-searxng.sh` and (optionally) set
//! `KASTELLAN_WEB_RESEARCH_ENDPOINT`. It validates DNS/TLS (or loopback) inside
//! the sandbox jail for both the search endpoint and the fetched content host.
//!
//! Ignored test (`real_research_with_hybrid_ranking`): the same composite but
//! with an embedding-only endpoint configured
//! (`KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT`, default a local Ollama
//! `/v1/embeddings`). This is the ONLY test that drives real `HttpEmbedder::post`
//! bytes over the worker's egress transport against a real embedding backend —
//! the hermetic `FakeEmbedder`/`FakeGet` unit tests never touch the wire. It
//! asserts `ranking == "hybrid"`, which only holds when the query embedded
//! successfully through the jail. Run with `--ignored` after standing up both
//! SearxNG and an embedding endpoint (e.g. `ollama serve` with `embeddinggemma`).
//!
//! `[SKIP]`s cleanly when PG, the supervisor, the worker binary, or a working
//! sandbox is missing — same posture as `web_search_e2e.rs`.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::PathBuf;

use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch, spawn_worker, WorkerSpec};
use kastellan_core::workers::web_research::{web_research_entry, web_research_entry_with_embed};
use kastellan_tests_common::{
    backend, bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor,
    skip_if_sandbox_unavailable, unique_suffix, workspace_target_binary, PgCluster,
};

async fn probe_and_pool(conn_spec: &kastellan_db::conn::ConnectSpec) -> sqlx::PgPool {
    kastellan_db::probe::run(
        conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "web-research-e2e"}),
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
    let worker_path = workspace_target_binary("kastellan-worker-web-research");
    if !worker_path.exists() {
        eprintln!("\n[SKIP] web-research worker binary not built; run cargo build --workspace\n");
        return None;
    }

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "wr-d",
        "wr-l",
        &format!("kastellan-supervisor-test-pg-webresearch-{suffix}"),
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
    // Endpoint host NOT on the allowlist → worker refuses at startup. Hermetic:
    // `from_env` validates the endpoint against the allowlist before serving, so
    // the round trip errors before any egress.
    let env = match ready_or_skip("https://searx.example.org/search", &["other.example.org"]) {
        Some(e) => e,
        None => return,
    };
    dispatch_runtime().block_on(async {
        let pool = probe_and_pool(&env.cluster.conn_spec).await;
        let policy =
            web_research_entry(env.worker_path.clone(), &env.endpoint, &env.allowlist).policy;
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
                "web-research",
                "web.research",
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
#[ignore = "hits a live SearxNG + a real content host; run scripts/web-search/setup-searxng.sh first; validates DNS/TLS/loopback in jail across search+fetch"]
fn real_research_against_searxng() {
    let endpoint = std::env::var("KASTELLAN_WEB_RESEARCH_ENDPOINT")
        .unwrap_or_else(|_| "http://127.0.0.1:8888/search".to_string());
    // Allowlist BOTH the SearxNG endpoint host (so `from_env` accepts it) AND a
    // real content host the search reliably surfaces for the query below (so the
    // fetch half has an allowlisted page to gather). `en.wikipedia.org` is a
    // stable, bot-friendly choice for a "programming language" query.
    let endpoint_host = url_host(&endpoint);
    let env = match ready_or_skip(&endpoint, &[&endpoint_host, "en.wikipedia.org"]) {
        Some(e) => e,
        None => return,
    };
    dispatch_runtime().block_on(async {
        let pool = probe_and_pool(&env.cluster.conn_spec).await;
        let policy =
            web_research_entry(env.worker_path.clone(), &env.endpoint, &env.allowlist).policy;
        let backend = backend();
        let worker_str = env.worker_path.to_string_lossy().into_owned();
        let spec = WorkerSpec {
            policy: &policy,
            program: &worker_str,
            args: &[],
            wall_clock_ms: Some(60_000),
        };
        let mut sworker = spawn_worker(&*backend, &spec).expect("spawn web-research under sandbox");

        let result = dispatch(
            &pool,
            &Vault::new(),
            &mut sworker,
            "web-research",
            "web.research",
            serde_json::json!({"query": "rust programming language", "max_sources": 3}),
        )
        .await
        .expect("web.research round trip (search + fetch + DNS/TLS in jail)");

        assert_composite_result_shape(&result, "rust programming language");

        let _ = sworker.close();
        pool.close().await;
    });
}

#[test]
#[ignore = "hits a live SearxNG + a real content host + a real embedding endpoint; \
            run scripts/web-search/setup-searxng.sh and an embedding backend (e.g. \
            ollama serve with embeddinggemma) first; drives real HttpEmbedder::post \
            bytes over the worker's egress transport and asserts hybrid ranking"]
fn real_research_with_hybrid_ranking() {
    let endpoint = std::env::var("KASTELLAN_WEB_RESEARCH_ENDPOINT")
        .unwrap_or_else(|_| "http://127.0.0.1:8888/search".to_string());
    // Default to a local Ollama OpenAI-compatible embeddings endpoint. Loopback
    // http is accepted by `validate_endpoint`; any override must be on-allowlist.
    let embed_endpoint = std::env::var("KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT")
        .unwrap_or_else(|_| "http://127.0.0.1:11434/v1/embeddings".to_string());

    // Allowlist the SearxNG endpoint host, the content host we expect the search
    // to surface, AND the embed endpoint host — the worker's fail-closed
    // `from_env` validates the embed endpoint against this same allowlist.
    let endpoint_host = url_host(&endpoint);
    let embed_host = url_host(&embed_endpoint);
    let env = match ready_or_skip(&endpoint, &[&endpoint_host, "en.wikipedia.org", &embed_host]) {
        Some(e) => e,
        None => return,
    };
    dispatch_runtime().block_on(async {
        let pool = probe_and_pool(&env.cluster.conn_spec).await;
        // `..._with_embed` unions the embed host:port into the egress allowlist and
        // injects the embed endpoint + model env, so the jailed worker builds an
        // `HttpEmbedder` and ranks hybrid.
        let policy = web_research_entry_with_embed(
            env.worker_path.clone(),
            &env.endpoint,
            Some(&embed_endpoint),
            None, // default model (embeddinggemma)
            &env.allowlist,
        )
        .policy;
        let backend = backend();
        let worker_str = env.worker_path.to_string_lossy().into_owned();
        let spec = WorkerSpec {
            policy: &policy,
            program: &worker_str,
            args: &[],
            wall_clock_ms: Some(60_000),
        };
        let mut sworker = spawn_worker(&*backend, &spec).expect("spawn web-research under sandbox");

        let result = dispatch(
            &pool,
            &Vault::new(),
            &mut sworker,
            "web-research",
            "web.research",
            serde_json::json!({"query": "rust programming language", "max_sources": 2}),
        )
        .await
        .expect("web.research round trip (search + fetch + embed in jail)");

        assert_composite_result_shape(&result, "rust programming language");

        // The core new coverage: `ranking == "hybrid"` holds ONLY when the query
        // was embedded successfully by the real backend through the jail (see
        // `research::research` — the effective embedder is `None` on query-embed
        // failure). If this is "lexical", the embed endpoint was unreachable from
        // inside the jail or the response failed to decode — inspect `embed_note`.
        assert_eq!(
            result["ranking"], "hybrid",
            "expected hybrid ranking (query embedded via the live endpoint); embed_note: {:?}",
            result.get("embed_note")
        );
        // A per-page passage embed MAY still degrade (e.g. a huge page bumping the
        // response cap — the deferred passages-per-POST cap), which sets a
        // page-level `embed_note` while keeping the overall ranking hybrid. So we
        // don't require `embed_note` to be absent; the hybrid flag above is the gate.

        let _ = sworker.close();
        pool.close().await;
    });
}

/// Assert the shared shape of a successful composite `web.research` result: the
/// query is echoed, search returned ≥1 hit, `sources_fetched`/`passage_count` are
/// internally consistent, and the first fetched source carries a non-empty,
/// numerically-scored passage. Used by both live tests (lexical + hybrid).
fn assert_composite_result_shape(result: &serde_json::Value, expected_query: &str) {
    // The query is echoed back verbatim.
    assert_eq!(result["query"], expected_query);

    let sources = result["sources"].as_array().expect("sources array");
    let unfetched = result["unfetched"].as_array().expect("unfetched array");

    // Search half: at least one hit came back (either fetched into a source or
    // recorded in `unfetched`). No hits ⇒ SearxNG returned nothing → the live
    // search path is broken (or the instance has no engines configured).
    assert!(
        !sources.is_empty() || !unfetched.is_empty(),
        "expected SearxNG to return at least one hit; got zero sources and zero unfetched"
    );

    // `sources_fetched` is the count of successfully-gathered sources.
    assert_eq!(
        result["sources_fetched"].as_u64().unwrap_or(u64::MAX),
        sources.len() as u64,
        "sources_fetched must equal sources.len()"
    );

    // Composite (fetch + rank) half: at least one allowlisted hit was fetched
    // and produced a ranked passage. Depends on `en.wikipedia.org` surfacing
    // for the query and being fetchable over HTTPS from inside the jail — the
    // whole point of this end-to-end test. If this fails while the assertion
    // above passed, inspect `unfetched` reasons (off-allowlist vs fetch-failed).
    assert!(
        !sources.is_empty(),
        "expected at least one fetched source with passages; unfetched reasons: {:?}",
        unfetched
            .iter()
            .map(|u| u["reason"].as_str().unwrap_or("?"))
            .collect::<Vec<_>>()
    );

    let first = &sources[0];
    assert!(
        first["url"].as_str().unwrap_or("").starts_with("http"),
        "source url should be an absolute http(s) URL, got: {}",
        first["url"]
    );
    assert_eq!(first["fetched"], true);
    let passages = first["passages"].as_array().expect("passages array");
    assert!(!passages.is_empty(), "a fetched source must carry ≥1 passage");
    assert!(
        !passages[0]["text"].as_str().unwrap_or("").is_empty(),
        "passage text must be non-empty"
    );
    assert!(
        passages[0]["score"].is_number(),
        "passage must carry a numeric relevance score, got: {}",
        passages[0]["score"]
    );

    // `passage_count` is the total passages across all sources.
    let counted: usize = sources
        .iter()
        .map(|s| s["passages"].as_array().map(|p| p.len()).unwrap_or(0))
        .sum();
    assert_eq!(
        result["passage_count"].as_u64().unwrap_or(u64::MAX),
        counted as u64,
        "passage_count must equal the summed passage arrays"
    );
}

/// Extract the host from a URL string for the ignored test's allowlist.
fn url_host(endpoint: &str) -> String {
    url::Url::parse(endpoint)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_else(|| "127.0.0.1".to_string())
}
