//! Typed [`ServiceSpec`] builders for the canonical kastellan services.
//!
//! Centralizes "what a real kastellan service looks like" so both the
//! Linux ([`crate::systemd_user`]) and macOS ([`crate::launchd_agents`])
//! backends emit the same semantics from one source of truth.
//!
//! Pure: no I/O, no env probing — the caller resolves the binary path
//! and the log directory and passes them in. That keeps these helpers
//! trivially unit-testable and means a test can synthesize a spec
//! without filesystem side effects.
//!
//! Today this module ships the agent-core and Postgres daemon specs, plus the
//! `kastellan_target_spec` bundle that ties the canonical services together.
//! More will land here as services are added (inference router, etc.).

use std::path::Path;

use crate::{RestartBackoff, ServiceSpec, TargetSpec};

/// Canonical name used for the agent-core daemon's unit/agent file.
///
/// Same string on both OSes — the file becomes `kastellan-core.service`
/// on Linux and `kastellan-core.plist` on macOS, and the launchd `Label`
/// is the same. We deliberately don't use a reverse-DNS form
/// (`org.kastellan.core`) so the same name works through both backends
/// without per-OS branching in caller code (the supervisor lib.rs
/// doc-comment on `ServiceSpec.name` calls out that either style is
/// acceptable).
pub const CORE_SERVICE_NAME: &str = "kastellan-core";

/// Canonical name used for the per-user Postgres daemon's unit/agent
/// file. Same shape rationale as [`CORE_SERVICE_NAME`].
pub const POSTGRES_SERVICE_NAME: &str = "kastellan-postgres";

/// Canonical name of the service bundle that brings up the whole agent.
/// Becomes `kastellan.target` on systemd; on launchd it names the member
/// set only. Same string on both OSes (see [`CORE_SERVICE_NAME`]).
pub const KASTELLAN_TARGET_NAME: &str = "kastellan";

/// Build a [`ServiceSpec`] for the agent-core daemon (`kastellan`
/// binary, see `core/src/main.rs`).
///
/// Arguments:
/// - `binary` — absolute path to the compiled `kastellan` binary.
///   Today this is `target/debug/kastellan` in dev. Production install
///   location is an open question (see HANDOVER "Open questions"
///   #6); when that lands, the *caller* changes, this helper does
///   not.
/// - `log_dir` — directory where stdout/stderr append logs go.
///   The supervisor backends require the parent directory of each
///   log path to exist; the caller must create `log_dir` before
///   calling [`crate::Supervisor::install`]. The two log files are
///   named `<CORE_SERVICE_NAME>.out` and `<CORE_SERVICE_NAME>.err`
///   under `log_dir`.
///
/// Choices baked into today's spec, with reasons:
/// - `args` is empty: the daemon parses no flags yet.
/// - `env` is empty: `core/src/main.rs` reads `RUST_LOG` from the
///   environment but defaults to `"info"` when unset (see the
///   `unwrap_or_else` in `main`), so we don't need to inject it.
///   When the daemon grows real config-via-env, populate this.
/// - `working_dir` is `None`: nothing in the daemon depends on cwd.
/// - `keep_alive` is `true`: the daemon now blocks on SIGTERM/SIGINT
///   and is meant to stay running until the supervisor stops it. On
///   systemd this becomes `Restart=on-failure` so a *crash* (non-zero
///   exit) triggers a respawn while a clean SIGTERM-induced exit does
///   not. On launchd this becomes `KeepAlive=true` with the same
///   intent (bootout removes the agent from the domain entirely, so
///   `stop` still ends the process for good). The regression test
///   ([`tests::core_service_spec_keep_alive_is_true`]) pins today's
///   value so a regression can't sneak in unnoticed.
/// - `restart_backoff` is `Some({ max_delay_sec: 300, steps: 8 })`: on
///   systemd a crash-looping daemon ramps the restart delay
///   `RestartSec=5` → `RestartMaxDelaySec=300` over `RestartSteps=8`
///   instead of hammering every 5 s; on launchd this is warned-and-ignored
///   (no equivalent knob). Pinned by
///   [`tests::core_service_spec_carries_expected_backoff_curve`].
pub fn core_service_spec(binary: &Path, log_dir: &Path) -> ServiceSpec {
    ServiceSpec {
        name: CORE_SERVICE_NAME.into(),
        program: binary.to_path_buf(),
        args: vec![],
        // Egress force-routing is on by default in the supervised deployment
        // (egress proxy slice #2): every `Net::Allowlist` worker is routed
        // through a per-worker egress-proxy sidecar with no direct network
        // route. **Fail-closed** — the daemon refuses to start if the
        // `kastellan-worker-egress-proxy` binary isn't installed beside it
        // (see `worker_lifecycle::force_route::from_env`), so a packaged
        // deployment must ship the proxy alongside the core binary. Operators
        // can override via the unit's `EnvironmentFile` (set to `0`) for a
        // bring-up without the proxy.
        env: vec![(
            "KASTELLAN_EGRESS_FORCE_ROUTING".to_string(),
            "1".to_string(),
        )],
        working_dir: None,
        keep_alive: true,
        stdout_log: Some(log_dir.join(format!("{CORE_SERVICE_NAME}.out"))),
        stderr_log: Some(log_dir.join(format!("{CORE_SERVICE_NAME}.err"))),
        after: vec![POSTGRES_SERVICE_NAME.to_string()],
        part_of: Some(KASTELLAN_TARGET_NAME.to_string()),
        restart_backoff: Some(RestartBackoff { max_delay_sec: 300, steps: 8 }),
        environment_files: Vec::new(),
    }
}

/// Build a [`ServiceSpec`] for the per-user Postgres daemon (`postgres`
/// binary, see PGDG `postgresql-18` package on Linux / Homebrew
/// `postgresql@18` on macOS).
///
/// Arguments:
/// - `postgres_binary` — absolute path to the `postgres` executable.
///   Linux PGDG default: `/usr/lib/postgresql/18/bin/postgres`.
///   macOS Homebrew default:
///   `/opt/homebrew/opt/postgresql@18/bin/postgres` (Apple Silicon)
///   or `/usr/local/opt/postgresql@18/bin/postgres` (Intel).
///   Caller resolves which one — see [`kastellan_db::find_pg_bin_dir`]
///   in the `db` crate.
/// - `data_dir` — absolute path to the cluster data dir (the one that
///   `kastellan-db-init` populated; postgres is invoked with `-D <path>`).
/// - `log_dir` — directory where the supervisor appends stdout/stderr.
///   Caller must create the dir before [`crate::Supervisor::install`].
///   Files: `<POSTGRES_SERVICE_NAME>.out` and `.err`.
///
/// Choices baked in:
/// - **`args = ["-D", <data_dir>]`** — the only argument postgres needs.
///   The unix socket directory and `listen_addresses=''` come from
///   `postgresql.auto.conf` inside the data dir, so no `-k` flag is
///   needed at the supervisor layer (and we keep the spec minimal so
///   the same shape works whether the caller sets the socket inside
///   or outside the data dir).
/// - **`env` is empty** — postgres does not require any environment
///   variables to start cleanly when given `-D`. Locale defaults are
///   already baked into the cluster by `initdb`'s `--encoding=UTF8`.
///   When workers later need to override `LC_ALL` or set
///   `PGTZ`, populate this; today we deliberately pass nothing so the
///   process inherits a clean env from the supervisor.
/// - **`working_dir = None`** — postgres reads `data_dir` exclusively
///   from `-D` and writes its own pidfile/logs there. Cwd is irrelevant.
/// - **`keep_alive = true`** — postgres is a long-running daemon. On
///   systemd this is `Restart=on-failure RestartSec=5` (a crash means
///   we restart, a clean stop via SIGTERM does not). On launchd this
///   is `KeepAlive=true` (same intent; `bootout` removes the agent
///   from the domain entirely so `stop` still ends the process).
/// - **`restart_backoff = Some({ max_delay_sec: 300, steps: 8 })`** — on
///   systemd a crash-looping cluster ramps `RestartSec=5` →
///   `RestartMaxDelaySec=300` over `RestartSteps=8` rather than respawning
///   every 5 s; on launchd this is warned-and-ignored (no equivalent knob).
///   Pinned by [`tests::postgres_service_spec_carries_expected_backoff_curve`].
///
/// Pure: no I/O, no env probing. Same call → same spec every time.
pub fn postgres_service_spec(
    postgres_binary: &Path,
    data_dir: &Path,
    log_dir: &Path,
) -> ServiceSpec {
    ServiceSpec {
        name: POSTGRES_SERVICE_NAME.into(),
        program: postgres_binary.to_path_buf(),
        args: vec!["-D".into(), data_dir.to_string_lossy().into_owned()],
        env: vec![],
        working_dir: None,
        keep_alive: true,
        stdout_log: Some(log_dir.join(format!("{POSTGRES_SERVICE_NAME}.out"))),
        stderr_log: Some(log_dir.join(format!("{POSTGRES_SERVICE_NAME}.err"))),
        after: vec![],
        part_of: Some(KASTELLAN_TARGET_NAME.to_string()),
        restart_backoff: Some(RestartBackoff { max_delay_sec: 300, steps: 8 }),
        environment_files: Vec::new(),
    }
}

/// Build the canonical [`TargetSpec`] that brings up the whole agent.
///
/// Members in **start order**: Postgres first (the dependency leaf),
/// then core (which must start after Postgres). Inference is **not** a
/// member — it is an operator-managed external dependency that core's
/// startup probe health-checks. Workers are **not** members either —
/// `tool_host` spawns them on demand inside sandboxes when core runs.
///
/// Pure: no I/O, same call → same value.
pub fn kastellan_target_spec() -> TargetSpec {
    TargetSpec {
        name: KASTELLAN_TARGET_NAME.into(),
        members: vec![
            POSTGRES_SERVICE_NAME.into(),
            CORE_SERVICE_NAME.into(),
        ],
    }
}

#[cfg(test)]
mod tests;
