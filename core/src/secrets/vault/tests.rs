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
