//! net-demo: a minimal LONG-LIVED `Net::Allowlist` worker that does its OWN
//! end-to-end TLS to an origin through the per-worker egress proxy's UDS
//! (transparent tunnel — the proxy never terminates the TLS). It exists to
//! exercise slice 5c: network egress inside a persistent VM. `net.stats` proves
//! many-calls-one-boot; `net.tls_probe` proves the transparent-tunnel TLS path.
//!
//! Env: `KASTELLAN_EGRESS_PROXY_UDS` (the proxy socket the worker dials) and the
//! optional test-only `KASTELLAN_NETDEMO_EXTRA_CA` (a self-signed loopback
//! origin's cert, added on top of the compiled-in webpki roots for hermetic e2e).
use std::path::PathBuf;

use kastellan_protocol::{codes, server::Handler, RpcError};
use kastellan_worker_prelude::serve_stdio;
use serde::Deserialize;

// `host`/`port` are read by `net.tls_probe` (wired in Task 3); until then the
// stub only validates the params shape, so silence the dead-field lint here.
#[derive(Deserialize)]
#[allow(dead_code)]
struct ProbeParams {
    host: String,
    #[serde(default)]
    port: Option<u16>,
}

// `uds`/`extra_ca` are read by `net.tls_probe` (wired in Task 3); silence the
// dead-field lint until then.
#[allow(dead_code)]
struct NetHandler {
    uds: Option<PathBuf>,
    extra_ca: Option<PathBuf>,
    calls_served: u64,
}

impl NetHandler {
    fn new(uds: Option<PathBuf>, extra_ca: Option<PathBuf>) -> Self {
        Self { uds, extra_ca, calls_served: 0 }
    }
}

impl Handler for NetHandler {
    fn call(&mut self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, RpcError> {
        self.calls_served += 1;
        match method {
            "net.tls_probe" => {
                let _p: ProbeParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}")))?;
                // Filled in Task 3.
                Err(RpcError::new(codes::OPERATION_FAILED, "net.tls_probe not yet implemented"))
            }
            "net.stats" => Ok(serde_json::json!({
                "calls_served": self.calls_served,
                "pid": std::process::id(),
            })),
            // net.crash: deterministic worker-death trigger for lifecycle e2e.
            // Exits without replying so the caller sees an I/O error, which
            // PersistentWorker treats as a death and respawns. Debug-only.
            #[cfg(debug_assertions)]
            "net.crash" => std::process::exit(1),
            other => Err(RpcError::new(codes::METHOD_NOT_FOUND, format!("unknown method {other}"))),
        }
    }
}

fn main() -> anyhow::Result<()> {
    let uds = std::env::var("KASTELLAN_EGRESS_PROXY_UDS").ok().map(PathBuf::from);
    let extra_ca = std::env::var("KASTELLAN_NETDEMO_EXTRA_CA").ok().map(PathBuf::from);
    let mut handler = NetHandler::new(uds, extra_ca);
    serve_stdio(&mut handler)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_counts_calls_and_reports_pid() {
        let mut h = NetHandler::new(None, None);
        let s1 = h.call("net.stats", serde_json::json!({})).unwrap();
        assert_eq!(s1["calls_served"], 1);
        assert_eq!(s1["pid"].as_u64(), Some(std::process::id() as u64));
        let s2 = h.call("net.stats", serde_json::json!({})).unwrap();
        assert_eq!(s2["calls_served"], 2);
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let mut h = NetHandler::new(None, None);
        let err = h.call("net.nope", serde_json::json!({})).unwrap_err();
        assert_eq!(err.code, codes::METHOD_NOT_FOUND);
    }

    #[test]
    fn tls_probe_rejects_bad_params() {
        let mut h = NetHandler::new(None, None);
        // Missing required `host`.
        let err = h.call("net.tls_probe", serde_json::json!({"port": 443})).unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
    }
}
