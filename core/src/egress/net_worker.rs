//! Couple a `Net::Allowlist` worker with its egress-proxy sidecar so the
//! worker cannot be spawned without a live proxy and has no egress except the
//! proxy UDS (force-routing, slice #2). The OS-level barrier lives in
//! `kastellan-sandbox` (Linux private netns / macOS Seatbelt deny-outbound);
//! this module is the host-side coupling:
//!   1. spawn the sidecar **first** (fail-closed — no proxy ⇒ no worker),
//!   2. rewrite the worker's policy onto the sidecar UDS (drop direct DNS,
//!      inject the UDS env so its transport switches to CONNECT-over-UDS),
//!   3. spawn the worker, and
//!   4. bundle the sidecar + a decision-ingest thread into the returned
//!      [`SupervisedWorker`] so teardown is 1:1 with the worker.

use std::io::BufReader;
use std::path::Path;
use std::thread::JoinHandle;

use kastellan_sandbox::{SandboxBackend, SandboxPolicy};

use super::audit::{ingest_decisions_into, EgressAuditRow};
use super::spawn::{spawn_sidecar, SidecarHandle};
use crate::tool_host::{spawn_worker, SupervisedWorker, ToolHostError, WorkerSpec};

/// Env key the worker-side transport reads to switch onto CONNECT-over-UDS
/// (`kastellan_worker_web_common::http::make_get`). Must match that constant.
const ENV_UDS: &str = "KASTELLAN_EGRESS_PROXY_UDS";

/// Sidecar + decision-ingest bundle carried by a force-routed net worker, held
/// in [`SupervisedWorker`]'s additive `egress` field. Its [`Drop`] kills the
/// proxy; the ingest thread then sees EOF on the proxy stdout and exits on its
/// own — it is deliberately **not** joined, so a slow/stuck audit insert can
/// never wedge worker teardown.
pub struct EgressSidecar {
    sidecar: SidecarHandle,
    /// The decision-ingest thread. Dropped (detached) on teardown; it exits
    /// when the killed sidecar's stdout reaches EOF.
    _ingest: JoinHandle<()>,
}

impl Drop for EgressSidecar {
    fn drop(&mut self) {
        // Kill + reap the proxy. Its stdout EOFs → the ingest thread drains any
        // buffered decisions and exits. We do NOT join the thread here.
        self.sidecar.terminate();
    }
}

/// Rewrite a net worker's policy for force-routing: point it at the proxy UDS,
/// drop its direct DNS file (the proxy resolves now), and inject the UDS env so
/// the worker's transport switches onto CONNECT-over-UDS. Pure — no spawn,
/// fully testable.
pub fn rewrite_worker_policy(mut policy: SandboxPolicy, uds: &Path) -> SandboxPolicy {
    policy.proxy_uds = Some(uds.to_path_buf());
    // The worker no longer resolves DNS (the proxy does); revoke the file so a
    // compromised worker can't even read the resolver config.
    policy.fs_read.retain(|p| p != Path::new("/etc/resolv.conf"));
    // Inject the UDS env (overwrite any stale entry).
    policy.env.retain(|(k, _)| k != ENV_UDS);
    policy
        .env
        .push((ENV_UDS.to_string(), uds.to_string_lossy().into_owned()));
    policy
}

/// Spawn a force-routed net worker. The sidecar is spawned **first** and
/// fail-closed: if it cannot start, no worker is spawned (`Err`). The worker is
/// then spawned under a policy rewritten onto the sidecar UDS, and a
/// decision-ingest thread maps each proxy decision via `on_decision`. The
/// returned [`SupervisedWorker`] owns the sidecar bundle, so dropping/closing
/// the worker tears the proxy down 1:1.
///
/// `on_decision` is invoked once per proxy decision line (already mapped to an
/// [`EgressAuditRow`]); the live caller persists it to `audit_log` (see
/// [`pg_decision_sink`]), tests pass a capturing or no-op closure. Kept generic
/// so the spawn path itself needs no Postgres.
pub fn spawn_net_worker<F>(
    backend: &dyn SandboxBackend,
    proxy_bin: &Path,
    spec: &WorkerSpec<'_>,
    allowlist: &[String],
    scratch: &Path,
    worker_name: &str,
    on_decision: F,
) -> Result<SupervisedWorker, ToolHostError>
where
    F: FnMut(EgressAuditRow) + Send + 'static,
{
    // 1. Sidecar first; fail-closed on its Err (no worker without a proxy).
    let mut sidecar = spawn_sidecar(backend, proxy_bin, allowlist, scratch, worker_name)
        .map_err(|e| ToolHostError::Io(std::io::Error::other(format!("egress sidecar: {e}"))))?;
    // Capture the proxy stdout for the ingest thread before the handle moves.
    let stdout = sidecar.stdout();
    // 2. Rewrite the worker policy onto the sidecar UDS.
    let uds = sidecar.uds_path.clone();
    let forced = rewrite_worker_policy(spec.policy.clone(), &uds);
    let forced_spec = WorkerSpec {
        policy: &forced,
        program: spec.program,
        args: spec.args,
        wall_clock_ms: spec.wall_clock_ms,
    };
    // 3. Spawn the worker under the forced policy. If this fails, `sidecar`
    //    drops here and its Drop kills the proxy — fail-closed, no orphan.
    let mut worker = spawn_worker(backend, &forced_spec)?;
    // 4. Decision-ingest thread on the proxy stdout; attach the bundle so
    //    teardown is 1:1 with the worker.
    let ingest = spawn_ingest_thread(stdout, on_decision);
    worker.egress = Some(EgressSidecar {
        sidecar,
        _ingest: ingest,
    });
    Ok(worker)
}

/// Spawn the decision-ingest thread over the proxy's stdout. Reads decision
/// lines and feeds each mapped row to `on_decision`. If `stdout` is `None`
/// (already taken) the thread exits immediately.
fn spawn_ingest_thread<F>(
    stdout: Option<std::process::ChildStdout>,
    on_decision: F,
) -> JoinHandle<()>
where
    F: FnMut(EgressAuditRow) + Send + 'static,
{
    std::thread::spawn(move || {
        let Some(stdout) = stdout else { return };
        ingest_decisions_into(BufReader::new(stdout), on_decision);
    })
}

/// Build the live decision sink: persist each row to `audit_log` via the
/// runtime pool, running the async insert on `handle` (the scheduler runtime)
/// from the ingest thread. Best-effort — an insert error is logged, not fatal
/// (a dropped audit row must never kill the worker). The proxy itself never
/// touches Postgres (core-only-DB invariant); decisions flow proxy → core
/// stdout-ingest → PG here.
pub fn pg_decision_sink(
    pool: sqlx::PgPool,
    handle: tokio::runtime::Handle,
) -> impl FnMut(EgressAuditRow) + Send + 'static {
    move |row| {
        let EgressAuditRow {
            actor,
            action,
            payload,
        } = row;
        let res = handle.block_on(kastellan_db::audit::insert(&pool, actor, &action, payload));
        if let Err(e) = res {
            tracing::warn!(error = %e, %action, "egress decision audit insert failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kastellan_sandbox::{Net, SandboxError};

    #[test]
    fn rewrite_worker_policy_forces_routing() {
        let base = SandboxPolicy {
            net: Net::Allowlist(vec!["api.example.com:443".into()]),
            fs_read: vec!["/etc/resolv.conf".into(), "/bin/worker".into()],
            env: vec![],
            ..SandboxPolicy::default()
        };
        let uds = std::path::PathBuf::from("/scratch/egress.sock");
        let out = rewrite_worker_policy(base, &uds);
        // proxy_uds set → bwrap/Seatbelt force-route.
        assert_eq!(out.proxy_uds.as_deref(), Some(uds.as_path()));
        // resolv.conf removed (worker no longer resolves directly).
        assert!(!out.fs_read.contains(&"/etc/resolv.conf".into()));
        // The worker binary path survives.
        assert!(out.fs_read.contains(&"/bin/worker".into()));
        // env carries the UDS path.
        assert!(out
            .env
            .iter()
            .any(|(k, v)| k == ENV_UDS && v == "/scratch/egress.sock"));
    }

    #[test]
    fn rewrite_overwrites_stale_uds_env() {
        let base = SandboxPolicy {
            net: Net::Allowlist(vec!["api.example.com:443".into()]),
            env: vec![(ENV_UDS.to_string(), "/old/stale.sock".to_string())],
            ..SandboxPolicy::default()
        };
        let out = rewrite_worker_policy(base, std::path::Path::new("/scratch/egress.sock"));
        let uds_entries: Vec<&String> = out
            .env
            .iter()
            .filter(|(k, _)| k == ENV_UDS)
            .map(|(_, v)| v)
            .collect();
        assert_eq!(uds_entries, vec!["/scratch/egress.sock"], "exactly one, fresh");
    }

    /// A backend whose spawn always fails — stands in for "the sidecar can't
    /// start" without needing a real sandbox.
    struct FailBackend;
    impl SandboxBackend for FailBackend {
        fn spawn_under_policy(
            &self,
            _policy: &SandboxPolicy,
            _program: &str,
            _args: &[&str],
        ) -> Result<std::process::Child, SandboxError> {
            Err(SandboxError::Backend("test: spawn refused".into()))
        }
    }

    #[test]
    fn spawn_net_worker_fails_closed_when_sidecar_unavailable() {
        let backend = FailBackend;
        let policy = SandboxPolicy {
            net: Net::Allowlist(vec!["api.example.com:443".into()]),
            ..SandboxPolicy::default()
        };
        let spec = WorkerSpec {
            policy: &policy,
            program: "/bin/worker",
            args: &[],
            wall_clock_ms: None,
        };
        let res = spawn_net_worker(
            &backend,
            Path::new("/nonexistent/egress-proxy"),
            &spec,
            &["api.example.com:443".to_string()],
            Path::new("/tmp/kastellan-net-worker-test"),
            "web-fetch",
            |_row| {},
        );
        assert!(res.is_err(), "no proxy => no net worker (fail-closed)");
    }
}
