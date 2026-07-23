//! mail: read-only access to a localmail archive over its /v1 REST API.
//! Search, message + attachment retrieval; attachments delivered as extracted
//! text or as original-format files written to the task workspace out/ dir.
//! Design: docs/superpowers/specs/2026-07-22-localmail-mail-worker-integration-design.md

mod client;
mod handler;

use kastellan_worker_prelude::serve_stdio;

fn main() -> anyhow::Result<()> {
    let mut handler = handler::MailHandler::from_env()?;
    serve_stdio(&mut handler)?;
    Ok(())
}
