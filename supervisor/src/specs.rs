//! Typed [`ServiceSpec`] builders for the canonical hhagent services.
//!
//! Centralizes "what a real hhagent service looks like" so both the
//! Linux ([`crate::systemd_user`]) and macOS ([`crate::launchd_agents`])
//! backends emit the same semantics from one source of truth.
//!
//! Pure: no I/O, no env probing — the caller resolves the binary path
//! and the log directory and passes them in. That keeps these helpers
//! trivially unit-testable and means a test can synthesize a spec
//! without filesystem side effects.
//!
//! Today this module ships the agent-core and Postgres daemon specs, plus the
//! `hhagent_target_spec` bundle that ties the canonical services together.
//! More will land here as services are added (inference router, etc.).

use std::path::Path;

use crate::{RestartBackoff, ServiceSpec, TargetSpec};

/// Canonical name used for the agent-core daemon's unit/agent file.
///
/// Same string on both OSes — the file becomes `hhagent-core.service`
/// on Linux and `hhagent-core.plist` on macOS, and the launchd `Label`
/// is the same. We deliberately don't use a reverse-DNS form
/// (`org.hhagent.core`) so the same name works through both backends
/// without per-OS branching in caller code (the supervisor lib.rs
/// doc-comment on `ServiceSpec.name` calls out that either style is
/// acceptable).
pub const CORE_SERVICE_NAME: &str = "hhagent-core";

/// Canonical name used for the per-user Postgres daemon's unit/agent
/// file. Same shape rationale as [`CORE_SERVICE_NAME`].
pub const POSTGRES_SERVICE_NAME: &str = "hhagent-postgres";

/// Canonical name of the service bundle that brings up the whole agent.
/// Becomes `hhagent.target` on systemd; on launchd it names the member
/// set only. Same string on both OSes (see [`CORE_SERVICE_NAME`]).
pub const HHAGENT_TARGET_NAME: &str = "hhagent";

/// Build a [`ServiceSpec`] for the agent-core daemon (`hhagent`
/// binary, see `core/src/main.rs`).
///
/// Arguments:
/// - `binary` — absolute path to the compiled `hhagent` binary.
///   Today this is `target/debug/hhagent` in dev. Production install
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
        env: vec![],
        working_dir: None,
        keep_alive: true,
        stdout_log: Some(log_dir.join(format!("{CORE_SERVICE_NAME}.out"))),
        stderr_log: Some(log_dir.join(format!("{CORE_SERVICE_NAME}.err"))),
        after: vec![POSTGRES_SERVICE_NAME.to_string()],
        part_of: Some(HHAGENT_TARGET_NAME.to_string()),
        restart_backoff: Some(RestartBackoff { max_delay_sec: 300, steps: 8 }),
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
///   Caller resolves which one — see [`hhagent_db::find_pg_bin_dir`]
///   in the `db` crate.
/// - `data_dir` — absolute path to the cluster data dir (the one that
///   `hhagent-db-init` populated; postgres is invoked with `-D <path>`).
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
        part_of: Some(HHAGENT_TARGET_NAME.to_string()),
        restart_backoff: Some(RestartBackoff { max_delay_sec: 300, steps: 8 }),
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
pub fn hhagent_target_spec() -> TargetSpec {
    TargetSpec {
        name: HHAGENT_TARGET_NAME.into(),
        members: vec![
            POSTGRES_SERVICE_NAME.into(),
            CORE_SERVICE_NAME.into(),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Pin the canonical name so a typo can't silently rename the
    /// service in a future refactor.
    #[test]
    fn core_service_spec_uses_canonical_name() {
        let spec = core_service_spec(
            Path::new("/usr/local/bin/hhagent"),
            Path::new("/var/log/hhagent"),
        );
        assert_eq!(spec.name, "hhagent-core");
        assert_eq!(spec.name, CORE_SERVICE_NAME);
    }

    /// The caller's binary path must flow through verbatim — both
    /// backends require an absolute `program`, but it's the caller's
    /// job to pass one (this helper is pure).
    #[test]
    fn core_service_spec_program_is_caller_supplied() {
        let bin = PathBuf::from("/opt/hhagent/bin/hhagent");
        let spec = core_service_spec(&bin, Path::new("/tmp/logs"));
        assert_eq!(spec.program, bin);
    }

    /// Defends against accidentally injecting unintended argv.
    #[test]
    fn core_service_spec_args_are_empty() {
        let spec = core_service_spec(
            Path::new("/usr/local/bin/hhagent"),
            Path::new("/tmp"),
        );
        assert!(spec.args.is_empty(), "daemon takes no flags yet");
    }

    /// Defends against accidentally injecting env that would override
    /// the daemon's RUST_LOG default or leak host config.
    #[test]
    fn core_service_spec_env_is_empty() {
        let spec = core_service_spec(
            Path::new("/usr/local/bin/hhagent"),
            Path::new("/tmp"),
        );
        assert!(spec.env.is_empty(), "no env injection by default");
    }

    /// The daemon doesn't depend on cwd; not setting one keeps the
    /// supervisor unit shape small.
    #[test]
    fn core_service_spec_does_not_set_working_dir() {
        let spec = core_service_spec(
            Path::new("/usr/local/bin/hhagent"),
            Path::new("/tmp"),
        );
        assert!(spec.working_dir.is_none());
    }

    /// The core daemon now blocks on SIGTERM/SIGINT and is intended
    /// to stay running until the supervisor stops it. Pin
    /// `keep_alive=true` so a regression that flips it back to
    /// `false` (which would mean a daemon crash silently goes
    /// unrestarted) trips this test.
    #[test]
    fn core_service_spec_keep_alive_is_true() {
        let spec = core_service_spec(
            Path::new("/usr/local/bin/hhagent"),
            Path::new("/tmp"),
        );
        assert!(spec.keep_alive);
    }

    /// stdout / stderr are appended to predictable filenames under
    /// `log_dir`, so an operator can `tail -F` them without guessing.
    #[test]
    fn core_service_spec_emits_log_paths_under_log_dir() {
        let spec = core_service_spec(
            Path::new("/usr/local/bin/hhagent"),
            Path::new("/var/log/hhagent"),
        );
        assert_eq!(
            spec.stdout_log,
            Some(PathBuf::from("/var/log/hhagent/hhagent-core.out"))
        );
        assert_eq!(
            spec.stderr_log,
            Some(PathBuf::from("/var/log/hhagent/hhagent-core.err"))
        );
    }

    /// Distinct destinations so an operator can separate normal logs
    /// from error spam.
    #[test]
    fn core_service_spec_log_paths_are_distinct() {
        let spec = core_service_spec(
            Path::new("/usr/local/bin/hhagent"),
            Path::new("/tmp"),
        );
        assert_ne!(spec.stdout_log, spec.stderr_log);
    }

    // ----- postgres_service_spec -----

    /// Pin the canonical Postgres service name.
    #[test]
    fn postgres_service_spec_uses_canonical_name() {
        let spec = postgres_service_spec(
            Path::new("/usr/lib/postgresql/18/bin/postgres"),
            Path::new("/var/lib/hhagent/pg/data"),
            Path::new("/var/log/hhagent"),
        );
        assert_eq!(spec.name, "hhagent-postgres");
        assert_eq!(spec.name, POSTGRES_SERVICE_NAME);
    }

    /// Caller-supplied program path flows through verbatim.
    #[test]
    fn postgres_service_spec_program_is_caller_supplied() {
        let bin = PathBuf::from("/opt/homebrew/opt/postgresql@18/bin/postgres");
        let spec = postgres_service_spec(
            &bin,
            Path::new("/srv/data"),
            Path::new("/tmp/logs"),
        );
        assert_eq!(spec.program, bin);
    }

    /// Postgres needs `-D <data_dir>` to know where the cluster lives.
    /// Both the flag and the path must be present and in order.
    #[test]
    fn postgres_service_spec_passes_dash_d_data_dir_in_args() {
        let spec = postgres_service_spec(
            Path::new("/usr/lib/postgresql/18/bin/postgres"),
            Path::new("/srv/hhagent/pg/data"),
            Path::new("/tmp/logs"),
        );
        assert_eq!(spec.args.len(), 2, "args: {:?}", spec.args);
        assert_eq!(spec.args[0], "-D");
        assert_eq!(spec.args[1], "/srv/hhagent/pg/data");
    }

    /// We deliberately pass no env so the daemon inherits the clean
    /// environment the supervisor sets up. Defends against accidentally
    /// shipping a `PGDATA` or `PGPORT` that would override postgresql.conf.
    #[test]
    fn postgres_service_spec_env_is_empty() {
        let spec = postgres_service_spec(
            Path::new("/usr/lib/postgresql/18/bin/postgres"),
            Path::new("/d"),
            Path::new("/tmp"),
        );
        assert!(spec.env.is_empty());
    }

    /// Postgres reads everything it needs from `-D <data_dir>` and
    /// writes its pidfile/logs there. Cwd is irrelevant.
    #[test]
    fn postgres_service_spec_does_not_set_working_dir() {
        let spec = postgres_service_spec(
            Path::new("/usr/lib/postgresql/18/bin/postgres"),
            Path::new("/d"),
            Path::new("/tmp"),
        );
        assert!(spec.working_dir.is_none());
    }

    /// Postgres is the system's spine; if it crashes we want it back.
    /// `keep_alive=true` gives `Restart=on-failure` (systemd) /
    /// `KeepAlive=true` (launchd). Pin so a regression flipping this
    /// to `false` (which would leave a crashed PG offline indefinitely)
    /// trips the test.
    #[test]
    fn postgres_service_spec_keep_alive_is_true() {
        let spec = postgres_service_spec(
            Path::new("/usr/lib/postgresql/18/bin/postgres"),
            Path::new("/d"),
            Path::new("/tmp"),
        );
        assert!(spec.keep_alive);
    }

    #[test]
    fn postgres_service_spec_emits_log_paths_under_log_dir() {
        let spec = postgres_service_spec(
            Path::new("/usr/lib/postgresql/18/bin/postgres"),
            Path::new("/d"),
            Path::new("/var/log/hhagent"),
        );
        assert_eq!(
            spec.stdout_log,
            Some(PathBuf::from("/var/log/hhagent/hhagent-postgres.out"))
        );
        assert_eq!(
            spec.stderr_log,
            Some(PathBuf::from("/var/log/hhagent/hhagent-postgres.err"))
        );
    }

    #[test]
    fn postgres_service_spec_log_paths_are_distinct() {
        let spec = postgres_service_spec(
            Path::new("/usr/lib/postgresql/18/bin/postgres"),
            Path::new("/d"),
            Path::new("/tmp"),
        );
        assert_ne!(spec.stdout_log, spec.stderr_log);
    }

    /// The two canonical service names are distinct so they map to
    /// distinct unit/agent files and never collide on disk.
    #[test]
    fn canonical_service_names_are_distinct() {
        assert_ne!(CORE_SERVICE_NAME, POSTGRES_SERVICE_NAME);
        assert_ne!(HHAGENT_TARGET_NAME, CORE_SERVICE_NAME);
        assert_ne!(HHAGENT_TARGET_NAME, POSTGRES_SERVICE_NAME);
    }

    #[test]
    fn postgres_spec_belongs_to_target_with_no_dependency() {
        let spec = postgres_service_spec(
            Path::new("/usr/lib/postgresql/18/bin/postgres"),
            Path::new("/var/lib/hhagent/pgdata"),
            Path::new("/tmp/logs"),
        );
        assert!(spec.after.is_empty(), "postgres is the dependency leaf");
        assert_eq!(spec.part_of.as_deref(), Some(HHAGENT_TARGET_NAME));
    }

    #[test]
    fn core_spec_starts_after_postgres_and_belongs_to_target() {
        let spec = core_service_spec(Path::new("/opt/hhagent/hhagent"), Path::new("/tmp/logs"));
        assert_eq!(spec.after, vec![POSTGRES_SERVICE_NAME.to_string()]);
        assert_eq!(spec.part_of.as_deref(), Some(HHAGENT_TARGET_NAME));
    }

    #[test]
    fn hhagent_target_lists_postgres_then_core_in_order() {
        let t = hhagent_target_spec();
        assert_eq!(t.name, HHAGENT_TARGET_NAME);
        assert_eq!(
            t.members,
            vec![POSTGRES_SERVICE_NAME.to_string(), CORE_SERVICE_NAME.to_string()],
            "Postgres must precede core (start order)"
        );
    }

    #[test]
    fn core_service_spec_carries_expected_backoff_curve() {
        let spec = core_service_spec(
            Path::new("/usr/local/bin/hhagent"),
            Path::new("/tmp"),
        );
        assert_eq!(
            spec.restart_backoff,
            Some(RestartBackoff { max_delay_sec: 300, steps: 8 })
        );
    }

    #[test]
    fn postgres_service_spec_carries_expected_backoff_curve() {
        let spec = postgres_service_spec(
            Path::new("/usr/lib/postgresql/18/bin/postgres"),
            Path::new("/d"),
            Path::new("/tmp"),
        );
        assert_eq!(
            spec.restart_backoff,
            Some(RestartBackoff { max_delay_sec: 300, steps: 8 })
        );
    }
}
