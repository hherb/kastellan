//! Spawn the sandboxed egress-proxy sidecar on a per-worker UDS and wait for it
//! to be ready. Reusable host-side API; slice #2 calls this from the net-worker
//! bring-up path and ties `SidecarHandle::shutdown` to worker-terminal teardown.

use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};

use kastellan_sandbox::{Net, Profile, SandboxBackend, SandboxPolicy};

/// Env keys the sidecar binary reads (must match `egress-proxy::main`).
const ENV_UDS: &str = "KASTELLAN_EGRESS_PROXY_UDS";
const ENV_ALLOWLIST: &str = "KASTELLAN_EGRESS_PROXY_ALLOWLIST";
const ENV_WORKER: &str = "KASTELLAN_EGRESS_PROXY_WORKER";

/// How long `spawn_sidecar` waits for the proxy to `bind()` its UDS.
const READY_TIMEOUT: Duration = Duration::from_secs(5);
const READY_POLL: Duration = Duration::from_millis(25);

/// A running sidecar. Drop or `shutdown()` kills it.
#[derive(Debug)]
pub struct SidecarHandle {
    child: Child,
    pub uds_path: PathBuf,
}

impl SidecarHandle {
    /// Kill the sidecar and reap it. Idempotent-ish (errors ignored).
    pub fn shutdown(mut self) {
        self.terminate();
    }

    /// Kill + reap the sidecar and remove its UDS, in place. Idempotent-ish
    /// (errors ignored). Shared by [`shutdown`](Self::shutdown) and by the
    /// coupled-teardown `Drop` of `egress::net_worker::EgressSidecar`, which
    /// holds the handle by value and cannot consume `self`.
    pub fn terminate(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.uds_path);
    }

    /// Borrow the child's stdout for the caller's decision-ingest loop.
    pub fn stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.child.stdout.take()
    }
}

/// Build the sandbox policy for the proxy: `Net::ProxyEgress` (real outbound +
/// DNS, self-enforcing), `WorkerNetClient` (permits `socket(2)`), fs_read for
/// the DNS resolver files + the binary, fs_write for the scratch dir (to create
/// the UDS), and the env contract.
pub fn proxy_policy(binary: &Path, allowlist: &[String], scratch: &Path, worker: &str) -> SandboxPolicy {
    let uds = scratch.join("egress.sock");
    let allow_json = serde_json::to_string(allowlist).expect("Vec<String> serializes");
    SandboxPolicy {
        fs_read: vec![
            binary.to_path_buf(),
            PathBuf::from("/etc/resolv.conf"),
            PathBuf::from("/etc/hosts"),
            PathBuf::from("/etc/nsswitch.conf"),
        ],
        fs_write: vec![scratch.to_path_buf()],
        net: Net::ProxyEgress,
        cpu_ms: 10_000,
        mem_mb: 256,
        profile: Profile::WorkerNetClient,
        cpu_quota_pct: None,
        tasks_max: None,
        env: vec![
            (ENV_UDS.to_string(), uds.to_string_lossy().into_owned()),
            (ENV_ALLOWLIST.to_string(), allow_json),
            (ENV_WORKER.to_string(), worker.to_string()),
        ],
        proxy_uds: None,
    }
}

/// Spawn the proxy under `backend` and wait (bounded) for its UDS to appear.
/// Fail-closed: returns `Err` on spawn failure or bind timeout.
pub fn spawn_sidecar(
    backend: &dyn SandboxBackend,
    binary: &Path,
    allowlist: &[String],
    scratch: &Path,
    worker: &str,
) -> anyhow::Result<SidecarHandle> {
    let policy = proxy_policy(binary, allowlist, scratch, worker);
    let uds_path = scratch.join("egress.sock");
    let _ = std::fs::remove_file(&uds_path);

    let program = binary.to_string_lossy();
    let child = backend
        .spawn_under_policy(&policy, &program, &[])
        .map_err(|e| anyhow::anyhow!("spawn egress-proxy sidecar: {e}"))?;

    let deadline = Instant::now() + READY_TIMEOUT;
    while !uds_path.exists() {
        if Instant::now() >= deadline {
            let mut handle = SidecarHandle { child, uds_path: uds_path.clone() };
            handle.child.kill().ok();
            handle.child.wait().ok();
            anyhow::bail!("egress-proxy sidecar did not bind {uds_path:?} within {READY_TIMEOUT:?}");
        }
        std::thread::sleep(READY_POLL);
    }
    Ok(SidecarHandle { child, uds_path })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_uses_proxy_egress_and_net_client() {
        let p = proxy_policy(Path::new("/opt/proxy"), &["example.com".into()], Path::new("/scratch"), "web-fetch");
        assert!(matches!(p.net, Net::ProxyEgress));
        assert!(matches!(p.profile, Profile::WorkerNetClient));
        assert!(p.fs_read.contains(&PathBuf::from("/etc/resolv.conf")));
        assert!(p.fs_write.contains(&PathBuf::from("/scratch")));
        // env carries the UDS path + allowlist + worker name.
        let env: std::collections::HashMap<_, _> = p.env.into_iter().collect();
        assert_eq!(env[ENV_UDS], "/scratch/egress.sock");
        assert_eq!(env[ENV_ALLOWLIST], r#"["example.com"]"#);
        assert_eq!(env[ENV_WORKER], "web-fetch");
    }
}
