use anyhow::{anyhow, Context, Result};
use hhagent_db::conn::ConnectSpec;
use hhagent_db::default_data_dir;
use tokio::signal::unix::{signal, SignalKind};
use tracing::info;

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
    bring_up_database().await?;

    wait_for_shutdown().await?;

    info!("hhagent core shutting down");
    Ok(())
}

/// Resolve cluster connection params from the environment, run the
/// `hhagent-db` probe, and emit the bring-up `audit_log` row.
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
async fn bring_up_database() -> Result<()> {
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
    Ok(())
}

/// Block until the supervisor (or an interactive operator) tells us
/// to stop. systemd's `systemctl --user stop` sends SIGTERM by default;
/// macOS launchd's `bootout` sends SIGTERM too. SIGINT is the Ctrl-C
/// path for `cargo run` in dev. Either signal returns Ok and lets
/// `main` log a clean shutdown line and exit 0 — exactly what
/// `Restart=on-failure` (systemd's translation of `keep_alive=true`)
/// treats as success, so a stop-induced exit doesn't trip the restart
/// policy and trigger an unwanted respawn.
///
/// The Phase 1 scheduler will plug in here. Today the daemon has no
/// periodic work, so the signal future is the *only* thing that
/// should ever wake us — anything else would be a bug.
async fn wait_for_shutdown() -> Result<()> {
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = sigterm.recv() => {}
        _ = sigint.recv() => {}
    }
    Ok(())
}
