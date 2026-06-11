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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;

use kastellan_sandbox::{SandboxBackend, SandboxPolicy};

use super::audit::{ingest_decisions_into, EgressAuditRow};
use super::spawn::{spawn_sidecar, SidecarHandle, UDS_FILE_NAME};
use crate::tool_host::{spawn_worker, SupervisedWorker, ToolHostError, WorkerSpec};

/// Maximum byte length of a Unix-domain-socket path. `sockaddr_un.sun_path` is
/// 104 bytes on macOS and 108 on Linux; the path must fit including its NUL
/// terminator. The sidecar `bind()`s `<scratch>/egress.sock`, so a force-routing
/// scratch dir must be short enough that the projected socket path still fits —
/// see [`make_worker_scratch_dir`].
#[cfg(target_os = "macos")]
const SUN_PATH_MAX: usize = 104;
#[cfg(not(target_os = "macos"))]
const SUN_PATH_MAX: usize = 108;

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
    /// Per-worker scratch dir holding the sidecar UDS, owned for RAII cleanup
    /// when force-routing created it (see [`spawn_forced_net_worker`]). `None`
    /// when the caller manages the scratch dir itself (e.g. the raw
    /// [`spawn_net_worker`] used by tests/e2e, which pass a borrowed path).
    scratch: Option<PathBuf>,
}

impl Drop for EgressSidecar {
    fn drop(&mut self) {
        // Kill + reap the proxy (also removes the UDS file). Its stdout EOFs →
        // the ingest thread drains any buffered decisions and exits. We do NOT
        // join the thread here.
        self.sidecar.terminate();
        // Remove the per-worker scratch dir we own (now that the UDS is gone).
        // Best-effort — a left-behind scratch dir is a leak, never a safety
        // issue, and must not wedge worker teardown.
        if let Some(dir) = self.scratch.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
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
        scratch: None,
    });
    Ok(worker)
}

/// Force-route a net worker, owning its scratch dir for RAII cleanup.
///
/// Thin wrapper over [`spawn_net_worker`] for the live auto-flip (Task 4.4): it
/// mints a **unique per-worker scratch subdir** under `scratch_root` to hold the
/// sidecar UDS, spawns the coupled worker+sidecar in it, and ties the scratch
/// dir's lifetime to the returned worker (the [`EgressSidecar`]'s `Drop` removes
/// it once the worker — and any warm reuse of it — is finally torn down). On the
/// fail-closed path (sidecar unavailable ⇒ no worker) the freshly-created
/// scratch dir is removed immediately, since no worker exists to own it.
pub fn spawn_forced_net_worker<F>(
    backend: &dyn SandboxBackend,
    proxy_bin: &Path,
    spec: &WorkerSpec<'_>,
    allowlist: &[String],
    scratch_root: &Path,
    worker_name: &str,
    on_decision: F,
) -> Result<SupervisedWorker, ToolHostError>
where
    F: FnMut(EgressAuditRow) + Send + 'static,
{
    let scratch = make_worker_scratch_dir(scratch_root)?;
    match spawn_net_worker(
        backend,
        proxy_bin,
        spec,
        allowlist,
        &scratch,
        worker_name,
        on_decision,
    ) {
        Ok(mut worker) => {
            // Hand scratch ownership to the worker's sidecar bundle so it is
            // cleaned exactly when the worker is finally dropped. `egress` is
            // always `Some` on a successful `spawn_net_worker`, but guard
            // defensively rather than `expect` — a missing bundle just means the
            // dir is cleaned eagerly below instead of at teardown.
            match worker.egress.as_mut() {
                Some(egress) => egress.scratch = Some(scratch),
                None => {
                    // Unreachable: a successful `spawn_net_worker` always sets
                    // `egress`. If that invariant were ever broken the worker is
                    // *live* and its UDS lives inside `scratch`, so we LEAK the
                    // dir (log it) rather than `remove_dir_all` a directory the
                    // running worker still depends on. A leaked scratch dir is
                    // harmless; pulling one out from under a live worker is not.
                    tracing::warn!(
                        scratch = %scratch.display(),
                        "force-routed worker has no egress bundle to own its scratch dir; \
                         leaking it (unreachable — spawn_net_worker should always attach one)"
                    );
                }
            }
            Ok(worker)
        }
        Err(e) => {
            // No worker to own the scratch dir — remove it now (fail-closed).
            let _ = std::fs::remove_dir_all(&scratch);
            Err(e)
        }
    }
}

/// Create a unique scratch subdir under `scratch_root` for one force-routed
/// worker's sidecar UDS. The name is `egress-<pid>-<seq>` — `pid` scopes it to
/// this daemon process, `seq` (a process-lifetime atomic) guarantees uniqueness
/// across concurrent spawns without needing a wall clock or RNG.
fn make_worker_scratch_dir(scratch_root: &Path) -> Result<PathBuf, ToolHostError> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = scratch_root.join(format!("egress-{}-{}", std::process::id(), seq));
    // Reject up front if the sidecar's `<dir>/egress.sock` would overflow
    // `sockaddr_un.sun_path`. The default scratch root (`std::env::temp_dir()`)
    // is short; only a deep `KASTELLAN_EGRESS_SCRATCH_DIR` override can hit this.
    // Failing here with a clear message beats an opaque `bind()` failure from
    // inside the sandboxed sidecar after the dir is already created.
    let projected_uds = dir.join(UDS_FILE_NAME);
    let uds_len = projected_uds.as_os_str().len();
    if uds_len + 1 > SUN_PATH_MAX {
        return Err(ToolHostError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "egress sidecar socket path is {uds_len} bytes (+NUL), over the \
                 {SUN_PATH_MAX}-byte sockaddr_un.sun_path limit — shorten \
                 KASTELLAN_EGRESS_SCRATCH_DIR (projected: {})",
                projected_uds.display()
            ),
        )));
    }
    std::fs::create_dir_all(&dir).map_err(ToolHostError::Io)?;
    Ok(dir)
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
///
/// **Back-pressure note (revisit before Task 4.4 goes live):** the insert is
/// synchronous per row, so a slow `audit_log` write stalls the ingest thread,
/// which stops draining the proxy's stdout, which back-pressures the proxy on
/// its decision write. That can't lose security (egress is already gated by the
/// OS barrier, not by audit throughput) but could throttle a chatty worker. If
/// that shows up under load, decouple via a bounded channel + a dedicated
/// async writer task rather than blocking the ingest thread here.
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

    fn allowlist_spec(policy: &SandboxPolicy) -> WorkerSpec<'_> {
        WorkerSpec {
            policy,
            program: "/bin/worker",
            args: &[],
            wall_clock_ms: None,
        }
    }

    #[test]
    fn spawn_forced_net_worker_fails_closed_when_sidecar_unavailable() {
        let backend = FailBackend;
        let policy = SandboxPolicy {
            net: Net::Allowlist(vec!["api.example.com:443".into()]),
            ..SandboxPolicy::default()
        };
        let scratch_root = tempfile::tempdir().expect("scratch root");
        let res = spawn_forced_net_worker(
            &backend,
            Path::new("/nonexistent/egress-proxy"),
            &allowlist_spec(&policy),
            &["api.example.com:443".to_string()],
            scratch_root.path(),
            "web-fetch",
            |_row| {},
        );
        assert!(res.is_err(), "no proxy => no net worker (fail-closed)");
    }

    #[test]
    fn make_worker_scratch_dir_rejects_overlong_socket_path() {
        // A deep scratch root whose projected `<dir>/egress.sock` overflows
        // sockaddr_un.sun_path must be rejected up front with a clear Io error,
        // not deferred to an opaque bind() failure inside the sidecar. The guard
        // runs before any mkdir, so the (nonexistent) root needs no setup.
        let long_root = PathBuf::from(format!("/{}", "x".repeat(2 * SUN_PATH_MAX)));
        let res = make_worker_scratch_dir(&long_root);
        assert!(
            matches!(res, Err(ToolHostError::Io(_))),
            "overlong scratch root must be rejected with an Io error, got {res:?}"
        );
    }

    #[test]
    fn spawn_forced_net_worker_cleans_scratch_on_failure() {
        // When the sidecar can't spawn, the per-worker scratch subdir created
        // under `scratch_root` must NOT leak — there is no worker to own it, so
        // the wrapper removes it on the failure path.
        let backend = FailBackend;
        let policy = SandboxPolicy {
            net: Net::Allowlist(vec!["api.example.com:443".into()]),
            ..SandboxPolicy::default()
        };
        let scratch_root = tempfile::tempdir().expect("scratch root");
        let _ = spawn_forced_net_worker(
            &backend,
            Path::new("/nonexistent/egress-proxy"),
            &allowlist_spec(&policy),
            &["api.example.com:443".to_string()],
            scratch_root.path(),
            "web-fetch",
            |_row| {},
        );
        let leftovers: Vec<_> = std::fs::read_dir(scratch_root.path())
            .expect("read scratch root")
            .collect();
        assert!(
            leftovers.is_empty(),
            "failed force-route spawn left {} scratch entries behind",
            leftovers.len()
        );
    }
}
