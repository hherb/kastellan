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
/// Env key that puts the sidecar into no-MITM (transparent-tunnel) mode for
/// workers that do their own end-to-end TLS (the browser). Must match the read
/// in `egress-proxy::main`.
const ENV_DISABLE_MITM: &str = "KASTELLAN_EGRESS_PROXY_DISABLE_MITM";

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

/// Cumulative CPU budget (ms → ceil-div RLIMIT_CPU seconds) for a **short-lived**
/// per-tool-call sidecar. Matches the web-fetch worker's own `cpu_ms` (the
/// sidecar lives 1:1 with that single dispatch), and restores the CPU
/// defense-in-depth that `e70174b` had to drop blanket-wide (issue #395). A
/// long-lived channel sidecar (matrix) gets `0` instead — see [`proxy_policy`].
const SHORT_LIVED_SIDECAR_CPU_MS: u64 = 10_000;

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
///
/// `long_lived` selects the CPU governance (issue #395). A channel sidecar
/// (matrix) lives 1:1 with a worker that runs for weeks, so a cumulative
/// `RLIMIT_CPU` would eventually SIGKILL it mid-flight → `cpu_ms: 0` (no cap;
/// bounded instead by the cgroup `CPUQuota` on Linux / the mem cap). A
/// short-lived per-tool-call sidecar (web-fetch) lives only for the one
/// dispatch, so it gets a bounded [`SHORT_LIVED_SIDECAR_CPU_MS`] cap back —
/// restoring the defense-in-depth that only mattered on macOS, where
/// `RLIMIT_CPU` is the sole per-process CPU-governance primitive.
pub fn proxy_policy(
    binary: &Path,
    allowlist: &[String],
    scratch: &Path,
    worker: &str,
    cert_pins_json: Option<&str>,
    disable_mitm: bool,
    long_lived: bool,
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
    // Omit the disable-MITM key entirely when false so the no-flag path is
    // byte-identical to the default MITM path (mirrors the pins pattern).
    if disable_mitm {
        env.push((ENV_DISABLE_MITM.to_string(), "1".to_string()));
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
        // CPU governance is lifetime-scoped (issue #395). A long-lived channel
        // sidecar (matrix, weeks) gets no cumulative RLIMIT_CPU — same
        // convention as `build_matrix_policy` — because the historical `10_000`
        // WOULD have SIGKILLed it mid-flight once the spawn fix below made the
        // lockdown env actually reach the proxy (it never did before `e70174b`).
        // A short-lived per-tool-call sidecar lives only for its one dispatch,
        // so it keeps the bounded cap as defense-in-depth (the only CPU primitive
        // on macOS, where there is no cgroup quota).
        cpu_ms: if long_lived { 0 } else { SHORT_LIVED_SIDECAR_CPU_MS },
        mem_mb: 256,
        profile: Profile::WorkerNetClient,
        cpu_quota_pct: None,
        tasks_max: None,
        env,
        proxy_uds: None,
        broker_uds: None,
        persistent_store: None,
    }
}

/// Spawn the proxy under `backend` and wait (bounded) for its UDS to appear.
/// Fail-closed: returns `Err` on spawn failure or bind timeout.
///
/// `long_lived` scopes the sidecar's CPU cap — see [`proxy_policy`]. Pass `true`
/// for a channel sidecar that outlives many dispatches (matrix), `false` for a
/// per-tool-call sidecar (web-fetch) so it gets a bounded `RLIMIT_CPU` back.
#[allow(clippy::too_many_arguments)] // mirrors `proxy_policy`'s descriptor args + `backend`
pub fn spawn_sidecar(
    backend: &dyn SandboxBackend,
    binary: &Path,
    allowlist: &[String],
    scratch: &Path,
    worker: &str,
    cert_pins_json: Option<&str>,
    disable_mitm: bool,
    long_lived: bool,
) -> anyhow::Result<SidecarHandle> {
    let policy = proxy_policy(
        binary,
        allowlist,
        scratch,
        worker,
        cert_pins_json,
        disable_mitm,
        long_lived,
    );
    let uds_path = scratch.join(UDS_FILE_NAME);
    let _ = std::fs::remove_file(&uds_path);

    // Derive the worker-side lockdown env (KASTELLAN_SECCOMP_PROFILE +
    // KASTELLAN_LANDLOCK_RW/RO) exactly like every other spawn path. Without
    // it the proxy's in-process lock_down ran with NO seccomp and — worse — a
    // Landlock ruleset missing the fs_read grants, so post-lockdown glibc
    // could not open /etc/resolv.conf|hosts|nsswitch.conf and EVERY
    // DNS-needing CONNECT failed EAI_AGAIN ("Temporary failure in name
    // resolution") on Linux. Literal-IP tunnels never resolve, which is why
    // the hermetic suites stayed green while real-hostname egress was broken.
    let derived = crate::tool_host::derive_lockdown_env(&policy);
    let program = binary.to_string_lossy();
    let child = backend
        .spawn_under_policy(&derived, &program, &[])
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
        let p = proxy_policy(Path::new("/opt/proxy"), &["example.com".into()], Path::new("/scratch"), "web-fetch", None, false, false);
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

    /// Regression pin for the live-gate bug (5b-4a): the sidecar spawn must
    /// derive the worker-prelude lockdown env from the policy. Without it the
    /// proxy self-applied Landlock WITHOUT the fs_read grants, so post-lockdown
    /// glibc could not read /etc/resolv.conf|hosts|nsswitch.conf and every
    /// DNS-needing CONNECT failed EAI_AGAIN on Linux (hermetic literal-IP
    /// suites stayed green, hiding it) — and ran with no seccomp at all.
    /// `spawn_sidecar` feeds `proxy_policy` through `derive_lockdown_env`;
    /// this pins what that derivation must yield for the proxy's policy.
    #[test]
    fn derived_proxy_policy_carries_lockdown_env_for_dns() {
        let p = proxy_policy(Path::new("/opt/proxy"), &["matrix.example.org:443".into()], Path::new("/scratch"), "matrix", None, true, true);
        let d = crate::tool_host::derive_lockdown_env(&p);
        let env: std::collections::HashMap<_, _> = d.env.into_iter().collect();
        assert_eq!(env["KASTELLAN_SECCOMP_PROFILE"], "net_client");
        let ro: Vec<String> = serde_json::from_str(&env["KASTELLAN_LANDLOCK_RO"]).unwrap();
        for path in ["/etc/resolv.conf", "/etc/hosts", "/etc/nsswitch.conf"] {
            assert!(ro.iter().any(|r| r == path), "Landlock RO must grant {path}");
        }
        let rw: Vec<String> = serde_json::from_str(&env["KASTELLAN_LANDLOCK_RW"]).unwrap();
        assert!(rw.iter().any(|r| r == "/scratch"), "Landlock RW must grant the scratch dir");
        // Long-lived sidecar: no cumulative RLIMIT_CPU (cpu_ms == 0 ⇒ env omitted).
        assert!(!env.contains_key("KASTELLAN_CPU_MS"), "no CPU rlimit for a long-lived sidecar");
    }

    /// Issue #395: the CPU cap is lifetime-scoped. A long-lived channel sidecar
    /// (matrix, weeks) must carry NO cumulative RLIMIT_CPU — a bounded cap would
    /// eventually SIGKILL it mid-flight now that the lockdown env actually
    /// reaches the proxy (post `e70174b`).
    #[test]
    fn proxy_policy_long_lived_has_no_cpu_cap() {
        let p = proxy_policy(
            Path::new("/opt/proxy"), &["matrix.example.org:443".into()],
            Path::new("/scratch"), "matrix", None, true, true,
        );
        assert_eq!(p.cpu_ms, 0, "long-lived sidecar must have no cumulative CPU cap");
    }

    /// Issue #395: a short-lived per-tool-call sidecar (web-fetch) lives 1:1 with
    /// its single dispatch, so it keeps a bounded RLIMIT_CPU as defense-in-depth
    /// — the only per-process CPU-governance primitive on macOS. This is the
    /// path `e70174b` had regressed to `0` blanket-wide.
    #[test]
    fn proxy_policy_short_lived_keeps_bounded_cpu_cap() {
        let p = proxy_policy(
            Path::new("/opt/proxy"), &["example.com".into()],
            Path::new("/scratch"), "web-fetch", None, false, false,
        );
        assert_eq!(
            p.cpu_ms, SHORT_LIVED_SIDECAR_CPU_MS,
            "short-lived sidecar must keep a bounded CPU cap",
        );
        assert!(p.cpu_ms > 0);
    }

    /// The short-lived cap must survive lockdown-env derivation as
    /// `KASTELLAN_CPU_MS` (the wire form the worker prelude reads for
    /// `setrlimit(RLIMIT_CPU)`) — the long-lived case omits it entirely (pinned
    /// by `derived_proxy_policy_carries_lockdown_env_for_dns`).
    #[test]
    fn derived_short_lived_policy_carries_cpu_ms_env() {
        let p = proxy_policy(
            Path::new("/opt/proxy"), &["example.com".into()],
            Path::new("/scratch"), "web-fetch", None, false, false,
        );
        let d = crate::tool_host::derive_lockdown_env(&p);
        let env: std::collections::HashMap<_, _> = d.env.into_iter().collect();
        assert_eq!(
            env["KASTELLAN_CPU_MS"],
            SHORT_LIVED_SIDECAR_CPU_MS.to_string(),
            "short-lived sidecar must derive a CPU rlimit env",
        );
    }

    #[test]
    fn proxy_policy_omits_pins_env_when_none() {
        let p = proxy_policy(Path::new("/bin/proxy"), &["example.com".into()], Path::new("/scratch"), "web-fetch", None, false, false);
        let env: std::collections::HashMap<_, _> = p.env.into_iter().collect();
        assert!(!env.contains_key(ENV_PINS));
    }

    #[test]
    fn proxy_policy_includes_pins_env_when_set() {
        let pins = r#"{"api.anthropic.com":["sha256/AAAA"]}"#;
        let p = proxy_policy(Path::new("/bin/proxy"), &["example.com".into()], Path::new("/scratch"), "web-fetch", Some(pins), false, false);
        let env: std::collections::HashMap<_, _> = p.env.into_iter().collect();
        assert_eq!(env[ENV_PINS], pins);
    }

    #[test]
    fn proxy_policy_sets_disable_mitm_env_when_requested() {
        let p = proxy_policy(
            Path::new("/bin/proxy"), &["example.com:443".into()],
            Path::new("/scratch"), "browser-driver", None, true, false,
        );
        let env: std::collections::HashMap<_, _> = p.env.into_iter().collect();
        assert_eq!(env[ENV_DISABLE_MITM], "1");
    }

    #[test]
    fn proxy_policy_omits_disable_mitm_env_when_false() {
        let p = proxy_policy(
            Path::new("/bin/proxy"), &["example.com:443".into()],
            Path::new("/scratch"), "web-fetch", None, false, false,
        );
        let env: std::collections::HashMap<_, _> = p.env.into_iter().collect();
        assert!(!env.contains_key(ENV_DISABLE_MITM));
    }
}
