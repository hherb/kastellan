//! tool_host/post_process: the post-`worker.call` half of the dispatch
//! chokepoint.
//!
//! Lifted verbatim out of [`super::dispatch_with_sink`] (Item 9b prod-split) so
//! the parent module stays under the LOC cap; the behaviour is byte-identical.
//! It runs, in order:
//!
//! 1. **python-exec output secret-scrub** — for a worker that runs
//!    agent-authored code, redact every secret materialized into this
//!    dispatch's params out of the result before it is screened, audited, or
//!    returned. No-op (byte-identical) for every other worker.
//! 2. **Prompt-injection output screen** — screen the (scrubbed) result;
//!    substitute a placeholder on a Block so the planner gets an intelligible
//!    "withheld" signal.
//! 3. **Audit-emission arms** — the tool row (carrying the placeholder on a
//!    Block), one `policy / secret.redeemed` row per substitution, and the
//!    forensic `policy / injection.blocked` row on a Block. All best-effort
//!    (a transient audit-insert failure is logged, never propagated).

use sha2::{Digest, Sha256};

use kastellan_protocol::client::ClientError;

use super::{injection_blocked_placeholder, secret_scrub, AuditSink, ToolHostError};
use crate::secrets::RedemptionEvent;

/// Finalize a dispatch after `worker.call` has returned: scrub + screen the
/// result, then emit the audit rows, and return the caller-facing value (the
/// `injection_blocked_placeholder` on a Block, the worker's own value on Allow,
/// the worker's error on a call failure).
///
/// `elapsed_ms` is measured by the caller immediately after `worker.call`
/// returns so the audit rows carry the true dispatch latency; `req_for_audit`
/// is the pre-substitution snapshot (issue #147) — its opaque `secret://` refs
/// are still present for scrub fingerprinting and are what the tool row records.
/// Byte-identical to the inline logic it replaced.
#[allow(clippy::too_many_arguments)]
pub(super) async fn finalize(
    sink: &dyn AuditSink,
    vault: &crate::secrets::Vault,
    tool: &str,
    method: &str,
    req_for_audit: &serde_json::Value,
    redemption_events: &[RedemptionEvent],
    call_result: Result<serde_json::Value, ClientError>,
    elapsed_ms: u64,
) -> Result<serde_json::Value, ToolHostError> {
    // Prompt-injection screen on successful results. Errors are not
    // text-channel content (the planner sees them as failure codes,
    // not as text), so they can't carry injection — skip.
    let (final_result, blocked_meta) = match call_result {
        Ok(mut v) => {
            // ── python-exec output secret-scrub (design 2026-06-17). ──
            // For a worker that runs agent-authored code, redact every secret
            // materialized into THIS dispatch's params out of the result before
            // it is screened, audited (tool row + JSONL mirror), or returned to
            // the operator's InvokeReport. No-op (byte-identical) for every other
            // worker and for any call with no scannable secrets. `req_for_audit`
            // is the pre-substitution snapshot, so its `secret://` refs are still
            // present for fingerprinting.
            if secret_scrub::worker_redacts_output(tool) {
                let fps = secret_scrub::fingerprints_for_dispatch(req_for_audit, vault);
                if !fps.is_empty() {
                    let hits = secret_scrub::scrub_result_value(&mut v, &fps);
                    secret_scrub::emit_scrub_audit(sink, tool, &hits).await;
                }
            }

            let (body, truncated) = crate::cassandra::injection_guard::extract_scannable_text(
                &v,
                crate::cassandra::injection_guard::SCAN_BYTE_CAP,
            );
            // Per-tool sensitivity (issue #142): doc-fetching net workers
            // use the Relaxed profile so quoted chat-template tokens in
            // fetched documentation do not auto-Block; every other worker
            // (incl. shell-exec and any unknown) stays Strict, fail-closed.
            let verdict = crate::cassandra::injection_guard::screen_with_profile(
                &body,
                crate::cassandra::injection_guard::GuardProfile::for_tool(tool),
            );
            match verdict.decision {
                crate::cassandra::injection_guard::InjectionDecision::Allow => {
                    (Ok(v), None)
                }
                crate::cassandra::injection_guard::InjectionDecision::Block => {
                    // Substitute a placeholder carrying a human-readable `note`
                    // string — the only field the planner-summary render
                    // surfaces (extract_scannable_text emits string leaves
                    // only), so the planner gets an intelligible "withheld"
                    // signal rather than a silent gap (#340). Structured fields
                    // stay for audit-shape parity with fetch_screen.
                    let placeholder = injection_blocked_placeholder(
                        verdict.score,
                        &verdict.reason_codes,
                    );
                    (Ok(placeholder), Some((verdict, body, truncated)))
                }
            }
        }
        Err(e) => (Err(e), None),
    };

    // Tool audit row (existing) — now carrying the placeholder on Block.
    let actor = format!("tool:{tool}");
    let audit_payload = match &final_result {
        Ok(v) => serde_json::json!({
            "req":    req_for_audit,
            "result": v,
            "ms":     elapsed_ms,
        }),
        Err(e) => serde_json::json!({
            "req": req_for_audit,
            "err": e.to_string(),
            "ms":  elapsed_ms,
        }),
    };
    // ── Emit `secret.redeemed` audit rows (one per substitution). ──
    //
    // Best-effort: a transient audit insert failure is logged but
    // does not propagate. The plaintext is already substituted into
    // params and the worker already ran; turning the dispatch into
    // an error because the audit log was unreachable would be worse
    // than missing rows. (Materialize-time audit IS hard-fail; see
    // Vault::materialize and spec §5.4 for the asymmetry rationale.)
    for event in redemption_events {
        let payload = serde_json::json!({
            "tool":     tool,
            "method":   method,
            "ref_hash": event.ref_hash,
            "ms":       elapsed_ms,
        });
        if let Err(e) = sink.insert("policy", "secret.redeemed", payload).await {
            tracing::error!(
                tool = %tool,
                ref_hash = %event.ref_hash,
                error = %e,
                "secret.redeemed audit insert failed"
            );
        }
    }

    if let Err(audit_err) = sink.insert(&actor, method, audit_payload).await {
        tracing::error!(
            tool = %tool,
            method = %method,
            error = %audit_err,
            "audit_log INSERT failed; tool result still propagated"
        );
    }

    // Forensic policy row on Block. SHA-256 of the body that was
    // scanned (which may have been truncated at SCAN_BYTE_CAP).
    // The raw body is never written to any audit column — only the
    // hash, byte length, score, and class codes are stored.
    if let Some((verdict, body, truncated)) = blocked_meta {
        let mut hasher = Sha256::new();
        hasher.update(body.as_bytes());
        let body_sha256 = format!("{:x}", hasher.finalize());
        let body_byte_len = body.len();
        let policy_payload = serde_json::json!({
            "tool":                    tool,
            "method":                  method,
            "score":                   verdict.score,
            "decision":                "block",
            "reason_codes":            verdict.reason_codes,
            "body_sha256":             body_sha256,
            "body_byte_len":           body_byte_len,
            "body_truncated_at_64kib": truncated,
        });
        if let Err(e) = sink.insert("policy", "injection.blocked", policy_payload).await {
            tracing::error!(
                tool = %tool,
                method = %method,
                error = %e,
                "policy audit insert failed"
            );
        }
    }

    Ok(final_result?)
}
