//! Shared audit-sink fixtures for worker-dispatch integration tests.
//!
//! [`NoopAuditSink`] was byte-duplicated across five `core/tests/*.rs` files
//! (`python_exec_warm_idle_e2e`, `python_exec_firecracker_e2e`,
//! `python_exec_firecracker_warm_idle_e2e`, `python_exec_container_e2e`,
//! `worker_lifecycle_idle_timeout_e2e`). Consolidated here per the issue-#15
//! posture so a change to the [`AuditSink`] trait signature lands in one place
//! instead of drifting across copies.

use async_trait::async_trait;
use kastellan_core::tool_host::AuditSink;
use kastellan_db::DbError;

/// An [`AuditSink`] that drops every event and reports success, so a
/// `dispatch_with_sink`-based test exercises the sandbox + worker binary
/// without a Postgres cluster. The returned row id is a constant `1`.
pub struct NoopAuditSink;

#[async_trait]
impl AuditSink for NoopAuditSink {
    async fn insert(
        &self,
        _actor: &str,
        _action: &str,
        _payload: serde_json::Value,
    ) -> Result<i64, DbError> {
        Ok(1)
    }
}
