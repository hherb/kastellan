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

fn pin_str(bytes: &[u8; 32]) -> String {
    use base64::Engine;
    format!("sha256/{}", base64::engine::general_purpose::STANDARD.encode(bytes))
}

#[test]
fn parse_valid_multi_host_multi_pin() {
    let a = [0x11u8; 32];
    let b = [0x22u8; 32];
    let json = format!(
        r#"{{"api.anthropic.com":["{}","{}"],"API.OpenAI.com":["{}"]}}"#,
        pin_str(&a), pin_str(&b), pin_str(&a)
    );
    let set = PinSet::parse(&json).expect("valid pins parse");
    assert!(!set.is_empty());
    // Host lookup is case-insensitive.
    let anthropic = set.pins_for("api.anthropic.com").unwrap();
    assert!(anthropic.contains(&a) && anthropic.contains(&b));
    assert!(set.pins_for("api.openai.com").unwrap().contains(&a));
    assert!(set.pins_for("unpinned.example.com").is_none());
}

#[test]
fn parse_empty_object_is_empty_set() {
    assert!(PinSet::parse("{}").unwrap().is_empty());
}

#[test]
fn parse_drops_hosts_with_empty_pin_list() {
    // An empty pin list would permanently block its host — treat as "no pin".
    let set = PinSet::parse(r#"{"h.example.com":[]}"#).unwrap();
    assert!(set.pins_for("h.example.com").is_none());
    assert!(set.is_empty());
}

#[test]
fn parse_rejects_malformed() {
    assert!(PinSet::parse("not json").is_err());
    assert!(PinSet::parse(r#"["array","not","object"]"#).is_err());
    assert!(PinSet::parse(r#"{"h":"string-not-array"}"#).is_err());
    assert!(PinSet::parse(r#"{"h":["nothashprefix"]}"#).is_err()); // missing sha256/
    assert!(PinSet::parse(r#"{"h":["sha256/!!!notbase64!!!"]}"#).is_err());
    assert!(PinSet::parse(r#"{"h":["sha256/YWJj"]}"#).is_err()); // decodes to 3 bytes, not 32
}

#[test]
fn chain_has_pin_matches_end_entity_and_intermediate() {
    let ee = [0xAAu8; 32];
    let inter = [0xBBu8; 32];
    let pins: std::collections::HashSet<[u8; 32]> = [inter].into_iter().collect();
    // We can't cheaply forge real chain certs here, so exercise the matcher with
    // a small shim: chain_has_pin takes pre-hashed inputs via a test seam.
    assert!(super::chain_pins_contains(&pins, &[ee, inter]));
    assert!(!super::chain_pins_contains(&pins, &[ee]));
}

#[test]
fn server_name_host_renders_dns_and_ip_canonically() {
    use rustls::pki_types::ServerName;
    // DNS is lowercased.
    let dns = ServerName::try_from("API.Anthropic.com").unwrap();
    assert_eq!(super::server_name_host(&dns), "api.anthropic.com");
    // IPv4 literal renders as plain dotted-quad (NOT the Debug form).
    let v4 = ServerName::try_from("203.0.113.7").unwrap();
    assert_eq!(super::server_name_host(&v4), "203.0.113.7");
    // IPv6 literal renders bare (no brackets).
    let v6 = ServerName::IpAddress(std::net::Ipv6Addr::LOCALHOST.into());
    assert_eq!(super::server_name_host(&v6), "::1");
}

use std::sync::Arc;
use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};

fn install_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// A self-signed cert that IS its own root, so the inner webpki verifier accepts
/// it. Returns (chain-as-roots, end-entity DER, its pin).
fn trusted_self_signed(host: &str) -> (rustls::RootCertStore, Vec<u8>, [u8; 32]) {
    use sha2::{Digest, Sha256};
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(vec![host.to_string()]).unwrap();
    params.extended_key_usages.push(rcgen::ExtendedKeyUsagePurpose::ServerAuth);
    let cert = params.self_signed(&key).unwrap();
    let der = cert.der().to_vec();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from(der.clone())).unwrap();
    let pin: [u8; 32] = Sha256::digest(key.public_key_der()).into();
    (roots, der, pin)
}

fn verify(verifier: &PinningVerifier, host: &str, ee_der: &[u8]) -> Result<(), rustls::Error> {
    let ee = CertificateDer::from(ee_der.to_vec());
    let name = ServerName::try_from(host.to_string()).unwrap();
    verifier
        .verify_server_cert(&ee, &[], &name, &[], UnixTime::since_unix_epoch(std::time::Duration::from_secs(1_700_000_000)))
        .map(|_| ())
}

#[test]
fn unpinned_host_passes_on_webpki_alone() {
    install_provider();
    let (roots, ee, _pin) = trusted_self_signed("origin.test");
    // No pins at all → behaves like plain webpki.
    let verifier = PinningVerifier::new(Arc::new(roots), PinSet::default()).unwrap();
    assert!(verify(&verifier, "origin.test", &ee).is_ok());
}

#[test]
fn pinned_host_with_matching_spki_passes() {
    install_provider();
    let (roots, ee, pin) = trusted_self_signed("origin.test");
    let pins = PinSet::parse(&format!(r#"{{"origin.test":["{}"]}}"#, pin_str(&pin))).unwrap();
    let verifier = PinningVerifier::new(Arc::new(roots), pins).unwrap();
    assert!(verify(&verifier, "origin.test", &ee).is_ok());
}

#[test]
fn pinned_host_with_wrong_spki_is_rejected() {
    install_provider();
    let (roots, ee, _pin) = trusted_self_signed("origin.test");
    let wrong = [0x99u8; 32];
    let pins = PinSet::parse(&format!(r#"{{"origin.test":["{}"]}}"#, pin_str(&wrong))).unwrap();
    let verifier = PinningVerifier::new(Arc::new(roots), pins).unwrap();
    let err = verify(&verifier, "origin.test", &ee).unwrap_err();
    assert!(err.to_string().contains(PIN_MISMATCH_MARKER), "got: {err}");
}

#[test]
fn build_upstream_none_is_plain_webpki() {
    install_provider();
    assert!(build_upstream_client_config(None).is_ok());
}

#[test]
fn build_upstream_empty_string_is_plain_webpki() {
    install_provider();
    assert!(build_upstream_client_config(Some("   ")).is_ok());
    assert!(build_upstream_client_config(Some("{}")).is_ok());
}

#[test]
fn build_upstream_valid_pins_builds() {
    install_provider();
    let pin = [0x33u8; 32];
    let json = format!(r#"{{"api.anthropic.com":["{}"]}}"#, pin_str(&pin));
    assert!(build_upstream_client_config(Some(&json)).is_ok());
}

#[test]
fn build_upstream_malformed_pins_is_err() {
    install_provider();
    assert!(build_upstream_client_config(Some("{ this is not json")).is_err());
}
