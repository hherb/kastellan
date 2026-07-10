//! End-to-end: a jailed `web-research` worker embeds through a core-spawned
//! **trusted embed-broker sidecar** over a bound UDS — with the embedding
//! backend host **removed from the worker's egress allowlist entirely** — and
//! still ranks passages `"hybrid"`.
//!
//! This is the Slice-C acceptance for the embedding-broker arc (Slices A/B are
//! merged). It composes the two halves that earlier slices tested separately:
//!
//! * `embed_broker_spawn_e2e.rs` proved core can spawn the real broker under the
//!   sandbox and forward an `embed` to a *stub* backend over the UDS.
//! * `web_research_e2e.rs::real_research_with_hybrid_ranking` proved a jailed
//!   web-research worker ranks `"hybrid"` when it reaches the embed backend
//!   **directly** (the embed host is in its `Net::Allowlist`).
//!
//! Slice C removes that direct reach: in **broker mode**
//! ([`web_research_broker_entry`]) the embed host is dropped from the worker's
//! allowlist, and the query only embeds because core bound the broker's
//! `embed.sock` into the jail (`embed_broker_uds` + `KASTELLAN_EMBED_BROKER_UDS`)
//! and the broker — running host-side on its own `Net::Allowlist([embed host])` —
//! forwarded the POST to the real backend. So `ranking == "hybrid"` here is a
//! strictly stronger claim than the direct e2e: it holds **despite the worker
//! having zero embed egress**.
//!
//! ## Two tests, two postures
//!
//! * `brokered_policy_has_broker_uds_and_zero_embed_egress` (hermetic, always
//!   runs): pins the exact post-rewrite worker policy the live e2e depends on —
//!   the broker UDS is bound + injected, the direct-embed env is omitted, and the
//!   embed host is absent from `Net::Allowlist`. It drives the **real**
//!   `worker_lifecycle::force_route::rewrite_policy_for_broker` (exposed
//!   `#[doc(hidden)] pub` for this e2e), so it genuinely guards the production
//!   manifest + rewrite pair against drift rather than a local replica.
//! * `brokered_worker_ranks_hybrid_with_zero_embed_egress` (`#[ignore]`, live):
//!   the real wire. Needs a live SearxNG *and* a live embedding backend (e.g.
//!   `ollama serve` with `embeddinggemma`). Run with `--ignored` after standing
//!   up both (`scripts/web-search/setup-searxng.sh`).
//!
//! `[SKIP]`s cleanly (never fails) when PG, the supervisor, either worker
//! binary, or a working sandbox is missing — same posture as the sibling e2es.
//!
//! **Deferred (documented, not covered here):** force-routed × broker (the embed
//! rides the broker while SearxNG/content ride the egress proxy — the
//! `embed_broker_uds`-survives-force-route composition is unit-tested in
//! `worker_lifecycle::force_route`) and VM × broker (the broker runs host-side,
//! reached via the slice-4a vsock UDS bound as `embed_broker_uds`). Both are
//! orthogonal plumbing on top of the property this test establishes.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::PathBuf;

use kastellan_core::embed_broker::{
    spawn_embed_broker, EmbedBrokerConfig, EMBED_BROKER_UDS_ENV,
};
use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch, spawn_worker, WorkerSpec};
use kastellan_core::worker_lifecycle::force_route::rewrite_policy_for_broker;
use kastellan_core::workers::web_research::web_research_broker_entry;
use kastellan_sandbox::Net;
use kastellan_tests_common::{
    backend, bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor,
    skip_if_sandbox_unavailable, unique_suffix, workspace_target_binary, PgCluster,
};

/// The embed endpoint the worker never reaches directly. Defaults to a local
/// Ollama OpenAI-compatible embeddings endpoint; override for a routable host.
const DEFAULT_EMBED_ENDPOINT: &str = "http://127.0.0.1:11434/v1/embeddings";
/// SearxNG search endpoint. Loopback http is accepted by `validate_endpoint`.
const DEFAULT_SEARX_ENDPOINT: &str = "http://127.0.0.1:8888/search";

/// Extract the `host:port` authority from a URL, matching how `net_entries`
/// records allowlist entries (so the "embed host absent" check compares like
/// with like). Falls back to the host alone if no explicit port.
fn url_authority(endpoint: &str) -> String {
    let parsed = url::Url::parse(endpoint).expect("parse endpoint URL");
    let host = parsed.host_str().expect("endpoint has a host").to_string();
    match parsed.port() {
        Some(p) => format!("{host}:{p}"),
        None => host,
    }
}

/// Extract the bare host from a URL, for the SearxNG allowlist entry.
fn url_host(endpoint: &str) -> String {
    url::Url::parse(endpoint)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_else(|| "127.0.0.1".to_string())
}

#[test]
fn brokered_policy_has_broker_uds_and_zero_embed_egress() {
    // Hermetic: no spawn, no network — just assert the shape of the policy the
    // live e2e (and production's `spawn_worker_with_optional_broker`) hands the
    // jailed worker in broker mode.
    let worker = PathBuf::from("/nonexistent/kastellan-worker-web-research");
    let searx = "https://searx.example.org/search";
    let embed = "http://127.0.0.1:11434/v1/embeddings";
    let allowlist = vec!["en.wikipedia.org".to_string()];

    let entry = web_research_broker_entry(worker, searx, embed, None, &allowlist);

    // The manifest declares the broker backend but leaves the UDS unset — core
    // fills it at spawn time. Simulate that rewrite with a placeholder socket.
    let uds = PathBuf::from("/tmp/embed-brokered-test/embed.sock");
    let policy = rewrite_policy_for_broker(entry.policy, &uds);

    // (1) The broker socket is bound into the jail (Slice B1) …
    assert_eq!(
        policy.embed_broker_uds.as_deref(),
        Some(uds.as_path()),
        "broker UDS must be bound into the jail"
    );
    // … and (2) the env that makes `choose_embedder` pick `BrokeredEmbedder` is
    // injected with the same path.
    let injected = policy
        .env
        .iter()
        .find(|(k, _)| k == EMBED_BROKER_UDS_ENV)
        .map(|(_, v)| v.as_str());
    assert_eq!(
        injected,
        Some(uds.to_string_lossy().as_ref()),
        "KASTELLAN_EMBED_BROKER_UDS must be injected pointing at the bound socket"
    );

    // (3) The direct-embed env is NOT injected — broker mode routes embeds over
    // the UDS, never the endpoint.
    assert!(
        !policy
            .env
            .iter()
            .any(|(k, _)| k == "KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT"),
        "broker mode must omit the direct embed-endpoint env"
    );

    // (4) The core Slice-C property: the embed backend host is ABSENT from the
    // worker's egress allowlist. A compromised worker cannot reach the embedding
    // backend directly; its only path to a vector is the trusted broker's UDS.
    let embed_authority = url_authority(embed);
    match &policy.net {
        Net::Allowlist(entries) => assert!(
            !entries.iter().any(|e| e == &embed_authority),
            "embed host {embed_authority:?} must NOT be in the worker's Net::Allowlist \
             (broker mode drops it); got {entries:?}"
        ),
        other => panic!("expected Net::Allowlist in broker mode, got {other:?}"),
    }
}

struct TestEnv {
    cluster: PgCluster,
    worker_path: PathBuf,
    broker_path: PathBuf,
    searx_endpoint: String,
    embed_endpoint: String,
    allowlist: Vec<String>,
}

/// Bring up the PG cluster + resolve both worker binaries, or `[SKIP]` (return
/// `None`) if any live prerequisite is missing.
fn ready_or_skip(searx_endpoint: &str, embed_endpoint: &str, allowlist: &[&str]) -> Option<TestEnv> {
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
    let broker_path = workspace_target_binary("kastellan-worker-embed-broker");
    if !broker_path.exists() {
        eprintln!("\n[SKIP] embed-broker binary not built; run cargo build --workspace\n");
        return None;
    }

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "eb-d",
        "eb-l",
        &format!("kastellan-supervisor-test-pg-embedbroker-{suffix}"),
    );

    Some(TestEnv {
        cluster,
        worker_path,
        broker_path,
        searx_endpoint: searx_endpoint.to_string(),
        embed_endpoint: embed_endpoint.to_string(),
        allowlist: allowlist.iter().map(|s| s.to_string()).collect(),
    })
}

async fn probe_and_pool(conn_spec: &kastellan_db::conn::ConnectSpec) -> sqlx::PgPool {
    kastellan_db::probe::run(
        conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "embed-broker-egress-e2e"}),
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

#[test]
#[ignore = "hits a live SearxNG + a real content host + a real embedding backend \
            reached ONLY through the trusted broker; run \
            scripts/web-search/setup-searxng.sh and an embedding backend (e.g. \
            ollama serve with embeddinggemma) first, then --ignored. Asserts \
            hybrid ranking with the embed host absent from the worker's egress."]
fn brokered_worker_ranks_hybrid_with_zero_embed_egress() {
    let searx_endpoint = std::env::var("KASTELLAN_WEB_RESEARCH_ENDPOINT")
        .unwrap_or_else(|_| DEFAULT_SEARX_ENDPOINT.to_string());
    let embed_endpoint = std::env::var("KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT")
        .unwrap_or_else(|_| DEFAULT_EMBED_ENDPOINT.to_string());

    // Allowlist the SearxNG endpoint host + the content host we expect the
    // search to surface — but deliberately NOT the embed host. In broker mode
    // the worker's `from_env` builds a `BrokeredEmbedder` from the injected UDS
    // and does no allowlist check on the embed, so this must still rank hybrid.
    let searx_host = url_host(&searx_endpoint);
    let env = match ready_or_skip(
        &searx_endpoint,
        &embed_endpoint,
        &[&searx_host, "en.wikipedia.org"],
    ) {
        Some(e) => e,
        None => return,
    };

    dispatch_runtime().block_on(async {
        let pool = probe_and_pool(&env.cluster.conn_spec).await;

        // Broker-mode entry: embed host dropped from the allowlist, `embed_broker`
        // carries the backend the broker forwards to.
        let entry = web_research_broker_entry(
            env.worker_path.clone(),
            &env.searx_endpoint,
            &env.embed_endpoint,
            None, // default model (embeddinggemma)
            &env.allowlist,
        );
        let broker_spec = entry
            .embed_broker
            .as_ref()
            .expect("broker-mode entry declares an embed_broker spec");

        // Spawn the real broker under the sandbox, pointed at the live backend.
        // Short scratch root so `<scratch>/embed.sock` fits sun_path on macOS.
        let backend = backend();
        let cfg = EmbedBrokerConfig::new(env.broker_path.clone(), std::env::temp_dir());
        let (sidecar, uds) = spawn_embed_broker(&cfg, broker_spec, &*backend)
            .expect("spawn embed-broker sidecar under sandbox");
        assert!(uds.exists(), "broker must have bound its UDS at {uds:?}");

        // Rewrite the worker policy onto the bound broker UDS (what core's
        // chokepoint does). This is where the worker gains its ONLY path to a
        // vector — over the UDS, not the network.
        let policy = rewrite_policy_for_broker(entry.policy, &uds);

        // Zero-embed-egress invariant, re-asserted on the live policy: the embed
        // host is not reachable directly. If this held only hermetically we could
        // not claim the live hybrid result came through the broker.
        let embed_authority = url_authority(&env.embed_endpoint);
        match &policy.net {
            Net::Allowlist(entries) => assert!(
                !entries.iter().any(|e| e == &embed_authority),
                "embed host {embed_authority:?} must be absent from the live worker's \
                 egress allowlist; got {entries:?}"
            ),
            // Fail closed, matching the hermetic pin: broker mode must stay on an
            // allowlist. A different variant would silently skip the egress check.
            other => panic!("expected Net::Allowlist in broker mode, got {other:?}"),
        }

        let worker_str = env.worker_path.to_string_lossy().into_owned();
        let spec = WorkerSpec {
            policy: &policy,
            program: &worker_str,
            args: &[],
            wall_clock_ms: Some(60_000),
        };
        let mut sworker =
            spawn_worker(&*backend, &spec).expect("spawn brokered web-research under sandbox");

        let result = dispatch(
            &pool,
            &Vault::new(),
            &mut sworker,
            "web-research",
            "web.research",
            serde_json::json!({"query": "rust programming language", "max_sources": 2}),
        )
        .await
        .expect("web.research round trip (search + fetch + brokered embed)");

        // The Slice-C payoff: hybrid ranking holds even though the worker had NO
        // embed egress — the query embedded through the broker's UDS. "lexical"
        // here means the broker path failed (backend down, or the broker could
        // not reach it) — inspect `embed_note`.
        assert_eq!(
            result["ranking"], "hybrid",
            "expected hybrid ranking via the broker (embed host absent from egress); \
             embed_note: {:?}",
            result.get("embed_note")
        );

        // Teardown: dropping the sidecar kills the broker + removes its scratch.
        let _ = sworker.close();
        drop(sidecar);
        pool.close().await;
    });
}
