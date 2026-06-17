//! Egress slice #3b — cross-boundary provisioning contract.
//!
//! Pins that `core`'s `leak_provision::write_secret_hashes` produces exactly the
//! file shape + name the egress proxy reads back via `kastellan_leak_scan`'s
//! wire parser. The streaming detection itself is covered hermetically by the
//! egress-proxy `scan_relay` duplex tests and the `RollingMatcher` units; this
//! guards the contract those two sides agree on (the file name is an independent
//! string literal in `egress-proxy::main` and `core::egress::leak_provision`).

use kastellan_core::egress::leak_provision::{
    merge_secret_hashes, write_secret_hashes, SECRET_HASHES_FILE_NAME,
};
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

/// The dispatch-time append (`merge_secret_hashes`, #268) accumulates the union
/// across calls and writes exactly what the proxy's `parse_hashes` reads back —
/// the same contract the spawn-time `write_secret_hashes` honours.
#[test]
fn dispatch_append_union_round_trips_through_proxy_parser() {
    let dir = tempfile::tempdir().unwrap();
    let a = fingerprint_value(b"dispatch-secret-alpha").unwrap();
    let b = fingerprint_value(b"dispatch-secret-bravo").unwrap();

    // First dispatch provisions `a`; a later dispatch on the same (reused)
    // worker provisions `b` — both must be present (union, decision D2).
    assert_eq!(
        merge_secret_hashes(dir.path(), std::slice::from_ref(&a)).unwrap(),
        vec![a.clone()]
    );
    assert_eq!(
        merge_secret_hashes(dir.path(), std::slice::from_ref(&b)).unwrap(),
        vec![b.clone()]
    );

    // The proxy reads the file with the same parser it uses per-connection.
    let s = std::fs::read_to_string(dir.path().join(SECRET_HASHES_FILE_NAME)).unwrap();
    let got = parse_hashes(&s);
    assert_eq!(got.len(), 2);
    assert!(got.contains(&a) && got.contains(&b));
}
