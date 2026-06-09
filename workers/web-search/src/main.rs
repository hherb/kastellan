//! web-search: query an operator-configured SearxNG instance and return ranked
//! structured hits over JSON-RPC stdio. GET-only; the LLM supplies only the
//! query string. Design:
//! docs/superpowers/specs/2026-06-09-web-search-worker-design.md

mod handler;
mod parse;
mod search;

use hhagent_worker_prelude::serve_stdio;

fn main() -> anyhow::Result<()> {
    let mut handler = handler::WebSearchHandler::from_env()?;
    serve_stdio(&mut handler)?;
    Ok(())
}
