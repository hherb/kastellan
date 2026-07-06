//! Daemon bring-up helpers for the `kastellan` binary entrypoint.
//!
//! These are the cohesive infrastructure pieces `async fn main` (in the parent
//! `main.rs`) delegates to during startup and graceful shutdown: the database
//! probe, the audit-log JSONL mirror, the shutdown-signal wait, the two
//! best-effort startup audit-row writers, and the two pure env-parse helpers.
//! Lifted out of `main.rs` to keep the binary entrypoint under the 500-LOC cap
//! (Item 9b); every function's behaviour is byte-identical to its former inline
//! form — only the visibility widened to `pub(crate)` so the parent can call
//! across the module boundary.
//!
//! The unit tests for the pure parse helpers live in the sibling
//! `main/bootstrap_tests.rs` (`#[path]`-included below); `super::` there
//! resolves to this module.

use anyhow::{anyhow, Context, Result};
use kastellan_db::conn::ConnectSpec;
use kastellan_db::default_data_dir;
use sqlx::PgPool;
use tokio::signal::unix::{signal, SignalKind};
use tracing::info;

use kastellan_core::audit_mirror::{self, MirrorHandle};

/// Resolve cluster connection params from the environment, run the
/// `kastellan-db` probe, emit the bring-up `audit_log` row, and return
/// the resolved [`ConnectSpec`] for downstream pool/mirror setup.
///
/// Knobs:
///   * `KASTELLAN_DATA_DIR` (optional) — absolute path to the cluster
///     data dir. The probe assumes
///     `default_socket_dir(data_dir) = <data_dir>/sockets`. Used by
///     integration tests (`core/tests/supervisor_e2e.rs`) to point
///     a test build of `kastellan` at a per-test temp cluster instead
///     of the user's installed one. Production deployments leave
///     this unset and rely on the `$HOME` default below.
///   * `$HOME` — used by `default_data_dir()` when
///     `KASTELLAN_DATA_DIR` is unset.
///   * `$USER` — peer-auth role identity (read by
///     `ConnectSpec::default_for`). systemd's `--user` manager and
///     macOS launchd both inherit it from the operator's login
///     record; the probe fails closed if it's missing.
pub(crate) async fn bring_up_database() -> Result<ConnectSpec> {
    let data_dir = match std::env::var_os("KASTELLAN_DATA_DIR") {
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

    kastellan_db::probe::run(
        &spec,
        "core",
        "startup",
        serde_json::json!({
            "version": kastellan_core::VERSION,
        }),
    )
    .await
    .context("kastellan_db::probe::run failed")?;

    info!("{}", kastellan_core::STARTUP_READY_MSG);
    Ok(spec)
}

/// Spawn the audit-log JSONL mirror task.
///
/// Uses [`audit_mirror::ENV_STATE_DIR`] when set (test seam, mirroring
/// `KASTELLAN_DATA_DIR` for the cluster path), otherwise
/// [`audit_mirror::default_state_dir`] = `$HOME/.local/state/kastellan`.
///
/// Returns `None` if the mirror task could not be spawned. We log the
/// error and continue rather than aborting daemon startup: the audit
/// row in Postgres is the source of truth, and missing JSONL output
/// is an operator-visibility regression, not a correctness one. A
/// future hardening pass could promote this to fail-closed if the
/// JSONL stream becomes a contractual signal for any consumer.
pub(crate) async fn start_audit_mirror(pool: PgPool) -> Option<MirrorHandle> {
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
pub(crate) async fn wait_for_shutdown() -> Result<()> {
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = sigterm.recv() => {}
        _ = sigint.recv() => {}
    }
    Ok(())
}

/// Best-effort `registry.loaded` audit row. Written after the tool registry is
/// built (the builder is side-effect-free); a transient DB failure here is
/// logged and swallowed by the caller — it must not block daemon bring-up.
pub(crate) async fn write_registry_loaded_row(
    pool: &sqlx::PgPool,
    tools: &[kastellan_core::registry_build::LoadedToolRecord],
) -> Result<(), kastellan_db::DbError> {
    let payload = kastellan_core::registry_build::build_registry_loaded_payload(tools);
    kastellan_db::audit::insert(
        pool,
        "core",
        kastellan_core::scheduler::audit::ACTION_REGISTRY_LOADED,
        payload,
    )
    .await
    .map(|_| ())
}

/// Best-effort `l0.seeded` audit row. The L0 rows are already committed by the
/// time this runs, so (like [`write_registry_loaded_row`]) the caller logs and
/// swallows any transient DB failure rather than aborting startup.
pub(crate) async fn write_l0_seeded_row(
    pool: &sqlx::PgPool,
    report: &kastellan_core::memory::l0_seed::L0SeedReport,
) -> Result<(), kastellan_db::DbError> {
    let payload = serde_json::json!({
        "rules_loaded": report.rules_loaded,
        "new_rows_written": report.new_rows_written,
        "unchanged_skipped": report.unchanged_skipped,
        "source_path": report.source_path.to_string_lossy(),
        "source_sha256": report.source_sha256,
        "entities_linked": report.entities_linked,
        "link_failures": report.link_failures,
    });
    kastellan_db::audit::insert(
        pool,
        "core",
        kastellan_core::scheduler::audit::ACTION_L0_SEEDED,
        payload,
    )
    .await
    .map(|_| ())
}

/// Parses the `KASTELLAN_BOOTSTRAP_SECRETS` CSV value into a list of
/// trimmed, non-empty secret names. Handles leading/trailing commas,
/// internal whitespace, and all-whitespace entries.
///
/// Pure function — no I/O, no side effects.
pub(crate) fn parse_bootstrap_secrets_csv(csv: &str) -> Vec<&str> {
    csv.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

/// Parses the **test-only** `KASTELLAN_TEST_VAULT_SEED` value (`<ref_hex>=<plaintext>`)
/// into its `(ref_hex, plaintext)` halves, splitting on the **first** `=` only
/// (a secret may itself contain `=`). Returns `None` when no `=` is present.
///
/// Pure function — no I/O, no side effects, no trimming (the plaintext is taken
/// verbatim; trimming a secret could corrupt it). The ref tail's own format is
/// validated by [`kastellan_core::secrets::Vault::seed_known_ref_for_test`].
///
/// `#[cfg(debug_assertions)]`: the only caller is the debug-only seed block, so
/// this helper does not exist in a release build (keeps it off the production
/// surface and clippy-`dead_code`-clean).
#[cfg(debug_assertions)]
pub(crate) fn parse_test_vault_seed(spec: &str) -> Option<(&str, &str)> {
    spec.split_once('=')
}

#[cfg(test)]
#[path = "bootstrap_tests.rs"]
mod tests;
