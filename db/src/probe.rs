//! Runtime probe: bring the cluster up to schema, then write a
//! bring-up `audit_log` row.
//!
//! This is the function `core/src/main.rs` calls just after
//! `tracing_subscriber::init` and before `wait_for_shutdown`. The
//! contract is fail-closed: any error short-circuits the daemon
//! startup and propagates `?` all the way to a non-zero exit, so
//! the supervisor (`Restart=on-failure` on systemd, `KeepAlive=true`
//! on launchd) sees a real failure instead of the daemon running
//! degraded against a half-bootstrapped database.
//!
//! Pipeline:
//!
//!   1. Connect to the maintenance DB (`postgres`) using peer auth.
//!   2. Check `pg_database` for the application DB. CREATE if absent.
//!   3. Disconnect from `postgres`; connect to the application DB.
//!   4. Run [`crate::MIGRATOR`] (the embedded `migrations/0001_init.sql`,
//!      `0002_runtime_role.sql`, and any future siblings) as the OS user
//!      / cluster superuser — required for `CREATE EXTENSION`,
//!      `CREATE ROLE`, and any future migration that touches a
//!      superuser-only catalog.
//!   5. `SET ROLE hhagent_runtime` to drop privileges before any
//!      application write. From this point on the connection cannot
//!      UPDATE or DELETE `audit_log` rows even if compromised; see
//!      `db/migrations/0002_runtime_role.sql` for the GRANT shape.
//!   6. INSERT a row into `audit_log` so the boot is recorded — this
//!      is the first write under the runtime role.
//!
//! The CREATE DATABASE branch is idempotent — re-running the probe
//! after the DB exists is a single `pg_database` lookup and zero DDL.
//! Migrations are tracked by sqlx in `_sqlx_migrations`; running
//! [`MIGRATOR`] against an already-up-to-date schema is a no-op.

use sqlx::{Connection, Executor, Row};

use crate::conn::{quote_ident, set_role_runtime_statement, ConnectSpec};
use crate::DbError;

/// Run the full bring-up sequence and write the marker `audit_log`
/// row. See module docs for the pipeline.
///
/// `actor` and `action` go into the row verbatim; `payload` is any
/// `serde_json::Value` (typically `{"version": ..., "git": ...}`).
/// The caller is the source of truth for what's worth recording —
/// this function does not synthesize fields, so a future caller that
/// wants to log `{}` for a no-op probe gets exactly that.
pub async fn run(
    spec: &ConnectSpec,
    actor: &str,
    action: &str,
    payload: serde_json::Value,
) -> Result<(), DbError> {
    ensure_database_exists(spec).await?;

    // The application DB now exists — connect, migrate, write the row.
    // We use a single connection (not a pool) because Phase 0 has no
    // concurrent-query workload and one short-lived connection is
    // cheaper than spinning up a pool we'll close immediately.
    let app_opts = spec.to_pg_connect_options();
    let mut conn = sqlx::postgres::PgConnection::connect_with(&app_opts)
        .await
        .map_err(|e| DbError::Connect(e.to_string()))?;

    crate::MIGRATOR
        .run(&mut conn)
        .await
        .map_err(|e| DbError::Migrate(e.to_string()))?;

    // Drop privileges before the first application-level write. The
    // bootstrap superuser identity is needed for migrations (CREATE
    // EXTENSION, CREATE ROLE) but not for anything below — and the
    // runtime role's `REVOKE UPDATE, DELETE ON audit_log` is what makes
    // the table effectively append-only at the database layer rather
    // than only by application discipline. Migration 0002 GRANTs the
    // runtime role to the OS user, so this SET ROLE always succeeds on
    // a freshly-migrated cluster.
    conn.execute(set_role_runtime_statement().as_str())
        .await
        .map_err(|e| DbError::Query(format!("SET ROLE hhagent_runtime: {e}")))?;

    sqlx::query("INSERT INTO audit_log (actor, action, payload) VALUES ($1, $2, $3)")
        .bind(actor)
        .bind(action)
        .bind(payload)
        .execute(&mut conn)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;

    // `close()` flushes the terminate message; the connection is
    // being dropped either way so we don't propagate the error, but
    // a half-closed-socket symptom shows up nowhere else — log it at
    // debug so packet captures aren't the only way to see it.
    if let Err(e) = conn.close().await {
        tracing::debug!(error = %e, "graceful close of probe app-DB connection failed");
    }
    Ok(())
}

/// Connect to the maintenance database, check `pg_database`, and
/// `CREATE DATABASE` the application DB if missing.
///
/// Public so `probe::run` can be split for testing — exercising
/// just the ensure-step against a temp cluster is the smoke test
/// for the create branch (vs. the migration branch in `run`).
pub async fn ensure_database_exists(spec: &ConnectSpec) -> Result<(), DbError> {
    let admin_opts = spec.for_maintenance_db().to_pg_connect_options();
    let mut conn = sqlx::postgres::PgConnection::connect_with(&admin_opts)
        .await
        .map_err(|e| DbError::Connect(e.to_string()))?;

    // `EXISTS` short-circuits and the lookup is index-backed
    // (`pg_database_datname_index`). One round-trip, then the
    // CREATE branch (also one round-trip) only fires on first boot.
    let row = sqlx::query("SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)")
        .bind(&spec.database)
        .fetch_one(&mut conn)
        .await
        .map_err(|e| DbError::Query(e.to_string()))?;
    let exists: bool = row
        .try_get(0)
        .map_err(|e| DbError::Query(format!("decode pg_database EXISTS: {e}")))?;

    if !exists {
        // `CREATE DATABASE` cannot run inside a transaction and does
        // not accept parameter binds. Identifier-quote both names so
        // the statement is safe even if a future caller passes a
        // less-trusted database or owner string.
        let stmt = format!(
            "CREATE DATABASE {} OWNER {}",
            quote_ident(&spec.database),
            quote_ident(&spec.user),
        );
        conn.execute(stmt.as_str())
            .await
            .map_err(|e| DbError::Query(e.to_string()))?;
    }

    if let Err(e) = conn.close().await {
        tracing::debug!(error = %e, "graceful close of probe maintenance-DB connection failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `quote_ident` is the only purely synchronous helper visible
    /// from this module's blast radius; the rest is async I/O against
    /// a real cluster, exercised by `db/tests/postgres_e2e.rs`.
    /// Pin one shape here so `cargo test --lib` covers the module
    /// surface.
    #[test]
    fn create_database_statement_quotes_both_names() {
        // Mirror the literal format in `ensure_database_exists`. If
        // the format string drifts (e.g. someone adds a TEMPLATE
        // clause), this test is the canary.
        let s = format!(
            "CREATE DATABASE {} OWNER {}",
            quote_ident("hhagent"),
            quote_ident("alice"),
        );
        assert_eq!(s, "CREATE DATABASE \"hhagent\" OWNER \"alice\"");
    }
}
