//! Dispatch-time egress leak-scanner provisioning (egress slice #3b, #268).
//!
//! Pulled out of the dispatch chokepoint so `tool_host.rs` stays near the
//! 500-LOC cap and so the fail-closed (D1) + audit (D3) decision is testable
//! with a fake [`AuditSink`]. [`compute_provision`] runs **synchronously** (no
//! `.await`) so the `&EgressSidecar` borrow of the worker is released before
//! `worker.call`; [`emit_provision`] then writes the audit rows.

use kastellan_leak_scan::SecretFingerprint;

use super::audit_sink::AuditSink;
use super::ToolHostError;
use crate::egress::leak_provision::{provision_audit_row, provision_failed_audit_row};
use crate::egress::net_worker::EgressSidecar;
use crate::secrets::{collect_refs_in_params, Vault};

/// Outcome of attempting dispatch-time provisioning. Computed without `.await`.
// Variants wired into dispatch in Task 5 (#268).
#[allow(dead_code)]
pub(crate) enum Provision {
    /// No egress sidecar, or no scannable secrets in this call — no-op.
    Noop,
    /// The union gained these fingerprints (emit one audit row each — D3).
    Added(Vec<SecretFingerprint>),
    /// Write failed for a secret-bearing net worker — caller fails closed (D1).
    Failed { pending: usize, err: String },
}

/// Decide + perform the file write synchronously. `egress` is the worker's
/// optional sidecar bundle; `req_for_audit` is the pre-substitution params
/// snapshot (so the `secret://` refs are still present). Secrets are
/// fingerprinted via the vault **without exposing plaintext**; sub-`MIN_SECRET_LEN`
/// values yield `None` and are skipped (not a failure — unscannable by design).
// Wired into dispatch in Task 5 (#268).
#[allow(dead_code)]
pub(crate) fn compute_provision(
    egress: Option<&EgressSidecar>,
    req_for_audit: &serde_json::Value,
    vault: &Vault,
) -> Provision {
    let Some(egress) = egress else {
        return Provision::Noop;
    };
    let refs = collect_refs_in_params(req_for_audit);
    let fps: Vec<SecretFingerprint> = refs
        .iter()
        .filter_map(|r| vault.value_fingerprint(r))
        .collect();
    if fps.is_empty() {
        return Provision::Noop;
    }
    match egress.provision_dispatch_secrets(&fps) {
        Ok(added) => Provision::Added(added),
        Err(e) => Provision::Failed {
            pending: fps.len(),
            err: e.to_string(),
        },
    }
}

/// Emit the audit rows for a provisioning outcome and, on failure, return the
/// fail-closed error (D1). Audit inserts are best-effort (logged, not
/// propagated) — consistent with the other dispatch audit rows — but the
/// fail-closed *decision* is hard: `Failed` always returns `Err`.
// Wired into dispatch in Task 5 (#268).
#[allow(dead_code)]
pub(crate) async fn emit_provision(
    sink: &dyn AuditSink,
    tool: &str,
    provision: Provision,
) -> Result<(), ToolHostError> {
    match provision {
        Provision::Noop => Ok(()),
        Provision::Added(added) => {
            for fp in &added {
                // No human secret *name* at dispatch — only the one-way value
                // hash. Pass "" for `name`; `worker` + `value_sha256` identify it.
                let row = provision_audit_row(tool, "", fp);
                if let Err(e) = sink.insert(row.actor, &row.action, row.payload).await {
                    tracing::error!(
                        tool = %tool,
                        error = %e,
                        "egress.secret_hash.provisioned audit insert failed"
                    );
                }
            }
            Ok(())
        }
        Provision::Failed { pending, err } => {
            let row = provision_failed_audit_row(tool, pending, &err);
            if let Err(ae) = sink.insert(row.actor, &row.action, row.payload).await {
                tracing::error!(
                    tool = %tool,
                    error = %ae,
                    "egress.secret_hash.provision_failed audit insert failed"
                );
            }
            Err(ToolHostError::EgressProvisionFailed(err))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use kastellan_db::DbError;
    use kastellan_leak_scan::fingerprint_value;
    use serde_json::Value;
    use std::sync::Mutex;

    /// Records the (actor, action) of every insert; always succeeds.
    #[derive(Default)]
    struct RecordingSink {
        rows: Mutex<Vec<(String, String)>>,
    }

    #[async_trait]
    impl AuditSink for RecordingSink {
        async fn insert(&self, actor: &str, action: &str, _payload: Value) -> Result<i64, DbError> {
            self.rows
                .lock()
                .unwrap()
                .push((actor.to_string(), action.to_string()));
            Ok(1)
        }
    }

    #[tokio::test]
    async fn noop_emits_nothing_and_is_ok() {
        let sink = RecordingSink::default();
        emit_provision(&sink, "web-fetch", Provision::Noop)
            .await
            .unwrap();
        assert!(sink.rows.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn added_emits_one_provisioned_row_per_fingerprint() {
        let sink = RecordingSink::default();
        let fps = vec![
            fingerprint_value(b"secret-value-one").unwrap(),
            fingerprint_value(b"secret-value-two").unwrap(),
        ];
        emit_provision(&sink, "web-fetch", Provision::Added(fps))
            .await
            .unwrap();
        let rows = sink.rows.lock().unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows
            .iter()
            .all(|(_, action)| action == "egress.secret_hash.provisioned"));
    }

    #[tokio::test]
    async fn failed_emits_a_failure_row_and_returns_err_d1() {
        let sink = RecordingSink::default();
        let res = emit_provision(
            &sink,
            "web-fetch",
            Provision::Failed {
                pending: 1,
                err: "boom".to_string(),
            },
        )
        .await;
        assert!(matches!(res, Err(ToolHostError::EgressProvisionFailed(_))));
        let rows = sink.rows.lock().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, "egress.secret_hash.provision_failed");
    }
}
