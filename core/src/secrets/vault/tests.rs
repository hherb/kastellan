//! Vault lifecycle tests. PG-free; uses an inline test helper to
//! insert entries without going through the async `materialize` path.

use std::time::Duration;

use super::*;

/// Pure test-only insert. Constructs an `Entry` with the given
/// plaintext and `now + ttl` expiry, stores under `r`. Mirrors the
/// `_test_*` inspector pattern from `worker_lifecycle::idle_timeout`.
fn _test_insert(vault: &Vault, r: SecretRef, plaintext: Vec<u8>) {
    let entry = Entry {
        plaintext: Zeroizing::new(plaintext),
        expires_at: Instant::now() + vault.ttl,
    };
    vault
        .map
        .write()
        .expect("vault map poisoned")
        .insert(r, entry);
}

#[test]
fn new_uses_default_ttl() {
    let v = Vault::new();
    assert_eq!(v.ttl, super::super::DEFAULT_TTL);
}

#[test]
fn with_ttl_overrides() {
    let v = Vault::with_ttl(Duration::from_millis(250));
    assert_eq!(v.ttl, Duration::from_millis(250));
}

#[test]
fn default_constructs_with_default_ttl() {
    let v = Vault::default();
    assert_eq!(v.ttl, super::super::DEFAULT_TTL);
}

#[test]
fn redeem_hits_within_ttl() {
    let v = Vault::with_ttl(Duration::from_secs(60));
    let r = SecretRef::from_raw("secret://00000001".to_string());
    _test_insert(&v, r.clone(), b"plaintext-a".to_vec());

    match v.redeem(&r) {
        RedeemResult::Hit(z) => assert_eq!(z.as_slice(), b"plaintext-a"),
        other => panic!("expected Hit, got {other:?}"),
    }
}

#[test]
fn redeem_returns_not_found_when_absent() {
    let v = Vault::new();
    let r = SecretRef::from_raw("secret://00000002".to_string());

    match v.redeem(&r) {
        RedeemResult::NotFound => (),
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[test]
fn redeem_returns_expired_past_ttl_and_gcs_entry() {
    let v = Vault::with_ttl(Duration::from_millis(50));
    let r = SecretRef::from_raw("secret://00000003".to_string());
    _test_insert(&v, r.clone(), b"plaintext-b".to_vec());

    std::thread::sleep(Duration::from_millis(80));

    match v.redeem(&r) {
        RedeemResult::Expired => (),
        other => panic!("expected Expired, got {other:?}"),
    }
    // Second redeem proves the entry was lazy-GC'd on the first call.
    match v.redeem(&r) {
        RedeemResult::NotFound => (),
        other => panic!("expected NotFound after lazy GC, got {other:?}"),
    }
}

#[test]
fn redeem_returns_owned_zeroizing_clone() {
    // Caller's Zeroizing is independent of the vault's stored copy —
    // dropping it doesn't invalidate subsequent redeems within TTL.
    let v = Vault::with_ttl(Duration::from_secs(60));
    let r = SecretRef::from_raw("secret://00000004".to_string());
    _test_insert(&v, r.clone(), b"plaintext-c".to_vec());

    let first = v.redeem(&r);
    drop(first);
    match v.redeem(&r) {
        RedeemResult::Hit(z) => assert_eq!(z.as_slice(), b"plaintext-c"),
        other => panic!("expected Hit on second redeem, got {other:?}"),
    }
}

#[test]
fn vault_drop_zeroes_plaintext() {
    // Smoke: build a vault, insert, drop — no panic, no UB. Real
    // zeroing is the responsibility of Zeroizing<Vec<u8>>'s Drop impl,
    // which is already pinned by the upstream `zeroize` crate's own
    // tests.
    let v = Vault::with_ttl(Duration::from_secs(60));
    let r = SecretRef::from_raw("secret://00000005".to_string());
    _test_insert(&v, r, b"plaintext-to-zero".to_vec());
    drop(v);
}

#[test]
fn insert_fresh_rejects_ref_collision_without_overwriting() {
    // #149: exercises the `Entry::Occupied` arm of `materialize` step 4,
    // extracted into `insert_fresh` so it's reachable without a live PG pool.
    // A second insert at an already-bound ref must return `RefCollision` and
    // leave the original plaintext intact — a silent rebind would re-point the
    // `SecretRef` at a different secret on its next `redeem`.
    let v = Vault::with_ttl(Duration::from_secs(60));
    let r = SecretRef::from_raw("secret://deadbeef".to_string());

    let original = Entry {
        plaintext: Zeroizing::new(b"original-plaintext".to_vec()),
        expires_at: Instant::now() + v.ttl,
    };
    v.insert_fresh(r.clone(), original)
        .expect("first insert hits the Vacant arm");

    let attacker = Entry {
        plaintext: Zeroizing::new(b"attacker-plaintext".to_vec()),
        expires_at: Instant::now() + v.ttl,
    };
    match v.insert_fresh(r.clone(), attacker) {
        Err(VaultError::RefCollision) => (),
        other => panic!("expected RefCollision on the Occupied arm, got {other:?}"),
    }

    // The original entry must be untouched — no silent overwrite.
    match v.redeem(&r) {
        RedeemResult::Hit(z) => assert_eq!(z.as_slice(), b"original-plaintext"),
        other => panic!("expected the original plaintext on redeem, got {other:?}"),
    }
}

#[test]
fn vault_redeem_concurrent_readers_dont_block_each_other() {
    // Spawn 4 threads each redeeming the same ref 100 times. No panic,
    // no deadlock, all return Hit. Light smoke for the RwLock fast path.
    //
    // NOTE: this test does NOT distinguish RwLock from Mutex — it only
    // validates correctness under concurrent access. If the map type were
    // ever switched to Mutex, this test would remain green. The RwLock
    // choice is justified separately by the read-heavy access pattern
    // (one materialize per secret vs. many redeems per dispatch).
    let v = std::sync::Arc::new(Vault::with_ttl(Duration::from_secs(60)));
    let r = SecretRef::from_raw("secret://00000006".to_string());
    _test_insert(&v, r.clone(), b"plaintext-concurrent".to_vec());

    let mut handles = Vec::new();
    for _ in 0..4 {
        let v = v.clone();
        let r = r.clone();
        handles.push(std::thread::spawn(move || {
            for _ in 0..100 {
                match v.redeem(&r) {
                    RedeemResult::Hit(z) => assert_eq!(z.as_slice(), b"plaintext-concurrent"),
                    other => panic!("expected Hit, got {other:?}"),
                }
            }
        }));
    }
    for h in handles {
        h.join().expect("thread panicked");
    }
}

#[test]
fn value_fingerprint_matches_plaintext_hash() {
    use sha2::{Digest, Sha256};
    let vault = Vault::with_ttl(Duration::from_secs(60));
    let value = b"a-real-secret-value-1234";
    let r = SecretRef::from_raw("secret://aabbccdd".to_string());
    _test_insert(&vault, r.clone(), value.to_vec());
    let fp = vault.value_fingerprint(&r).expect("fingerprint");
    let mut h = Sha256::new();
    h.update(value);
    let expected: [u8; 32] = h.finalize().into();
    assert_eq!(fp.sha256, expected);
    assert_eq!(fp.len, value.len());
}

// ---------------------------------------------------------------------------
// Test-only Vault seam (`seed_known_ref_for_test`, #298). The method is
// `#[cfg(debug_assertions)]`-gated — physically absent from release builds —
// so these tests are gated the same way and still compile under
// `cargo test --release` (where the method, and hence the error variant, do
// not exist).
// ---------------------------------------------------------------------------

#[cfg(debug_assertions)]
#[test]
fn seed_known_ref_binds_plaintext_under_caller_chosen_ref() {
    // The whole point of the seam: an out-of-process test can pick the ref
    // string up front (here `secret://deadbe01`) and later pass it as a param,
    // which the daemon's `dispatch` substitutes back to this plaintext.
    let v = Vault::with_ttl(Duration::from_secs(60));
    let r = v
        .seed_known_ref_for_test("deadbe01", b"SCRUBME-plaintext")
        .expect("seed a known ref");
    assert_eq!(r.as_str(), "secret://deadbe01");
    match v.redeem(&r) {
        RedeemResult::Hit(z) => assert_eq!(z.as_slice(), b"SCRUBME-plaintext"),
        other => panic!("expected Hit, got {other:?}"),
    }
}

#[cfg(debug_assertions)]
#[test]
fn seed_known_ref_rejects_malformed_ref() {
    // Only an exact 8-char lowercase-hex tail is accepted — the same
    // well-formed-ref invariant `materialize` mints and `substitute` parses.
    // A caller-supplied tail that is too long/short, uppercase, non-hex, or
    // already prefixed must be rejected, not silently coerced.
    let v = Vault::new();
    for bad in [
        "deadbeef0",          // 9 chars
        "deadbee",            // 7 chars
        "DEADBEEF",           // uppercase
        "secret://deadbeef",  // already prefixed
        "zzzzzzzz",           // non-hex
        "",                   // empty
    ] {
        match v.seed_known_ref_for_test(bad, b"plaintext-value") {
            Err(VaultError::MalformedTestRef(_)) => (),
            other => panic!("expected MalformedTestRef for {bad:?}, got {other:?}"),
        }
    }
}

#[cfg(debug_assertions)]
#[test]
fn seed_known_ref_rejects_empty_plaintext() {
    // Mirrors `materialize`'s EmptyPlaintext guard — an empty seed is operator
    // error, never a usable secret.
    let v = Vault::new();
    match v.seed_known_ref_for_test("deadbeef", b"") {
        Err(VaultError::EmptyPlaintext) => (),
        other => panic!("expected EmptyPlaintext, got {other:?}"),
    }
}

#[cfg(debug_assertions)]
#[test]
fn seed_known_ref_collision_does_not_overwrite() {
    // Delegates to the same `insert_fresh` collision guard as `materialize`:
    // a second seed at an already-bound ref returns RefCollision and leaves the
    // original plaintext intact (no silent rebind).
    let v = Vault::with_ttl(Duration::from_secs(60));
    v.seed_known_ref_for_test("aabbccdd", b"original-value")
        .expect("first seed");
    match v.seed_known_ref_for_test("aabbccdd", b"attacker-value") {
        Err(VaultError::RefCollision) => (),
        other => panic!("expected RefCollision on the second seed, got {other:?}"),
    }
    match v.redeem(&SecretRef::from_raw("secret://aabbccdd".to_string())) {
        RedeemResult::Hit(z) => assert_eq!(z.as_slice(), b"original-value"),
        other => panic!("expected the original plaintext preserved, got {other:?}"),
    }
}

#[test]
fn value_fingerprint_none_for_absent_or_short() {
    let vault = Vault::with_ttl(Duration::from_secs(60));
    // Absent ref.
    assert!(vault
        .value_fingerprint(&SecretRef::from_raw("secret://00000000".to_string()))
        .is_none());
    // Present but below MIN_SECRET_LEN.
    let r = SecretRef::from_raw("secret://11111111".to_string());
    _test_insert(&vault, r.clone(), b"short".to_vec());
    assert!(vault.value_fingerprint(&r).is_none());
}
