use super::*;

/// Build a self-signed cert with rcgen and return its DER plus the pin computed
/// the way an operator would (SHA-256 of the DER SubjectPublicKeyInfo). The pin
/// here is computed by an *independent* path (rcgen's own SPKI DER) so the test
/// guards `spki_sha256`'s x509-cert `to_der()` re-encode against drift.
fn self_signed_der_and_pin() -> (Vec<u8>, [u8; 32]) {
    use sha2::{Digest, Sha256};
    let key = rcgen::KeyPair::generate().unwrap();
    let params = rcgen::CertificateParams::new(vec!["pin.test".to_string()]).unwrap();
    let cert = params.self_signed(&key).unwrap();
    let cert_der = cert.der().to_vec();
    // rcgen exposes the public key DER as the full SubjectPublicKeyInfo.
    let spki_der = key.public_key_der();
    let pin: [u8; 32] = Sha256::digest(&spki_der).into();
    (cert_der, pin)
}

#[test]
fn spki_sha256_matches_independently_computed_pin() {
    let (cert_der, expected) = self_signed_der_and_pin();
    let got = spki_sha256(&cert_der).expect("parse + hash a valid cert");
    assert_eq!(got, expected, "x509-cert SPKI re-encode must match rcgen's SPKI DER");
}

#[test]
fn spki_sha256_rejects_garbage() {
    assert!(spki_sha256(b"not a certificate").is_err());
}
