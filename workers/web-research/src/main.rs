//! web-research: one-call web research — SearxNG search, fetch the top-N
//! allowlisted result pages, extract readable text, and return the passages
//! most relevant to the query over JSON-RPC stdio. GET-only; the LLM supplies
//! only the query string. Design:
//! docs/superpowers/specs/2026-07-07-web-research-composite-worker-design.md

mod chunk;
mod embed;
mod handler;
mod rank;
mod research;

use kastellan_worker_prelude::serve_stdio;

fn main() -> anyhow::Result<()> {
    let mut handler = handler::WebResearchHandler::from_env()?;
    serve_stdio(&mut handler)?;
    Ok(())
}
