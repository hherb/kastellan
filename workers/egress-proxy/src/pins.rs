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

#[cfg(test)]
mod tests;
