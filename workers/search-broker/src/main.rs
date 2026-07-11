//! Search broker sidecar binary.
//!
//! Spawned by core like the embed-broker: bind the UDS, apply the worker-prelude
//! lockdown, then serve JSON-RPC `search` over the socket, forwarding each to the
//! operator's SearxNG backend. Two env vars: `KASTELLAN_SEARCH_BROKER_UDS` (socket
//! path) and `KASTELLAN_SEARCH_BROKER_ENDPOINT` (the SearxNG search URL).

use std::os::unix::net::UnixListener;

use kastellan_worker_web_common::allowlist::HostAllowlist;
use kastellan_worker_web_common::search::validate_endpoint;
use kastellan_worker_search_broker::{serve_connection, SearchHandler};

fn main() -> anyhow::Result<()> {
    let uds = std::env::var("KASTELLAN_SEARCH_BROKER_UDS")
        .map_err(|_| anyhow::anyhow!("KASTELLAN_SEARCH_BROKER_UDS unset"))?;
    let endpoint_raw = std::env::var("KASTELLAN_SEARCH_BROKER_ENDPOINT")
        .map_err(|_| anyhow::anyhow!("KASTELLAN_SEARCH_BROKER_ENDPOINT unset"))?;

    // One backend → the allowlist IS its host. `from_endpoints` with a bare host
    // yields an any-port rule so `is_allowed(host)` passes in `validate_endpoint`
    // and `search()`. Validate the endpoint (https anywhere; http for loopback
    // only) and fail closed BEFORE binding if it is malformed or off-policy.
    let host = url::Url::parse(&endpoint_raw)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .ok_or_else(|| anyhow::anyhow!("KASTELLAN_SEARCH_BROKER_ENDPOINT has no host"))?;
    let allowlist = HostAllowlist::from_endpoints(&[host]);
    let endpoint = validate_endpoint(&endpoint_raw, &allowlist)
        .map_err(|e| anyhow::anyhow!("search-broker endpoint rejected: {e:?}"))?;

    // A remote/TLS backend needs the rustls provider up front; a loopback http
    // backend never builds a TLS config, so this is a no-op there.
    if endpoint.scheme() == "https" {
        kastellan_worker_web_common::http::ensure_crypto_provider();
    }
    let transport = kastellan_worker_web_common::http::make_get("kastellan-search-broker/0")?;

    // Bind BEFORE lock-down (Landlock forbids fs mutation after) — the embed-broker
    // / egress-proxy ordering.
    let _ = std::fs::remove_file(&uds);
    let listener = UnixListener::bind(&uds)?;

    // Worker-side defense-in-depth (Linux Landlock+seccomp; no-op on macOS, where
    // the parent Seatbelt profile contains us).
    let _report = kastellan_worker_prelude::lock_down()?;

    let mut handler = SearchHandler::new(endpoint, allowlist, transport);
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        if let Err(e) = serve_connection(&mut handler, conn) {
            eprintln!("search-broker: connection error: {e}");
        }
    }
    Ok(())
}
