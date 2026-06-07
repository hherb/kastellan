//! web-fetch: fetch a URL (HTTPS-only, against a host allowlist) and return
//! extracted readable text over JSON-RPC stdio. GET-only; no caller-supplied
//! headers/body. Design:
//! docs/superpowers/specs/2026-06-08-web-fetch-worker-design.md

mod allowlist;

fn main() -> anyhow::Result<()> {
    Ok(())
}
