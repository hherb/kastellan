//! kastellan-db: bring up a private per-user PostgreSQL instance.
//!
//! Containment shape:
//!   - **Data dir** lives under `~/.local/share/kastellan/pg/data` (XDG-style).
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
//! config strings) and the small *driver* in `bin/kastellan-db-init.rs`
//! that performs the I/O.
//!
//! The split mirrors `sandbox::linux_bwrap` (pure `build_argv` separately
//! testable from the spawn) and `supervisor::systemd_user` (pure
//! `build_unit_file` separately testable from the actual `systemctl`
//! call). Same pattern, same payoff: the parts that decide *what* to do
//! are unit-tested with no host dependencies.

use std::path::{Path, PathBuf};

use thiserror::Error;

pub mod agent_prompts;
pub mod audit;
pub mod conn;
pub mod entities;
pub mod entity_embedding;
pub mod entity_kinds;
pub mod entity_name;
pub mod graph;
pub mod memories;
pub mod pairings;
pub mod pool;
pub mod probe;
pub mod relation_kinds;
pub mod secrets;
pub mod tasks;
pub mod tool_allowlists;

pub use entity_name::normalize_entity_name;

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

    /// A persisted value violates a schema invariant the code relies
    /// on (e.g. a CHECK-constrained column read back with a value
    /// outside the allowed range). Distinct from [`DbError::Query`]
    /// because the schema, not the query, is the broken contract —
    /// retrying won't help; an operator must investigate.
    #[error("postgres invariant violated: {0}")]
    Invariant(String),

    /// A caller asked the storage layer to do something the storage
    /// layer is *contractually forbidden to do*, independent of any
    /// SQL state — e.g. writing an L0 (meta-rule) memory row through
    /// the agent-loop writer. Distinct from [`DbError::Query`] (the
    /// SQL itself is fine) and from [`DbError::Invariant`] (no read
    /// surfaced bad state). Retrying never helps; the caller must
    /// route the request through the correct admin path.
    #[error("storage policy violation: {0}")]
    PolicyViolation(String),

    /// Catchall for other errors not fitting other variants.
    #[error("{0}")]
    Other(String),
}

impl From<std::io::Error> for DbError {
    fn from(value: std::io::Error) -> Self {
        DbError::Io(value.to_string())
    }
}

impl From<sqlx::Error> for DbError {
    fn from(value: sqlx::Error) -> Self {
        DbError::Query(value.to_string())
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
    /// the OS username running `initdb`. Defaults to "kastellan" if empty.
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
            username: "kastellan".into(),
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
        "kastellan"
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
        "# Managed by kastellan-db-init. Do not edit by hand.\n\
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
///
/// **Postgres.app is deliberately NOT in this list.** Postgres.app v18
/// works fine as a runtime database (binaries are launchable; sockets
/// are created cleanly), but per-test launchd-driven bring-up via the
/// test fixture's `bring_up_pg_cluster` overshoots the fixture's 15s
/// status-Active polling window for Postgres.app under macOS launchd
/// (the postmaster status takes longer to register Active than
/// Homebrew Postgres does). Adding Postgres.app paths here would mean
/// `cargo test --workspace` regresses for any macOS dev who has
/// Postgres.app installed — and they're the majority. Operators who
/// want to use Postgres.app or any other non-standard install with the
/// test fixtures should call [`pg_bin_dir_candidates_with_env_override`]
/// and set the `KASTELLAN_PG_BIN_DIR` env var to that install's `bin/`
/// directory before running the tests.
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

/// Name of the env var that prepends a single bin-dir to the search
/// path returned by [`pg_bin_dir_candidates_with_env_override`].
///
/// Defined as a `const` so the literal cannot drift between the
/// production read site and the unit tests that exercise it.
pub const PG_BIN_DIR_ENV: &str = "KASTELLAN_PG_BIN_DIR";

/// Test-fixture discovery: `KASTELLAN_PG_BIN_DIR` (if set and non-blank),
/// followed by [`default_pg_bin_dir_candidates`].
///
/// Intended for test infrastructure (`tests-common::pg_bin_dir_or_skip`
/// and the per-file local copies in `core/tests/scheduler_*_e2e.rs` /
/// `core/tests/agent_prompts_e2e.rs`), NOT for production runtime
/// discovery — production should bind to a known cluster via the
/// supervisor spec, not auto-probe at startup.
///
/// **Semantics:**
///
/// - Var unset OR empty string OR whitespace-only → defaults only.
///   Trim-then-check matches the strict-only-"1" parse style used
///   elsewhere in the workspace for boolean env knobs and silently
///   tolerates the common shell-script footgun
///   `export KASTELLAN_PG_BIN_DIR=$(some_failing_lookup)` producing a
///   blank value.
/// - Non-blank value → prepended to the defaults so a bogus or
///   missing override falls through to defaults rather than skipping
///   tests. The override only "wins" when it actually points at a
///   directory containing executable `postgres` + `initdb`; see
///   [`find_pg_bin_dir`].
///
/// **Single-path only.** A `:`-separated list would invite shell-PATH
/// confusion; if a future workflow needs multiple overrides, switch
/// to a deliberate split-and-parse design rather than overloading
/// this surface.
pub fn pg_bin_dir_candidates_with_env_override() -> Vec<PathBuf> {
    let override_path = std::env::var(PG_BIN_DIR_ENV).ok().and_then(|raw| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(PathBuf::from(trimmed))
        }
    });
    let defaults = default_pg_bin_dir_candidates();
    let mut out = Vec::with_capacity(defaults.len() + usize::from(override_path.is_some()));
    out.extend(override_path);
    out.extend(defaults);
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
/// Linux + macOS use the same path: `$HOME/.local/share/kastellan/pg/data`.
/// On macOS we deliberately don't follow Apple's
/// `~/Library/Application Support/` convention because:
///  - kastellan is a portable agent system; the same path on both OSes
///    means scripts and docs don't need per-OS branches.
///  - `~/.local/share` is well-supported on macOS too (Homebrew,
///    XDG-aware tools, etc.).
///
/// Returns `None` when `$HOME` is unset (extremely unusual; tests use
/// [`InitDbOptions::data_dir`] directly to avoid relying on env).
pub fn default_data_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| {
        let mut p = PathBuf::from(h);
        p.push(".local/share/kastellan/pg/data");
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
mod tests;
