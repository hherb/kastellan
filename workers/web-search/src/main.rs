//! web-search: query an operator-configured SearxNG instance and return ranked
//! structured hits over JSON-RPC stdio. Design:
//! docs/superpowers/specs/2026-06-09-web-search-worker-design.md

mod parse;
mod search;

fn main() -> anyhow::Result<()> {
    Ok(())
}
