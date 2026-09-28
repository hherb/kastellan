//! Long-lived net worker transport (slice 5c): bundle a JSON-RPC `Client` over a
//! sandboxed worker together with its egress `EgressSidecar`, so
//! `PersistentWorker` respawns both 1:1 (its off-thread drop of the dead
//! transport reaps the old worker AND tears down the old sidecar; the factory
//! then spawns a fresh pair). The sidecar's TLS posture is the caller's choice
//! (`NetTransportSpawn::mitm`, #494): `Mitm::Transparent` relays ciphertext
//! untouched and the worker receives no CA (matrix-sdk, the browser);
//! `Mitm::Intercept` terminates the worker's TLS and hands it the sidecar's
//! per-instance CA (the email channel).

use std::path::Path;

use kastellan_sandbox::{SandboxBackend, SandboxPolicy};

use super::audit::EgressAuditRow;
use super::net_worker::{rewrite_worker_policy, spawn_ingest_thread, EgressSidecar};
use super::spawn::{spawn_sidecar, Mitm, SidecarSpawn};
use crate::worker_lifecycle::persistent::{ClientTransport, PersistentTransport};

/// Rewrite `base` for transparent-tunnel force-routing onto `uds`: proxy_uds set,
/// resolv.conf dropped, UDS env injected, and NO CA (transparent tunnel — `ca`
/// is `None`, so `rewrite_worker_policy` never injects or announces one).
pub(crate) fn forced_transparent_policy(base: SandboxPolicy, uds: &Path) -> SandboxPolicy {
    rewrite_worker_policy(base, uds, None)
}

/// Rewrite `base` for MITM force-routing onto `uds`: proxy_uds set, resolv.conf
/// dropped, UDS env injected, AND the sidecar's per-instance CA made readable +
/// announced, so the worker's transport trusts the proxy instead of the origin.
/// The CA lives beside the UDS — the sidecar writes it there before it reports
/// ready, so the path is valid by the time any worker is spawned.
pub(crate) fn forced_intercept_policy(base: SandboxPolicy, uds: &Path) -> SandboxPolicy {
    let ca = uds
        .parent()
        .map(|d| d.join(super::spawn::CA_FILE_NAME))
        .unwrap_or_else(|| std::path::PathBuf::from(super::spawn::CA_FILE_NAME));
    rewrite_worker_policy(base, uds, Some(ca.as_path()))
}

/// Reject a worker-side origin CA handed to an intercepting transport.
///
/// Under interception the worker's transport (`web-common::http::make_get`)
/// trusts `KASTELLAN_EGRESS_PROXY_CA` and nothing else, so an origin anchor
/// given to the worker would be **silently inert** — the same false-belief
/// failure mode #491 exists to correct, one layer over. `worker_extra_ca` is
/// meaningful only for a transparent tunnel, where the worker does validate the
/// origin itself.
fn check_worker_extra_ca(mitm: Mitm<'_>, worker_extra_ca: Option<&Path>) -> anyhow::Result<()> {
    if worker_extra_ca.is_some() && !mitm.is_transparent() {
        anyhow::bail!(
            "worker_extra_ca was given to an intercepting transport, whose worker trusts only \
             the sidecar's per-instance CA — the origin anchor would be inert. Use \
             Mitm::Intercept {{ upstream_extra_ca }} to widen the SIDECAR's upstream trust instead"
        );
    }
    Ok(())
}

/// Everything `spawn_net_transport` needs. `base_policy` is the worker's policy
/// BEFORE force-routing (its `sandbox_backend`/`Net::Allowlist`/`env` are set by
/// the caller — e.g. `FirecrackerVm` for the DGX path, Seatbelt/bwrap for the
/// hermetic path).
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
    /// This transport's sidecar TLS posture — see [`super::spawn::Mitm`].
    /// Channel callers choose: the email channel intercepts (so the operator's
    /// upstream anchor reaches a self-signed private origin, and the plaintext
    /// leg becomes MITM-**visible**); Matrix tunnels transparently, because
    /// matrix-sdk terminates its own TLS through `ProxyBridge` and cannot be
    /// made to trust a per-instance CA.
    ///
    /// Visible is the precondition for the #3b credential-leak scanner, NOT
    /// coverage by it: this transport provisions no `secret_fingerprints` (only
    /// `super::net_worker`'s per-tool-call path does), so no `secret_hashes.json`
    /// ever lands in the sidecar's scratch and the proxy's `load_patterns` fails
    /// OPEN — nothing on this path is scanned today.
    pub mitm: Mitm<'a>,
    /// A **worker-side** origin cert, appended to `fs_read` so a VM RO-share
    /// carries it in-guest. Test-only today; `None` in production. Meaningful
    /// only under [`Mitm::Transparent`] — pairing it with `Intercept` is
    /// refused by `check_worker_extra_ca`. Note this is the opposite side of
    /// the connection from `Mitm::Intercept`'s `upstream_extra_ca`, which
    /// widens the SIDECAR's trust; the old name `extra_ca` invited exactly that
    /// confusion.
    pub worker_extra_ca: Option<&'a Path>,
}

/// A long-lived net worker + its egress sidecar (posture per `NetTransportSpawn::mitm`),
/// driven by `PersistentWorker`. `Drop` reaps BOTH children: `inner` (the
/// worker/VMM child, via `ClientTransport::drop`) then `egress` (the sidecar
/// child + scratch, via `EgressSidecar::drop`). Field declaration order fixes
/// drop order.
pub struct NetClientTransport {
    inner: ClientTransport,
    // Dropped after `inner`. Owns the sidecar + per-worker scratch dir.
    egress: EgressSidecar,
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
    fn sidecar_exited(&mut self) -> Option<String> {
        self.egress.exited()
    }
}

/// Spawn a long-lived net worker coupled to its egress sidecar, in the TLS
/// posture the caller chooses (`params.mitm`). Sidecar-first fail-closed: if the
/// sidecar cannot start, no worker is spawned. The worker's policy is
/// force-routed onto the sidecar UDS — transparent tunnel (no CA, the worker
/// does its own end-to-end TLS) or interception (the sidecar's per-instance CA
/// is made readable + announced to the worker), matching `params.mitm`; when
/// `worker_extra_ca` is set (transparent postures only — see
/// `check_worker_extra_ca`) it is appended to `fs_read` so a VM RO-share carries
/// it and the worker can trust a test origin. The caller owns `scratch` (a
/// unique per-worker dir); on the fail-closed path the sidecar's `Drop` removes
/// the UDS but NOT the dir — the caller cleans it. `on_decision` receives every
/// per-CONNECT allow/deny row the sidecar emits, so production consumers (e.g.
/// Matrix) can audit them like every other force-routed worker does;
/// demos/tests that don't audit pass `|_row| {}`.
pub fn spawn_net_transport(
    params: &NetTransportSpawn<'_>,
    scratch: &Path,
    on_decision: impl FnMut(EgressAuditRow) + Send + 'static,
) -> anyhow::Result<NetClientTransport> {
    // Fail before anything is spawned.
    check_worker_extra_ca(params.mitm, params.worker_extra_ca)?;

    // 1. Sidecar first, fail-closed.
    let mut sidecar = spawn_sidecar(
        params.sidecar_backend,
        &SidecarSpawn {
            binary: params.proxy_bin,
            allowlist: params.allowlist,
            scratch,
            worker: params.worker_name,
            cert_pins_json: None,
            mitm: params.mitm,
            long_lived: true, // a channel sidecar outlives many dispatches (#395)
        },
    )?;
    let stdout = sidecar.stdout();
    let uds = sidecar.uds_path.clone();

    // 2. Force-route the worker policy onto the chosen posture. Append the
    //    optional test CA to fs_read so a VM RO-share delivers it in-guest.
    let mut base = params.base_policy.clone();
    if let Some(ca) = params.worker_extra_ca {
        if !base.fs_read.iter().any(|p| p == ca) {
            base.fs_read.push(ca.to_path_buf());
        }
    }
    let forced = match params.mitm {
        Mitm::Transparent => forced_transparent_policy(base, &uds),
        Mitm::Intercept { .. } => forced_intercept_policy(base, &uds),
    };

    // 3. Spawn the worker + connect the Client (ClientTransport applies the same
    //    lockdown-env derivation every spawn path uses). Fail-closed: if this
    //    errors, `sidecar` (still a bare `SidecarHandle`, not yet wrapped by
    //    `EgressSidecar`) is dropped here by `?`, and `SidecarHandle::drop`
    //    kills + reaps the proxy and removes the UDS — no orphan (issue #502,
    //    fixed; this used to leak one proxy per attempt, and both callers of
    //    this function retry it in a backoff loop). The scratch DIR is not this
    //    function's to remove — the caller owns it and reclaims it on the error
    //    path (see `channel::matrix`/`channel::email`'s factories).
    let inner = ClientTransport::spawn(params.backend, &forced, params.program, params.args)?;

    // 4. Drain the sidecar's decision stdout into the caller's sink (draining
    //    prevents a full-pipe stall past ~64 KiB regardless of whether the sink
    //    audits). Bundle for 1:1 teardown; the caller hands the scratch dir to
    //    the bundle for RAII.
    let ingest = spawn_ingest_thread(stdout, params.worker_name.to_string(), on_decision);
    let egress = EgressSidecar::from_parts(sidecar, ingest, Some(scratch.to_path_buf()));
    Ok(NetClientTransport {
        inner,
        egress,
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

    #[test]
    fn forced_intercept_policy_gives_the_worker_the_sidecar_ca() {
        let base = SandboxPolicy {
            net: Net::Allowlist(vec!["10.0.0.3:8443".into()]),
            fs_read: vec!["/etc/resolv.conf".into(), "/bin/email-in".into()],
            ..SandboxPolicy::default()
        };
        let uds = std::path::PathBuf::from("/scratch/egress-1/egress.sock");
        let out = forced_intercept_policy(base, &uds);
        let ca = std::path::PathBuf::from("/scratch/egress-1/ca.pem");
        // Announced AND readable in-jail — the worker's transport opens the path it
        // is handed, so either half alone is useless.
        assert!(out.env.iter().any(|(k, v)| k == "KASTELLAN_EGRESS_PROXY_CA"
            && v == "/scratch/egress-1/ca.pem"));
        assert!(out.fs_read.contains(&ca));
        assert_eq!(out.proxy_uds.as_deref(), Some(uds.as_path()));
        // The proxy resolves DNS now, in either posture.
        assert!(!out.fs_read.contains(&"/etc/resolv.conf".into()));
        assert!(out.fs_read.contains(&"/bin/email-in".into()));
    }

    #[test]
    fn a_worker_side_origin_ca_is_refused_under_interception() {
        let ca = std::path::PathBuf::from("/tmp/origin-ca.pem");
        let err = check_worker_extra_ca(
            Mitm::Intercept { upstream_extra_ca: None },
            Some(&ca),
        )
        .expect_err("worker-side origin CA under MITM must be refused");
        assert!(err.to_string().contains("inert"), "unhelpful error: {err}");
    }

    #[test]
    fn a_worker_side_origin_ca_is_accepted_under_a_transparent_tunnel() {
        let ca = std::path::PathBuf::from("/tmp/origin-ca.pem");
        assert!(check_worker_extra_ca(Mitm::Transparent, Some(&ca)).is_ok());
        assert!(check_worker_extra_ca(Mitm::Transparent, None).is_ok());
        assert!(check_worker_extra_ca(Mitm::Intercept { upstream_extra_ca: None }, None).is_ok());
    }
}
