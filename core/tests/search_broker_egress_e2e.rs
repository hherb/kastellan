//! End-to-end: a jailed `web-search` worker searches through a core-spawned
//! **trusted search-broker sidecar** over a bound UDS — with the worker holding
//! an **empty egress allowlist** (zero direct network reach) — and still returns
//! real results.
//!
//! This is the search analog of `embed_broker_egress_e2e.rs`. The search worker's
//! broker mode is a *stronger* zero-egress posture than the embed worker's: web-
//! search has a single backend (SearxNG), so in broker mode the worker's
//! `Net::Allowlist` is **empty** (the embed worker keeps SearxNG + content hosts
//! and drops only the embed host). The query only succeeds because core bound the
//! broker's `search.sock` into the jail (`broker_uds` + `KASTELLAN_SEARCH_BROKER_UDS`)
//! and the broker — running host-side on its own `Net::Allowlist([searx host])` —
//! forwarded the request to the (loopback) SearxNG. So a non-empty `results` array
//! here holds **despite the worker having zero network egress**.
//!
//! ## Two tests, two postures
//!
//! * `brokered_web_search_policy_has_zero_egress` (hermetic, always runs): pins the
//!   exact post-rewrite worker policy the live e2e depends on — the broker UDS is
//!   bound + injected, the direct endpoint env is omitted, and `Net::Allowlist` is
//!   empty. It drives the **real** `web_search_broker_entry` +
//!   `worker_lifecycle::force_route::rewrite_policy_for_broker`, so it guards the
//!   production manifest + rewrite pair against drift, not a local replica.
//! * `brokered_web_search_returns_results_with_zero_egress` (`#[ignore]`, live):
//!   the real wire. Needs a live SearxNG (e.g. on `127.0.0.1:8888`). Run with
//!   `--ignored` after standing one up.
//!
//! `[SKIP]`s cleanly (never fails) when PG, the supervisor, either worker binary,
//! or a working sandbox is missing — same posture as the sibling e2es.
//!
//! **The DGX force-routing gate:** on the production DGX daemon
//! (`KASTELLAN_EGRESS_FORCE_ROUTING=1`) the worker is *also* force-routed, so its
//! netns has no route at all except the bound broker UDS — the `broker_uds`-
//! survives-force-route composition is unit-tested in
//! `worker_lifecycle::force_route`. This e2e establishes the host-mode property;
//! the DGX cutover (`scripts/web-search/dgx-search-broker-cutover.md`) exercises it
//! under force-routing.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::PathBuf;

use kastellan_core::broker::{spawn_broker, BrokerConfig, BrokerKind};
use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch, spawn_worker, WorkerSpec};
use kastellan_core::worker_lifecycle::force_route::rewrite_policy_for_broker;
use kastellan_core::workers::web_search::web_search_broker_entry;
use kastellan_sandbox::Net;
use kastellan_tests_common::{
    backend, bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor,
    skip_if_sandbox_unavailable, unique_suffix, workspace_target_binary, PgCluster,
};

/// SearxNG search endpoint the broker forwards to. Loopback http is accepted by
/// `validate_endpoint`; override for a different host/port.
const DEFAULT_SEARX_ENDPOINT: &str = "http://127.0.0.1:8888/search";

#[test]
fn brokered_web_search_policy_has_zero_egress() {
    // Hermetic: no spawn, no network — assert the shape of the policy the live
    // e2e (and production's `spawn_worker_with_optional_broker`) hands the jailed
    // worker in broker mode.
    let worker = PathBuf::from("/nonexistent/kastellan-worker-web-search");
    let searx = "http://127.0.0.1:8888/search";

    let entry = web_search_broker_entry(worker, searx);

    // The manifest declares the broker backend but leaves the UDS unset — core
    // fills it at spawn time. Simulate that rewrite with a placeholder socket.
    let uds = PathBuf::from("/tmp/search-brokered-test/search.sock");
    let policy = rewrite_policy_for_broker(entry.policy, &uds, BrokerKind::Search);

    // (1) The broker socket is bound into the jail …
    assert_eq!(
        policy.broker_uds.as_deref(),
        Some(uds.as_path()),
        "broker UDS must be bound into the jail"
    );
    // … and (2) the env that makes `choose_search_provider` pick the broker is
    // injected with the same path.
    let injected = policy
        .env
        .iter()
        .find(|(k, _)| k == BrokerKind::Search.uds_env())
        .map(|(_, v)| v.as_str());
    assert_eq!(
        injected,
        Some(uds.to_string_lossy().as_ref()),
        "KASTELLAN_SEARCH_BROKER_UDS must be injected pointing at the bound socket"
    );

    // (3) The direct endpoint env is NOT injected — broker mode routes searches
    // over the UDS, never a direct SearxNG endpoint.
    assert!(
        !policy
            .env
            .iter()
            .any(|(k, _)| k == "KASTELLAN_WEB_SEARCH_ENDPOINT"),
        "broker mode must omit the direct endpoint env"
    );

    // (4) The core property, stronger than the embed worker's: the worker's egress
    // allowlist is EMPTY. A compromised web-search worker cannot reach SearxNG,
    // loopback, the LAN, or the internet directly; its only path to a result is the
    // trusted broker's UDS.
    match &policy.net {
        Net::Allowlist(entries) => assert!(
            entries.is_empty(),
            "broker-mode web-search worker must have an EMPTY egress allowlist \
             (zero direct reach); got {entries:?}"
        ),
        other => panic!("expected Net::Allowlist in broker mode, got {other:?}"),
    }
}

struct TestEnv {
    cluster: PgCluster,
    worker_path: PathBuf,
    broker_path: PathBuf,
    searx_endpoint: String,
}

/// Bring up the PG cluster + resolve both worker binaries, or `[SKIP]` (return
/// `None`) if any live prerequisite is missing.
fn ready_or_skip(searx_endpoint: &str) -> Option<TestEnv> {
    if skip_if_no_supervisor() {
        return None;
    }
    if skip_if_sandbox_unavailable() {
        return None;
    }
    let bin_dir = pg_bin_dir_or_skip()?;
    let worker_path = workspace_target_binary("kastellan-worker-web-search");
    if !worker_path.exists() {
        eprintln!("\n[SKIP] web-search worker binary not built; run cargo build --workspace\n");
        return None;
    }
    let broker_path = workspace_target_binary("kastellan-worker-search-broker");
    if !broker_path.exists() {
        eprintln!("\n[SKIP] search-broker binary not built; run cargo build --workspace\n");
        return None;
    }

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "sb-d",
        "sb-l",
        &format!("kastellan-supervisor-test-pg-searchbroker-{suffix}"),
    );

    Some(TestEnv {
        cluster,
        worker_path,
        broker_path,
        searx_endpoint: searx_endpoint.to_string(),
    })
}

async fn probe_and_pool(conn_spec: &kastellan_db::conn::ConnectSpec) -> sqlx::PgPool {
    kastellan_db::probe::run(
        conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "search-broker-egress-e2e"}),
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
#[ignore = "hits a live SearxNG reached ONLY through the trusted search-broker; \
            stand up a SearxNG (e.g. on 127.0.0.1:8888) then run --ignored. \
            Asserts a non-empty results array with the worker holding an empty \
            egress allowlist (zero direct reach)."]
fn brokered_web_search_returns_results_with_zero_egress() {
    let searx_endpoint = std::env::var("KASTELLAN_WEB_SEARCH_ENDPOINT")
        .unwrap_or_else(|_| DEFAULT_SEARX_ENDPOINT.to_string());

    let env = match ready_or_skip(&searx_endpoint) {
        Some(e) => e,
        None => return,
    };

    dispatch_runtime().block_on(async {
        let pool = probe_and_pool(&env.cluster.conn_spec).await;

        // Broker-mode entry: empty allowlist, `broker` carries the SearxNG endpoint
        // the broker forwards to.
        let entry = web_search_broker_entry(env.worker_path.clone(), &env.searx_endpoint);
        let broker_spec = entry
            .broker
            .as_ref()
            .expect("broker-mode entry declares a broker spec");

        // Spawn the real search-broker under the sandbox, pointed at the live
        // SearxNG. Short scratch root so `<scratch>/search.sock` fits sun_path.
        let backend = backend();
        let cfg = BrokerConfig::new(BrokerKind::Search, env.broker_path.clone(), std::env::temp_dir());
        let (sidecar, uds) = spawn_broker(&cfg, broker_spec, &*backend)
            .expect("spawn search-broker sidecar under sandbox");
        assert!(uds.exists(), "broker must have bound its UDS at {uds:?}");

        // Rewrite the worker policy onto the bound broker UDS (what core's
        // chokepoint does). This is the worker's ONLY path to a result — over the
        // UDS, not the network.
        let policy = rewrite_policy_for_broker(entry.policy, &uds, BrokerKind::Search);

        // Zero-egress invariant, re-asserted on the live policy.
        match &policy.net {
            Net::Allowlist(entries) => assert!(
                entries.is_empty(),
                "live broker-mode worker must have an empty egress allowlist; got {entries:?}"
            ),
            other => panic!("expected Net::Allowlist in broker mode, got {other:?}"),
        }

        let worker_str = env.worker_path.to_string_lossy().into_owned();
        let spec = WorkerSpec {
            policy: &policy,
            program: &worker_str,
            args: &[],
            wall_clock_ms: Some(30_000),
        };
        let mut sworker =
            spawn_worker(&*backend, &spec).expect("spawn brokered web-search under sandbox");

        let result = dispatch(
            &pool,
            &Vault::new(),
            &mut sworker,
            "web-search",
            "web.search",
            serde_json::json!({"query": "rust programming language"}),
        )
        .await
        .expect("web.search round trip through the broker");

        // The payoff: real results even though the worker had ZERO network egress —
        // the search rode the broker's UDS.
        let results = result["results"]
            .as_array()
            .expect("results must be an array");
        assert!(
            !results.is_empty(),
            "expected a non-empty results array via the broker (worker has zero egress); got {result:?}"
        );

        // Teardown: dropping the sidecar kills the broker + removes its scratch.
        let _ = sworker.close();
        drop(sidecar);
        pool.close().await;
    });
}
