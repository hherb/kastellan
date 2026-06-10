//! egress-proxy: a per-worker egress boundary. Listens on a UDS, enforces the
//! worker's host allowlist + SSRF/IP defense per CONNECT, tunnels to the pinned
//! IP. Slice #1: no TLS interception, no live worker routing.
//! Design: docs/superpowers/specs/2026-06-10-egress-proxy-boundary-enforcement-design.md

mod ssrf;

fn main() -> anyhow::Result<()> {
    Ok(())
}
