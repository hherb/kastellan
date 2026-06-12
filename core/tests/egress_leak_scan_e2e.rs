//! Egress slice #3b — cross-boundary provisioning contract.
//!
//! Pins that `core`'s `leak_provision::write_secret_hashes` produces exactly the
//! file shape + name the egress proxy reads back via `kastellan_leak_scan`'s
//! wire parser. The streaming detection itself is covered hermetically by the
//! egress-proxy `scan_relay` duplex tests and the `RollingMatcher` units; this
//! guards the contract those two sides agree on (the file name is an independent
//! string literal in `egress-proxy::main` and `core::egress::leak_provision`).

use kastellan_core::egress::leak_provision::{write_secret_hashes, SECRET_HASHES_FILE_NAME};
use kastellan_leak_scan::{fingerprint_value, parse_hashes};

#[test]
fn provisioned_file_round_trips_through_the_proxy_parser() {
    let dir = tempfile::tempdir().expect("scratch");
    let fps = vec![
        fingerprint_value(b"cross-boundary-secret-1").unwrap(),
        fingerprint_value(b"cross-boundary-secret-22").unwrap(),
    ];
    write_secret_hashes(dir.path(), &fps).expect("provision");

    // The proxy reads exactly this file name from the UDS sibling dir.
    let path = dir.path().join(SECRET_HASHES_FILE_NAME);
    assert_eq!(SECRET_HASHES_FILE_NAME, "secret_hashes.json");
    let body = std::fs::read_to_string(&path).expect("read provisioned file");

    // The proxy's parser recovers the exact fingerprints core wrote.
    let recovered = parse_hashes(&body);
    assert_eq!(recovered, fps);
}

#[test]
fn empty_provisioning_is_safe_no_scanning() {
    let dir = tempfile::tempdir().expect("scratch");
    write_secret_hashes(dir.path(), &[]).expect("provision empty");
    let body = std::fs::read_to_string(dir.path().join(SECRET_HASHES_FILE_NAME)).unwrap();
    assert!(parse_hashes(&body).is_empty(), "empty file => proxy scans nothing");
}
