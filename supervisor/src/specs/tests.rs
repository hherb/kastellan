use super::*;
use std::path::PathBuf;

/// Pin the canonical name so a typo can't silently rename the
/// service in a future refactor.
#[test]
fn core_service_spec_uses_canonical_name() {
    let spec = core_service_spec(
        Path::new("/usr/local/bin/kastellan"),
        Path::new("/var/log/kastellan"),
    );
    assert_eq!(spec.name, "kastellan-core");
    assert_eq!(spec.name, CORE_SERVICE_NAME);
}

/// The caller's binary path must flow through verbatim — both
/// backends require an absolute `program`, but it's the caller's
/// job to pass one (this helper is pure).
#[test]
fn core_service_spec_program_is_caller_supplied() {
    let bin = PathBuf::from("/opt/kastellan/bin/kastellan");
    let spec = core_service_spec(&bin, Path::new("/tmp/logs"));
    assert_eq!(spec.program, bin);
}

/// Defends against accidentally injecting unintended argv.
#[test]
fn core_service_spec_args_are_empty() {
    let spec = core_service_spec(
        Path::new("/usr/local/bin/kastellan"),
        Path::new("/tmp"),
    );
    assert!(spec.args.is_empty(), "daemon takes no flags yet");
}

/// The unit injects exactly one env var: the egress force-routing
/// opt-in (slice #2), on by default in the supervised deployment. Pin
/// that it is present and is the *only* injected var — so a regression
/// that drops the secure default (or starts leaking other host config)
/// trips this test.
#[test]
fn core_service_spec_enables_egress_force_routing() {
    let spec = core_service_spec(
        Path::new("/usr/local/bin/kastellan"),
        Path::new("/tmp"),
    );
    assert_eq!(
        spec.env,
        vec![("KASTELLAN_EGRESS_FORCE_ROUTING".to_string(), "1".to_string())],
        "the core unit must enable egress force-routing by default, and inject nothing else"
    );
}

/// The daemon doesn't depend on cwd; not setting one keeps the
/// supervisor unit shape small.
#[test]
fn core_service_spec_does_not_set_working_dir() {
    let spec = core_service_spec(
        Path::new("/usr/local/bin/kastellan"),
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
        Path::new("/usr/local/bin/kastellan"),
        Path::new("/tmp"),
    );
    assert!(spec.keep_alive);
}

/// stdout / stderr are appended to predictable filenames under
/// `log_dir`, so an operator can `tail -F` them without guessing.
#[test]
fn core_service_spec_emits_log_paths_under_log_dir() {
    let spec = core_service_spec(
        Path::new("/usr/local/bin/kastellan"),
        Path::new("/var/log/kastellan"),
    );
    assert_eq!(
        spec.stdout_log,
        Some(PathBuf::from("/var/log/kastellan/kastellan-core.out"))
    );
    assert_eq!(
        spec.stderr_log,
        Some(PathBuf::from("/var/log/kastellan/kastellan-core.err"))
    );
}

/// Distinct destinations so an operator can separate normal logs
/// from error spam.
#[test]
fn core_service_spec_log_paths_are_distinct() {
    let spec = core_service_spec(
        Path::new("/usr/local/bin/kastellan"),
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
        Path::new("/var/lib/kastellan/pg/data"),
        Path::new("/var/log/kastellan"),
    );
    assert_eq!(spec.name, "kastellan-postgres");
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
        Path::new("/srv/kastellan/pg/data"),
        Path::new("/tmp/logs"),
    );
    assert_eq!(spec.args.len(), 2, "args: {:?}", spec.args);
    assert_eq!(spec.args[0], "-D");
    assert_eq!(spec.args[1], "/srv/kastellan/pg/data");
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
        Path::new("/var/log/kastellan"),
    );
    assert_eq!(
        spec.stdout_log,
        Some(PathBuf::from("/var/log/kastellan/kastellan-postgres.out"))
    );
    assert_eq!(
        spec.stderr_log,
        Some(PathBuf::from("/var/log/kastellan/kastellan-postgres.err"))
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
    assert_ne!(KASTELLAN_TARGET_NAME, CORE_SERVICE_NAME);
    assert_ne!(KASTELLAN_TARGET_NAME, POSTGRES_SERVICE_NAME);
}

#[test]
fn postgres_spec_belongs_to_target_with_no_dependency() {
    let spec = postgres_service_spec(
        Path::new("/usr/lib/postgresql/18/bin/postgres"),
        Path::new("/var/lib/kastellan/pgdata"),
        Path::new("/tmp/logs"),
    );
    assert!(spec.after.is_empty(), "postgres is the dependency leaf");
    assert_eq!(spec.part_of.as_deref(), Some(KASTELLAN_TARGET_NAME));
}

#[test]
fn core_spec_starts_after_postgres_and_belongs_to_target() {
    let spec = core_service_spec(Path::new("/opt/kastellan/kastellan"), Path::new("/tmp/logs"));
    assert_eq!(spec.after, vec![POSTGRES_SERVICE_NAME.to_string()]);
    assert_eq!(spec.part_of.as_deref(), Some(KASTELLAN_TARGET_NAME));
}

#[test]
fn kastellan_target_lists_postgres_then_core_in_order() {
    let t = kastellan_target_spec();
    assert_eq!(t.name, KASTELLAN_TARGET_NAME);
    assert_eq!(
        t.members,
        vec![POSTGRES_SERVICE_NAME.to_string(), CORE_SERVICE_NAME.to_string()],
        "Postgres must precede core (start order)"
    );
}

#[test]
fn core_service_spec_carries_expected_backoff_curve() {
    let spec = core_service_spec(
        Path::new("/usr/local/bin/kastellan"),
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
