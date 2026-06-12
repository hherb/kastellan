//! kastellan-worker-matrix: the sandboxed Matrix channel worker. Wraps
//! matrix-rust-sdk (login + E2E sync loop + buffered inbound) behind a JSON-RPC
//! stdio surface (`matrix.init` / `matrix.poll` / `matrix.send`), served via the
//! prelude's `serve_stdio` after `lock_down`. Design:
//! docs/superpowers/specs/2026-06-12-matrix-inbound-sandboxed-worker-design.md
//!
//! The real matrix-rust-sdk integration is gated behind the `live-matrix`
//! feature (Phase D, verified on the DGX). The default build compiles the
//! handler + seam (hermetically tested) but refuses to run — it has no SDK.

// In the default (non-live) build the handler + SDK seam are exercised only by
// unit tests and by the `live-matrix` build, so they are dead code in the plain
// bin target. Allow it there; the live build + tests use everything.
#![cfg_attr(not(feature = "live-matrix"), allow(dead_code))]

mod handler;
mod sdk;

#[cfg(feature = "live-matrix")]
mod sdk_live;

fn main() -> anyhow::Result<()> {
    #[cfg(feature = "live-matrix")]
    {
        // Phase D: build the live SDK (login + first sync through the egress
        // proxy UDS — network needed here), THEN lock_down, THEN serve. See
        // sdk_live.rs.
        let sdk = sdk_live::LiveSdk::from_env()?;
        kastellan_worker_prelude::lock_down()?;
        let mut h = handler::MatrixHandler::new(sdk);
        kastellan_protocol::server::serve_stdio(&mut h)?;
        Ok(())
    }
    #[cfg(not(feature = "live-matrix"))]
    {
        anyhow::bail!(
            "kastellan-worker-matrix was built without the `live-matrix` feature; \
             rebuild with `--features live-matrix` to run the real Matrix client"
        )
    }
}
