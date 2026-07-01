//! Long-lived net worker transport (slice 5c): bundle a JSON-RPC `Client` over a
//! sandboxed worker together with its transparent-tunnel egress `EgressSidecar`,
//! so `PersistentWorker` respawns both 1:1 (its off-thread drop of the dead
//! transport reaps the old worker AND tears down the old sidecar; the factory
//! then spawns a fresh pair). The sidecar runs in `disable_mitm` mode; the worker
//! does its own end-to-end TLS and receives no CA.

use std::path::Path;

use kastellan_sandbox::{SandboxBackend, SandboxPolicy};

use super::net_worker::{rewrite_worker_policy, spawn_ingest_thread, EgressSidecar};
use super::spawn::{spawn_sidecar, CA_FILE_NAME};
use crate::worker_lifecycle::persistent::{ClientTransport, PersistentTransport};

/// Rewrite `base` for transparent-tunnel force-routing onto `uds`: proxy_uds set,
/// resolv.conf dropped, UDS env injected, and NO CA (transparent tunnel). The
/// `ca` path handed to `rewrite_worker_policy` is a placeholder — `mitm=false`
/// means it is never read or injected.
pub(crate) fn forced_transparent_policy(base: SandboxPolicy, uds: &Path) -> SandboxPolicy {
    let ca_placeholder = uds
        .parent()
        .map(|d| d.join(CA_FILE_NAME))
        .unwrap_or_else(|| std::path::PathBuf::from(CA_FILE_NAME));
    rewrite_worker_policy(base, uds, &ca_placeholder, false)
}

/// Everything `spawn_net_transport` needs. `base_policy` is the worker's policy
/// BEFORE force-routing (its `sandbox_backend`/`Net::Allowlist`/`env` are set by
/// the caller — e.g. `FirecrackerVm` for the DGX path, Seatbelt/bwrap for the
/// hermetic path). `extra_ca` is a test-only origin cert delivered to the worker
/// (added to `fs_read` so the VM RO-share carries it); `None` in production.
pub struct NetTransportSpawn<'a> {
    pub backend: &'a dyn SandboxBackend,
    /// The HOST backend (bwrap on Linux, Seatbelt on macOS) the egress-proxy
    /// sidecar runs under. The egress-proxy sidecar ALWAYS runs on the host (it
    /// is the real-network egress boundary — it needs `Net::ProxyEgress` with a
    /// real host route); only the worker (`backend`) may run in a VM. On non-VM
    /// paths pass the same backend for both.
    pub sidecar_backend: &'a dyn SandboxBackend,
    pub proxy_bin: &'a Path,
    pub program: &'a str,
    pub args: &'a [&'a str],
    pub base_policy: SandboxPolicy,
    pub allowlist: &'a [String],
    pub worker_name: &'a str,
    pub extra_ca: Option<&'a Path>,
}

/// A long-lived net worker + its transparent-tunnel sidecar, driven by
/// `PersistentWorker`. `Drop` reaps BOTH children: `inner` (the worker/VMM child,
/// via `ClientTransport::drop`) then `_egress` (the sidecar child + scratch, via
/// `EgressSidecar::drop`). Field declaration order fixes drop order.
pub struct NetClientTransport {
    inner: ClientTransport,
    // Dropped after `inner`. Owns the sidecar + per-worker scratch dir.
    _egress: EgressSidecar,
}

impl PersistentTransport for NetClientTransport {
    fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        self.inner.call(method, params)
    }
    fn death_report(&mut self) -> Option<String> {
        self.inner.death_report()
    }
}

/// Spawn a long-lived net worker coupled to a transparent-tunnel egress sidecar.
/// Sidecar-first fail-closed: if the sidecar cannot start, no worker is spawned.
/// The worker's policy is force-routed onto the sidecar UDS with NO CA (the
/// worker does its own end-to-end TLS); when `extra_ca` is set it is appended to
/// `fs_read` so a VM RO-share carries it and the worker can trust a test origin.
/// The caller owns `scratch` (a unique per-worker dir); on the fail-closed path
/// the sidecar's `Drop` removes the UDS but NOT the dir — the caller cleans it.
pub fn spawn_net_transport(
    params: &NetTransportSpawn<'_>,
    scratch: &Path,
) -> anyhow::Result<NetClientTransport> {
    // 1. Sidecar first (transparent tunnel), fail-closed.
    let mut sidecar = spawn_sidecar(
        params.sidecar_backend,
        params.proxy_bin,
        params.allowlist,
        scratch,
        params.worker_name,
        None, // no cert pins
        true, // disable_mitm — transparent tunnel
    )?;
    let stdout = sidecar.stdout();
    let uds = sidecar.uds_path.clone();

    // 2. Force-route the worker policy (transparent, no CA). Append the optional
    //    test CA to fs_read so a VM RO-share delivers it in-guest.
    let mut base = params.base_policy.clone();
    if let Some(ca) = params.extra_ca {
        if !base.fs_read.iter().any(|p| p == ca) {
            base.fs_read.push(ca.to_path_buf());
        }
    }
    let forced = forced_transparent_policy(base, &uds);

    // 3. Spawn the worker + connect the Client (ClientTransport applies the same
    //    lockdown-env derivation every spawn path uses). Fail-closed: if this
    //    errors, `sidecar` drops here and its Drop kills the proxy.
    let inner = ClientTransport::spawn(params.backend, &forced, params.program, params.args)?;

    // 4. Drain the sidecar's decision stdout (no-op sink — the demo doesn't audit
    //    to PG; draining prevents a full-pipe stall past ~64 KiB). Bundle for 1:1
    //    teardown; the caller hands the scratch dir to the bundle for RAII.
    let ingest = spawn_ingest_thread(stdout, |_row| {});
    let egress = EgressSidecar::from_parts(sidecar, ingest, Some(scratch.to_path_buf()));
    Ok(NetClientTransport {
        inner,
        _egress: egress,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kastellan_sandbox::Net;

    #[test]
    fn forced_transparent_policy_sets_uds_and_no_ca() {
        let base = SandboxPolicy {
            net: Net::Allowlist(vec!["origin.example.com:443".into()]),
            fs_read: vec!["/etc/resolv.conf".into(), "/bin/net-demo".into()],
            ..SandboxPolicy::default()
        };
        let uds = std::path::PathBuf::from("/scratch/egress-1/egress.sock");
        let out = forced_transparent_policy(base, &uds);
        assert_eq!(out.proxy_uds.as_deref(), Some(uds.as_path()));
        assert!(!out.env.iter().any(|(k, _)| k == "KASTELLAN_EGRESS_PROXY_CA"));
        assert!(out.env.iter().any(|(k, v)| k == "KASTELLAN_EGRESS_PROXY_UDS"
            && v == "/scratch/egress-1/egress.sock"));
        assert!(!out.fs_read.contains(&"/etc/resolv.conf".into()));
        assert!(out.fs_read.contains(&"/bin/net-demo".into()));
    }
}
