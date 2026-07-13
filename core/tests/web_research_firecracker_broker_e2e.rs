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
//! * (DGX-only, `#[ignore]`, authored on the DGX in the gate run) the live
//!   full-stack tier: real KVM + vsock + a live egress path (real egress-proxy
//!   sidecar → live SearxNG + content) + the real broker → a live embed backend,
//!   asserting `ranking == "hybrid"` from inside a VM with the embed host absent
//!   from egress — strictly stronger than Slice C's host-mode test because the
//!   worker is VM-isolated and its embed reaches the host broker only over vsock
//!   port 1026. Composed from `web_research_firecracker_egress_e2e.rs` (force-routed
//!   web-research VM) + `net_demo_firecracker_egress_e2e.rs` (real egress proxy over
//!   vsock) + `embed_broker_egress_e2e.rs` (spawn_broker + hybrid assertion). Kept
//!   out of this file until it is verified live so no unverified VM e2e body ships.
//!   **DEFERRED to issue #445** (blocker: web-research needs a MITM egress proxy, but
//!   the reusable `spawn_net_transport` is transparent-tunnel-only and the MITM
//!   force-route spawn is `pub(crate)` — the live tier needs a production test-seam).
//!   The refactored guest relay itself IS live-verified on real KVM: the existing
//!   `web_research_firecracker_egress_e2e` boots a VM through it and the egress
//!   channel delivers the CONNECT (`/run`-mount-once + relay generalization intact).
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

use kastellan_core::broker::BrokerKind;
use kastellan_core::worker_lifecycle::force_route::rewrite_policy_for_broker;
use kastellan_core::workers::web_research::web_research_firecracker_broker_entry;
use kastellan_sandbox::linux_firecracker::{build_launch_plan, FirecrackerImage, BROKER_VSOCK_PORT};
use kastellan_sandbox::{Net, SandboxBackendKind};

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
