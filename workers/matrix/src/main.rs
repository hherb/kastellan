//! kastellan-worker-matrix: the sandboxed Matrix channel worker. Wraps
//! matrix-rust-sdk (login + E2E sync loop + buffered inbound) behind a JSON-RPC
//! stdio surface (`matrix.init` / `matrix.poll` / `matrix.send`), served via the
//! prelude's `serve_stdio` after `lock_down`. Design:
//! docs/superpowers/specs/2026-06-12-matrix-inbound-sandboxed-worker-design.md
//!
//! The real matrix-rust-sdk integration is gated behind the `live-matrix`
//! feature (Phase D, verified on the DGX). This slice lands the `matrix-sdk`
//! dependency and proves the egress transport (see `bridge.rs` + the
//! `egress_spike` test); the live serving path (`sdk_live::LiveSdk` + the
//! `serve_stdio` wiring) is the NEXT slice.

// This bin crate is mid-construction: the handler + SDK seam + the egress
// `ProxyBridge` are exercised by unit/spike tests and by the next slice's live
// wiring, but `main()` does not serve yet (it bails under both cfgs). Allow the
// resulting dead code crate-wide for now; the live-wiring slice narrows this
// back to `#![cfg_attr(not(feature = "live-matrix"), allow(dead_code))]`.
#![allow(dead_code)]

mod bridge;
mod handler;
mod sdk;

#[cfg(all(test, feature = "live-matrix"))]
mod egress_spike;

fn main() -> anyhow::Result<()> {
    #[cfg(feature = "live-matrix")]
    {
        anyhow::bail!(
            "kastellan-worker-matrix `live-matrix` build: the matrix-sdk egress \
             transport is proven (see bridge.rs + the egress_spike test) but the \
             live serving path (LiveSdk: login + sync loop + poll/send) is wired \
             in the next slice"
        )
    }
    #[cfg(not(feature = "live-matrix"))]
    {
        anyhow::bail!(
            "kastellan-worker-matrix was built without the `live-matrix` feature; \
             rebuild with `--features live-matrix` to run the real Matrix client"
        )
    }
}
