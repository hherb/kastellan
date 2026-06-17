//! Walker tests. Use a `FakeVault` fixture so tests are PG-free.

use std::collections::HashMap;

use serde_json::json;
use zeroize::Zeroizing;

use super::*;
use crate::secrets::vault::{RedeemResult, SecretRef};

/// Stub vault for walker tests. Each entry is either present
/// (with plaintext), absent (NotFound), or marked Expired.
enum FakeEntry {
    Present(Vec<u8>),
    Expired,
}

struct FakeVault(HashMap<SecretRef, FakeEntry>);

impl FakeVault {
    fn new() -> Self {
        FakeVault(HashMap::new())
    }
    fn with(mut self, r: SecretRef, plaintext: &[u8]) -> Self {
        self.0.insert(r, FakeEntry::Present(plaintext.to_vec()));
        self
    }
    fn with_expired(mut self, r: SecretRef) -> Self {
        self.0.insert(r, FakeEntry::Expired);
        self
    }
}

impl RedeemFromVault for FakeVault {
    fn redeem(&self, r: &SecretRef) -> RedeemResult {
        match self.0.get(r) {
            Some(FakeEntry::Present(pt)) => RedeemResult::Hit(Zeroizing::new(pt.clone())),
            Some(FakeEntry::Expired) => RedeemResult::Expired,
            None => RedeemResult::NotFound,
        }
    }
}

fn make_ref(tail: &str) -> SecretRef {
    SecretRef::from_raw(format!("secret://{tail}"))
}

#[test]
fn top_level_ref_string_is_substituted() {
    let r = make_ref("aabbccdd");
    let vault = FakeVault::new().with(r.clone(), b"plaintext-X");

    let mut v = json!("secret://aabbccdd");
    let events = substitute_refs_in_params(&mut v, &vault).expect("substitute Ok");

    assert_eq!(v, json!("plaintext-X"));
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].ref_hash, r.ref_hash());
}

#[test]
fn nested_ref_in_object_is_substituted() {
    let r = make_ref("11223344");
    let vault = FakeVault::new().with(r.clone(), b"PT-1");

    let mut v = json!({
        "argv": ["printf", "%s", "secret://11223344"],
        "env":  {"TOKEN": "secret://11223344"}
    });
    let events = substitute_refs_in_params(&mut v, &vault).expect("substitute Ok");

    assert_eq!(events.len(), 2);
    for e in &events {
        assert_eq!(e.ref_hash, r.ref_hash());
    }
    assert_eq!(v["argv"][2], json!("PT-1"));
    assert_eq!(v["env"]["TOKEN"], json!("PT-1"));
}

#[test]
fn nested_ref_in_array_is_substituted() {
    let r = make_ref("aabbccdd");
    let vault = FakeVault::new().with(r.clone(), b"PT-arr");

    let mut v = json!(["leave-me-alone", "secret://aabbccdd", 42]);
    let events = substitute_refs_in_params(&mut v, &vault).expect("substitute Ok");

    assert_eq!(events.len(), 1);
    assert_eq!(v[1], json!("PT-arr"));
    assert_eq!(v[0], json!("leave-me-alone"));
    assert_eq!(v[2], json!(42));
}

#[test]
fn embedded_substring_left_alone() {
    // The spec is exact-match-only. `"Bearer secret://aabbccdd"` is NOT
    // a well-formed ref string; pass through verbatim. The vault is
    // populated with the would-be ref to prove the walker doesn't
    // even consult it on a non-exact-match string.
    let r = make_ref("aabbccdd");
    let vault = FakeVault::new().with(r, b"PT-X");

    let mut v = json!({"header": "Bearer secret://aabbccdd"});
    let events = substitute_refs_in_params(&mut v, &vault).expect("substitute Ok");

    assert_eq!(events.len(), 0);
    assert_eq!(v["header"], json!("Bearer secret://aabbccdd"));
}

#[test]
fn ref_in_object_key_is_not_substituted() {
    // Object keys are not walked — a ref-shaped string used as an
    // object key remains a key. The vault is populated to prove
    // the walker doesn't even consult it for keys.
    let r = make_ref("aabbccdd");
    let vault = FakeVault::new().with(r, b"PT-X");

    let mut v = json!({"secret://aabbccdd": "some-value"});
    let events = substitute_refs_in_params(&mut v, &vault).expect("substitute Ok");

    assert_eq!(events.len(), 0);
    // The key is still in the object verbatim:
    assert_eq!(v["secret://aabbccdd"], json!("some-value"));
}

#[test]
fn uppercase_hex_left_alone() {
    let mut v = json!("secret://AABBCCDD");
    let vault = FakeVault::new();
    let events = substitute_refs_in_params(&mut v, &vault).expect("substitute Ok");

    assert_eq!(events.len(), 0);
    assert_eq!(v, json!("secret://AABBCCDD"));
}

#[test]
fn wrong_length_hex_left_alone() {
    let vault = FakeVault::new();
    for tail in ["aabbccd", "aabbccdde", "aabbccdde0"] {
        let mut v = json!(format!("secret://{tail}"));
        let events = substitute_refs_in_params(&mut v, &vault).expect("substitute Ok");
        assert_eq!(events.len(), 0, "tail {tail} should not match");
    }
}

#[test]
fn missing_ref_returns_missing_ref_with_not_found_reason() {
    let r = make_ref("dead0001");
    let vault = FakeVault::new();
    let mut v = json!("secret://dead0001");

    let err = substitute_refs_in_params(&mut v, &vault).expect_err("must fail closed");
    match err {
        SubstituteError::MissingRef { ref_hash, reason } => {
            assert_eq!(ref_hash, r.ref_hash());
            assert_eq!(reason, MissingReason::NotFound);
        }
        other => panic!("expected MissingRef(NotFound), got {other:?}"),
    }
}

#[test]
fn expired_ref_returns_missing_ref_with_expired_reason() {
    let r = make_ref("dead0002");
    let vault = FakeVault::new().with_expired(r.clone());
    let mut v = json!("secret://dead0002");

    let err = substitute_refs_in_params(&mut v, &vault).expect_err("must fail closed");
    match err {
        SubstituteError::MissingRef { ref_hash, reason } => {
            assert_eq!(ref_hash, r.ref_hash());
            assert_eq!(reason, MissingReason::Expired);
        }
        other => panic!("expected MissingRef(Expired), got {other:?}"),
    }
}

#[test]
fn first_sub_ok_second_miss_pins_partial_walk_contract() {
    // Walker contract (per `substitute_refs_in_params` doc): on
    // error, substitutions that completed BEFORE the failing ref
    // remain visible; the failing ref is unchanged; later refs are
    // not walked. Callers (the `tool_host::dispatch` chokepoint)
    // MUST drop `value` on error — never forward it to the worker,
    // because `value` now mixes redeemed plaintext with an
    // unresolved opaque ref. This test pins both halves of that
    // contract so a future refactor that either (a) rolls back
    // partial work or (b) keeps walking past the first error trips
    // here.
    let a = make_ref("11111111");
    // b is intentionally NOT staged in the vault.
    let vault = FakeVault::new().with(a, b"PT-a");

    let mut v = json!({"first": "secret://11111111", "second": "secret://22222222"});

    // Note: `serde_json::Map` orders keys lexically by default
    // (BTreeMap-backed when the `preserve_order` feature is off),
    // so "first" is walked before "second".
    let err = substitute_refs_in_params(&mut v, &vault).expect_err("must fail closed");
    match err {
        SubstituteError::MissingRef { reason: MissingReason::NotFound, .. } => (),
        other => panic!("expected MissingRef(NotFound), got {other:?}"),
    }
    assert_eq!(
        v["first"], json!("PT-a"),
        "substitutions before the failing ref must remain visible",
    );
    assert_eq!(
        v["second"], json!("secret://22222222"),
        "refs after the failing one must not be walked",
    );
}

#[test]
fn non_utf8_plaintext_returns_plaintext_not_utf8_error() {
    let r = make_ref("00ff00ff");
    let vault = FakeVault::new().with(r.clone(), &[0xFF, 0xFE, 0xFD]);
    let mut v = json!("secret://00ff00ff");

    let err = substitute_refs_in_params(&mut v, &vault).expect_err("must reject binary");
    match err {
        SubstituteError::PlaintextNotUtf8 { ref_hash } => {
            assert_eq!(ref_hash, r.ref_hash());
        }
        other => panic!("expected PlaintextNotUtf8, got {other:?}"),
    }
}

#[test]
fn empty_object_is_no_op() {
    let vault = FakeVault::new();
    let mut v = json!({});
    let events = substitute_refs_in_params(&mut v, &vault).expect("substitute Ok");
    assert_eq!(events.len(), 0);
    assert_eq!(v, json!({}));
}

#[test]
fn empty_array_is_no_op() {
    let vault = FakeVault::new();
    let mut v = json!([]);
    let events = substitute_refs_in_params(&mut v, &vault).expect("substitute Ok");
    assert_eq!(events.len(), 0);
    assert_eq!(v, json!([]));
}

#[test]
fn null_number_bool_are_no_ops() {
    let vault = FakeVault::new();
    // Use `2.5` rather than `3.14` to avoid `clippy::approx_constant`
    // — the test only cares about exercising `Value::Number(f64)`.
    for mut v in [json!(null), json!(42), json!(2.5), json!(true), json!(false)] {
        let events = substitute_refs_in_params(&mut v, &vault).expect("substitute Ok");
        assert_eq!(events.len(), 0);
    }
}

#[test]
fn non_ref_string_is_no_op() {
    let vault = FakeVault::new();
    let mut v = json!("just some unrelated text");
    let events = substitute_refs_in_params(&mut v, &vault).expect("substitute Ok");
    assert_eq!(events.len(), 0);
    assert_eq!(v, json!("just some unrelated text"));
}

/// Anti-drift guard (#268): the read-only [`for_each_ref`] traversal that
/// `collect_refs_in_params` drives MUST visit exactly the refs the mutating
/// [`walk`] redeems — same positions, same order. If they diverge, a secret
/// could be substituted into the worker's params (plaintext egresses) yet never
/// collected for the leak scanner (silent fail-open). We compare the two on a
/// structure that exercises every position: object value, nested array,
/// deeply-nested object, a non-ref string, a ref-shaped object *key* (must be
/// ignored by both), and number/null leaves. The mutating side is observed via
/// `RedemptionEvent`s (one per redeemed ref) from a vault staged with every ref.
#[test]
fn mutating_and_readonly_walkers_visit_the_same_refs() {
    let staged = ["11111111", "22222222", "33333333", "aabbccdd"];
    let mut vault = FakeVault::new();
    for tail in staged {
        vault = vault.with(make_ref(tail), b"PT");
    }

    let shape = json!({
        "top":    "secret://11111111",
        "nested": {"deep": {"k": "secret://22222222"}},
        "arr":    ["plain", "secret://33333333", 42, null],
        "plain":  "not a ref",
        // ref-shaped KEY — neither walker may treat it as a ref:
        "secret://aabbccdd": "value-under-ref-key",
    });

    // Read-only side: refs collected by the shared traversal.
    let mut collected: Vec<String> = Vec::new();
    for_each_ref(&shape, &mut |s| collected.push(s.to_string()));

    // Mutating side: refs the substitution walker actually redeemed.
    let mut to_mutate = shape.clone();
    let events =
        substitute_refs_in_params(&mut to_mutate, &vault).expect("all staged ⇒ substitute Ok");
    let redeemed_hashes: Vec<String> = events.into_iter().map(|e| e.ref_hash).collect();

    // Same refs, same document order — compared by `ref_hash` (the read-only
    // side yields raw ref strings, the mutating side yields hashes).
    let collected_hashes: Vec<String> = collected
        .iter()
        .map(|s| SecretRef::from_raw(s.clone()).ref_hash())
        .collect();
    assert_eq!(
        collected_hashes, redeemed_hashes,
        "for_each_ref and the mutating walk must visit the same refs in the same order",
    );
    // And exactly the three value-position refs — never the key-position one.
    assert_eq!(collected.len(), 3);
    assert!(!collected.iter().any(|s| s == "secret://aabbccdd"));
}

#[test]
fn multiple_distinct_refs_in_one_value_all_substituted() {
    let a = make_ref("11111111");
    let b = make_ref("22222222");
    let vault = FakeVault::new()
        .with(a.clone(), b"PT-a")
        .with(b.clone(), b"PT-b");

    let mut v = json!({"left": "secret://11111111", "right": "secret://22222222"});
    let events = substitute_refs_in_params(&mut v, &vault).expect("substitute Ok");

    assert_eq!(events.len(), 2);
    assert_eq!(v["left"], json!("PT-a"));
    assert_eq!(v["right"], json!("PT-b"));
}
