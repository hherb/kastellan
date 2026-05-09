//! Daemon-scoped Postgres connection pool with automatic privilege
//! drop on every acquired connection.
//!
//! ## Why a pool *now*
//!
//! Before Phase 0 Option I, the daemon's only DB writes were the one
//! bring-up `audit_log` row in [`crate::probe::run`] — short-lived
//! connections sufficed. With Option I the dispatcher write site fires
//! once per tool call and concurrent tool calls are inevitable as soon
//! as the Phase 1 scheduler lands. A single shared pool with bounded
//! `max_connections` is the standard sqlx shape; building it now means
//! the dispatcher's audit insert can use `&PgPool` directly without
//! ad-hoc connection ceremony at every call site.
//!
//! Tracked as issue #11 in HANDOVER's open list ("switch core to a
//! daemon-scoped PgPool when Phase 1's concurrent workload lands").
//! Option I lands the pool a phase early; Phase 1 only needs to
//! consume what's already here.
//!
//! ## Why `after_connect` does the SET ROLE
//!
//! Migration `0002_runtime_role.sql` carved the GRANT shape:
//! `audit_log` is INSERT+SELECT only for [`crate::conn::RUNTIME_ROLE`]
//! (no UPDATE / DELETE / TRUNCATE), the other tables get full CRUD,
//! and the OS user is GRANTed the runtime role so `SET ROLE` succeeds.
//!
//! Connecting under peer auth gives us the OS user (= cluster
//! bootstrap superuser). Without dropping privilege, the daemon's
//! application writes would still be running as superuser — and the
//! `audit_log` REVOKE is a no-op against superuser. The
//! `after_connect` hook is sqlx's standard place to run per-connection
//! setup that must outlive the pool checkout/return cycle: every time
//! the pool dials a new physical connection (initial fill or
//! replacement after timeout/death), the hook runs once. Connection
//! reuse skips the hook because the role is sticky for the connection
//! lifetime — exactly what we want.
//!
//! ## What's *NOT* covered
//!
//! The migration runner ([`crate::MIGRATOR`]) needs superuser to
//! `CREATE EXTENSION`, `CREATE ROLE`, etc. So [`crate::probe::run`]
//! intentionally uses a one-shot connection (not the pool) for the
//! migrate step, then switches to runtime via inline `SET ROLE` for
//! its own audit insert. The pool from this module is for
//! *post-migration* application work only. Calling
//! `connect_runtime_pool` against a cluster where 0002 hasn't run
//! yet will fail at `after_connect` with `role "hhagent_runtime"
//! does not exist`.

use std::time::Duration;

use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::Executor;

use crate::conn::{set_role_runtime_statement, ConnectSpec};
use crate::DbError;

/// Default maximum connections in the runtime pool.
///
/// Phase 0's only hot path is the dispatcher write site, which is one
/// short INSERT per tool call. A handful of pool slots covers every
/// envisioned concurrency shape (parallel tool calls, the audit-mirror
/// task's catch-up SELECTs, occasional Phase 1 memory queries) without
/// the cluster's `max_connections = 32` ceiling becoming a concern.
///
/// Tunable via [`connect_runtime_pool_with_max`] if a measured workload
/// ever justifies it.
pub const DEFAULT_MAX_CONNECTIONS: u32 = 4;

/// Idle-connection timeout. sqlx will close a connection that hasn't
/// been used for this long, freeing the cluster slot. 5 minutes is
/// long enough that bursty workloads don't churn dials, short enough
/// that an idle daemon doesn't pin connections forever.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Connect-timeout for new physical connections. UDS dials are fast;
/// 10 s is generous and leaves room for a slow `after_connect` hook
/// while still surfacing a stuck cluster as a real error rather than
/// hanging the daemon's startup.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(10);

/// Build a [`PgPool`] for the cluster described by `spec`, with every
/// new connection automatically running [`set_role_runtime_statement`]
/// before use.
///
/// Returns once the pool's first connection has been opened and the
/// SET ROLE hook has run successfully — so a `permission denied` from
/// a missing 0002 migration surfaces here at startup, not later under
/// load.
///
/// Uses [`DEFAULT_MAX_CONNECTIONS`] for the cap. See
/// [`connect_runtime_pool_with_max`] if you need a different size.
pub async fn connect_runtime_pool(spec: &ConnectSpec) -> Result<PgPool, DbError> {
    connect_runtime_pool_with_max(spec, DEFAULT_MAX_CONNECTIONS).await
}

/// Variant of [`connect_runtime_pool`] that lets the caller override
/// the pool size. Useful in tests where multiple per-test pools share
/// a cluster and the cluster's `max_connections` would be hit by the
/// default sizing × test count.
pub async fn connect_runtime_pool_with_max(
    spec: &ConnectSpec,
    max_connections: u32,
) -> Result<PgPool, DbError> {
    let opts = spec.to_pg_connect_options();
    PgPoolOptions::new()
        .max_connections(max_connections.max(1))
        .acquire_timeout(ACQUIRE_TIMEOUT)
        .idle_timeout(IDLE_TIMEOUT)
        .after_connect(|conn, _meta| {
            // sqlx's `after_connect` callback returns a boxed future.
            // The `move` ensures the captured statement string lives
            // long enough; `set_role_runtime_statement()` is cheap
            // enough to call once per dial that we don't bother
            // hoisting it out of the closure.
            Box::pin(async move {
                let stmt = set_role_runtime_statement();
                conn.execute(stmt.as_str()).await?;
                Ok(())
            })
        })
        .connect_with(opts)
        .await
        .map_err(|e| DbError::Connect(format!("runtime pool connect: {e}")))
}
