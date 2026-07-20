#![cfg(target_os = "linux")]
//! VM × embed-broker e2e: a web-research worker in a Firecracker VM embeds through
//! the host-side broker over the second vsock channel (port 1026), with the embed
//! host absent from its egress — the VM analogue of Slice C's zero-embed-egress
//! hybrid property (`embed_broker_egress_e2e.rs`).
//!
//! ## Tiers
//!
//! * Two hermetic pins (always run on Linux; no KVM/network) guard the production
//!   `web_research_firecracker_broker_entry` + rewrite + plan chain against drift:
//!   `vm_broker_policy_has_broker_uds_and_zero_embed_egress` pins the post-rewrite
//!   VM broker *policy* the live tier depends on — VM backend + a broker spec, the
//!   broker UDS bound + injected by the REAL `rewrite_policy_for_broker`, the embed
//!   host absent from `Net::Allowlist`, the direct embed-endpoint env omitted, the
//!   embed model env present; `vm_broker_real_rewrite_flows_through_plan_to_guest_path`
//!   then feeds that real rewrite output through the REAL sandbox
//!   `build_launch_plan` and pins that the broker channel materializes (port +
//!   host UDS + `kastellan.broker=1` token) and the injected UDS env is rewritten
//!   to the in-guest relay path — the full cross-crate contract in one body.
//!
//! * `brokered_vm_worker_ranks_hybrid_over_vsock_with_zero_embed_egress`
//!   (DGX-only, `#[ignore]`): the live full-stack tier. Real KVM + vsock + a
//!   real egress-proxy sidecar (SearxNG + content over vsock 1025) + the real
//!   host broker (embed over vsock 1026 → a live embed backend). A web-research
//!   worker booted in a Firecracker VM ranks passages `"hybrid"` with the embed
//!   host absent from egress — strictly stronger than Slice C's host-mode test
//!   because the worker is VM-isolated and its embed reaches the host broker
//!   only over vsock port 1026. Composed from
//!   `web_research_firecracker_egress_e2e.rs` (force-routed web-research VM +
//!   in-guest CA), `net_demo_firecracker_egress_e2e.rs` (host sidecar backend vs
//!   VM worker backend), and `embed_broker_egress_e2e.rs` (`spawn_broker` +
//!   hybrid assertion). Unblocked by the `NetWorkerSpawn.sidecar_backend` seam
//!   (a host egress proxy in front of a VM worker). Closes issue #445.
//!
//! ## Containment vs functionality (why deferring the live tier is safe)
//!
//! The deferred live tier is a **functionality** check (does hybrid ranking
//! actually flow over port 1026 in a booted VM?), NOT a **containment** check.
//! The zero-embed-egress property does not depend on the live channel working:
//! the embed host is absent from `Net::Allowlist` regardless (pinned hermetically
//! below), so if the broker channel were broken at runtime the worst case is a
//! degrade-to-lexical-with-signal — never an embed-egress leak. The security
//! invariant this feature exists to preserve is therefore fully verified here;
//! only the end-to-end plumbing awaits #445.

use std::path::PathBuf;
use std::sync::Arc;

use kastellan_core::broker::{spawn_broker, BrokerConfig, BrokerKind};
use kastellan_core::egress::net_worker::{spawn_forced_net_worker, NetWorkerSpawn};
use kastellan_core::secrets::Vault;
use kastellan_core::tool_host::{dispatch, WorkerSpec};
use kastellan_core::worker_lifecycle::force_route::rewrite_policy_for_broker;
use kastellan_core::workers::web_research::web_research_firecracker_broker_entry;
use kastellan_sandbox::linux_firecracker::{
    build_launch_plan, FirecrackerImage, LinuxFirecracker, BROKER_VSOCK_PORT,
};
use kastellan_sandbox::{Net, SandboxBackend, SandboxBackendKind, SandboxBackends};
use kastellan_tests_common::microvm::{firecracker_backend, image_dir, skip_if_no_microvm};
use kastellan_tests_common::{
    bring_up_pg_cluster, pg_bin_dir_or_skip, skip_if_no_supervisor,
    skip_if_sandbox_unavailable, unique_suffix, workspace_target_binary,
};

/// The rootfs image this suite boots. Passed to the shared
/// `kastellan_tests_common::microvm` helpers, which own the `[SKIP]` wording,
/// the launcher discovery and the `KASTELLAN_MICROVM_DIR` lookup (issue #475).
const VM_ROOTFS: &str = "web-research.ext4";

/// Hermetic (no KVM/network): pin the post-rewrite VM broker policy the live tier
/// depends on — VM backend, broker UDS bound + injected, embed host absent from
/// egress, direct embed-endpoint env omitted, embed model present.
#[test]
fn vm_broker_policy_has_broker_uds_and_zero_embed_egress() {
    let worker = PathBuf::from("/usr/local/bin/kastellan-worker-web-research");
    let searx = "https://searx.example.org/search";
    let embed = "http://127.0.0.1:11434/v1/embeddings";
    let allowlist = vec!["searx.example.org".to_string(), "en.wikipedia.org".to_string()];

    let entry = web_research_firecracker_broker_entry(
        worker,
        "/var/lib/kastellan/microvm".to_string(),
        searx,
        embed,
        None,
        &allowlist,
    );
    // VM backend + broker spec present.
    assert!(matches!(entry.sandbox_backend, Some(SandboxBackendKind::FirecrackerVm)));
    let spec = entry.broker.as_ref().expect("VM broker entry declares a broker");
    assert_eq!(spec.kind, BrokerKind::Embed);
    assert_eq!(spec.endpoint, embed);
    // The VM entry shares no host paths in (the CA is added at spawn); no direct
    // embed-endpoint env; the embed MODEL is present (the worker's BrokeredEmbedder
    // sends it per request).
    assert!(entry.policy.fs_read.is_empty(), "VM fs_read must be empty");
    assert!(
        !entry.policy.env.iter().any(|(k, _)| k == "KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT"),
        "broker mode must omit the direct embed-endpoint env"
    );
    assert!(
        entry
            .policy
            .env
            .iter()
            .any(|(k, v)| k == "KASTELLAN_WEB_RESEARCH_EMBED_MODEL" && v == "embeddinggemma"),
        "embed model env must be injected for the worker's BrokeredEmbedder"
    );

    // Simulate core's spawn-time rewrite onto the bound broker UDS (the REAL
    // production rewrite, not a replica).
    let uds = PathBuf::from("/tmp/embed-vm-test/embed.sock");
    let policy = rewrite_policy_for_broker(entry.policy, &uds, BrokerKind::Embed);
    // (1) broker UDS bound into the jail …
    assert_eq!(policy.broker_uds.as_deref(), Some(uds.as_path()));
    // … and (2) injected under the kind's env key with the same path.
    let injected = policy
        .env
        .iter()
        .find(|(k, _)| k == BrokerKind::Embed.uds_env())
        .map(|(_, v)| v.as_str());
    assert_eq!(injected, Some(uds.to_string_lossy().as_ref()));
    // (3) Zero embed egress: the loopback embed host is absent from the allowlist.
    match &policy.net {
        Net::Allowlist(entries) => assert!(
            entries.iter().all(|e| !e.starts_with("127.0.0.1")),
            "embed host must be absent from egress; got {entries:?}"
        ),
        other => panic!("expected Net::Allowlist in broker mode, got {other:?}"),
    }
}

/// Hermetic: chain the REAL `rewrite_policy_for_broker` output through the REAL
/// sandbox `build_launch_plan` and pin that the broker channel materializes — the
/// broker vsock port + host UDS are set, the `kastellan.broker=1` cmdline token is
/// emitted, and the injected `KASTELLAN_EMBED_BROKER_UDS` value is REWRITTEN from
/// the host path to the in-guest relay path (`/run/kastellan-broker.sock`).
///
/// This is the one test that exercises the full cross-crate contract in a single
/// body: core's rewrite injects the env value, the sandbox's kind-agnostic
/// value-match rewrite consumes it. The unit tests either side use a replica
/// (`plan.rs`'s `forced_broker_policy`) or stop before the plan (the hermetic pin
/// above), so drift in the env key/value shape or the value-match would slip past
/// them individually — this closes that seam.
#[test]
fn vm_broker_real_rewrite_flows_through_plan_to_guest_path() {
    let worker = PathBuf::from("/usr/local/bin/kastellan-worker-web-research");
    let searx = "https://searx.example.org/search";
    let embed = "http://127.0.0.1:11434/v1/embeddings";
    let allowlist = vec!["searx.example.org".to_string()];

    let entry = web_research_firecracker_broker_entry(
        worker,
        "/var/lib/kastellan/microvm".to_string(),
        searx,
        embed,
        None,
        &allowlist,
    );
    // Apply the REAL spawn-time rewrites: broker onto its bound UDS, then
    // force-routing sets proxy_uds (a VM Net::Allowlist worker is ALWAYS
    // force-routed — build_launch_plan rejects one without proxy_uds).
    let uds = PathBuf::from("/tmp/embed-77-0/embed.sock");
    let mut policy = rewrite_policy_for_broker(entry.policy, &uds, BrokerKind::Embed);
    policy.proxy_uds = Some(PathBuf::from("/tmp/egress-77-0/egress.sock"));

    let image = FirecrackerImage {
        kernel_path: "/img/vmlinux".into(),
        rootfs_path: "/img/web-research.ext4".into(),
    };
    let plan = build_launch_plan(&policy, &image, "/usr/local/bin/kastellan-worker-web-research", &[])
        .expect("a force-routed broker-backed policy builds a launch plan");

    // Broker channel fields flow from policy.broker_uds.
    assert_eq!(plan.broker_vsock_port, Some(BROKER_VSOCK_PORT));
    assert_eq!(plan.broker_host_uds.as_deref(), Some(uds.as_path()));
    assert!(
        plan.boot_args.contains(" kastellan.broker=1"),
        "boot_args must carry the broker token: {}",
        plan.boot_args
    );
    // The env the worker sees: the host UDS path (from the real rewrite) is
    // REWRITTEN to the in-guest relay path by the plan's value-match. Pins the
    // GUEST_BROKER_UDS wire contract (private to the sandbox crate; also asserted
    // by microvm-init — the manual cross-crate contract).
    let injected = plan
        .env
        .iter()
        .find(|(k, _)| k == BrokerKind::Embed.uds_env())
        .map(|(_, v)| v.as_str());
    assert_eq!(
        injected,
        Some("/run/kastellan-broker.sock"),
        "the broker UDS env must be rewritten from the host path to the guest relay path"
    );
    // The unreachable host path must NOT survive anywhere in the guest env.
    let host_str = uds.to_string_lossy();
    assert!(
        plan.env.iter().all(|(_, v)| *v != host_str),
        "the unreachable host broker path must not survive into the guest env"
    );
}

// ── live-tier harness (DGX-only) ────────────────────────────────────────────

/// Default SearxNG endpoint (loopback). In force-routed mode the egress proxy
/// reaches it via its literal-IP allowlist carve-out — the net_entries derived
/// from this endpoint include the literal `127.0.0.1:8888`. Override with
/// `KASTELLAN_WEB_RESEARCH_ENDPOINT` if SearxNG lives on a routable host.
const DEFAULT_SEARX_ENDPOINT: &str = "http://127.0.0.1:8888/search";
/// Default embed backend (loopback Ollama). Reached ONLY by the host broker;
/// the worker never has it in egress.
const DEFAULT_EMBED_ENDPOINT: &str = "http://127.0.0.1:11434/v1/embeddings";

/// The HOST backend (bwrap on Linux) for the egress-proxy sidecar AND the embed
/// broker — both are host-side services, never in the VM.
fn host_backend() -> Arc<dyn SandboxBackend> {
    SandboxBackends::default_for_current_os().resolve(None, None)
}

/// Resolve the host egress-proxy binary, or `[SKIP]` (return `None`).
fn egress_proxy_bin_or_skip() -> Option<PathBuf> {
    let p = workspace_target_binary("kastellan-worker-egress-proxy");
    if p.is_file() {
        Some(p)
    } else {
        eprintln!("[SKIP] egress-proxy not built; run `cargo build -p kastellan-worker-egress-proxy`");
        None
    }
}

async fn probe_and_pool(conn_spec: &kastellan_db::conn::ConnectSpec) -> sqlx::PgPool {
    kastellan_db::probe::run(
        conn_spec,
        "core",
        "startup",
        serde_json::json!({"version": "test", "purpose": "web-research-vm-broker-hybrid-e2e"}),
    )
    .await
    .expect("probe run");
    kastellan_db::pool::connect_runtime_pool(conn_spec)
        .await
        .expect("connect runtime pool")
}

/// Bare host of a URL (for the content allowlist entry).
fn url_host(endpoint: &str) -> String {
    url::Url::parse(endpoint)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_else(|| "127.0.0.1".to_string())
}

/// Live full-stack: a web-research worker booted in a Firecracker VM ranks
/// `hybrid` by embedding through the host broker over vsock 1026, while SearxNG
/// + content ride the host MITM egress sidecar over vsock 1025 — with the embed
/// host absent from egress. Uses the `sidecar_backend` seam (host proxy, VM
/// worker). Closes #445.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "DGX-only: real KVM + vsock + web-research rootfs + egress-proxy \
            sidecar + live SearxNG + live embed backend (embeddinggemma). \
            Asserts hybrid ranking from inside a VM with the embed host absent \
            from egress — embed rides vsock 1026 to the host broker."]
async fn brokered_vm_worker_ranks_hybrid_over_vsock_with_zero_embed_egress() {
    if skip_if_no_microvm(VM_ROOTFS) {
        return;
    }
    if skip_if_no_supervisor() {
        return;
    }
    if skip_if_sandbox_unavailable() {
        return;
    }
    let Some(bin_dir) = pg_bin_dir_or_skip() else {
        return;
    };
    let Some(proxy_bin) = egress_proxy_bin_or_skip() else {
        return;
    };

    // The web-research worker runs FROM THE ROOTFS at this baked in-guest path —
    // NOT the host `target/debug` binary, which does not exist inside the guest
    // (passing the host path bakes it into `kastellan.worker=`, the guest execv
    // fails ENOENT, PID1 panics). `skip_if_no_microvm` already gates on the rootfs
    // (built from that binary), so no host-path existence check is needed here.
    // (Contrast host-mode e2es, which DO run the host `workspace_target_binary`.)
    let worker_in_guest = "/usr/local/bin/kastellan-worker-web-research";
    // The embed broker runs host-side, so it IS the host binary.
    let broker_bin = workspace_target_binary("kastellan-worker-embed-broker");
    if !broker_bin.exists() {
        eprintln!("\n[SKIP] embed-broker binary not built; run cargo build --workspace\n");
        return;
    }

    let searx_endpoint = std::env::var("KASTELLAN_WEB_RESEARCH_ENDPOINT")
        .unwrap_or_else(|_| DEFAULT_SEARX_ENDPOINT.to_string());
    let embed_endpoint = std::env::var("KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT")
        .unwrap_or_else(|_| DEFAULT_EMBED_ENDPOINT.to_string());

    let suffix = unique_suffix();
    let cluster = bring_up_pg_cluster(
        &bin_dir,
        "wrb-d",
        "wrb-l",
        &format!("kastellan-supervisor-test-pg-wrbroker-{suffix}"),
    );
    let pool = probe_and_pool(&cluster.conn_spec).await;

    let vm_backend = firecracker_backend();
    let host_backend = host_backend();

    // Allowlist: the SearxNG endpoint host (validate_endpoint + net_entries →
    // the literal 127.0.0.1:8888 for the proxy carve-out) + the content host.
    // NOT the embed host — its only path is the broker over vsock 1026.
    let content_host = "en.wikipedia.org".to_string();
    let allowlist = vec![url_host(&searx_endpoint), content_host];

    // VM×broker manifest entry: embed host absent from egress, broker spec
    // carries the backend the broker forwards to.
    let entry = web_research_firecracker_broker_entry(
        PathBuf::from(worker_in_guest),
        image_dir(),
        &searx_endpoint,
        &embed_endpoint,
        None, // default embed model (embeddinggemma)
        &allowlist,
    );
    let broker_spec = entry
        .broker
        .as_ref()
        .expect("VM broker-mode entry declares a broker spec");

    // Spawn the real embed broker on the HOST, pointed at the live backend. Its
    // scratch (and UDS) live under /tmp so the VMM jail can bind the UDS
    // (confine.rs) and the second vsock relay (port 1026) can reach it.
    let broker_cfg = BrokerConfig::new(BrokerKind::Embed, broker_bin.clone(), std::env::temp_dir());
    let (broker_sidecar, broker_uds) = spawn_broker(&broker_cfg, broker_spec, &*host_backend)
        .expect("spawn embed-broker sidecar under the host sandbox");
    assert!(broker_uds.exists(), "broker must bind its UDS at {broker_uds:?}");

    // Real production rewrite onto the bound broker UDS (sets broker_uds +
    // injects KASTELLAN_EMBED_BROKER_UDS).
    let policy = rewrite_policy_for_broker(entry.policy, &broker_uds, BrokerKind::Embed);

    // The egress-proxy allowlist mirrors production: `spawn_worker_maybe_forced`
    // derives it from the worker's `policy.net`, so `net_entries` put the
    // SearxNG endpoint authority (`127.0.0.1:8888` — the literal the proxy's
    // loopback carve-out needs) and the content host here, with the embed host
    // already dropped. Passing a hand-built bare-host allowlist instead would
    // diverge from production AND miss the port the carve-out matches on. Also
    // re-assert zero-embed-egress here (fail-closed on a non-Allowlist variant
    // — matches the hermetic pin).
    let proxy_allowlist: Vec<String> = match &policy.net {
        Net::Allowlist(entries) => {
            assert!(
                entries.iter().all(|e| !e.starts_with("127.0.0.1:11434")),
                "embed host must be absent from egress; got {entries:?}"
            );
            entries.clone()
        }
        other => panic!("expected Net::Allowlist in broker mode, got {other:?}"),
    };

    // Force-route the VM worker onto a HOST MITM egress sidecar. broker_uds
    // survives the force-route clone, so both vsock channels (1025 egress, 1026
    // broker) are live. disable_mitm: false → the sidecar delivers its
    // per-instance CA in-guest (the web-research rootfs ships no system CA).
    let spec = WorkerSpec {
        policy: &policy,
        program: worker_in_guest,
        args: &[],
        // VM mode is slower than host mode (boot + vsock + MITM proxy + a
        // multi-page fetch + a brokered embed), so give the dispatch generous
        // headroom over the host-mode 60s.
        wall_clock_ms: Some(120_000),
    };
    let params = NetWorkerSpawn {
        backend: &*vm_backend,           // worker → VM
        sidecar_backend: &*host_backend, // egress proxy → host
        proxy_bin: &proxy_bin,
        spec: &spec,
        allowlist: &proxy_allowlist,
        worker_name: "web-research",
        secret_fingerprints: &[],
        cert_pins_json: None,
        disable_mitm: false, // MITM: deliver the per-instance CA into the VM
    };
    // Print each egress decision (allow/deny + host) so a live-tier failure is
    // diagnosable — which CONNECTs the proxy saw and whether any were blocked.
    let mut worker = spawn_forced_net_worker(&params, std::path::Path::new("/tmp"), |row| {
        eprintln!("[egress-decision] {} {}", row.action, row.payload);
    })
    .expect("force-route the VM web-research worker onto a host MITM egress sidecar");

    let result = dispatch(
        &pool,
        &Vault::new(),
        &mut worker,
        "web-research",
        "web.research",
        serde_json::json!({"query": "rust programming language", "max_sources": 2}),
    )
    .await
    .expect("web.research round trip (VM search + fetch over vsock 1025 + brokered embed over vsock 1026)");

    assert_eq!(
        result["ranking"], "hybrid",
        "expected hybrid ranking via the broker over vsock 1026 (embed host absent from egress); \
         embed_note: {:?}",
        result.get("embed_note")
    );

    let _ = worker.close();
    drop(broker_sidecar);
    pool.close().await;
}
