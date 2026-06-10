//! egress-proxy: a per-worker egress boundary. Listens on a UDS, enforces the
//! worker's host allowlist + SSRF/IP defense per CONNECT, tunnels to the pinned
//! IP. Slice #1: no TLS interception, no live worker routing.
//! Design: docs/superpowers/specs/2026-06-10-egress-proxy-boundary-enforcement-design.md
//!
//! Env contract (set by the host-side `core::egress::spawn_sidecar`):
//!   HHAGENT_EGRESS_PROXY_UDS       — absolute path of the UDS to bind.
//!   HHAGENT_EGRESS_PROXY_ALLOWLIST — JSON array of allowed host strings. Slice
//!       #1 matches the *host* only — an allowlisted host is reachable on any
//!       port. Port-scoped endpoints (`host:443`) land with slice #2's live
//!       routing (see `proxy::decide`).
//!   HHAGENT_EGRESS_PROXY_WORKER    — the calling worker's name (for audit).

mod proxy;
mod report;
mod request_line;
mod ssrf;

use std::os::unix::net::UnixListener;

use hhagent_worker_web_common::allowlist::HostAllowlist;

use proxy::{handle_conn, StdResolve};
use report::LineReporter;

fn main() -> anyhow::Result<()> {
    let uds = std::env::var("HHAGENT_EGRESS_PROXY_UDS")
        .map_err(|_| anyhow::anyhow!("HHAGENT_EGRESS_PROXY_UDS unset"))?;
    let allow_json = std::env::var("HHAGENT_EGRESS_PROXY_ALLOWLIST")
        .map_err(|_| anyhow::anyhow!("HHAGENT_EGRESS_PROXY_ALLOWLIST unset"))?;
    let worker = std::env::var("HHAGENT_EGRESS_PROXY_WORKER").unwrap_or_else(|_| "unknown".into());
    let allow = HostAllowlist::from_env_json(&allow_json)?;

    // Bind the UDS *before* lock-down (Landlock will forbid fs mutation after).
    let _ = std::fs::remove_file(&uds);
    let listener = UnixListener::bind(&uds)?;

    // Worker-side defense-in-depth (Linux Landlock+seccomp; no-op on macOS,
    // where the parent Seatbelt profile contains us). Outbound socket(2) +
    // AF_UNIX accept must remain permitted — see the net_client profile.
    // NOTE (Linux verification, run on the DGX — tracked in #243): confirm the
    // seccomp profile permits AF_UNIX bind/listen/accept *and* AF_INET connect
    // for a process that both serves and dials; widen `seccomp_lock` if `accept`
    // is refused.
    let _report = hhagent_worker_prelude::lock_down()?;

    let resolver = StdResolve;
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let allow = &allow;
        let worker = worker.clone();
        // One thread per connection; the proxy is SingleUse + short-lived.
        std::thread::scope(|s| {
            s.spawn(|| {
                let mut reporter = LineReporter { out: std::io::stdout().lock() };
                handle_conn(conn, &worker, allow, &resolver, &mut reporter);
            });
        });
    }
    Ok(())
}
