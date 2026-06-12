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

use kastellan_leak_scan::{serialize_hashes, SecretFingerprint};

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
}
