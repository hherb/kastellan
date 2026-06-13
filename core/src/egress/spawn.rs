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
const ENV_PINS: &str = "KASTELLAN_EGRESS_PROXY_PINS";

/// Basename of the per-worker sidecar UDS under the scratch dir. Shared so the
/// force-routing scratch-dir guard (`net_worker::make_worker_scratch_dir`) can
/// project the exact socket path the sidecar will `bind()`.
pub(crate) const UDS_FILE_NAME: &str = "egress.sock";

/// Basename of the per-worker CA cert the sidecar exports for the host to inject
/// into the worker's trust store (slice #3a). Lives beside the UDS in scratch.
pub(crate) const CA_FILE_NAME: &str = "ca.pem";

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
pub fn proxy_policy(
    binary: &Path,
    allowlist: &[String],
    scratch: &Path,
    worker: &str,
    cert_pins_json: Option<&str>,
) -> SandboxPolicy {
    let uds = scratch.join(UDS_FILE_NAME);
    let allow_json = serde_json::to_string(allowlist).expect("Vec<String> serializes");
    let mut env = vec![
        (ENV_UDS.to_string(), uds.to_string_lossy().into_owned()),
        (ENV_ALLOWLIST.to_string(), allow_json),
        (ENV_WORKER.to_string(), worker.to_string()),
    ];
    // Pins are static operator config (slice #4). Omit the key entirely when
    // absent so the no-pin path is byte-identical to slice #3b.
    if let Some(pins) = cert_pins_json.filter(|s| !s.trim().is_empty()) {
        env.push((ENV_PINS.to_string(), pins.to_string()));
    }
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
        env,
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
    cert_pins_json: Option<&str>,
) -> anyhow::Result<SidecarHandle> {
    let policy = proxy_policy(binary, allowlist, scratch, worker, cert_pins_json);
    let uds_path = scratch.join(UDS_FILE_NAME);
    let _ = std::fs::remove_file(&uds_path);

    let program = binary.to_string_lossy();
    let child = backend
        .spawn_under_policy(&policy, &program, &[])
        .map_err(|e| anyhow::anyhow!("spawn egress-proxy sidecar: {e}"))?;

    // Slice #3a: the sidecar also exports its per-instance MITM CA next to the
    // UDS. Wait for BOTH so the host never binds a worker before the CA it must
    // trust exists on disk.
    let ca_path = scratch.join(CA_FILE_NAME);
    let deadline = Instant::now() + READY_TIMEOUT;
    while !(uds_path.exists() && ca_path.exists()) {
        if Instant::now() >= deadline {
            let mut handle = SidecarHandle { child, uds_path: uds_path.clone() };
            handle.child.kill().ok();
            handle.child.wait().ok();
            anyhow::bail!(
                "egress-proxy sidecar did not bind {uds_path:?} + write {ca_path:?} within {READY_TIMEOUT:?}"
            );
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
        let p = proxy_policy(Path::new("/opt/proxy"), &["example.com".into()], Path::new("/scratch"), "web-fetch", None);
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

    #[test]
    fn proxy_policy_omits_pins_env_when_none() {
        let p = proxy_policy(Path::new("/bin/proxy"), &["example.com".into()], Path::new("/scratch"), "web-fetch", None);
        let env: std::collections::HashMap<_, _> = p.env.into_iter().collect();
        assert!(!env.contains_key(ENV_PINS));
    }

    #[test]
    fn proxy_policy_includes_pins_env_when_set() {
        let pins = r#"{"api.anthropic.com":["sha256/AAAA"]}"#;
        let p = proxy_policy(Path::new("/bin/proxy"), &["example.com".into()], Path::new("/scratch"), "web-fetch", Some(pins));
        let env: std::collections::HashMap<_, _> = p.env.into_iter().collect();
        assert_eq!(env[ENV_PINS], pins);
    }
}
