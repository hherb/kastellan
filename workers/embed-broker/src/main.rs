//! Embedding broker sidecar binary.
//!
//! Spawned by core (Slice B) like the egress proxy: it binds its UDS, applies
//! the worker-prelude lockdown, then serves JSON-RPC `embed` requests over the
//! socket, forwarding each to the operator's embedding backend. Two env vars:
//! `KASTELLAN_EMBED_BROKER_UDS` (socket path) and `KASTELLAN_EMBED_BROKER_ENDPOINT`
//! (the backend's OpenAI-compatible embeddings URL).

use std::os::unix::net::UnixListener;

use kastellan_worker_embed_broker::{serve_connection, EmbedHandler};

fn main() -> anyhow::Result<()> {
    let uds = std::env::var("KASTELLAN_EMBED_BROKER_UDS")
        .map_err(|_| anyhow::anyhow!("KASTELLAN_EMBED_BROKER_UDS unset"))?;
    let endpoint_raw = std::env::var("KASTELLAN_EMBED_BROKER_ENDPOINT")
        .map_err(|_| anyhow::anyhow!("KASTELLAN_EMBED_BROKER_ENDPOINT unset"))?;
    let endpoint = url::Url::parse(&endpoint_raw)
        .map_err(|e| anyhow::anyhow!("KASTELLAN_EMBED_BROKER_ENDPOINT is not a URL: {e}"))?;

    // The backend transport: direct (loopback Ollama/vLLM) in v1. `make_get`
    // returns a proxy-connect transport only if KASTELLAN_EGRESS_PROXY_UDS is set
    // (a remote backend force-routed through the egress proxy — out of scope here).
    let transport = kastellan_worker_web_common::http::make_get("kastellan-embed-broker/0")?;

    // Bind the UDS BEFORE lock-down (Landlock forbids fs mutation after) — the
    // same ordering the egress proxy uses.
    let _ = std::fs::remove_file(&uds);
    let listener = UnixListener::bind(&uds)?;

    // Worker-side defense-in-depth (Linux Landlock+seccomp; no-op on macOS, where
    // the parent Seatbelt profile contains us). The net_client profile must permit
    // AF_UNIX accept + AF_INET connect (serve + dial) — verified on the DGX in Slice B.
    let _report = kastellan_worker_prelude::lock_down()?;

    let mut handler = EmbedHandler::new(transport, endpoint);
    // Connections are handled serially: one web-research worker per broker, and
    // its embeds are sequential. Each connection runs to EOF, then the next is
    // accepted. (Thread-per-connection can come with a second consumer.)
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        if let Err(e) = serve_connection(&mut handler, conn) {
            eprintln!("embed-broker: connection error: {e}");
        }
    }
    Ok(())
}
