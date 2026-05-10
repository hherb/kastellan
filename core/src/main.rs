use anyhow::{anyhow, Context, Result};
use hhagent_core::audit_mirror::{self, MirrorHandle};
use hhagent_db::conn::ConnectSpec;
use hhagent_db::default_data_dir;
use sqlx::PgPool;
use tokio::signal::unix::{signal, SignalKind};
use tracing::info;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .json()
        .init();

    info!(
        version = hhagent_core::VERSION,
        "hhagent core starting"
    );

    // Bring up the database before announcing readiness or accepting
    // any (future) work. Fail-closed: any error here propagates `?` to
    // a non-zero exit, the supervisor sees the failure, and the next
    // restart attempt re-runs the probe. Running degraded against a
    // half-bootstrapped database would silently lose audit-log rows
    // and corrupt memory writes — a much worse failure mode than a
    // restart loop, which at least surfaces in logs.
    let spec = bring_up_database().await?;

    // Open the daemon-scoped pool and start the audit-log JSONL
    // mirror task. The pool's `after_connect` hook drops privilege to
    // `hhagent_runtime` on every dialed connection (see
    // `db::pool` module docs); the mirror replicates committed
    // `audit_log` rows to `~/.local/state/hhagent/audit-*.jsonl` so
    // operators can `tail -f` without a DB client.
    //
    // Pool failures here are fatal (the dispatcher write site needs
    // them); mirror failures are NOT fatal — the mirror is an
    // operator-visibility layer, not a correctness requirement.
    let pool = hhagent_db::pool::connect_runtime_pool(&spec)
        .await
        .context("opening daemon-scoped Postgres pool")?;
    let mirror = start_audit_mirror(pool.clone()).await;

    // Crash sweep: any task left in 'running' from a previous daemon
    // instance whose lease has elapsed gets marked 'crashed'. Idempotent.
    if let Err(e) = hhagent_db::tasks::sweep_crashed(&pool).await {
        tracing::warn!(error = %e, "tasks::sweep_crashed failed (non-fatal)");
    }

    // Load every prompts/*.md, hash, upsert into agent_prompts.
    let prompts_dir = std::env::var("HHAGENT_PROMPTS_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("prompts"));
    let prompts = hhagent_core::scheduler::prompts::load_prompts_from_dir(&pool, &prompts_dir)
        .await
        .with_context(|| format!("loading prompts from {:?}", prompts_dir))?;

    // LLM router (existing skeleton).
    let router_cfg = hhagent_llm_router::RouterConfig::from_env()
        .map_err(|e| anyhow!("RouterConfig::from_env: {e}"))?;
    let router = Arc::new(
        hhagent_llm_router::Router::new(router_cfg)
            .map_err(|e| anyhow!("Router::new: {e}"))?,
    );

    // Production review pipeline: stub stages in this scope (see spec
    // §6.1). Real implementations replace these structs in place.
    let review = Arc::new(
        hhagent_core::cassandra::review::ChainReviewStage::new(vec![
            Arc::new(hhagent_core::cassandra::review::ConstitutionalGuard),
            Arc::new(hhagent_core::cassandra::review::DeterministicPolicy),
        ]),
    );

    let formulator: Arc<dyn hhagent_core::scheduler::agent::PlanFormulator> =
        Arc::new(hhagent_core::scheduler::agent::RouterAgent::new(
            router.clone(),
            prompts.clone(),
        ));

    // Workspace root: env override → default under HOME state dir.
    let workspace_root = std::env::var(hhagent_core::workspace::ENV_WORKSPACE_ROOT)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var_os("HOME")
                .map(|h| std::path::PathBuf::from(h)
                    .join(".local/state/hhagent/workspace"))
                .unwrap_or_else(|| std::path::PathBuf::from("/tmp/hhagent-workspace"))
        });

    // Sandbox backend (cross-platform). The dispatcher in this commit is
    // a NOT_IMPLEMENTED placeholder; wiring to tool_host::dispatch lands
    // in Task 3.2.bis. The placeholder still owns these handles so the
    // follow-up does not have to change call sites.
    let sandbox: Arc<dyn hhagent_sandbox::SandboxBackend> = sandbox_backend();

    let dispatcher: Arc<dyn hhagent_core::scheduler::inner_loop::StepDispatcher> =
        Arc::new(
            hhagent_core::scheduler::runner::ToolHostStepDispatcher::new(
                pool.clone(),
                sandbox.clone(),
                workspace_root.clone(),
            ),
        );

    let scheduler = hhagent_core::scheduler::spawn_scheduler(
        pool.clone(),
        formulator,
        review,
        dispatcher,
        workspace_root.clone(),
    );
    info!("scheduler spawned (lane_fast + lane_long)");

    wait_for_shutdown().await?;

    // Stop the scheduler before the audit-mirror so any final audit
    // rows it writes during graceful drain land in the mirror's
    // catch-up SELECT.
    scheduler.shutdown().await;

    // Graceful shutdown: stop the mirror task first so any in-flight
    // catch-up SELECT completes its fsync, then close the pool.
    if let Some(handle) = mirror {
        handle.shutdown().await;
    }
    pool.close().await;

    info!("hhagent core shutting down");
    Ok(())
}

/// Resolve cluster connection params from the environment, run the
/// `hhagent-db` probe, emit the bring-up `audit_log` row, and return
/// the resolved [`ConnectSpec`] for downstream pool/mirror setup.
///
/// Knobs:
///   * `HHAGENT_DATA_DIR` (optional) — absolute path to the cluster
///     data dir. The probe assumes
///     `default_socket_dir(data_dir) = <data_dir>/sockets`. Used by
///     integration tests (`core/tests/supervisor_e2e.rs`) to point
///     a test build of `hhagent` at a per-test temp cluster instead
///     of the user's installed one. Production deployments leave
///     this unset and rely on the `$HOME` default below.
///   * `$HOME` — used by `default_data_dir()` when
///     `HHAGENT_DATA_DIR` is unset.
///   * `$USER` — peer-auth role identity (read by
///     `ConnectSpec::default_for`). systemd's `--user` manager and
///     macOS launchd both inherit it from the operator's login
///     record; the probe fails closed if it's missing.
async fn bring_up_database() -> Result<ConnectSpec> {
    let data_dir = match std::env::var_os("HHAGENT_DATA_DIR") {
        Some(p) => std::path::PathBuf::from(p),
        None => default_data_dir()
            .ok_or_else(|| anyhow!("$HOME unset; cannot resolve cluster data dir"))?,
    };
    let spec = ConnectSpec::default_for(&data_dir)
        .context("resolving Postgres connection from environment")?;

    info!(
        data_dir = %data_dir.display(),
        socket_dir = %spec.socket_dir.display(),
        user = %spec.user,
        database = %spec.database,
        "running database probe"
    );

    hhagent_db::probe::run(
        &spec,
        "core",
        "startup",
        serde_json::json!({
            "version": hhagent_core::VERSION,
        }),
    )
    .await
    .context("hhagent_db::probe::run failed")?;

    info!("database probe succeeded");
    Ok(spec)
}

/// Spawn the audit-log JSONL mirror task.
///
/// Uses [`audit_mirror::ENV_STATE_DIR`] when set (test seam, mirroring
/// `HHAGENT_DATA_DIR` for the cluster path), otherwise
/// [`audit_mirror::default_state_dir`] = `$HOME/.local/state/hhagent`.
///
/// Returns `None` if the mirror task could not be spawned. We log the
/// error and continue rather than aborting daemon startup: the audit
/// row in Postgres is the source of truth, and missing JSONL output
/// is an operator-visibility regression, not a correctness one. A
/// future hardening pass could promote this to fail-closed if the
/// JSONL stream becomes a contractual signal for any consumer.
async fn start_audit_mirror(pool: PgPool) -> Option<MirrorHandle> {
    let state_dir = match std::env::var_os(audit_mirror::ENV_STATE_DIR) {
        Some(p) => std::path::PathBuf::from(p),
        None => match audit_mirror::default_state_dir() {
            Some(p) => p,
            None => {
                tracing::warn!(
                    "$HOME unset; audit_mirror disabled (operator visibility \
                     reduced — DB row is still the source of truth)"
                );
                return None;
            }
        },
    };
    match audit_mirror::spawn_mirror(pool, state_dir.clone()).await {
        Ok(h) => {
            info!(state_dir = %state_dir.display(), "audit_mirror spawned");
            Some(h)
        }
        Err(e) => {
            tracing::error!(
                state_dir = %state_dir.display(),
                error = %e,
                "audit_mirror spawn failed; continuing without on-disk JSONL"
            );
            None
        }
    }
}

/// Block until the supervisor (or an interactive operator) tells us
/// to stop. systemd's `systemctl --user stop` sends SIGTERM by default;
/// macOS launchd's `bootout` sends SIGTERM too. SIGINT is the Ctrl-C
/// path for `cargo run` in dev. Either signal returns Ok and lets
/// `main` log a clean shutdown line and exit 0 — exactly what
/// `Restart=on-failure` (systemd's translation of `keep_alive=true`)
/// treats as success, so a stop-induced exit doesn't trip the restart
/// policy and trigger an unwanted respawn.
async fn wait_for_shutdown() -> Result<()> {
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = sigterm.recv() => {}
        _ = sigint.recv() => {}
    }
    Ok(())
}

/// Return the default sandbox backend for the current OS.
///
/// Linux uses bubblewrap (`LinuxBwrap`); macOS uses Seatbelt
/// (`MacosSeatbelt`). The `ToolHostStepDispatcher` owns the resulting
/// `Arc` so the real `tool_host::dispatch` wiring in Task 3.2.bis
/// does not have to change call sites.
#[cfg(target_os = "linux")]
fn sandbox_backend() -> Arc<dyn hhagent_sandbox::SandboxBackend> {
    Arc::new(hhagent_sandbox::linux_bwrap::LinuxBwrap::new())
}

#[cfg(target_os = "macos")]
fn sandbox_backend() -> Arc<dyn hhagent_sandbox::SandboxBackend> {
    Arc::new(hhagent_sandbox::macos_seatbelt::MacosSeatbelt::new())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn sandbox_backend() -> Arc<dyn hhagent_sandbox::SandboxBackend> {
    panic!("no sandbox backend for this OS — only Linux and macOS are supported")
}
