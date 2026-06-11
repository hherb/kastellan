//! egress-proxy: a per-worker egress boundary. Listens on a UDS, enforces the
//! worker's host allowlist + SSRF/IP defense per CONNECT, tunnels to the pinned
//! IP. For TLS tunnels it MITM-terminates with a per-instance CA leaf and
//! re-originates a validated TLS session to the origin (slice #3a inspection);
//! plaintext tunnels pass through untouched. The public CA cert is exported as
//! `ca.pem` next to the UDS for the host to inject into the worker's trust store.
//! Design: docs/superpowers/specs/2026-06-10-egress-proxy-boundary-enforcement-design.md
//!
//! Env contract (set by the host-side `core::egress::spawn_sidecar`):
//!   KASTELLAN_EGRESS_PROXY_UDS       — absolute path of the UDS to bind.
//!   KASTELLAN_EGRESS_PROXY_ALLOWLIST — JSON array of `host[:port]` endpoint
//!       strings. A `:port` suffix scopes the grant to that port (#241); a bare
//!       host grants any port (the weaker back-compat form, flagged in the audit
//!       reason). See `proxy::decide` / `proxy::allowed_reason`.
//!   KASTELLAN_EGRESS_PROXY_WORKER    — the calling worker's name (for audit).

mod ca;
mod leaf_cache;
mod mitm;
mod proxy;
mod report;
mod request_line;
mod ssrf;

use std::os::unix::net::UnixListener;

use kastellan_worker_web_common::allowlist::HostAllowlist;

use proxy::{handle_conn, MitmCtx, StdResolve};
use report::LineReporter;

fn main() -> anyhow::Result<()> {
    // rustls 0.23: install the ring provider as the process default (needed by
    // both the server-side leaf configs and the upstream client config).
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("install rustls ring provider"))?;

    let uds = std::env::var("KASTELLAN_EGRESS_PROXY_UDS")
        .map_err(|_| anyhow::anyhow!("KASTELLAN_EGRESS_PROXY_UDS unset"))?;
    let allow_json = std::env::var("KASTELLAN_EGRESS_PROXY_ALLOWLIST")
        .map_err(|_| anyhow::anyhow!("KASTELLAN_EGRESS_PROXY_ALLOWLIST unset"))?;
    let worker = std::env::var("KASTELLAN_EGRESS_PROXY_WORKER").unwrap_or_else(|_| "unknown".into());
    // Parse `host[:port]` endpoint entries so the boundary check is port-scoped
    // (#241), not host-only.
    let entries: Vec<String> = serde_json::from_str(&allow_json)
        .map_err(|e| anyhow::anyhow!("KASTELLAN_EGRESS_PROXY_ALLOWLIST is not a JSON array: {e}"))?;
    let allow = HostAllowlist::from_endpoints(&entries);

    // Bind the UDS *before* lock-down (Landlock will forbid fs mutation after).
    let _ = std::fs::remove_file(&uds);
    let listener = UnixListener::bind(&uds)?;

    // Generate the per-instance CA and export ONLY its public cert next to the
    // UDS, before lock-down. The host waits for this file and injects it into
    // the worker's trust store.
    let ca = std::sync::Arc::new(ca::generate_ca().map_err(|e| anyhow::anyhow!("generate CA: {e}"))?);
    let ca_path = std::path::Path::new(&uds)
        .parent()
        .ok_or_else(|| anyhow::anyhow!("UDS path has no parent dir"))?
        .join("ca.pem");
    std::fs::write(&ca_path, ca.cert_pem())
        .map_err(|e| anyhow::anyhow!("write CA cert {ca_path:?}: {e}"))?;

    // Upstream trust for the re-origination leg: the REAL public roots. The
    // proxy validates the true origin here (the worker only trusts our CA).
    let mut upstream_roots = rustls::RootCertStore::empty();
    upstream_roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let upstream_tls = std::sync::Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(upstream_roots)
            .with_no_client_auth(),
    );

    // Worker-side defense-in-depth (Linux Landlock+seccomp; no-op on macOS,
    // where the parent Seatbelt profile contains us). Outbound socket(2) +
    // AF_UNIX accept must remain permitted — see the net_client profile.
    // NOTE (Linux verification, run on the DGX — tracked in #243): confirm the
    // seccomp profile permits AF_UNIX bind/listen/accept *and* AF_INET connect
    // for a process that both serves and dials; widen `seccomp_lock` if `accept`
    // is refused.
    let _report = kastellan_worker_prelude::lock_down()?;

    let resolver = StdResolve;
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let allow = &allow;
        let worker = worker.clone();
        let ca = &ca;
        let upstream_tls = &upstream_tls;
        // One thread per connection; the proxy is SingleUse + short-lived.
        std::thread::scope(|s| {
            s.spawn(|| {
                let mut reporter = LineReporter { out: std::io::stdout().lock() };
                let mut cache = leaf_cache::LeafCache::new();
                let mut mitm = MitmCtx {
                    ca: ca.as_ref(),
                    leaf_cache: &mut cache,
                    upstream_tls: std::sync::Arc::clone(upstream_tls),
                };
                handle_conn(conn, &worker, allow, &resolver, &mut reporter, &mut mitm);
            });
        });
    }
    Ok(())
}
