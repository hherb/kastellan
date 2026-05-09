//! hhagent-db: bring up a private per-user PostgreSQL instance.
//!
//! Containment shape:
//!   - **Data dir** lives under `~/.local/share/hhagent/pg/data` (XDG-style).
//!     One cluster, one user, no system-wide PG involvement.
//!   - **Socket dir** lives inside the data dir itself
//!     (`<data_dir>/sockets`, mode 0700). Avoids the `/run/user/<uid>`
//!     vs `/tmp` cross-platform mess and inherits the data dir's
//!     ownership/permissions.
//!   - **No TCP**: `listen_addresses=''` in `postgresql.auto.conf`.
//!   - **Peer auth only**: `--auth-local=peer --auth-host=reject` at
//!     `initdb` time bakes the `pg_hba.conf` policy in. Combined with
//!     "no TCP listener", remote auth is structurally impossible.
//!
//! This module is split between *pure functions* (everything in `lib.rs`,
//! tested without Postgres installed — they only build argv vectors and
//! config strings) and the small *driver* in `bin/hhagent-db-init.rs`
//! that performs the I/O.
//!
//! The split mirrors `sandbox::linux_bwrap` (pure `build_argv` separately
//! testable from the spawn) and `supervisor::systemd_user` (pure
//! `build_unit_file` separately testable from the actual `systemctl`
//! call). Same pattern, same payoff: the parts that decide *what* to do
//! are unit-tested with no host dependencies.

use std::path::{Path, PathBuf};

use thiserror::Error;

pub mod conn;
pub mod graph;
pub mod probe;

/// Serialise unit tests that mutate process-wide environment variables.
///
/// `cargo test` runs all unit tests in a single binary across multiple
/// threads by default. Tests that `std::env::set_var` / `remove_var` race
/// against any *other* test in this crate that reads the same variable —
/// today that's `$USER` (read by `conn::current_os_user`) and `$HOME`
/// (read by `default_data_dir`). Hold this guard for the entire scope of
/// the env mutation; the guard's `Drop` releases the lock.
///
/// Mutex is poisoned-on-panic-resistant via `unwrap_or_else(into_inner)`
/// so a panicking test cannot wedge the rest of the suite.
#[cfg(test)]
pub(crate) fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// Compile-time-embedded migration set.
///
/// `sqlx::migrate!()` expands at build time into a sorted list of every
/// `<version>_<slug>.sql` file under [`db/migrations/`]. Embedding (vs
/// reading from disk at runtime) means a binary install does not need
/// the source tree on disk — same shape as the Linux/macOS sandbox
/// fixture binaries that are baked into `target/`.
///
/// Run via `MIGRATOR.run(&pool).await?` from [`probe::run`]; sqlx
/// handles the `_sqlx_migrations` bookkeeping table itself, so calling
/// this on an already-up-to-date database is a cheap no-op.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Errors surfaced by the db helpers, CLI, and runtime probe.
#[derive(Debug, Error)]
pub enum DbError {
    /// A path argument that must be absolute was relative.
    #[error("path must be absolute: {0}")]
    RelativePath(PathBuf),

    /// `initdb` exited non-zero. Wrapped string is the captured stderr
    /// (trimmed) so the operator can see what went wrong.
    #[error("initdb failed: {0}")]
    InitDbFailed(String),

    /// I/O error during data-dir setup (write of `postgresql.auto.conf`,
    /// directory creation, etc.).
    #[error("db I/O error: {0}")]
    Io(String),

    /// Could not locate Postgres binaries on the host. The wrapped string
    /// lists the candidates we probed so the operator can fix their PATH
    /// or pass `--bin-dir` explicitly.
    #[error("postgres binaries not found; tried: {0}")]
    PgBinariesMissing(String),

    /// Could not connect to Postgres. Wraps `sqlx::Error::Display` so
    /// the underlying cause (UDS socket missing, role not allowed by
    /// pg_hba, server still booting) is visible in the log line.
    #[error("postgres connection failed: {0}")]
    Connect(String),

    /// `sqlx::migrate!().run(&pool)` failed. Wrapped string is the
    /// `MigrateError::Display` — typically a SQL error in one of the
    /// embedded migrations or a checksum mismatch on a previously
    /// applied file.
    #[error("postgres migration failed: {0}")]
    Migrate(String),

    /// A specific SQL query (one-off DDL like `CREATE DATABASE`,
    /// the `audit_log` insert in [`probe::run`], or anything in
    /// [`graph::PgGraph`]) returned an error.
    #[error("postgres query failed: {0}")]
    Query(String),

    /// A required environment variable was unset. Today this is just
    /// `$USER`, used as the peer-auth identity in
    /// [`conn::ConnectSpec::default_for`]. Any others added later
    /// (e.g. `$XDG_DATA_HOME`) reuse this variant.
    #[error("required environment variable unset: {0}")]
    EnvVarMissing(&'static str),
}

impl From<std::io::Error> for DbError {
    fn from(value: std::io::Error) -> Self {
        DbError::Io(value.to_string())
    }
}

/// Inputs to [`build_initdb_argv`]. Caller resolves all paths.
///
/// The struct exists so we can grow new options (e.g. `--locale`,
/// `--data-checksums`) without breaking the function signature.
#[derive(Clone, Debug)]
pub struct InitDbOptions {
    /// Absolute path to the cluster data dir. `initdb` creates it if
    /// absent or refuses if non-empty (we treat both as the caller's
    /// responsibility — see [`is_data_dir_initialized`] for the
    /// idempotency check).
    pub data_dir: PathBuf,
    /// Username (Postgres role) that owns the cluster. Almost always
    /// the OS username running `initdb`. Defaults to "hhagent" if empty.
    pub username: String,
    /// Cluster encoding. Default: "UTF8".
    pub encoding: String,
    /// When `true`, request `--data-checksums`. Cheap CRC of every page;
    /// catches silent disk corruption. Recommended on; flipping later
    /// requires a `pg_checksums` rebuild so set it correctly the first
    /// time. Default: `true`.
    pub data_checksums: bool,
}

impl Default for InitDbOptions {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::new(),
            username: "hhagent".into(),
            encoding: "UTF8".into(),
            data_checksums: true,
        }
    }
}

/// Build the argv vector for `initdb`.
///
/// The first element is the absolute path to the `initdb` binary; the
/// rest are flags. The caller invokes `Command::new(&argv[0]).args(&argv[1..])`
/// — splitting program from arg-tail at call time is the single
/// platform-independent shape Rust's `std::process::Command` likes.
///
/// Flags baked in (with reasons):
/// - `--pgdata <dir>`: where the cluster lives.
/// - `--username <name>`: superuser role for the new cluster.
/// - `--encoding=UTF8`: the only sane default for a modern Postgres.
/// - `--auth-local=peer`: local UDS connections must come from the same
///   OS uid as the role they're connecting as. Combined with the
///   listen-on-UDS-only config below, this is the only auth path that
///   works at all, so remote auth is structurally impossible.
/// - `--auth-host=reject`: even though we set `listen_addresses=''` in
///   the runtime config, baking `host all all reject` into pg_hba.conf
///   means *any* future operator misconfiguration that re-enables TCP
///   still gets refused at the auth layer. Defense-in-depth.
/// - `--data-checksums` (when enabled): page-level CRC.
///
/// Pure: no I/O, deterministic — same input, same argv every call.
pub fn build_initdb_argv(initdb_bin: &Path, opts: &InitDbOptions) -> Vec<String> {
    let mut argv: Vec<String> = Vec::with_capacity(8);
    argv.push(initdb_bin.to_string_lossy().into_owned());

    argv.push("--pgdata".into());
    argv.push(opts.data_dir.to_string_lossy().into_owned());

    let username = if opts.username.trim().is_empty() {
        "hhagent"
    } else {
        opts.username.as_str()
    };
    argv.push(format!("--username={}", username));

    let encoding = if opts.encoding.trim().is_empty() {
        "UTF8"
    } else {
        opts.encoding.as_str()
    };
    argv.push(format!("--encoding={}", encoding));

    argv.push("--auth-local=peer".into());
    argv.push("--auth-host=reject".into());

    if opts.data_checksums {
        argv.push("--data-checksums".into());
    }

    argv
}

/// Inputs to [`build_postgresql_auto_conf`].
///
/// `socket_dir` should already exist with mode 0700 by the time
/// Postgres starts (the driver creates it after `initdb`).
#[derive(Clone, Debug)]
pub struct PgConfigOptions {
    /// Absolute path to the directory holding the unix socket.
    pub socket_dir: PathBuf,
    /// Maximum connections. Default 32 is plenty for a single-user
    /// agent host; bump if you point multiple workers at it.
    pub max_connections: u32,
    /// shared_buffers in megabytes. Default 256 MiB — comfortably under
    /// any laptop's RAM, big enough for a memory store with embeddings.
    pub shared_buffers_mb: u32,
}

impl Default for PgConfigOptions {
    fn default() -> Self {
        Self {
            socket_dir: PathBuf::new(),
            max_connections: 32,
            shared_buffers_mb: 256,
        }
    }
}

/// Build the contents of `<data_dir>/postgresql.auto.conf` that we
/// drop after `initdb`.
///
/// Postgres applies `postgresql.auto.conf` *after* `postgresql.conf`,
/// so values here always win. This is the canonical override mechanism
/// (the same file `ALTER SYSTEM SET …` writes into).
///
/// Settings, with reasons:
/// - `listen_addresses = ''` — no TCP listener at all. Remote auth is
///   structurally impossible.
/// - `unix_socket_directories = '<dir>'` — single named directory; we
///   own its lifecycle.
/// - `unix_socket_permissions = 0700` — only the owning user can `connect()`.
///   Defense-in-depth on top of peer auth (a compromised app running
///   as a different user on the same host literally cannot open the
///   socket file).
/// - `log_destination = 'stderr'` + `logging_collector = off` — let the
///   service supervisor (systemd / launchd) capture the stream into
///   the per-service log files we already configure on `ServiceSpec`.
/// - `password_encryption = scram-sha-256` — defense-in-depth even
///   though peer auth means passwords are never used today; if a future
///   role ever gets `host` rules (which would also require the existing
///   `--auth-host=reject` to be loosened), at least the hash is modern.
///
/// Pure: returns a `String`, performs no I/O. Caller writes it to
/// disk via the atomic-rename idiom (same as `supervisor::systemd_user::install`).
pub fn build_postgresql_auto_conf(opts: &PgConfigOptions) -> String {
    let socket = opts.socket_dir.to_string_lossy();
    let max_conn = opts.max_connections.max(1);
    let buffers = opts.shared_buffers_mb.max(1);

    format!(
        "# Managed by hhagent-db-init. Do not edit by hand.\n\
         # Postgres applies this file after postgresql.conf, so values here win.\n\
         listen_addresses = ''\n\
         unix_socket_directories = '{socket}'\n\
         unix_socket_permissions = 0700\n\
         log_destination = 'stderr'\n\
         logging_collector = off\n\
         max_connections = {max_conn}\n\
         shared_buffers = {buffers}MB\n\
         password_encryption = 'scram-sha-256'\n",
        socket = socket,
        max_conn = max_conn,
        buffers = buffers,
    )
}

/// Default candidate directories to search for `postgres` / `initdb`,
/// in priority order (highest version first).
///
/// We don't trust `$PATH` because a user could have an old PG in PATH
/// (e.g. `psql` from a different version installed by Homebrew or
/// Postgres.app) while having PG 18 binaries available at the canonical
/// PGDG / Homebrew locations. Returning an explicit candidate list and
/// preferring higher versions gives deterministic auto-detection.
///
/// Linux candidates target the PGDG layout (`/usr/lib/postgresql/<ver>/bin`).
/// macOS candidates target Homebrew on Apple Silicon
/// (`/opt/homebrew/opt/postgresql@<ver>/bin`) and Intel
/// (`/usr/local/opt/postgresql@<ver>/bin`).
///
/// Versions probed: 18 down to 14. Older versions are not interesting —
/// the project explicitly targets PG 18+ and 14 is the oldest still
/// receiving upstream community support during 2026.
pub fn default_pg_bin_dir_candidates() -> Vec<PathBuf> {
    let mut out = Vec::with_capacity(16);
    for ver in [18u32, 17, 16, 15, 14] {
        #[cfg(target_os = "linux")]
        {
            out.push(PathBuf::from(format!("/usr/lib/postgresql/{ver}/bin")));
        }
        #[cfg(target_os = "macos")]
        {
            out.push(PathBuf::from(format!("/opt/homebrew/opt/postgresql@{ver}/bin")));
            out.push(PathBuf::from(format!("/usr/local/opt/postgresql@{ver}/bin")));
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = ver;
        }
    }
    out
}

/// Resolve a Postgres binary directory by probing each candidate.
///
/// Returns the first candidate that contains both `postgres` and
/// `initdb` as executable files. If none match, returns
/// [`DbError::PgBinariesMissing`] with the candidate list embedded so
/// the operator sees exactly what was probed.
///
/// This function performs file-system stat calls but no process spawn.
pub fn find_pg_bin_dir(candidates: &[PathBuf]) -> Result<PathBuf, DbError> {
    for cand in candidates {
        if pg_bin_dir_is_complete(cand) {
            return Ok(cand.clone());
        }
    }
    let listed = candidates
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(", ");
    Err(DbError::PgBinariesMissing(if listed.is_empty() {
        "(none)".into()
    } else {
        listed
    }))
}

fn pg_bin_dir_is_complete(dir: &Path) -> bool {
    let postgres = dir.join("postgres");
    let initdb = dir.join("initdb");
    is_executable(&postgres) && is_executable(&initdb)
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(p) {
        Ok(m) => m.is_file() && (m.permissions().mode() & 0o111) != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

/// Has `initdb` already been run against this directory?
///
/// Postgres always writes a `PG_VERSION` file (containing the major
/// version, e.g. "18") into the data dir as the very first step of
/// `initdb`. Its presence is the canonical "this is a populated
/// cluster" marker — `pg_ctl` and the PG docs both rely on it. We
/// reuse it for our idempotency check.
///
/// Returns `true` only when `<data_dir>/PG_VERSION` is a regular file.
/// A symlink, directory, or anything else returns `false` — defensive
/// because a rogue file at that path would mean the cluster is
/// corrupt anyway.
pub fn is_data_dir_initialized(data_dir: &Path) -> bool {
    let pg_version = data_dir.join("PG_VERSION");
    matches!(std::fs::metadata(&pg_version), Ok(m) if m.is_file())
}

/// XDG-style default cluster data dir for the current user.
///
/// Linux + macOS use the same path: `$HOME/.local/share/hhagent/pg/data`.
/// On macOS we deliberately don't follow Apple's
/// `~/Library/Application Support/` convention because:
///  - hhagent is a portable agent system; the same path on both OSes
///    means scripts and docs don't need per-OS branches.
///  - `~/.local/share` is well-supported on macOS too (Homebrew,
///    XDG-aware tools, etc.).
///
/// Returns `None` when `$HOME` is unset (extremely unusual; tests use
/// [`InitDbOptions::data_dir`] directly to avoid relying on env).
pub fn default_data_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| {
        let mut p = PathBuf::from(h);
        p.push(".local/share/hhagent/pg/data");
        p
    })
}

/// Default socket directory: `<data_dir>/sockets`. See module doc.
pub fn default_socket_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("sockets")
}

/// Reject relative paths up front so the rest of the pipeline can
/// assume absolutes (mirrors `sandbox::linux_bwrap::spawn_under_policy`).
pub fn require_absolute(p: &Path) -> Result<(), DbError> {
    if p.is_absolute() {
        Ok(())
    } else {
        Err(DbError::RelativePath(p.to_path_buf()))
    }
}

#[cfg(test)]
mod tests {
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
}
