//! Pure unit tests for the `hhagent-db` crate root.
//!
//! Lifted from an inline `#[cfg(test)] mod tests` block in `lib.rs` to keep
//! the crate-root file under the 500-LOC soft cap. The body is byte-identical
//! to what it was inline; `use super::*` resolves to the crate root via the
//! Rust 2018 sibling-directory module pattern (`mod tests;` in `lib.rs`
//! resolves to `src/tests.rs`).
//!
//! `crate::env_lock` (defined in `lib.rs` and shared across the crate's
//! `#[cfg(test)]` code) deliberately stays at the crate root because
//! `db::conn::tests` and any future test module also depend on it. Tests
//! here continue to call `crate::env_lock()` (and one call site uses the
//! bare `env_lock()` form that resolves via `use super::*`).
//!
//! Integration tests against a real Postgres cluster live in
//! `db/tests/postgres_e2e.rs`.

use super::*;

fn opts(dir: &str) -> InitDbOptions {
    InitDbOptions {
        data_dir: PathBuf::from(dir),
        ..InitDbOptions::default()
    }
}

/// First element of the argv must be the binary path itself —
/// `Command::new(&argv[0]).args(&argv[1..])` is the call shape.
#[test]
fn build_initdb_argv_starts_with_binary_path() {
    let argv = build_initdb_argv(Path::new("/usr/bin/initdb"), &opts("/tmp/data"));
    assert_eq!(argv[0], "/usr/bin/initdb");
}

/// `--pgdata` must always carry the configured data dir, since
/// initdb defaults to `$PGDATA` env which we never set.
#[test]
fn build_initdb_argv_includes_pgdata_flag_with_data_dir() {
    let argv = build_initdb_argv(Path::new("/u/initdb"), &opts("/srv/pgdata"));
    let pgdata_idx = argv.iter().position(|a| a == "--pgdata").unwrap();
    assert_eq!(argv[pgdata_idx + 1], "/srv/pgdata");
}

/// Defends against a typo flipping the auth model to `trust` or
/// `md5`. Both `--auth-local=peer` and `--auth-host=reject` must
/// be present — peer is the only auth method that works without
/// a password, reject means a future TCP listener can never
/// authenticate even by accident.
#[test]
fn build_initdb_argv_pins_secure_auth_defaults() {
    let argv = build_initdb_argv(Path::new("/u/initdb"), &opts("/d"));
    assert!(
        argv.iter().any(|a| a == "--auth-local=peer"),
        "argv must include --auth-local=peer, got {argv:?}"
    );
    assert!(
        argv.iter().any(|a| a == "--auth-host=reject"),
        "argv must include --auth-host=reject, got {argv:?}"
    );
}

#[test]
fn build_initdb_argv_omits_data_checksums_when_disabled() {
    let mut o = opts("/d");
    o.data_checksums = false;
    let argv = build_initdb_argv(Path::new("/u/initdb"), &o);
    assert!(!argv.iter().any(|a| a == "--data-checksums"));
}

#[test]
fn build_initdb_argv_includes_data_checksums_when_enabled() {
    let argv = build_initdb_argv(Path::new("/u/initdb"), &opts("/d"));
    assert!(argv.iter().any(|a| a == "--data-checksums"));
}

#[test]
fn build_initdb_argv_falls_back_to_hhagent_when_username_blank() {
    let mut o = opts("/d");
    o.username = "   ".into();
    let argv = build_initdb_argv(Path::new("/u/initdb"), &o);
    assert!(
        argv.iter().any(|a| a == "--username=hhagent"),
        "blank username should fall back to hhagent, got {argv:?}"
    );
}

#[test]
fn build_initdb_argv_uses_supplied_username() {
    let mut o = opts("/d");
    o.username = "alice".into();
    let argv = build_initdb_argv(Path::new("/u/initdb"), &o);
    assert!(argv.iter().any(|a| a == "--username=alice"));
}

fn cfg(socket: &str) -> PgConfigOptions {
    PgConfigOptions {
        socket_dir: PathBuf::from(socket),
        ..PgConfigOptions::default()
    }
}

/// Without `listen_addresses=''` Postgres binds 0.0.0.0:5432 by
/// default — the single most important hardening line.
#[test]
fn auto_conf_disables_tcp_listener() {
    let s = build_postgresql_auto_conf(&cfg("/run/sock"));
    assert!(s.contains("listen_addresses = ''"));
}

/// Socket dir flows through verbatim so the supervisor's
/// `psql -h <socket_dir>` connect string lines up.
#[test]
fn auto_conf_pins_unix_socket_dir() {
    let s = build_postgresql_auto_conf(&cfg("/srv/hhagent/sockets"));
    assert!(
        s.contains("unix_socket_directories = '/srv/hhagent/sockets'"),
        "auto.conf: {s}"
    );
}

/// 0700 means only the owning user can connect — prevents a
/// compromised process running as another OS user on the same
/// host from reaching the socket at all.
#[test]
fn auto_conf_pins_socket_perms_0700() {
    let s = build_postgresql_auto_conf(&cfg("/x"));
    assert!(s.contains("unix_socket_permissions = 0700"), "auto.conf: {s}");
}

#[test]
fn auto_conf_directs_logs_to_stderr_for_supervisor_capture() {
    let s = build_postgresql_auto_conf(&cfg("/x"));
    assert!(s.contains("log_destination = 'stderr'"));
    assert!(s.contains("logging_collector = off"));
}

/// max_connections=0 is an invalid Postgres config; the helper
/// must clamp to at least 1 (we use `.max(1)` in build_…).
#[test]
fn auto_conf_clamps_max_connections_to_at_least_one() {
    let mut o = cfg("/x");
    o.max_connections = 0;
    let s = build_postgresql_auto_conf(&o);
    assert!(s.contains("max_connections = 1"), "auto.conf: {s}");
}

/// shared_buffers=0 is also invalid; clamp.
#[test]
fn auto_conf_clamps_shared_buffers_to_at_least_one_mb() {
    let mut o = cfg("/x");
    o.shared_buffers_mb = 0;
    let s = build_postgresql_auto_conf(&o);
    assert!(s.contains("shared_buffers = 1MB"), "auto.conf: {s}");
}

#[test]
fn auto_conf_marks_itself_as_machine_managed() {
    let s = build_postgresql_auto_conf(&cfg("/x"));
    assert!(
        s.starts_with("# Managed by hhagent-db-init"),
        "auto.conf must start with the don't-edit warning, got: {s}"
    );
}

/// Sanity check: candidate list is in priority order (PG 18 first).
/// Future migrations to PG 19 can flip this without touching tests
/// that depend on auto-detect — those should pass an explicit
/// candidate list.
#[test]
fn pg_bin_dir_candidates_prefer_higher_versions() {
    let cands = default_pg_bin_dir_candidates();
    if cands.is_empty() {
        return; // non-Linux/macOS host
    }
    let first = cands[0].to_string_lossy().into_owned();
    assert!(
        first.contains("18"),
        "first candidate should be PG 18, got {first}"
    );
}

/// `pg_bin_dir_candidates_with_env_override` MUST behave identically
/// to `default_pg_bin_dir_candidates` when the env var is not set.
/// This is the load-bearing no-regression guarantee for every existing
/// test fixture: switching call sites from the default to the
/// override-aware helper must NOT change behaviour for any developer
/// who does not set `HHAGENT_PG_BIN_DIR`.
#[test]
fn pg_bin_dir_candidates_with_env_override_returns_defaults_when_unset() {
    // Hold `env_lock` so concurrent tests in this crate cannot race
    // a stale `HHAGENT_PG_BIN_DIR` into our read.
    let _guard = crate::env_lock();
    let prior = std::env::var(PG_BIN_DIR_ENV).ok();
    std::env::remove_var(PG_BIN_DIR_ENV);

    let got = pg_bin_dir_candidates_with_env_override();
    assert_eq!(
        got,
        default_pg_bin_dir_candidates(),
        "with env unset, override helper must mirror defaults"
    );

    match prior {
        Some(v) => std::env::set_var(PG_BIN_DIR_ENV, v),
        None => std::env::remove_var(PG_BIN_DIR_ENV),
    }
}

/// A non-blank env value is prepended to the defaults so it wins
/// the `find_pg_bin_dir` first-match probe, but the defaults remain
/// in the list as a fallback when the override is itself bogus.
#[test]
fn pg_bin_dir_candidates_with_env_override_prepends_valid_env_path() {
    let _guard = crate::env_lock();
    let prior = std::env::var(PG_BIN_DIR_ENV).ok();
    std::env::set_var(PG_BIN_DIR_ENV, "/custom/pg/bin");

    let got = pg_bin_dir_candidates_with_env_override();
    let defaults = default_pg_bin_dir_candidates();
    assert_eq!(
        got.len(),
        defaults.len() + 1,
        "override must add exactly one entry (prepended)"
    );
    assert_eq!(
        got[0],
        PathBuf::from("/custom/pg/bin"),
        "override must occupy index 0; got {:?}",
        got[0]
    );
    assert_eq!(
        &got[1..],
        defaults.as_slice(),
        "defaults must remain unchanged after the prepended override"
    );

    match prior {
        Some(v) => std::env::set_var(PG_BIN_DIR_ENV, v),
        None => std::env::remove_var(PG_BIN_DIR_ENV),
    }
}

/// An empty-string env value is treated as unset — operators can
/// disable the override via `export HHAGENT_PG_BIN_DIR=""` without
/// having to `unset`. This also tolerates a shell expression like
/// `export HHAGENT_PG_BIN_DIR=$(some_lookup)` evaluating to empty
/// rather than poisoning every test run with an obviously-bogus
/// "" → `PathBuf::from("")` first candidate.
#[test]
fn pg_bin_dir_candidates_with_env_override_treats_empty_string_as_unset() {
    let _guard = crate::env_lock();
    let prior = std::env::var(PG_BIN_DIR_ENV).ok();
    std::env::set_var(PG_BIN_DIR_ENV, "");

    let got = pg_bin_dir_candidates_with_env_override();
    assert_eq!(
        got,
        default_pg_bin_dir_candidates(),
        "empty-string override must behave as if unset"
    );

    match prior {
        Some(v) => std::env::set_var(PG_BIN_DIR_ENV, v),
        None => std::env::remove_var(PG_BIN_DIR_ENV),
    }
}

/// Whitespace-only env value is treated as unset for the same
/// reasons as the empty-string case — defensive against shell
/// quoting accidents like `export HHAGENT_PG_BIN_DIR=" "`.
#[test]
fn pg_bin_dir_candidates_with_env_override_treats_whitespace_as_unset() {
    let _guard = crate::env_lock();
    let prior = std::env::var(PG_BIN_DIR_ENV).ok();
    std::env::set_var(PG_BIN_DIR_ENV, "  \t \n");

    let got = pg_bin_dir_candidates_with_env_override();
    assert_eq!(
        got,
        default_pg_bin_dir_candidates(),
        "whitespace-only override must behave as if unset"
    );

    match prior {
        Some(v) => std::env::set_var(PG_BIN_DIR_ENV, v),
        None => std::env::remove_var(PG_BIN_DIR_ENV),
    }
}

/// `find_pg_bin_dir` must be honest: an empty candidate list is
/// not a silent success.
#[test]
fn find_pg_bin_dir_with_empty_list_is_an_error() {
    let err = find_pg_bin_dir(&[]).unwrap_err();
    assert!(
        matches!(err, DbError::PgBinariesMissing(_)),
        "got {err:?}"
    );
}

/// Find returns a tempdir that we populated with executable
/// `postgres` + `initdb` stub files. Sanity-pins the discovery
/// logic without needing real Postgres on the host.
#[test]
fn find_pg_bin_dir_picks_first_candidate_with_both_binaries() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let tmp = std::env::temp_dir().join(format!(
        "hhagent-db-find-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    for name in ["postgres", "initdb"] {
        let mut f = std::fs::File::create(tmp.join(name)).unwrap();
        f.write_all(b"#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = f.metadata().unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(tmp.join(name), perms).unwrap();
    }
    let dir = find_pg_bin_dir(&[
        PathBuf::from("/no/such/dir/xyz"),
        tmp.clone(),
    ])
    .unwrap();
    assert_eq!(dir, tmp);
    std::fs::remove_dir_all(&tmp).ok();
}

#[test]
fn is_data_dir_initialized_returns_false_for_empty_dir() {
    let tmp = std::env::temp_dir().join(format!(
        "hhagent-db-empty-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    assert!(!is_data_dir_initialized(&tmp));
    std::fs::remove_dir_all(&tmp).ok();
}

#[test]
fn is_data_dir_initialized_returns_true_when_pg_version_present() {
    use std::io::Write;
    let tmp = std::env::temp_dir().join(format!(
        "hhagent-db-populated-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let mut f = std::fs::File::create(tmp.join("PG_VERSION")).unwrap();
    f.write_all(b"18\n").unwrap();
    assert!(is_data_dir_initialized(&tmp));
    std::fs::remove_dir_all(&tmp).ok();
}

#[test]
fn require_absolute_accepts_absolute_paths() {
    require_absolute(Path::new("/etc")).unwrap();
}

#[test]
fn require_absolute_rejects_relative_paths() {
    let err = require_absolute(Path::new("etc")).unwrap_err();
    assert!(matches!(err, DbError::RelativePath(_)));
}

/// Default data dir lives under `$HOME/.local/share/hhagent/pg/data`
/// — same on Linux and macOS. Pinned so a refactor doesn't silently
/// move existing users' data.
#[test]
fn default_data_dir_is_under_xdg_data_home() {
    // Override HOME locally so the test is hermetic. Holds
    // `env_lock` to serialise against any other test mutating
    // process-wide env (currently the `$USER` tests in `conn`).
    let _guard = env_lock();
    let prev = std::env::var_os("HOME");
    std::env::set_var("HOME", "/tmp/fakehome-hhagent-db-test");
    let dd = default_data_dir().unwrap();
    assert_eq!(
        dd,
        PathBuf::from("/tmp/fakehome-hhagent-db-test/.local/share/hhagent/pg/data")
    );
    match prev {
        Some(v) => std::env::set_var("HOME", v),
        None => std::env::remove_var("HOME"),
    }
}

#[test]
fn default_socket_dir_lives_inside_data_dir() {
    let sock = default_socket_dir(Path::new("/srv/hhagent/pg/data"));
    assert_eq!(sock, PathBuf::from("/srv/hhagent/pg/data/sockets"));
}
