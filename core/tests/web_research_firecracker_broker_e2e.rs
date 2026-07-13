#![cfg(target_os = "linux")]
//! VM × embed-broker e2e: a web-research worker in a Firecracker VM embeds through
//! the host-side broker over the second vsock channel (port 1026), with the embed
//! host absent from its egress — the VM analogue of Slice C's zero-embed-egress
//! hybrid property (`embed_broker_egress_e2e.rs`).
//!
//! ## Tiers
//!
//! * `vm_broker_policy_has_broker_uds_and_zero_embed_egress` (hermetic, always
//!   runs on Linux; no KVM/network): pins the post-rewrite VM broker policy the
//!   live tier depends on — VM backend + a broker spec, the broker UDS bound +
//!   injected by the REAL `rewrite_policy_for_broker`, the embed host absent from
//!   `Net::Allowlist`, the direct embed-endpoint env omitted, the embed model env
//!   present. This guards the production `web_research_firecracker_broker_entry` +
//!   rewrite pair against drift.
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

use std::path::PathBuf;

use kastellan_core::broker::BrokerKind;
use kastellan_core::worker_lifecycle::force_route::rewrite_policy_for_broker;
use kastellan_core::workers::web_research::web_research_firecracker_broker_entry;
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
