//! Audit-write seam for the dispatch chokepoint.
//!
//! [`super::dispatch`] writes audit rows at four points (tool row, the two
//! secret-ref rows, and the `injection.blocked` forensic row). In production
//! all four go straight to Postgres via [`kastellan_db::audit::insert`]. This
//! module factors that single dependency behind the [`AuditSink`] trait so a
//! test can substitute a fake sink and force individual inserts to fail —
//! exercising the best-effort *swallow-and-continue* paths that are otherwise
//! impossible to reach without a fault-injecting database (issue #148).
//!
//! ## Why a `pub` seam on a security chokepoint
//!
//! [`AuditSink`] and [`super::dispatch_with_sink`] are `pub` only because the
//! fault-injection tests live in the separate `core/tests/` integration crate
//! (they need a *real spawned worker*, so they cannot be in-crate unit tests).
//! A misused sink could silently drop audit rows — so **production code must
//! always route through [`super::dispatch`]**, which is hard-wired to
//! [`PgAuditSink`]. `dispatch_with_sink` exists for tests; the seam does not
//! widen the spawn/jail trust boundary (that stays sealed behind
//! `WorkerCommand` / `SupervisedWorker::call`), only where audit rows are sent.

use async_trait::async_trait;
use serde_json::Value;
use sqlx::PgPool;

use kastellan_db::DbError;

/// Where [`super::dispatch_with_sink`] sends its audit rows.
///
/// Mirrors the shape of [`kastellan_db::audit::insert`] (actor, action,
/// payload → row id) so [`PgAuditSink`] is a one-line adapter and the prod
/// behaviour is byte-for-byte what `dispatch` did before the seam existed.
#[async_trait]
pub trait AuditSink: Send + Sync {
    /// Insert one audit row. Returns the new row id on success, mirroring
    /// [`kastellan_db::audit::insert`].
    async fn insert(&self, actor: &str, action: &str, payload: Value) -> Result<i64, DbError>;
}

/// Production [`AuditSink`]: forwards straight to [`kastellan_db::audit::insert`]
/// over a borrowed pool. This is the only sink ever used in production —
/// [`super::dispatch`] constructs it from its `pool` argument.
pub struct PgAuditSink<'a> {
    pool: &'a PgPool,
}

impl<'a> PgAuditSink<'a> {
    /// Wrap a pool reference. Cheap — borrows, does not clone the pool.
    pub fn new(pool: &'a PgPool) -> Self {
        PgAuditSink { pool }
    }
}

#[async_trait]
impl AuditSink for PgAuditSink<'_> {
    async fn insert(&self, actor: &str, action: &str, payload: Value) -> Result<i64, DbError> {
        kastellan_db::audit::insert(self.pool, actor, action, payload).await
    }
}
