//! Matrix channel bring-up for the `kastellan` binary entrypoint.
//!
//! Extracted verbatim from `async fn main`'s "Channel bus (comms slice #2 —
//! Matrix)" block (Item 9b, to keep `main.rs` under the 500-LOC cap). The one
//! structural change is the extract-function boundary itself: the block's local
//! `matrix_bus` is now this function's return value, and the three daemon-scope
//! values it read (`pool`, `sandboxes`, `force_routing`) are now parameters.
//! Behaviour is otherwise identical — same env gate, same 60s login timeout,
//! same fail-soft posture (any spawn/login/connect failure logs and returns
//! `None`, leaving the daemon a byte-identical Matrix-less build).

use std::sync::Arc;

use sqlx::PgPool;
use tracing::{error, info};

use kastellan_core::channel::ChannelBus;
use kastellan_core::worker_lifecycle::force_route::ForceRoutingConfig;
use kastellan_sandbox::{SandboxBackend, SandboxBackends};

/// Spawn the Matrix channel bus if `KASTELLAN_MATRIX_HOMESERVER_URL` is set.
///
/// Gated on the homeserver env var (surfaced via
/// [`kastellan_core::channel::matrix::daemon_spawn_config_from_env`]): unset ⇒
/// returns `None` and the daemon is byte-identical to a Matrix-less build. When
/// set, spawns the sandboxed live worker (which restores its persisted session —
/// the one-time initial login is done separately with `kastellan-cli matrix
/// probe`) and runs a [`ChannelBus`] over the DB-backed pairing/authorizer + the
/// tasks-queue event/completion seams. Authorization is fail-closed at the bus:
/// only DB-paired peers' messages are enqueued.
///
/// Fail-soft: an unreachable homeserver (or any spawn/login/connect error)
/// logs and returns `None` rather than aborting startup — the channel is an
/// add-on, not a bring-up precondition.
///
/// * `pool` — daemon-scoped runtime pool (cloned into the authorizer, pairing
///   service, events, and completion seams).
/// * `sandboxes` — the per-OS backend bundle; selects the worker backend
///   (Firecracker VM when `KASTELLAN_MATRIX_USE_MICROVM=1` on Linux, else the
///   host jail) and the sidecar backend (always the host bwrap/Seatbelt — the
///   5c invariant: the egress proxy needs a real network route).
/// * `force_routing` — the resolved egress force-routing config; `Some` ⇒ each
///   (re)spawn gets a 1:1 transparent-tunnel sidecar via `MatrixEgress`.
pub(crate) async fn spawn_matrix_channel(
    pool: &PgPool,
    sandboxes: &SandboxBackends,
    force_routing: &Option<Arc<ForceRoutingConfig>>,
) -> Option<ChannelBus> {
    let mut matrix_bus: Option<ChannelBus> = None;
    if let Some(spawn_cfg) = kastellan_core::channel::matrix::daemon_spawn_config_from_env(
        std::env::current_exe().ok().as_deref().and_then(|p| p.parent()),
    ) {
        // #459: a `localhost`-NAME homeserver is statically dead once egress
        // is force-routed (the proxy resolves the name → loopback →
        // range-denies every CONNECT), and the spawn path would respawn-loop
        // on it forever. Refuse the channel up front — fail-soft, daemon
        // unaffected. VM mode counts as always-forced: the Firecracker plan
        // refuses to boot a Net::Allowlist worker without the egress proxy.
        #[cfg(target_os = "linux")]
        let vm_mode = spawn_cfg.use_microvm;
        #[cfg(not(target_os = "linux"))]
        let vm_mode = false;
        if let Some(detail) = kastellan_core::channel::matrix::forced_localhost_homeserver(
            &spawn_cfg.homeserver_url,
            force_routing.is_some() || vm_mode,
        ) {
            error!(%detail, "matrix homeserver misconfigured; channel not started");
            return None;
        }
        // The worker's login is blocking (matrix.init waits for the SDK's login +
        // first sync), so run it on a blocking thread under a bounded timeout: an
        // unreachable homeserver fails-soft (channel not started) instead of
        // hanging daemon startup, and it doesn't block an async worker thread. On
        // timeout the blocking task is left to drain against the SDK's own HTTP
        // timeouts (a blocking task can't be force-cancelled).
        // Worker backend: Firecracker VM when the operator opted in
        // (KASTELLAN_MATRIX_USE_MICROVM=1, Linux); else the host jail. The SIDECAR
        // backend always stays the host bwrap/Seatbelt (5c invariant — the egress
        // proxy needs a real network route; a VM here would boot a proxy with none).
        #[cfg(target_os = "linux")]
        let sidecar_backend: Arc<dyn SandboxBackend> = Arc::clone(&sandboxes.bwrap);
        #[cfg(target_os = "linux")]
        let backend: Arc<dyn SandboxBackend> = if spawn_cfg.use_microvm {
            Arc::clone(&sandboxes.firecracker)
        } else {
            Arc::clone(&sandboxes.bwrap)
        };
        #[cfg(target_os = "macos")]
        let sidecar_backend: Arc<dyn SandboxBackend> = Arc::clone(&sandboxes.seatbelt);
        #[cfg(target_os = "macos")]
        let backend: Arc<dyn SandboxBackend> = Arc::clone(&sandboxes.seatbelt);

        let egress = force_routing.as_ref().map(|fr| {
            kastellan_core::channel::matrix::MatrixEgress {
                sidecar_backend: Arc::clone(&sidecar_backend),
                routing: Arc::clone(fr),
            }
        });
        let spawn = tokio::task::spawn_blocking(move || {
            kastellan_core::channel::matrix::spawn_matrix_worker(
                backend,
                kastellan_core::channel::ChannelId("matrix".to_string()),
                &spawn_cfg,
                egress,
            )
        });
        const MATRIX_LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
        match tokio::time::timeout(MATRIX_LOGIN_TIMEOUT, spawn).await {
            Ok(Ok(Ok(worker))) => {
                info!(identity = %worker.identity, "matrix worker logged in; starting channel bus");
                let authorizer = Arc::new(
                    kastellan_core::channel::auth::DbPeerAuthorizer::new(pool.clone()),
                );
                let pairing = Arc::new(
                    kastellan_core::channel::pairing::DbPairingService::new(pool.clone()),
                );
                let events = Arc::new(kastellan_core::channel::bus::PgChannelEvents::new(pool.clone()));
                match kastellan_core::channel::bus::PgCompletedTasks::connect(pool.clone()).await {
                    Ok(completed) => {
                        matrix_bus = Some(kastellan_core::channel::ChannelBus::spawn(
                            vec![Box::new(worker.channel)],
                            authorizer,
                            Some(pairing),
                            events,
                            Box::new(completed),
                        ));
                        info!("matrix channel bus running");
                    }
                    Err(e) => {
                        error!(error = %e, "matrix: PgCompletedTasks::connect failed; channel not started");
                    }
                }
            }
            Ok(Ok(Err(e))) => {
                error!(error = %format!("{e:#}"), "matrix worker spawn/login failed; channel not started");
            }
            Ok(Err(join_err)) => {
                error!(error = %join_err, "matrix worker spawn task panicked; channel not started");
            }
            Err(_elapsed) => {
                error!("matrix worker login timed out (60s); channel not started");
            }
        }
    }
    matrix_bus
}
