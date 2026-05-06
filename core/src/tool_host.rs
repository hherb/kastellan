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
use hhagent_sandbox::{Profile, SandboxBackend, SandboxError, SandboxPolicy};

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

/// Env var name read by `hhagent-worker-prelude::landlock_lock` for the
/// JSON-encoded list of writable scratch paths. Workers using
/// `prelude::serve_stdio` get a Landlock filter built from this.
pub const ENV_LANDLOCK_RW: &str = "HHAGENT_LANDLOCK_RW";
/// Env var name read by `hhagent-worker-prelude::seccomp_lock` for the
/// per-worker seccomp profile selector.
pub const ENV_SECCOMP_PROFILE: &str = "HHAGENT_SECCOMP_PROFILE";

/// Spawn the worker under `backend` and return a connected JSON-RPC client.
///
/// Before spawning, [`derive_lockdown_env`] augments the policy with the
/// `HHAGENT_LANDLOCK_RW` + `HHAGENT_SECCOMP_PROFILE` env entries that
/// `hhagent-worker-prelude` reads at worker start-up. This is the
/// chokepoint for the worker-side defence-in-depth layer: callers cannot
/// accidentally skip it because tool_host always derives the env, and
/// the worker installs the filters from inside its own process.
pub fn spawn_worker<B>(backend: &B, spec: &WorkerSpec<'_>) -> Result<Client, ToolHostError>
where
    B: SandboxBackend + ?Sized,
{
    let derived = derive_lockdown_env(spec.policy);
    let child = backend.spawn_under_policy(&derived, spec.program, spec.args)?;
    Ok(Client::from_child(child)?)
}

/// Pure transform: clone `policy` and append the worker-prelude lockdown
/// env entries that aren't already present. Callers that explicitly set
/// either env var win — useful in tests and for future per-worker overrides
/// (e.g. a probe worker that needs `HHAGENT_SECCOMP_PROFILE=none`).
///
/// Exposed for unit testing the env-derivation logic without spinning up
/// a real sandbox.
pub fn derive_lockdown_env(policy: &SandboxPolicy) -> SandboxPolicy {
    let mut out = policy.clone();
    let has_landlock = out.env.iter().any(|(k, _)| k == ENV_LANDLOCK_RW);
    let has_seccomp = out.env.iter().any(|(k, _)| k == ENV_SECCOMP_PROFILE);

    if !has_landlock {
        let rw_paths: Vec<String> = out
            .fs_write
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        // serde_json on a Vec<String> is infallible — `unwrap` is safe here.
        let json = serde_json::to_string(&rw_paths).unwrap();
        out.env.push((ENV_LANDLOCK_RW.into(), json));
    }
    if !has_seccomp {
        let value = match out.profile {
            Profile::WorkerStrict => "strict",
            Profile::WorkerNetClient => "net_client",
        };
        out.env.push((ENV_SECCOMP_PROFILE.into(), value.into()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use hhagent_sandbox::Net;
    use std::path::PathBuf;

    fn base_policy() -> SandboxPolicy {
        SandboxPolicy {
            fs_read: vec![],
            fs_write: vec![],
            net: Net::Deny,
            cpu_ms: 1_000,
            mem_mb: 64,
            profile: Profile::WorkerStrict,
            env: vec![],
        }
    }

    #[test]
    fn derive_adds_strict_profile_for_default() {
        let derived = derive_lockdown_env(&base_policy());
        let seccomp = derived
            .env
            .iter()
            .find(|(k, _)| k == ENV_SECCOMP_PROFILE)
            .expect("seccomp env must be derived");
        assert_eq!(seccomp.1, "strict");
    }

    #[test]
    fn derive_adds_net_client_profile() {
        let mut p = base_policy();
        p.profile = Profile::WorkerNetClient;
        let derived = derive_lockdown_env(&p);
        let seccomp = derived
            .env
            .iter()
            .find(|(k, _)| k == ENV_SECCOMP_PROFILE)
            .unwrap();
        assert_eq!(seccomp.1, "net_client");
    }

    #[test]
    fn derive_serialises_fs_write_into_landlock_env() {
        let mut p = base_policy();
        p.fs_write = vec![PathBuf::from("/tmp/scratch_a"), PathBuf::from("/tmp/b")];
        let derived = derive_lockdown_env(&p);
        let landlock = derived
            .env
            .iter()
            .find(|(k, _)| k == ENV_LANDLOCK_RW)
            .unwrap();
        // Both paths must appear in the JSON. Exact-string assertion is OK
        // because serde_json on a Vec<String> is deterministic.
        assert_eq!(landlock.1, r#"["/tmp/scratch_a","/tmp/b"]"#);
    }

    #[test]
    fn derive_does_not_overwrite_caller_supplied_env() {
        let mut p = base_policy();
        p.env.push((ENV_SECCOMP_PROFILE.into(), "none".into()));
        let derived = derive_lockdown_env(&p);
        let seccomp_entries: Vec<_> = derived
            .env
            .iter()
            .filter(|(k, _)| k == ENV_SECCOMP_PROFILE)
            .collect();
        assert_eq!(
            seccomp_entries.len(),
            1,
            "caller-supplied env must not be duplicated"
        );
        assert_eq!(seccomp_entries[0].1, "none");
    }
}
