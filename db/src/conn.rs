//! Connection-options builder for the per-user kastellan cluster.
//!
//! All call sites that need to talk to the cluster (the runtime probe,
//! tests, the future memory worker) go through [`ConnectSpec`] so the
//! connection conventions live in *one* place:
//!
//!   * UDS only — never TCP. The `host` field on
//!     [`sqlx::postgres::PgConnectOptions`] is interpreted as a UDS
//!     directory when it starts with `/` (libpq's longstanding rule).
//!   * Role = OS user, because `initdb --auth-local=peer` is the only
//!     authenticator the cluster knows. Peer auth maps the connecting
//!     OS uid → Postgres role of the same name; if those disagree the
//!     connection is refused at the auth layer.
//!   * Application database = `kastellan`. The cluster's bootstrap
//!     databases (`postgres`, `template0`, `template1`) are
//!     left alone; [`probe::ensure_database_exists`] creates `kastellan`
//!     on first bring-up.
//!
//! The helpers here are *pure* (build options, return strings) so they
//! can be unit-tested without a live Postgres. The matching async I/O
//! lives in [`crate::probe`].

use std::path::{Path, PathBuf};

use sqlx::postgres::PgConnectOptions;

use crate::{default_socket_dir, DbError};

/// Application database name. The cluster's bootstrap DB
/// (`postgres`) is reserved for one-off DDL like `CREATE DATABASE`;
/// every kastellan migration and runtime row goes in `kastellan`.
pub const DEFAULT_APPLICATION_DB: &str = "kastellan";

/// Postgres's always-present maintenance database. We connect to this
/// briefly to check `pg_database` and (if needed) `CREATE DATABASE
/// kastellan`. After that, every connection in the daemon uses the
/// application DB.
pub const MAINTENANCE_DB: &str = "postgres";

/// Non-superuser runtime role created by migration `0002_runtime_role.sql`.
///
/// The agent-core daemon connects to the cluster as the OS user (=
/// cluster bootstrap superuser under peer auth) so the migration runner
/// has the privilege it needs for `CREATE EXTENSION` / future DDL, and
/// then drops to this role via [`set_role_runtime_statement`] before
/// any application-level write — so e.g. `audit_log` rows are inserted
/// under a role that explicitly cannot UPDATE/DELETE them.
///
/// See `db/migrations/0002_runtime_role.sql` for the full GRANT/REVOKE
/// shape. Pinned as a constant so the role name lives in one place; a
/// rename here must be paired with a new migration that creates the new
/// role and migrates membership.
pub const RUNTIME_ROLE: &str = "kastellan_runtime";

/// SQL statement that switches the current connection's session role
/// to [`RUNTIME_ROLE`].
///
/// Wraps the role name with [`quote_ident`] so the statement is safe
/// even if a future caller parameterises the role (today's only caller
/// uses the constant). `SET ROLE` is per-session in Postgres — running
/// it once per connection acquisition (e.g. via sqlx's
/// `PoolOptions::after_connect` hook) is enough; subsequent statements
/// on the same connection inherit the new role until `RESET ROLE` or
/// the connection closes.
///
/// Pure: no I/O, deterministic. The matching async helper that opens a
/// `PgConnection` and runs this statement lives in
/// [`crate::probe`].
pub fn set_role_runtime_statement() -> String {
    format!("SET ROLE {}", quote_ident(RUNTIME_ROLE))
}

/// Pure description of how to reach the cluster.
///
/// Materialise into the sqlx options struct via
/// [`ConnectSpec::to_pg_connect_options`]; the indirection keeps the
/// shape testable and the connection string out of log lines (sqlx's
/// `PgConnectOptions` does not implement `Display` for the password
/// field even though we never set one).
#[derive(Clone, Debug)]
pub struct ConnectSpec {
    /// Absolute path to the directory containing `.s.PGSQL.5432`.
    /// Same value `postgresql.auto.conf`'s `unix_socket_directories`
    /// points to (see
    /// `build_postgresql_auto_conf` in `lib.rs`).
    pub socket_dir: PathBuf,
    /// Postgres role to authenticate as. Under peer auth this MUST
    /// equal the OS user running the connecting process.
    pub user: String,
    /// Database name. Defaults to [`DEFAULT_APPLICATION_DB`]; switched
    /// to [`MAINTENANCE_DB`] for the brief CREATE-DATABASE step in
    /// [`crate::probe::run`] via [`ConnectSpec::for_maintenance_db`].
    pub database: String,
}

impl ConnectSpec {
    /// Default connection for the cluster at `<data_dir>/sockets`,
    /// running as the OS user, against [`DEFAULT_APPLICATION_DB`].
    ///
    /// Returns [`DbError::EnvVarMissing`] when `$USER` is unset — peer
    /// auth has no fallback identity to claim, so failing closed beats
    /// connecting as some libpq-default that may or may not match the
    /// cluster's superuser.
    pub fn default_for(data_dir: &Path) -> Result<Self, DbError> {
        let user = current_os_user().ok_or(DbError::EnvVarMissing("USER"))?;
        Ok(Self {
            socket_dir: default_socket_dir(data_dir),
            user,
            database: DEFAULT_APPLICATION_DB.to_string(),
        })
    }

    /// Build a fully-resolved [`PgConnectOptions`].
    ///
    /// `host` carries the socket-dir path; sqlx (via libpq's
    /// longstanding convention) treats any host string starting with
    /// `/` as a UDS directory rather than a hostname.
    pub fn to_pg_connect_options(&self) -> PgConnectOptions {
        PgConnectOptions::new()
            .host(&self.socket_dir.to_string_lossy())
            .username(&self.user)
            .database(&self.database)
    }

    /// Variant pointing at the cluster's maintenance database
    /// (`postgres`). Used by [`crate::probe::run`] for the one-shot
    /// `pg_database` lookup + `CREATE DATABASE` that materialises
    /// `kastellan` on first bring-up. CREATE DATABASE cannot run inside
    /// a transaction and connects must target an *existing* DB — so
    /// the bootstrap DB is the only viable connection target here.
    pub fn for_maintenance_db(&self) -> Self {
        Self {
            database: MAINTENANCE_DB.to_string(),
            ..self.clone()
        }
    }
}

/// Resolve `$USER` to a non-empty string, or return `None`.
///
/// `whoami(3)`-style POSIX fallbacks (`getpwuid(geteuid())`) are
/// intentionally NOT used — every Linux/macOS shell session sets
/// `$USER`, the daemon-launching supervisor (systemd / launchd) sets
/// it from the user's login record, and a missing value almost
/// certainly indicates a misconfigured environment that we want to
/// surface, not paper over.
pub fn current_os_user() -> Option<String> {
    std::env::var("USER").ok().filter(|s| !s.is_empty())
}

/// Quote a Postgres identifier (db/role/table name) by wrapping in
/// double quotes and doubling any embedded `"`.
///
/// We only call this on identifiers that come from our own constants
/// or from `$USER`, so the input is already trusted — but identifier-
/// quoting is the canonical defense against any future change that
/// might pipe a less-trusted name into a `CREATE DATABASE` statement.
pub fn quote_ident(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' {
            out.push('"');
        }
        out.push(c);
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn spec_with(user: &str, db: &str) -> ConnectSpec {
        ConnectSpec {
            socket_dir: PathBuf::from("/srv/kastellan/sockets"),
            user: user.into(),
            database: db.into(),
        }
    }

    /// `default_for` resolves the socket dir under the supplied data
    /// dir using the same `<data_dir>/sockets` convention `lib.rs`
    /// pins. The username comes from `$USER` (set by the test harness;
    /// always present on Linux+macOS).
    ///
    /// Holds `crate::env_lock` for the entire test body so concurrent
    /// tests in this crate that read or mutate `$USER`/`$HOME` cannot
    /// race with the `set_var` below.
    #[test]
    fn default_for_uses_data_dir_sockets_subdir() {
        let _guard = crate::env_lock();
        let prev = std::env::var("USER").ok();
        std::env::set_var("USER", "testuser");
        let spec = ConnectSpec::default_for(Path::new("/srv/kastellan")).unwrap();
        assert_eq!(spec.socket_dir, PathBuf::from("/srv/kastellan/sockets"));
        assert_eq!(spec.user, "testuser");
        assert_eq!(spec.database, DEFAULT_APPLICATION_DB);
        match prev {
            Some(v) => std::env::set_var("USER", v),
            None => std::env::remove_var("USER"),
        }
    }

    /// `default_for` fails closed when `$USER` is unset — peer auth
    /// has no fallback identity, so guessing a username would lead to
    /// either a confusing connection failure or (worse) connecting as
    /// some other role than the operator intended.
    #[test]
    fn default_for_errors_when_user_env_unset() {
        let _guard = crate::env_lock();
        let prev = std::env::var("USER").ok();
        std::env::remove_var("USER");
        let err = ConnectSpec::default_for(Path::new("/srv/x")).unwrap_err();
        assert!(matches!(err, DbError::EnvVarMissing("USER")), "got {err:?}");
        if let Some(v) = prev {
            std::env::set_var("USER", v);
        }
    }

    /// `default_for` rejects an empty `$USER` — same fail-closed shape
    /// as the unset case. The filter in `current_os_user` is what
    /// makes this work.
    #[test]
    fn default_for_errors_when_user_env_empty() {
        let _guard = crate::env_lock();
        let prev = std::env::var("USER").ok();
        std::env::set_var("USER", "");
        let err = ConnectSpec::default_for(Path::new("/srv/x")).unwrap_err();
        assert!(matches!(err, DbError::EnvVarMissing("USER")), "got {err:?}");
        match prev {
            Some(v) => std::env::set_var("USER", v),
            None => std::env::remove_var("USER"),
        }
    }

    /// `for_maintenance_db` swaps only the database field, preserving
    /// the socket dir and user. The probe relies on this so the
    /// CREATE DATABASE roundtrip uses the same peer-auth identity as
    /// the subsequent application-DB connection.
    #[test]
    fn for_maintenance_db_only_changes_database_field() {
        let app = spec_with("alice", DEFAULT_APPLICATION_DB);
        let admin = app.for_maintenance_db();
        assert_eq!(admin.database, MAINTENANCE_DB);
        assert_eq!(admin.user, app.user);
        assert_eq!(admin.socket_dir, app.socket_dir);
    }

    /// Pin the application DB name. Renaming it would silently break
    /// existing on-disk clusters (the migration runner would point at
    /// a non-existent DB), so the constant change is paired with this
    /// regression test as a forced acknowledgement.
    #[test]
    fn application_db_name_is_kastellan() {
        assert_eq!(DEFAULT_APPLICATION_DB, "kastellan");
    }

    /// Pin the maintenance DB name. Postgres bootstraps `postgres`,
    /// `template0`, `template1`; only `postgres` is intended for
    /// connections, so changing this would point CREATE DATABASE at a
    /// template (a hard failure) or somewhere fictitious.
    #[test]
    fn maintenance_db_name_is_postgres() {
        assert_eq!(MAINTENANCE_DB, "postgres");
    }

    /// Pin the runtime role name. Migration `0002_runtime_role.sql`
    /// hardcodes the same string in its CREATE ROLE / GRANT statements;
    /// renaming here without a new migration would mean the daemon
    /// runs `SET ROLE` against a role that doesn't exist and every
    /// post-bring-up application write fails at the auth layer.
    #[test]
    fn runtime_role_name_is_kastellan_runtime() {
        assert_eq!(RUNTIME_ROLE, "kastellan_runtime");
    }

    /// Pin the SET ROLE statement shape. The role name MUST be wrapped
    /// in double quotes (Postgres identifier quoting) so a future role
    /// rename containing a reserved word or unusual character does not
    /// silently parse as a different statement.
    #[test]
    fn set_role_runtime_statement_quotes_role_name() {
        assert_eq!(
            set_role_runtime_statement(),
            "SET ROLE \"kastellan_runtime\"",
        );
    }

    /// Identifier quoting wraps in double quotes and doubles any
    /// internal `"`. Belt-and-braces against any future code path
    /// that might pipe a less-trusted name into DDL.
    #[test]
    fn quote_ident_handles_plain_name() {
        assert_eq!(quote_ident("kastellan"), "\"kastellan\"");
    }

    #[test]
    fn quote_ident_doubles_embedded_double_quote() {
        assert_eq!(quote_ident(r#"a"b"#), r#""a""b""#);
    }

    #[test]
    fn quote_ident_handles_empty_string() {
        // Postgres rejects empty identifiers at parse time, but the
        // quoter must still produce well-formed SQL — `""` — so the
        // failure surfaces at the DB layer with a clear error rather
        // than at the string-formatter layer.
        assert_eq!(quote_ident(""), "\"\"");
    }
}
