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
///
/// ## Why [`AuditSink::insert`] is a *provided* method
///
/// [`kastellan_db::audit::truncate_payload`] runs **inside**
/// [`kastellan_db::audit::insert`], which is where PR #614's defect hid:
/// a double that records what the caller *passed* can never observe what
/// the database *stores*, so a payload key silently destroyed by the 4 KiB
/// cap looks perfectly preserved from every test in the tree. The guard
/// tier's per-dispatch score was lost that way on every tool result over
/// ~4 KiB, through seventeen e2e cases and a five-agent review, until it
/// was found in production.
///
/// Making the transform a provided method fixes the *class* rather than
/// that one key: a double implements [`insert_stored`](Self::insert_stored)
/// and therefore receives the stored payload whether or not its author
/// thought about truncation. `PgAuditSink` re-applies it via
/// `db::audit::insert`, which is harmless only because `truncate_payload`
/// is idempotent: an envelope is already under the cap, and the NUL escape
/// leaves an already-escaped payload — its `_nul_escaped` count included —
/// alone (issue #816; pinned by `db::audit::nul_escape`'s
/// `truncate_payload_is_idempotent_with_nuls_in_play`, and on a live
/// cluster by `db/tests/audit_nul_e2e.rs`). A storage step that is not
/// idempotent cannot be added here without changing this seam.
#[async_trait]
pub trait AuditSink: Send + Sync {
    /// Insert one row whose payload has **already** been through
    /// [`kastellan_db::audit::truncate_payload`].
    ///
    /// Implement this; call [`insert`](Self::insert).
    async fn insert_stored(
        &self,
        actor: &str,
        action: &str,
        payload: Value,
    ) -> Result<i64, DbError>;

    /// Insert one audit row. Returns the new row id on success, mirroring
    /// [`kastellan_db::audit::insert`].
    ///
    /// Applies the storage transform every production write goes through,
    /// then delegates to [`insert_stored`](Self::insert_stored). Not
    /// overridable in practice — override it and a double stops observing
    /// what Postgres would hold, which is the whole point of the seam.
    async fn insert(&self, actor: &str, action: &str, payload: Value) -> Result<i64, DbError> {
        self.insert_stored(actor, action, kastellan_db::audit::truncate_payload(payload)).await
    }
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
    async fn insert_stored(
        &self,
        actor: &str,
        action: &str,
        payload: Value,
    ) -> Result<i64, DbError> {
        kastellan_db::audit::insert(self.pool, actor, action, payload).await
    }
}
