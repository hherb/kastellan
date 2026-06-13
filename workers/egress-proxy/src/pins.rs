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
pub const PIN_MISMATCH_MARKER: &str = "certificate pin mismatch";

/// Errors from parsing pins or extracting an SPKI. Display-only.
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
#[derive(Debug, Default, Clone)]
pub struct PinSet {
    map: HashMap<String, HashSet<[u8; 32]>>,
}

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

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as RustlsError, RootCertStore, SignatureScheme};

/// Render a rustls `ServerName` to the host string used as the pin-map key.
fn server_name_host(name: &ServerName) -> String {
    match name {
        ServerName::DnsName(d) => d.as_ref().to_ascii_lowercase(),
        // Canonical text form (`1.2.3.4` / `::1`) so the key matches what an
        // operator writes in the pins JSON. IPv6 keys are BARE (no brackets);
        // operators must pin `"::1"`, not `"[::1]"`.
        ServerName::IpAddress(ip) => std::net::IpAddr::from(*ip).to_string(),
        // `ServerName` is non_exhaustive; an unknown kind is simply unpinnable.
        _ => String::new(),
    }
}

/// A rustls server-cert verifier that runs standard webpki chain validation and
/// then, for hosts in `pins`, additionally requires a chain SPKI to match a pin.
/// Unpinned hosts are unaffected (webpki only). Signature-verification methods
/// delegate to the inner webpki verifier unchanged.
#[derive(Debug)]
pub struct PinningVerifier {
    inner: Arc<WebPkiServerVerifier>,
    pins: PinSet,
}

impl PinningVerifier {
    /// Build over `roots`. Returns `Err` only if rustls refuses the roots.
    pub fn new(roots: Arc<RootCertStore>, pins: PinSet) -> Result<Self, PinError> {
        let inner = WebPkiServerVerifier::builder(roots)
            .build()
            .map_err(|e| PinError::Verifier(e.to_string()))?;
        Ok(Self { inner, pins })
    }
}

impl ServerCertVerifier for PinningVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, RustlsError> {
        // 1. ALWAYS: standard webpki chain validation. Fail-closed if it fails.
        self.inner
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)?;
        // 2. Pin overlay — only for hosts the operator pinned.
        if let Some(pins) = self.pins.pins_for(&server_name_host(server_name)) {
            let chain: Vec<&[u8]> = std::iter::once(end_entity.as_ref())
                .chain(intermediates.iter().map(|c| c.as_ref()))
                .collect();
            if !chain_has_pin(pins, &chain) {
                return Err(RustlsError::General(PIN_MISMATCH_MARKER.to_string()));
            }
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// Build the upstream-leg `ClientConfig` for the MITM re-origination.
///
/// * `None` / blank / `{}` ⇒ the plain webpki-roots config (byte-identical to
///   the pre-slice-#4 behaviour, zero added cost).
/// * a valid non-empty pin set ⇒ the same webpki roots wrapped in a
///   [`PinningVerifier`].
/// * a set-but-unparseable value ⇒ `Err` (the caller aborts startup — fail loud,
///   never silently degrade to no-pinning).
pub fn build_upstream_client_config(
    pins_env: Option<&str>,
) -> Result<Arc<rustls::ClientConfig>, PinError> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let pins = match pins_env.map(str::trim) {
        None | Some("") => PinSet::default(),
        Some(json) => PinSet::parse(json)?,
    };

    if pins.is_empty() {
        return Ok(Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        ));
    }

    let verifier = Arc::new(PinningVerifier::new(Arc::new(roots), pins)?);
    Ok(Arc::new(
        rustls::ClientConfig::builder()
            .dangerous() // custom verifier — STRENGTHENS validation (webpki + pin overlay)
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth(),
    ))
}

#[cfg(test)]
mod tests;
