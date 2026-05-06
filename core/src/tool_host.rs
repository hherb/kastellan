//! tool_host: spawn sandboxed worker processes and talk to them over the
//! JSON-RPC stdio protocol from `hhagent_protocol`.
//!
//! The agent core is the only thing that ever spawns a worker. Spawning goes
//! through the configured [`SandboxBackend`] so workers cannot run unjailed
//! by accident — there is intentionally no "spawn unsandboxed" escape hatch.
//!
//! Phase 0 covers single-shot spawn-and-talk usage. Long-lived workers,
//! restart-on-crash supervision, and per-worker UDS multiplexing are
//! follow-on work.

use hhagent_protocol::client::{Client, ClientError};
use hhagent_sandbox::{SandboxBackend, SandboxError, SandboxPolicy};

#[derive(Debug, thiserror::Error)]
pub enum ToolHostError {
    #[error("sandbox: {0}")]
    Sandbox(#[from] SandboxError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol: {0}")]
    Protocol(#[from] ClientError),
}

/// What to launch and how to jail it.
pub struct WorkerSpec<'a> {
    pub policy: &'a SandboxPolicy,
    /// Absolute path of the worker binary, as visible *inside* the jail.
    /// Caller must add the binary's host path (or its parent dir) to
    /// `policy.fs_read` so bwrap can mount it.
    pub program: &'a str,
    pub args: &'a [&'a str],
}

/// Spawn the worker under `backend` and return a connected JSON-RPC client.
pub fn spawn_worker<B>(backend: &B, spec: &WorkerSpec<'_>) -> Result<Client, ToolHostError>
where
    B: SandboxBackend + ?Sized,
{
    let child = backend.spawn_under_policy(spec.policy, spec.program, spec.args)?;
    Ok(Client::from_child(child)?)
}
