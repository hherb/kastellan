//! kastellan-worker-matrix: the sandboxed Matrix channel worker. Wraps
//! matrix-rust-sdk (login + E2E sync loop + buffered inbound) behind a JSON-RPC
//! stdio surface (`matrix.init` / `matrix.poll` / `matrix.send`). Design:
//! docs/superpowers/specs/2026-06-12-matrix-inbound-sandboxed-worker-design.md
//!
//! The real matrix-rust-sdk integration ([`sdk_live::LiveSdk`]) is gated behind
//! the `live-matrix` feature (Phase D, verified on the DGX). The default build
//! compiles only the hermetic parts (the handler + SDK seam, exercised by unit
//! tests) and refuses to run — it has no live client to serve.
//!
//! ### Lockdown ordering
//!
//! `LiveSdk::connect` does the network-needing init (login + first sync, through
//! the egress bridge) **first**, then the worker applies `rlimit` + `lock_down`,
//! then serves. This mirrors the egress proxy's "do the network init, THEN lock
//! down" order: the background sync task keeps running under the `net_client`
//! seccomp profile, which permits ongoing socket I/O.

// Without `live-matrix` the handler + SDK seam + `ProxyBridge` are exercised only
// by unit/spike tests, so they read as dead code in a default (non-feature)
// build. WITH `live-matrix` everything is live (LiveSdk consumes the seam +
// ProxyBridge; main serves), so no allowance is needed.
#![cfg_attr(not(feature = "live-matrix"), allow(dead_code))]
// matrix-sdk 0.18's crypto types nest deeply enough that the `Send` auto-trait
// solver overflows the default recursion limit when the sync task's future is
// type-checked (matrix-sdk-crypto account.rs). Raising the limit is the
// SDK-recommended fix; it affects only compile-time trait evaluation.
#![recursion_limit = "256"]

mod bridge;
mod handler;
mod sdk;
// Pure sync-retry policy: not feature-gated so its unit tests run in the default
// build (the `live-matrix` SDK glue that consumes it is DGX-gated — cf. #331).
mod sync_retry;

#[cfg(feature = "live-matrix")]
mod sdk_live;

#[cfg(all(test, feature = "live-matrix"))]
mod egress_spike;

#[cfg(feature = "live-matrix")]
fn main() -> anyhow::Result<()> {
    use handler::MatrixHandler;
    use kastellan_worker_prelude::{lock_down, rlimit};
    use sdk_live::{LiveSdk, LiveSdkConfig};

    let config = LiveSdkConfig::from_env()?;

    // 1) Network-needing init FIRST: login + persistent store + first sync,
    //    routed through the egress bridge when force-routed.
    let sdk = LiveSdk::connect(config)?;

    // 2) THEN lock down: rlimit (CPU) before any syscall restriction, then
    //    Landlock + seccomp (`net_client`, so the sync task can keep talking).
    rlimit::apply_from_env().map_err(|e| anyhow::anyhow!("rlimit: {e}"))?;
    lock_down().map_err(|e| anyhow::anyhow!("lockdown: {e}"))?;

    // 3) Serve the JSON-RPC surface. The raw protocol `serve_stdio` is used (not
    //    the prelude's, which would lock down a second time) since lockdown has
    //    already happened above, after the network init.
    let mut handler = MatrixHandler::new(sdk);
    kastellan_protocol::server::serve_stdio(&mut handler)
        .map_err(|e| anyhow::anyhow!("serve stdio: {e}"))?;
    Ok(())
}

#[cfg(not(feature = "live-matrix"))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!(
        "kastellan-worker-matrix was built without the `live-matrix` feature; \
         rebuild with `--features live-matrix` to run the real Matrix client"
    )
}
