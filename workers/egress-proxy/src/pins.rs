//! TLS certificate pinning for the egress-proxy upstream re-origination leg
//! (slice #4). Pure SPKI hashing + an RFC-7469 pin set + a custom rustls
//! `ServerCertVerifier` that overlays pin enforcement on top of (never instead
//! of) standard webpki chain validation. Design:
//! docs/superpowers/specs/2026-06-13-egress-proxy-slice4-tls-pinning-design.md

use sha2::{Digest, Sha256};
use x509_cert::der::{Decode, Encode};

/// Marker embedded in the rustls error a pin mismatch produces, so the sync
/// accept path (`proxy::run_mitm`) can distinguish a pin rejection from a
/// generic upstream-handshake failure without a typed error channel through
/// tokio-rustls.
// Forward-declared for later tasks in slice #4; not wired up yet.
#[allow(dead_code)]
pub const PIN_MISMATCH_MARKER: &str = "certificate pin mismatch";

/// Errors from parsing pins or extracting an SPKI. Display-only.
// Variants used by later slice #4 tasks (pin-set + verifier).
#[allow(dead_code)]
#[derive(Debug)]
pub enum PinError {
    /// The `KASTELLAN_EGRESS_PROXY_PINS` JSON did not parse / was the wrong shape.
    Json(String),
    /// A pin string was not a valid `sha256/<base64>` 32-byte digest.
    Pin(String),
    /// A certificate could not be parsed for SPKI extraction.
    X509(String),
    /// rustls refused to build the inner webpki verifier from the roots.
    Verifier(String),
}

impl std::fmt::Display for PinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PinError::Json(s) => write!(f, "pins JSON: {s}"),
            PinError::Pin(s) => write!(f, "pin value: {s}"),
            PinError::X509(s) => write!(f, "certificate SPKI: {s}"),
            PinError::Verifier(s) => write!(f, "webpki verifier: {s}"),
        }
    }
}
impl std::error::Error for PinError {}

/// Compute the RFC-7469 pin pre-image hash of a certificate: `SHA-256` over the
/// DER-encoded `SubjectPublicKeyInfo`. `to_der()` re-encodes; for canonical DER
/// (every CA-issued cert) that is byte-identical to the original SPKI bytes —
/// pinned by `spki_sha256_matches_independently_computed_pin`.
// Used by tests now; pin-set + verifier tasks (later slice #4) will use it from
// non-test code.
#[allow(dead_code)]
pub fn spki_sha256(cert_der: &[u8]) -> Result<[u8; 32], PinError> {
    let cert = x509_cert::Certificate::from_der(cert_der)
        .map_err(|e| PinError::X509(format!("parse cert: {e}")))?;
    let spki_der = cert
        .tbs_certificate
        .subject_public_key_info
        .to_der()
        .map_err(|e| PinError::X509(format!("encode SPKI: {e}")))?;
    Ok(Sha256::digest(&spki_der).into())
}

use std::collections::{HashMap, HashSet};

/// A parsed set of SPKI pins, keyed by lowercased host.
// Forward-declared for Task 3 (PinningVerifier); not yet used in production code.
#[allow(dead_code)]
#[derive(Debug, Default, Clone)]
pub struct PinSet {
    map: HashMap<String, HashSet<[u8; 32]>>,
}

#[allow(dead_code)]
impl PinSet {
    /// Parse the `KASTELLAN_EGRESS_PROXY_PINS` JSON:
    /// `{ "host": ["sha256/<base64>", ...], ... }`. Host keys are lowercased.
    /// A host whose pin list is empty is dropped (an empty list can never match
    /// → it would be a silent permanent block). Strict on structure: anything
    /// that is not an object of string→array-of-`sha256/<base64>`-strings, or a
    /// pin that does not decode to exactly 32 bytes, is an `Err`.
    pub fn parse(json: &str) -> Result<PinSet, PinError> {
        let raw: HashMap<String, Vec<String>> = serde_json::from_str(json)
            .map_err(|e| PinError::Json(e.to_string()))?;
        let mut map = HashMap::new();
        for (host, pin_strs) in raw {
            let mut pins = HashSet::new();
            for s in &pin_strs {
                pins.insert(parse_pin(s)?);
            }
            if !pins.is_empty() {
                map.insert(host.to_ascii_lowercase(), pins);
            }
        }
        Ok(PinSet { map })
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// The pin set for `host`, if the operator pinned it (case-insensitive).
    pub fn pins_for(&self, host: &str) -> Option<&HashSet<[u8; 32]>> {
        self.map.get(&host.to_ascii_lowercase())
    }
}

/// Decode one `sha256/<base64-standard>` pin string into a 32-byte digest.
#[allow(dead_code)]
fn parse_pin(s: &str) -> Result<[u8; 32], PinError> {
    use base64::Engine;
    let b64 = s
        .strip_prefix("sha256/")
        .ok_or_else(|| PinError::Pin(format!("missing `sha256/` prefix: {s:?}")))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| PinError::Pin(format!("base64: {e}")))?;
    bytes
        .try_into()
        .map_err(|v: Vec<u8>| PinError::Pin(format!("expected 32 bytes, got {}", v.len())))
}

/// True iff any DER cert in `chain` hashes to a pin in `pins`. A cert that fails
/// SPKI extraction is treated as "no match" (webpki has already validated the
/// chain by the time this runs), never fatal.
#[allow(dead_code)]
pub fn chain_has_pin(pins: &HashSet<[u8; 32]>, chain: &[&[u8]]) -> bool {
    chain
        .iter()
        .filter_map(|der| spki_sha256(der).ok())
        .any(|h| pins.contains(&h))
}

/// Test seam: match against already-hashed SPKIs (so unit tests need not forge
/// real chain certificates). Production uses [`chain_has_pin`].
#[cfg(test)]
pub(crate) fn chain_pins_contains(pins: &HashSet<[u8; 32]>, hashes: &[[u8; 32]]) -> bool {
    hashes.iter().any(|h| pins.contains(h))
}

#[cfg(test)]
mod tests;
