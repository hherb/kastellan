//! mail: read-only access to a localmail archive over its /v1 REST API.
//! Search, message + attachment retrieval; attachments delivered as extracted
//! text or as original-format files written to the task workspace out/ dir.
//! Design: docs/superpowers/specs/2026-07-22-localmail-mail-worker-integration-design.md

mod attach;
mod client;
mod detail;
mod handler;
mod headers;
mod ids;
mod problem;
mod search_params;
mod sort;
mod text_page;
mod version;

use kastellan_worker_prelude::serve_stdio_with;

fn main() -> anyhow::Result<()> {
    // Lock down FIRST, then construct (security audit 2026-09-02, F4): the
    // handler's `from_env` builds the egress-proxy CONNECT client's tokio
    // runtime (or a `reqwest::blocking` runtime thread in legacy mode), and
    // Landlock covers only threads created *after* `restrict_self` — the
    // network-facing threads were the ones without it.
    serve_stdio_with(handler::MailHandler::from_env)?;
    Ok(())
}
