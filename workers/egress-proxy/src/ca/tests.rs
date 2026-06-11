use super::{generate_ca, issue_leaf};
// `pem_slice_iter` is provided by the `PemObject` trait, which must be in scope.
use rustls_pki_types::pem::PemObject;

#[test]
fn ca_pem_round_trips_as_a_parseable_certificate() {
    let ca = generate_ca().expect("generate CA");
    let pem = ca.cert_pem();
    assert!(pem.starts_with("-----BEGIN CERTIFICATE-----"));
    // Parse it back as a DER cert via rustls-pki-types to prove it's well-formed.
    let der: Vec<_> = rustls_pki_types::CertificateDer::pem_slice_iter(pem.as_bytes())
        .collect::<Result<_, _>>()
        .expect("CA PEM parses as a certificate");
    assert_eq!(der.len(), 1, "exactly one CA certificate in the PEM");
}

#[test]
fn issued_leaf_carries_the_requested_host_as_san() {
    let ca = generate_ca().expect("generate CA");
    let leaf = issue_leaf(&ca, "api.example.com").expect("issue leaf");
    assert!(!leaf.cert_der().is_empty());
    assert!(!leaf.key_der().secret_der().is_empty());
    let needle = b"api.example.com";
    assert!(
        leaf.cert_der().windows(needle.len()).any(|w| w == needle),
        "leaf DER must encode the requested host as a SAN"
    );
}

#[test]
fn two_generated_cas_differ() {
    let a = generate_ca().unwrap();
    let b = generate_ca().unwrap();
    assert_ne!(a.cert_pem(), b.cert_pem(), "each CA must be unique");
}
