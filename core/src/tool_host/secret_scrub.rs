//! python-exec result secret-scrub (design 2026-06-17).
//!
//! `python-exec` runs agent-authored code, so — unlike the curated Rust workers
//! whose result-plaintext is trusted by design (#147) — we do NOT trust its
//! output to handle a materialized secret responsibly. It is `Net::Deny`, so its
//! returned stdout/stderr is its only output channel (the direct analog of
//! egress). We scan that output for the fingerprints of the secrets materialized
//! into THIS dispatch's params and redact them before the result is screened,
//! audited, or shown to the operator. Symmetric with egress slice #3b, which
//! scans force-routed net workers' egress.
//!
//! Pulled into a sibling (like `egress_provision.rs`) so `tool_host.rs` stays
//! near the 500-LOC cap and the pure pieces are unit-testable with a fake sink.

// All five `pub(crate)` functions in this module are forward-declarations: they
// are wired into the dispatch chokepoint in the follow-on task (Task 3). Until
// then, suppress the dead_code lint so `cargo clippy -D warnings` stays green.
#![allow(dead_code)]

use kastellan_leak_scan::{redact, RedactHit, SecretFingerprint};
use serde_json::Value;

use super::audit_sink::AuditSink;
use crate::secrets::{collect_refs_in_params, Vault};

/// True iff `tool`'s result must be scrubbed of materialized-secret plaintext.
/// Only `python-exec` opts in (it runs agent-authored code). The dispatch
/// chokepoint only carries the tool name, and there is exactly one such worker
/// today, so the gate keys on the name rather than threading a manifest flag
/// through the dispatch signature (YAGNI; revisit if a second untrusted-code
/// worker appears).
pub(crate) fn worker_redacts_output(tool: &str) -> bool {
    tool == crate::workers::python_exec::TOOL_NAME
}

/// Fingerprints of every scannable secret materialized into this dispatch.
/// `req_for_audit` is the PRE-substitution snapshot, so its `secret://` refs are
/// still present. `Vault::value_fingerprint` reads under the vault lock and
/// never exposes plaintext; values below `MIN_SECRET_LEN` yield `None` and are
/// skipped (unscannable by design — same limit as egress #3b).
pub(crate) fn fingerprints_for_dispatch(
    req_for_audit: &Value,
    vault: &Vault,
) -> Vec<SecretFingerprint> {
    collect_refs_in_params(req_for_audit)
        .iter()
        .filter_map(|r| vault.value_fingerprint(r))
        .collect()
}

/// Walk every JSON string leaf of `result` and redact any occurrence of the
/// `fps` secrets in place, returning the hits accumulated across all leaves.
/// Pure (no I/O). A no-op when `fps` is empty or nothing matches.
pub(crate) fn scrub_result_value(result: &mut Value, fps: &[SecretFingerprint]) -> Vec<RedactHit> {
    let mut hits = Vec::new();
    scrub_value(result, fps, &mut hits);
    hits
}

fn scrub_value(v: &mut Value, fps: &[SecretFingerprint], hits: &mut Vec<RedactHit>) {
    match v {
        Value::String(s) => {
            let outcome = redact(s.as_bytes(), fps);
            if !outcome.hits.is_empty() {
                // The input was valid UTF-8 and the marker is ASCII, so the
                // redacted bytes are valid UTF-8; the lossy fallback is purely
                // defensive and never expected to fire.
                *s = String::from_utf8(outcome.bytes)
                    .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
                hits.extend(outcome.hits);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|e| scrub_value(e, fps, hits)),
        Value::Object(o) => o.values_mut().for_each(|e| scrub_value(e, fps, hits)),
        _ => {}
    }
}

/// Emit one redacted `policy / secret.output_scrubbed` audit row when a scrub
/// removed at least one secret. Records hash/offset/len only — NEVER plaintext —
/// symmetric with the egress `egress.blocked.credential_leak` row. Best-effort
/// (logged, not propagated): the result is already redacted, so a transient
/// audit failure must not fail the dispatch (consistent with `secret.redeemed`).
pub(crate) async fn emit_scrub_audit(sink: &dyn AuditSink, tool: &str, hits: &[RedactHit]) {
    if hits.is_empty() {
        return;
    }
    let payload = serde_json::json!({
        "tool":  tool,
        "count": hits.len(),
        "hits":  hits
            .iter()
            .map(|h| serde_json::json!({
                "sha256_hex": h.sha256_hex,
                "offset":     h.offset,
                "len":        h.len,
            }))
            .collect::<Vec<_>>(),
    });
    if let Err(e) = sink.insert("policy", "secret.output_scrubbed", payload).await {
        tracing::error!(tool = %tool, error = %e, "secret.output_scrubbed audit insert failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn fp(v: &[u8]) -> SecretFingerprint {
        kastellan_leak_scan::fingerprint_value(v).expect("test secret >= MIN_SECRET_LEN")
    }

    // NOTE: match the EXACT AuditSink::insert signature + error type + async_trait
    // usage from the RecordingSink in egress_provision.rs's test module. The body
    // below assumes `insert(&self, actor, action, payload) -> Result<i64, DbError>`.
    #[derive(Default)]
    struct RecordingSink {
        rows: Mutex<Vec<(String, String)>>,
    }

    #[async_trait::async_trait]
    impl AuditSink for RecordingSink {
        async fn insert(
            &self,
            actor: &str,
            action: &str,
            _payload: Value,
        ) -> Result<i64, kastellan_db::DbError> {
            self.rows
                .lock()
                .unwrap()
                .push((actor.to_string(), action.to_string()));
            Ok(1)
        }
    }

    #[test]
    fn gate_is_on_only_for_python_exec() {
        assert!(worker_redacts_output("python-exec"));
        assert!(!worker_redacts_output("web-fetch"));
        assert!(!worker_redacts_output("shell-exec"));
    }

    #[test]
    fn scrubs_secret_in_stdout_leaf() {
        let secret = b"super-secret-token-123";
        let mut v = serde_json::json!({
            "exit_code": 0,
            "stdout": "leak: super-secret-token-123 done",
            "stderr": "",
        });
        let hits = scrub_result_value(&mut v, &[fp(secret)]);
        assert_eq!(hits.len(), 1);
        let stdout = v["stdout"].as_str().unwrap();
        assert!(!stdout.contains("super-secret-token-123"));
        assert!(stdout.contains("[redacted:"));
    }

    #[test]
    fn scrubs_secret_in_nested_string() {
        let secret = b"super-secret-token-123";
        let mut v = serde_json::json!({
            "nested": {"list": ["x", "super-secret-token-123"]},
        });
        let hits = scrub_result_value(&mut v, &[fp(secret)]);
        assert_eq!(hits.len(), 1);
        assert!(!serde_json::to_string(&v).unwrap().contains("super-secret-token-123"));
    }

    #[test]
    fn no_secret_leaves_value_byte_identical() {
        let mut v = serde_json::json!({"exit_code": 0, "stdout": "clean", "stderr": ""});
        let before = v.clone();
        let hits = scrub_result_value(&mut v, &[fp(b"super-secret-token-123")]);
        assert!(hits.is_empty());
        assert_eq!(v, before);
    }

    #[test]
    fn empty_fingerprints_is_a_noop() {
        let mut v = serde_json::json!({"stdout": "anything at all"});
        let before = v.clone();
        let hits = scrub_result_value(&mut v, &[]);
        assert!(hits.is_empty());
        assert_eq!(v, before);
    }

    #[tokio::test]
    async fn emit_writes_one_policy_row_on_hits_and_nothing_when_empty() {
        let sink = RecordingSink::default();
        emit_scrub_audit(&sink, "python-exec", &[]).await;
        assert!(sink.rows.lock().unwrap().is_empty());

        let hits = vec![RedactHit {
            sha256_hex: "ab".repeat(32),
            offset: 3,
            len: 22,
        }];
        emit_scrub_audit(&sink, "python-exec", &hits).await;
        let rows = sink.rows.lock().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0], ("policy".to_string(), "secret.output_scrubbed".to_string()));
    }
}
