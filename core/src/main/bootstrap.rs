//! Daemon bring-up helpers for the `kastellan` binary entrypoint.
//!
//! These are the cohesive infrastructure pieces `async fn main` (in the parent
//! `main.rs`) delegates to during startup and graceful shutdown: the database
//! probe, the audit-log JSONL mirror, the two-step shutdown signals (install,
//! then wait — #810), the two best-effort startup audit-row writers, and the
//! two pure env-parse helpers. Lifted out of `main.rs` to keep the binary
//! entrypoint under the 500-LOC cap (Item 9b), visibility widened to
//! `pub(crate)` so the parent can call across the module boundary. The
//! shutdown-signal wait has since been split in two (#810); see
//! [`ShutdownSignals`].
//!
//! The unit tests for the pure parse helpers live in the sibling
//! `main/bootstrap_tests.rs` (`#[path]`-included below); `super::` there
//! resolves to this module.

use anyhow::{anyhow, Context, Result};
use kastellan_db::conn::ConnectSpec;
use kastellan_db::default_data_dir;
use sqlx::PgPool;
use tokio::signal::unix::{signal, Signal, SignalKind};
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

/// The daemon's stop signals, registered but not yet awaited.
///
/// systemd's `systemctl --user stop` sends SIGTERM by default; macOS
/// launchd's `bootout` sends SIGTERM too. SIGINT is the Ctrl-C path for
/// `cargo run` in dev.
///
/// Shutdown is split into two steps (#810) so that the step which can FAIL
/// runs before anything is started that the graceful shutdown must stop:
///
/// 1. [`install_shutdown_signals`] registers the handlers. Registration can
///    fail, so `main` does it BEFORE it spawns the scheduler and the channel
///    supervisors. A failure there aborts start-up while nothing is yet
///    writing audit rows in the background, so nothing can be lost.
/// 2. [`ShutdownSignals::wait`] awaits them, and cannot fail. So once the
///    channels are running, `main` always reaches the audit drain, the ledger
///    close and the `[audit-lost]` report. (Before #810 the registration ran
///    here, after the channels had started, and a failure returned straight
///    out of `main`: the runtime drop then cancelled every pending audit row
///    with no line at all.)
///
/// A signal delivered between the two steps is not lost: once registered,
/// tokio's handler records it, and `wait` returns at once.
pub(crate) struct ShutdownSignals {
    sigterm: Signal,
    sigint: Signal,
}

/// Register the SIGTERM and SIGINT handlers — step 1 of [`ShutdownSignals`].
///
/// From this call on, the process no longer dies on SIGTERM/SIGINT; the
/// signal is held for [`ShutdownSignals::wait`] instead.
pub(crate) fn install_shutdown_signals() -> Result<ShutdownSignals> {
    let sigterm = signal(SignalKind::terminate()).context("registering the SIGTERM handler")?;
    let sigint = signal(SignalKind::interrupt()).context("registering the SIGINT handler")?;
    Ok(ShutdownSignals { sigterm, sigint })
}

impl ShutdownSignals {
    /// Block until the supervisor (or an interactive operator) tells us to
    /// stop — step 2 of [`ShutdownSignals`].
    ///
    /// Returning lets `main` log a clean shutdown line and exit 0 — exactly
    /// what `Restart=on-failure` (systemd's translation of `keep_alive=true`)
    /// treats as success, so a stop-induced exit doesn't trip the restart
    /// policy and trigger an unwanted respawn.
    ///
    /// Deliberately returns `()`, not `Result`: there must be no `?` between
    /// the channels starting and the audit drain in `main`.
    pub(crate) async fn wait(mut self) {
        // `recv` yields `None` only once the signal driver is gone, which
        // happens when the runtime shuts down — then there is nothing left to
        // wait for either way, so `None` also ends the wait.
        tokio::select! {
            _ = self.sigterm.recv() => {}
            _ = self.sigint.recv() => {}
        }
    }
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

/// Log whether the operator overlay actually reached this process (#531).
///
/// The overlay (`~/.config/kastellan/kastellan.env.local`) is where tuned
/// settings live so they survive the `kastellan.env` regeneration every install
/// performs (#458). Nothing confirmed it was found: "absent by choice" and
/// "absent by typo" produced identical silence, and the only symptom of the
/// second was the bot eventually answering wrongly.
///
/// **What this checks is deliberately stronger than "the file exists".** The
/// daemon cannot ask which file a variable came from — systemd merges every
/// `EnvironmentFile=` into one environment before exec, and launchd bakes the
/// pairs into the plist at install time — so re-reading the overlay and
/// printing a key count would assert only that a file is on disk. Comparing
/// each declared key against this process's own environment instead answers the
/// question actually being asked, identically on both platforms, and catches
/// three real failures: a mis-pathed overlay, a dropped `EnvironmentFile=`
/// directive (#530's fail-open), and an overlay ordered *before* the generated
/// file, where `kastellan.env` wins and the tuning is silently overridden.
///
/// Never fatal and never logs a value. A missing `$HOME` or an unreadable
/// overlay is reported and startup continues: this is an observability layer,
/// and refusing to boot over it would trade a silent misconfiguration for a
/// loud outage.
///
/// The verdict and its severity are both decided by
/// [`kastellan_supervisor::env_file::render_overlay_check`], which also warns on
/// the two shapes that otherwise pass as healthy — an overlay declaring **zero**
/// keys, and one with lines the grammar refused. This function is deliberately
/// thin: everything worth testing lives in the shared module that compiles on
/// both hosts.
///
/// **Absent is logged too, naming the path**, even though it is the normal
/// state on a host that wants no overlay. That line is the whole diagnostic for
/// #531's own scenario: the operator creates the overlay *after* installing,
/// their heredoc lands in `~/.config/kastellan.env.local` (missing directory
/// component), and they restart and read the log. Absence at the path we
/// actually read is the only thing that tells them; silence here reads as
/// confirmation. It stays `info!` rather than `warn!`, because on most hosts
/// nothing is wrong.
pub(crate) fn report_operator_overlay() {
    use kastellan_supervisor::env_file::{check_overlay, render_overlay_check, OverlaySeverity};

    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        tracing::warn!("$HOME unset; cannot check for the operator overlay");
        return;
    };
    let path = kastellan_core::install::plan::env_local_file_for(&home);

    // One read, one verdict, one wording — all of it in the shared `env_file`
    // module where both hosts compile it and tests can observe every branch.
    // The first cut classified here and then re-read the file to get the values
    // for the comparison, which bought a window where the count and the verdict
    // could describe different contents, and a second error path whose wording
    // no test ever saw.
    let check = check_overlay(&path, |k| std::env::var(k).ok());
    let (severity, line) = render_overlay_check(&path, &check);
    match severity {
        OverlaySeverity::Info => info!("{line}"),
        OverlaySeverity::Warn => tracing::warn!("{line}"),
    }
}

#[cfg(test)]
#[path = "bootstrap_tests.rs"]
mod tests;
