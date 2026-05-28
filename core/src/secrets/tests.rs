//! Module-level tests: public surface re-exports, const pins,
//! `SecretRef` round-trip. The richer Vault and Walker tests live in
//! `vault/tests.rs` and `substitute/tests.rs`.

use super::*;

#[test]
fn default_ttl_is_exactly_one_hour() {
    assert_eq!(DEFAULT_TTL, std::time::Duration::from_secs(3600));
}

#[test]
fn ref_prefix_is_secret_scheme() {
    assert_eq!(REF_PREFIX, "secret://");
}

#[test]
fn ref_hex_len_is_eight() {
    assert_eq!(REF_HEX_LEN, 8);
}

#[test]
fn secret_ref_as_str_roundtrip() {
    let r = SecretRef::from_raw("secret://deadbeef".to_string());
    assert_eq!(r.as_str(), "secret://deadbeef");
}

#[test]
fn secret_ref_hash_is_64_lowercase_hex() {
    let r = SecretRef::from_raw("secret://deadbeef".to_string());
    let h = r.ref_hash();
    assert_eq!(h.len(), 64, "SHA-256 hex must be 64 chars");
    assert!(
        h.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "ref_hash must be lowercase hex: got {h:?}"
    );
}

#[test]
fn secret_ref_hash_is_stable() {
    let r = SecretRef::from_raw("secret://aabbccdd".to_string());
    assert_eq!(r.ref_hash(), r.ref_hash());
}

#[test]
fn secret_ref_hash_distinguishes_refs() {
    let a = SecretRef::from_raw("secret://aabbccdd".to_string());
    let b = SecretRef::from_raw("secret://aabbccde".to_string());
    assert_ne!(a.ref_hash(), b.ref_hash());
}
