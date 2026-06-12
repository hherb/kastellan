//! The on-disk `secret_hashes.json` provisioning shape. `core` writes it into
//! the sidecar scratch dir; the proxy lazily re-reads it per connection. The
//! hex string encoding for `fp64`/`sha256` avoids JSON `u64`-precision pitfalls
//! and keeps the file human-auditable.

use serde::{Deserialize, Serialize};

use crate::fingerprint::SecretFingerprint;

/// File envelope. `version` lets a future format change be detected rather than
/// silently mis-parsed.
#[derive(Serialize, Deserialize)]
struct HashesFile {
    version: u32,
    secrets: Vec<WireFp>,
}

/// Wire form of one fingerprint: `len` plus hex-encoded `fp64` (16 hex chars)
/// and `sha256` (64 hex chars).
#[derive(Serialize, Deserialize)]
struct WireFp {
    len: usize,
    fp64: String,
    sha256: String,
}

const VERSION: u32 = 1;

/// Serialize fingerprints to the `secret_hashes.json` string.
pub fn serialize_hashes(fps: &[SecretFingerprint]) -> String {
    let secrets = fps
        .iter()
        .map(|f| WireFp {
            len: f.len,
            fp64: format!("{:016x}", f.fp64),
            sha256: hex32(&f.sha256),
        })
        .collect();
    let file = HashesFile {
        version: VERSION,
        secrets,
    };
    serde_json::to_string(&file).expect("HashesFile serialization never fails")
}

/// Parse the `secret_hashes.json` string. Lenient: a malformed file, an unknown
/// version, or a malformed entry yields an empty/partial list rather than an
/// error — a missing or corrupt provisioning file must degrade to "no scanning",
/// never crash the proxy mid-connection.
pub fn parse_hashes(s: &str) -> Vec<SecretFingerprint> {
    let Ok(file) = serde_json::from_str::<HashesFile>(s) else {
        return Vec::new();
    };
    if file.version != VERSION {
        return Vec::new();
    }
    file.secrets.into_iter().filter_map(decode_one).collect()
}

fn decode_one(w: WireFp) -> Option<SecretFingerprint> {
    let fp64 = u64::from_str_radix(&w.fp64, 16).ok()?;
    let sha256 = dehex32(&w.sha256)?;
    Some(SecretFingerprint {
        len: w.len,
        fp64,
        sha256,
    })
}

fn hex32(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn dehex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::fingerprint_value;

    #[test]
    fn round_trips() {
        let fps = vec![
            fingerprint_value(b"first-secret-value").unwrap(),
            fingerprint_value(b"second-secret-value-xyz").unwrap(),
        ];
        let s = serialize_hashes(&fps);
        let back = parse_hashes(&s);
        assert_eq!(back, fps);
    }

    #[test]
    fn empty_round_trips() {
        assert_eq!(parse_hashes(&serialize_hashes(&[])), Vec::new());
    }

    #[test]
    fn garbage_yields_empty() {
        assert!(parse_hashes("not json").is_empty());
        assert!(parse_hashes(r#"{"version":999,"secrets":[]}"#).is_empty());
    }

    #[test]
    fn malformed_entry_is_skipped_not_fatal() {
        let s = r#"{"version":1,"secrets":[{"len":5,"fp64":"zz","sha256":"short"}]}"#;
        assert!(parse_hashes(s).is_empty());
    }
}
