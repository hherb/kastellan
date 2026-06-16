//! Host side of egress slice #3b: provision per-worker secret-value
//! fingerprints into the sidecar scratch dir (the proxy lazily re-reads them)
//! and shape the provisioning audit row. The fingerprints themselves are
//! computed by [`crate::secrets::vault::Vault::value_fingerprint`]; this module
//! writes the `secret_hashes.json` file and builds the audit row.
//!
//! Spawn-wiring writes the file from the worker's known secret set (empty for
//! today's egress workers); the dispatch-chokepoint live-append is the tracked
//! follow-up.

use std::io;
use std::path::Path;

use kastellan_leak_scan::{parse_hashes, serialize_hashes, SecretFingerprint};

use super::audit::{EgressAuditRow, ACTOR};

/// File name written into the sidecar scratch dir (sibling of the UDS + ca.pem).
/// MUST match the name the proxy reads (`egress-proxy::main`).
pub const SECRET_HASHES_FILE_NAME: &str = "secret_hashes.json";

/// Atomically write `fps` to `<scratch>/secret_hashes.json`. Writes to a temp
/// file in the same dir then renames, so the proxy's lazy per-connection read
/// never observes a torn file. An empty slice writes an empty list (the proxy
/// treats that as "no scanning").
pub fn write_secret_hashes(scratch: &Path, fps: &[SecretFingerprint]) -> io::Result<()> {
    let final_path = scratch.join(SECRET_HASHES_FILE_NAME);
    let tmp_path = scratch.join(format!("{SECRET_HASHES_FILE_NAME}.tmp"));
    std::fs::write(&tmp_path, serialize_hashes(fps))?;
    std::fs::rename(&tmp_path, &final_path)
}

/// Build the `policy / egress.secret_hash.provisioned` audit row that lets a
/// later leak hash (which carries only the value SHA-256) be correlated back to
/// a secret *name*. The value hash is one-way, so `name + value_sha256` is safe
/// to persist. Pure — the caller inserts it.
pub fn provision_audit_row(worker: &str, name: &str, fp: &SecretFingerprint) -> EgressAuditRow {
    let value_sha256 = fp
        .sha256
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    EgressAuditRow {
        actor: ACTOR,
        action: "egress.secret_hash.provisioned".to_string(),
        payload: serde_json::json!({
            "worker": worker,
            "name": name,
            "value_sha256": value_sha256,
        }),
    }
}

/// Merge `new` fingerprints into `<scratch>/secret_hashes.json`, keeping the
/// UNION across the worker's lifetime (dedup by `sha256`), and return the
/// newly-added fingerprints (those not already present). Reads the existing
/// file leniently (missing/corrupt ⇒ empty), then atomically rewrites only if
/// the union grew. Union (not overwrite) means a later connection reusing an
/// earlier secret is still scanned (egress slice #3b, #268, decision D2).
///
/// A write failure surfaces as `Err` so the dispatch caller can fail closed
/// (decision D1) rather than let a secret-bearing worker egress unscanned.
pub fn merge_secret_hashes(
    scratch: &Path,
    new: &[SecretFingerprint],
) -> io::Result<Vec<SecretFingerprint>> {
    let existing = read_existing(scratch);
    let mut have: std::collections::HashSet<[u8; 32]> =
        existing.iter().map(|f| f.sha256).collect();
    let mut added: Vec<SecretFingerprint> = Vec::new();
    for fp in new {
        if have.insert(fp.sha256) {
            added.push(fp.clone());
        }
    }
    if !added.is_empty() {
        let mut union = existing;
        union.extend(added.iter().cloned());
        write_secret_hashes(scratch, &union)?;
    }
    Ok(added)
}

/// Lenient read of the existing fingerprint file: missing or corrupt ⇒ empty
/// (fail-safe; the merge then provisions at least this dispatch's secrets —
/// never silently fewer than the current call requires).
fn read_existing(scratch: &Path) -> Vec<SecretFingerprint> {
    match std::fs::read_to_string(scratch.join(SECRET_HASHES_FILE_NAME)) {
        Ok(s) => parse_hashes(&s),
        Err(_) => Vec::new(),
    }
}

/// Build the fail-closed `policy / egress.secret_hash.provision_failed` audit
/// row for when dispatch could not write the scanner patterns for a
/// secret-bearing net worker (the dispatch is then refused; the worker never
/// egresses). Carries the worker name, how many fingerprints were pending, and
/// the error string — no plaintext. Pure; the caller inserts it.
pub fn provision_failed_audit_row(worker: &str, pending: usize, err: &str) -> EgressAuditRow {
    EgressAuditRow {
        actor: ACTOR,
        action: "egress.secret_hash.provision_failed".to_string(),
        payload: serde_json::json!({
            "worker": worker,
            "pending": pending,
            "error": err,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kastellan_leak_scan::{fingerprint_value, parse_hashes};

    #[test]
    fn write_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let fps = vec![fingerprint_value(b"provisioned-secret-1").unwrap()];
        write_secret_hashes(dir.path(), &fps).unwrap();
        let s = std::fs::read_to_string(dir.path().join(SECRET_HASHES_FILE_NAME)).unwrap();
        assert_eq!(parse_hashes(&s), fps);
        // No leftover temp file.
        assert!(!dir.path().join("secret_hashes.json.tmp").exists());
    }

    #[test]
    fn empty_write_is_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        write_secret_hashes(dir.path(), &[]).unwrap();
        let s = std::fs::read_to_string(dir.path().join(SECRET_HASHES_FILE_NAME)).unwrap();
        assert!(parse_hashes(&s).is_empty());
    }

    #[test]
    fn provision_audit_row_shape() {
        let fp = fingerprint_value(b"provisioned-secret-2").unwrap();
        let row = provision_audit_row("web-fetch", "OPENAI_KEY", &fp);
        assert_eq!(row.actor, "egress_proxy");
        assert_eq!(row.action, "egress.secret_hash.provisioned");
        assert_eq!(row.payload["worker"], "web-fetch");
        assert_eq!(row.payload["name"], "OPENAI_KEY");
        assert_eq!(row.payload["value_sha256"].as_str().unwrap().len(), 64);
    }

    #[test]
    fn merge_into_empty_writes_and_reports_all_added() {
        let dir = tempfile::tempdir().unwrap();
        let a = fingerprint_value(b"first-secret-value").unwrap();
        let added = merge_secret_hashes(dir.path(), std::slice::from_ref(&a)).unwrap();
        assert_eq!(added, vec![a.clone()]);
        let s = std::fs::read_to_string(dir.path().join(SECRET_HASHES_FILE_NAME)).unwrap();
        assert_eq!(parse_hashes(&s), vec![a]);
    }

    #[test]
    fn merge_unions_across_calls_dedup_by_sha256() {
        let dir = tempfile::tempdir().unwrap();
        let a = fingerprint_value(b"first-secret-value").unwrap();
        let b = fingerprint_value(b"second-secret-value").unwrap();
        merge_secret_hashes(dir.path(), std::slice::from_ref(&a)).unwrap();
        let added = merge_secret_hashes(dir.path(), &[a.clone(), b.clone()]).unwrap();
        assert_eq!(added, vec![b.clone()]);
        let s = std::fs::read_to_string(dir.path().join(SECRET_HASHES_FILE_NAME)).unwrap();
        let got = parse_hashes(&s);
        assert_eq!(got.len(), 2);
        assert!(got.contains(&a) && got.contains(&b));
    }

    #[test]
    fn merge_of_already_present_adds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let a = fingerprint_value(b"first-secret-value").unwrap();
        merge_secret_hashes(dir.path(), std::slice::from_ref(&a)).unwrap();
        let added = merge_secret_hashes(dir.path(), std::slice::from_ref(&a)).unwrap();
        assert!(added.is_empty());
    }

    #[test]
    fn merge_treats_corrupt_existing_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(SECRET_HASHES_FILE_NAME), b"not json").unwrap();
        let a = fingerprint_value(b"first-secret-value").unwrap();
        let added = merge_secret_hashes(dir.path(), std::slice::from_ref(&a)).unwrap();
        assert_eq!(added, vec![a]);
    }

    #[test]
    fn merge_errors_when_scratch_unwritable() {
        let missing = std::path::Path::new("/nonexistent-kastellan-268/scratch");
        let a = fingerprint_value(b"first-secret-value").unwrap();
        assert!(merge_secret_hashes(missing, &[a]).is_err());
    }

    #[test]
    fn provision_failed_row_shape() {
        let row = provision_failed_audit_row("web-fetch", 2, "disk full");
        assert_eq!(row.actor, "egress_proxy");
        assert_eq!(row.action, "egress.secret_hash.provision_failed");
        assert_eq!(row.payload["worker"], "web-fetch");
        assert_eq!(row.payload["pending"], 2);
        assert_eq!(row.payload["error"], "disk full");
    }
}
