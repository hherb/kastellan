//! web-fetch: fetch a URL (HTTPS-only, against a host allowlist) and return
//! extracted readable text over JSON-RPC stdio. GET-only; no caller-supplied
//! headers/body. Design:
//! docs/superpowers/specs/2026-06-08-web-fetch-worker-design.md

mod allowlist;
mod extract;
mod fetch;
mod handler;

use hhagent_worker_prelude::serve_stdio;

fn main() -> anyhow::Result<()> {
    let mut handler = handler::WebFetchHandler::from_env()?;
    serve_stdio(&mut handler)?;
    Ok(())
}
