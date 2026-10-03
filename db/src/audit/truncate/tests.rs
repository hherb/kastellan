//! Unit tests for the [`super`] audit module.
//!
//! Lifted verbatim out of `audit.rs` in a movement-only commit (the repo
//! convention is to split *before* the change that grows a file, so the
//! `#[test]` name set is verifiable either side of the move). Nothing here
//! was rewritten: the assertions, the fixtures and the doc comments are the
//! ones that were in `audit.rs`, and the test-name set is unchanged at 25.

use super::*;

/// Small payloads pass through unchanged — the truncation envelope
/// must not be wrapped around already-fitting values.
///
/// The fixture carries a preserved key deliberately: the under-cap
/// path must return the payload *itself*, not an envelope that
/// happens to have rescued the same key. Those two are easy to
/// conflate once preservation exists.
#[test]
fn small_payload_is_not_truncated() {
    let v = serde_json::json!({
        "actor": "core",
        "ms": 12,
        (GUARD_KEY): {"state": "clear", "p": 0.5},
    });
    let out = truncate_payload(v.clone());
    assert_eq!(out, v);
    assert!(!is_truncation_envelope(&out), "a fitting payload is not an envelope");
}

/// Empty object is the canonical default and must stay byte-for-byte.
#[test]
fn empty_object_passes_through() {
    let v = serde_json::json!({});
    assert_eq!(truncate_payload(v.clone()), v);
}

/// A payload at exactly the threshold byte count must NOT be
/// truncated — the bound is inclusive. (Off-by-one regression
/// guard: an earlier draft used `<` instead of `<=`.)
#[test]
fn payload_at_exact_threshold_is_not_truncated() {
    // Build a string whose JSON serialisation is exactly
    // `PAYLOAD_MAX_BYTES`. The serialisation of `"...payload..."`
    // adds 2 bytes for the surrounding double quotes.
    let inner_len = PAYLOAD_MAX_BYTES - 2;
    let s: String = "x".repeat(inner_len);
    let v = serde_json::Value::String(s);
    // Sanity: serialised length is exactly the bound.
    assert_eq!(serde_json::to_vec(&v).unwrap().len(), PAYLOAD_MAX_BYTES);
    let out = truncate_payload(v.clone());
    assert_eq!(out, v, "boundary is inclusive: == max must not truncate");
}

/// One byte over the threshold must be truncated. The envelope
/// shape (`_truncated: true` + `sha256` + `len`) is the wire
/// contract the JSONL mirror relies on; a downstream parser will
/// notice if any of these keys go missing.
#[test]
fn over_threshold_payload_is_replaced_with_envelope() {
    let s: String = "y".repeat(PAYLOAD_MAX_BYTES);
    let v = serde_json::Value::String(s);
    let original_len = serde_json::to_vec(&v).unwrap().len();
    assert!(original_len > PAYLOAD_MAX_BYTES);

    let out = truncate_payload(v);
    let obj = out.as_object().expect("envelope must be a JSON object");
    assert_eq!(obj.get("_truncated"), Some(&serde_json::Value::Bool(true)));
    assert_eq!(
        obj.get("len").and_then(|v| v.as_i64()),
        Some(original_len as i64)
    );
    let sha = obj
        .get("sha256")
        .and_then(|v| v.as_str())
        .expect("sha256 must be a string");
    assert_eq!(sha.len(), 64, "sha256 hex must be 64 chars");
    assert!(
        sha.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "sha256 must be lowercase hex: got {sha}"
    );
}

/// Producer↔predicate round-trip: whatever `truncate_payload` emits,
/// `is_truncation_envelope` must recognize — and an untruncated payload
/// must NOT be recognized. Cross-crate readers (core's observation
/// capture, #62) key off this predicate, so a shape change that breaks
/// the pairing must fail here, in the file that owns both sides.
#[test]
fn is_truncation_envelope_round_trips_producer() {
    let big = serde_json::Value::String("z".repeat(PAYLOAD_MAX_BYTES + 1));
    assert!(is_truncation_envelope(&truncate_payload(big)));

    let small = serde_json::json!({"plan": {"steps": []}});
    assert!(!is_truncation_envelope(&truncate_payload(small.clone())));
    assert!(!is_truncation_envelope(&small));
    // A non-boolean or absent marker is not an envelope.
    assert!(!is_truncation_envelope(&serde_json::json!({"_truncated": "yes"})));
}

/// Same input → same fingerprint. This is what makes truncated
/// rows comparable: two operator queries that returned the same
/// big body show the same `sha256`, even though the body itself
/// is gone. Regression guard against accidentally salting the
/// hash.
#[test]
fn truncate_is_deterministic_for_same_input() {
    let s = "z".repeat(PAYLOAD_MAX_BYTES + 100);
    let v1 = serde_json::Value::String(s.clone());
    let v2 = serde_json::Value::String(s);
    let a = truncate_payload(v1);
    let b = truncate_payload(v2);
    assert_eq!(a, b);
}

/// A `guard` decision record survives truncation.
///
/// Found live on the DGX, 2026-08-23: two `web.fetch` rows whose
/// payloads serialised to 85,352 and 85,351 bytes were stored as bare
/// fingerprint envelopes, so the guard-tier score the dispatcher had
/// just computed was gone. The tool payload is
/// `{req, result, ms, guard}` and `result` carries the whole tool
/// output, so any result past ~4 KiB took the `guard` object down with
/// it.
///
/// **The bias runs the wrong way.** A *blocked* dispatch usually keeps
/// its score, because the result was already replaced by a short
/// withheld placeholder — `req` is still in the payload, so a block on a
/// multi-KiB `shell.exec` argv could lose one too, but not as a function
/// of document size. A *cleared* dispatch loses its score as soon as the
/// document is large. Recording `p` on the cleared half is the whole of the wiring
/// spec's D5 — it is what makes production a score source that is not
/// catalogue-selected — so what survived was every block plus only the
/// small clears: a size-selected sample that reads like data.
#[test]
fn truncation_preserves_the_guard_decision_record() {
    let guard = serde_json::json!({
        "state": "clear",
        "p": 0.0074157947,
        "tau": 0.79552656,
        "ms": 75,
        "body_byte_len": 285,
        "truncated": false,
    });
    let v = serde_json::json!({
        "req": {"argv": ["/usr/bin/printf", "x"]},
        "result": {"text": "w".repeat(PAYLOAD_MAX_BYTES)},
        "ms": 12,
        "guard": guard.clone(),
    });
    assert!(serde_json::to_vec(&v).unwrap().len() > PAYLOAD_MAX_BYTES);

    let out = truncate_payload(v);
    assert!(
        is_truncation_envelope(&out),
        "the row must still declare itself truncated: {out}"
    );
    assert_eq!(
        out.get("guard"),
        Some(&guard),
        "the guard score exists nowhere else -- unlike `req` and `result`, \
         it cannot be recovered from the worker or from the JSONL mirror"
    );
    // The data is still gone: preserving a decision record is not
    // preserving the document it was about.
    assert!(out.get("result").is_none(), "the oversized data must NOT be kept");
    assert!(out.get("req").is_none(), "only the allowlisted keys ride along");
}

/// The fingerprint describes the ORIGINAL payload, not the envelope.
///
/// Preserving a key must not change what `sha256`/`len` mean, or two
/// rows for the same body would stop comparing equal the moment one of
/// them carried a guard score.
#[test]
fn a_preserved_key_does_not_change_the_fingerprint() {
    let big = serde_json::json!({"result": "q".repeat(PAYLOAD_MAX_BYTES)});
    let mut with_guard = big.clone();
    with_guard["guard"] = serde_json::json!({"state": "clear", "p": 0.5});

    let bare = truncate_payload(big.clone());
    let kept = truncate_payload(with_guard.clone());

    // Each envelope fingerprints its OWN input...
    let expect = |v: &serde_json::Value| {
        use sha2::Digest;
        let bytes = serde_json::to_vec(v).unwrap();
        (format!("{:x}", sha2::Sha256::digest(&bytes)), bytes.len())
    };
    for (env, src) in [(&bare, &big), (&kept, &with_guard)] {
        let (sha, len) = expect(src);
        assert_eq!(env.get("sha256").and_then(|v| v.as_str()), Some(sha.as_str()));
        assert_eq!(env.get("len").and_then(|v| v.as_u64()), Some(len as u64));
    }
}

/// A payload with no preserved key keeps the exact three-key envelope.
///
/// Pins the shape existing readers were written against, so preserving
/// a key stays strictly additive.
#[test]
fn a_payload_without_a_preserved_key_keeps_the_bare_envelope() {
    let v = serde_json::json!({"plan": {"steps": ["x".repeat(PAYLOAD_MAX_BYTES)]}});
    let out = truncate_payload(v);
    let obj = out.as_object().expect("envelope is an object");
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, vec!["_truncated", "len", "sha256"]);
}

/// An oversized preserved key is dropped — and SAID to be dropped.
///
/// The allowlisted keys are bounded *by construction* at every site
/// this crate knows about, but `truncate_payload` is public and takes
/// an arbitrary `Value`. Its one hard postcondition is that the return
/// value fits the budget; a preserved key must never be able to break
/// that, and silently storing an over-budget row is exactly the failure
/// the cap exists to prevent.
///
/// But dropping it *silently* would be the other failure the cap
/// exists to prevent: an envelope with no `guard` and no explanation is
/// byte-identical to one whose dispatch never ran a guard tier at all,
/// which is the ambiguity that let the original defect hide. So the
/// budget wins, and the loss is recorded rather than merely suffered.
#[test]
fn an_oversized_preserved_key_is_dropped_but_named() {
    let v = serde_json::json!({
        "guard": {"state": "clear", "junk": "!".repeat(PAYLOAD_MAX_BYTES)},
    });
    let out = truncate_payload(v);
    assert!(is_truncation_envelope(&out));
    assert!(
        out.get("guard").is_none(),
        "a preserved key that does not fit is dropped, not stored over budget"
    );
    assert_eq!(
        out.get(DROPPED_PRESERVED_KEY),
        Some(&serde_json::json!(["guard"])),
        "and a reader must be able to tell this from a dispatch that never ran the tier"
    );
    assert!(serde_json::to_vec(&out).unwrap().len() <= PAYLOAD_MAX_BYTES);
}

/// A preserved key that fits leaves NO drop marker.
///
/// The marker means "something was lost". If it appeared on the happy
/// path it would mean nothing at all, and the assertion above would be
/// pinning noise rather than a signal.
#[test]
fn a_preserved_key_that_fits_leaves_no_drop_marker() {
    let v = serde_json::json!({
        "result": "z".repeat(PAYLOAD_MAX_BYTES),
        "guard": {"state": "clear", "p": 0.5},
    });
    let out = truncate_payload(v);
    assert!(out.get("guard").is_some(), "it fits, so it rides");
    assert!(out.get(DROPPED_PRESERVED_KEY).is_none(), "nothing was lost: {out}");
}

/// A preserved key is copied VERBATIM, whatever its value.
///
/// `truncate_payload` allowlists *keys*, not shapes — it has no opinion
/// about what a decision record should look like, and acquiring one
/// would make it a second, silent validator of every producer. `null` is
/// the case that matters in practice: `guard.p` is an `Option<f32>` and
/// serialises to `null` on the unadjudicated arm, so a reader already
/// has to handle it.
#[test]
fn a_preserved_key_is_copied_verbatim_including_null() {
    let big = "y".repeat(PAYLOAD_MAX_BYTES);
    for value in [
        serde_json::Value::Null,
        serde_json::json!(7),
        serde_json::json!("router_error"),
        serde_json::json!({"state": "unmeasured", "p": null}),
    ] {
        let out = truncate_payload(serde_json::json!({"result": big, "guard": value}));
        assert_eq!(out.get("guard"), Some(&value), "copied as-is, not normalised");
        assert!(out.get(DROPPED_PRESERVED_KEY).is_none());
    }
}

/// The admission boundary is exact, and it reserves the marker's room.
///
/// `truncate_payload` admits a key when the grown envelope plus
/// [`DROP_MARKER_RESERVE`] fits. Rather than hardcode the threshold —
/// which would be a second implementation of the thing under test, and
/// wrong the moment the `len` digit count changes — this walks a range
/// that straddles it and asserts the invariant on whichever side each
/// size lands, plus that the range really did contain both.
///
/// **What that does and does not catch.** Deleting the reserve turns
/// this red at the first admitted size with less than
/// [`DROP_MARKER_RESERVE`] bytes free. Reversing the comparator turns
/// it red via the budget assertion. Nudging `<=` to `<` does **not**:
/// the boundary moves by one and every assertion still holds on
/// whichever side each size lands. That is deliberate — a one-byte
/// shift in the conservative direction is not a defect worth pinning,
/// and pinning it would mean hardcoding the threshold this test exists
/// to avoid hardcoding.
#[test]
fn admission_reserves_room_for_the_drop_marker_on_both_sides() {
    let big = "r".repeat(PAYLOAD_MAX_BYTES);
    let (mut saw_admitted, mut saw_dropped) = (false, false);
    // The bare envelope is ~110 bytes, so the boundary sits just under
    // `PAYLOAD_MAX_BYTES - DROP_MARKER_RESERVE`. Straddle it widely
    // enough that the window cannot drift off the transition.
    let lo = PAYLOAD_MAX_BYTES.saturating_sub(DROP_MARKER_RESERVE).saturating_sub(300);
    for n in lo..(lo + 400) {
        let v = serde_json::json!({"result": big, "guard": "!".repeat(n)});
        let out = truncate_payload(v);
        let len = serde_json::to_vec(&out).expect("serialises").len();
        assert!(len <= PAYLOAD_MAX_BYTES, "n={n} produced {len} bytes");
        if out.get("guard").is_some() {
            saw_admitted = true;
            assert!(
                out.get(DROPPED_PRESERVED_KEY).is_none(),
                "n={n}: admitted and yet marked dropped"
            );
            assert!(
                len + DROP_MARKER_RESERVE <= PAYLOAD_MAX_BYTES,
                "n={n}: admitted with only {} bytes free, less than the reserve",
                PAYLOAD_MAX_BYTES - len
            );
        } else {
            saw_dropped = true;
            assert_eq!(
                out.get(DROPPED_PRESERVED_KEY),
                Some(&serde_json::json!(["guard"])),
                "n={n}: dropped without saying so"
            );
        }
    }
    assert!(saw_admitted && saw_dropped, "the window must straddle the boundary");
}

/// Every envelope this function can return is within budget.
///
/// The postcondition stated in the doc comment, asserted over both
/// arms rather than argued for in prose.
#[test]
fn every_envelope_fits_the_budget() {
    let cases = [
        serde_json::json!({"result": "a".repeat(PAYLOAD_MAX_BYTES)}),
        serde_json::json!({
            "result": "b".repeat(PAYLOAD_MAX_BYTES),
            "guard": {"state": "flagged", "p": 0.92, "tau": 0.79552656},
        }),
        serde_json::json!({"guard": {"x": "c".repeat(PAYLOAD_MAX_BYTES)}}),
        serde_json::Value::String("d".repeat(PAYLOAD_MAX_BYTES)),
        // ── The derived key (issue #617). ──
        //
        // Without these the one key this function adds itself was never
        // measured by the only test pinning the size postcondition, so a
        // `HEAD_MAX_BYTES` bump or a change to how `head` is escaped could
        // go green here.
        //
        // The second fixture is the expensive case: `head` is a prefix of
        // already-serialised JSON and is escaped AGAIN when stored as a
        // string, so a quote-dense request costs up to 2x its cap. An
        // all-ASCII argv does not exercise that.
        serde_json::json!({
            REQ_KEY: {"argv": ["/bin/bash", "-c", "e".repeat(PAYLOAD_MAX_BYTES * 10)]},
            "guard": {"state": "clear", "p": 0.5},
            "result": "f".repeat(PAYLOAD_MAX_BYTES),
        }),
        serde_json::json!({
            REQ_KEY: {"argv": vec!["\"".repeat(64); 200]},
            "guard": {"state": "clear", "p": 0.5},
            "result": "g".repeat(PAYLOAD_MAX_BYTES),
        }),
    ];
    for v in cases {
        let out = truncate_payload(v.clone());
        let n = serde_json::to_vec(&out).unwrap().len();
        // NOT `{v:.40}`: serde_json's `Display` streams straight through
        // `Formatter::write_str` and never consults `precision`, so the
        // width is silently ignored and a failure would dump the whole
        // 4 KiB fixture into the test output.
        let head: String = v.to_string().chars().take(40).collect();
        assert!(n <= PAYLOAD_MAX_BYTES, "envelope of {head} is {n} bytes");
    }
}

// ── The multi-key half of `preserve_onto`. ────────────────────────
//
// `PRESERVED_KEYS` has TWO members since issue #617, so every property
// below is structurally reachable through `truncate_payload` — what
// keeps the starvation arm from occurring in production is now sizing,
// not cardinality (a real envelope peaks near 1.4 KiB against a
// 4032-byte budget). They are the properties its doc comment claims,
// and the mutation that breaks them -- measuring each candidate against
// a FIXED envelope instead of the running one -- was invisible at N=1
// and produces an over-budget row at N=2. Tested through the
// parameterised helper so they are executed rather than argued, and
// with synthetic keys so they stay independent of how many members the
// production constant happens to have.

/// Build the bare envelope the way `truncate_payload` does, so these
/// tests measure the same starting size production does.
fn bare_envelope() -> serde_json::Map<String, serde_json::Value> {
    let mut m = serde_json::Map::new();
    m.insert(TRUNCATED_MARKER_KEY.to_string(), serde_json::Value::Bool(true));
    m.insert(FINGERPRINT_SHA256_KEY.to_string(), serde_json::json!("ab".repeat(32)));
    m.insert(FINGERPRINT_LEN_KEY.to_string(), serde_json::json!(85_352));
    m
}

fn source(pairs: &[(&str, serde_json::Value)]) -> serde_json::Map<String, serde_json::Value> {
    pairs.iter().map(|(k, v)| ((*k).to_string(), v.clone())).collect()
}

/// Two bounded keys both ride, and neither is named as dropped.
#[test]
fn preserve_onto_admits_every_key_that_fits() {
    let src = source(&[
        ("alpha", serde_json::json!({"state": "clear"})),
        ("beta", serde_json::json!(7)),
    ]);
    let out = preserve_onto(bare_envelope(), &src, &["alpha", "beta"]);
    assert_eq!(out.get("alpha"), Some(&serde_json::json!({"state": "clear"})));
    assert_eq!(out.get("beta"), Some(&serde_json::json!(7)));
    assert!(out.get(DROPPED_PRESERVED_KEY).is_none(), "nothing was lost: {out}");
}

/// **One oversized key must not take a bounded sibling down with it.**
///
/// This is `truncate_payload`'s headline claim for admitting keys one
/// at a time rather than all-or-nothing, and the only test that can
/// reach it. An all-or-nothing copy would drop `beta` too; a copy that
/// measured against a fixed `bare` would admit `alpha` and blow the
/// budget.
#[test]
fn preserve_onto_drops_only_the_key_that_does_not_fit() {
    let src = source(&[
        ("alpha", serde_json::json!("!".repeat(PAYLOAD_MAX_BYTES))),
        ("beta", serde_json::json!({"state": "clear", "p": 0.5})),
    ]);
    let out = preserve_onto(bare_envelope(), &src, &["alpha", "beta"]);
    assert!(out.get("alpha").is_none(), "the oversized key must not be stored");
    assert_eq!(
        out.get("beta"),
        Some(&serde_json::json!({"state": "clear", "p": 0.5})),
        "a bounded sibling must survive an oversized key"
    );
    assert_eq!(out.get(DROPPED_PRESERVED_KEY), Some(&serde_json::json!(["alpha"])));
    assert!(serde_json::to_vec(&out).unwrap().len() <= PAYLOAD_MAX_BYTES);
}

/// The drop marker names **every** key it lost, not just the first.
#[test]
fn preserve_onto_names_all_dropped_keys() {
    let huge = serde_json::json!("!".repeat(PAYLOAD_MAX_BYTES));
    let src = source(&[("alpha", huge.clone()), ("beta", huge)]);
    let out = preserve_onto(bare_envelope(), &src, &["alpha", "beta"]);
    assert_eq!(out.get(DROPPED_PRESERVED_KEY), Some(&serde_json::json!(["alpha", "beta"])));
    assert!(serde_json::to_vec(&out).unwrap().len() <= PAYLOAD_MAX_BYTES);
}

/// **Each candidate is measured against the RUNNING envelope.**
///
/// The mutation this pins: `serde_json::to_vec(&envelope)` measuring a
/// fixed starting envelope instead of the one already carrying
/// `alpha`. Two keys that each fit alone but not together would then
/// both be admitted and the row would land at roughly twice the cap --
/// breaking the one postcondition `truncate_payload` guarantees. Since
/// #617 gave `PRESERVED_KEYS` a second member this is a live shape rather
/// than a hypothetical one; `the_guard_record_is_admitted_before_the_req_summary`
/// exercises the same contention over the production constant.
#[test]
fn preserve_onto_measures_against_the_running_envelope() {
    // Each ~60% of the budget: either fits alone, together they cannot.
    let half = serde_json::json!("z".repeat(PAYLOAD_MAX_BYTES * 6 / 10));
    let src = source(&[("alpha", half.clone()), ("beta", half)]);

    // Sanity: each really does fit on its own, or the test proves nothing.
    for k in ["alpha", "beta"] {
        let solo = preserve_onto(bare_envelope(), &src, &[k]);
        assert!(solo.get(k).is_some(), "{k} must fit alone for this test to mean anything");
    }

    let out = preserve_onto(bare_envelope(), &src, &["alpha", "beta"]);
    let n = serde_json::to_vec(&out).unwrap().len();
    assert!(n <= PAYLOAD_MAX_BYTES, "two keys that fit alone produced {n} bytes together");
    assert_eq!(out.get(DROPPED_PRESERVED_KEY), Some(&serde_json::json!(["beta"])));
}

/// A key absent from `source` is skipped silently and is NOT a drop.
///
/// The marker means "the control ran and its verdict was discarded".
/// Reporting a key that was never there would make it mean nothing.
#[test]
fn preserve_onto_ignores_a_key_the_payload_never_carried() {
    let src = source(&[("alpha", serde_json::json!(1))]);
    let out = preserve_onto(bare_envelope(), &src, &["alpha", "absent"]);
    assert_eq!(out.get("alpha"), Some(&serde_json::json!(1)));
    assert!(out.get("absent").is_none());
    assert!(out.get(DROPPED_PRESERVED_KEY).is_none(), "absence is not loss: {out}");
}

/// A refused key leaves the envelope byte-for-byte as it found it.
///
/// `preserve_onto` measures by inserting and then taking back out. If
/// the take-back were wrong -- or clobbered a value it displaced --
/// the fingerprint could be corrupted by a key that was not even
/// stored. Unreachable via `PRESERVED_KEYS` (the compile-time block
/// forbids shadowing), reachable through the parameter.
#[test]
fn preserve_onto_restores_what_a_refused_key_displaced() {
    let src = source(&[(FINGERPRINT_LEN_KEY, serde_json::json!("!".repeat(PAYLOAD_MAX_BYTES)))]);
    let out = preserve_onto(bare_envelope(), &src, &[FINGERPRINT_LEN_KEY]);
    assert_eq!(
        out.get(FINGERPRINT_LEN_KEY),
        Some(&serde_json::json!(85_352)),
        "a refused key must not damage the value it displaced"
    );
    assert_eq!(out.get(DROPPED_PRESERVED_KEY), Some(&serde_json::json!([FINGERPRINT_LEN_KEY])));
}

/// Only TOP-LEVEL keys are promoted.
///
/// A recursive key search would look like a helpful refactor and would
/// promote arbitrary nested data past the cap under a preserved name,
/// defeating admission criterion 2 ("a decision record, not data").
#[test]
fn a_nested_preserved_key_is_not_promoted() {
    let v = serde_json::json!({
        "result": {(GUARD_KEY): {"state": "clear"}, "body": "q".repeat(PAYLOAD_MAX_BYTES)},
    });
    let out = truncate_payload(v);
    assert!(is_truncation_envelope(&out));
    assert!(out.get(GUARD_KEY).is_none(), "only a top-level key is a decision record");
    assert!(out.get(DROPPED_PRESERVED_KEY).is_none(), "it was never there to lose: {out}");
}

// ── The wire contract and the machinery that enforces it. ─────────

/// The marker's literal value is the wire contract, not the constant.
///
/// Every other reference to it in this crate and in `core` is
/// symbolic, so renaming the string would break every downstream `jq`
/// and SQL forensic query with a fully green suite. Same reason
/// `envelope_shape_is_pinned` spells `_truncated` out.
#[test]
fn wire_key_literals_are_pinned() {
    assert_eq!(DROPPED_PRESERVED_KEY, "_dropped_preserved");
    assert_eq!(GUARD_KEY, "guard");
    assert_eq!(REQ_SUMMARY_KEY, "req_summary");
    // Order is priority order, and `the_guard_record_is_admitted_before_the_
    // req_summary` proves what that costs the loser. Pinned here so a
    // re-ordering is a deliberate edit rather than a silent one.
    assert_eq!(PRESERVED_KEYS, ["guard", "req_summary"].as_slice());
}

/// `str_eq` is the sole enforcer of the compile-time shadow guard.
///
/// Mutate it to always return `false` and all of those assertions
/// become no-ops -- the guard silently stops guarding, with nothing in
/// the suite to notice. It is a hand-rolled comparator because
/// `PartialEq` is not const; hand-rolled comparators get tested.
#[test]
fn str_eq_matches_partial_eq() {
    for (a, b) in [
        ("guard", "guard"),
        ("guard", "guar"),
        ("guard", "guardx"),
        ("guard", "guarD"),
        ("", ""),
        ("", "x"),
        ("_truncated", "_dropped_preserved"),
    ] {
        assert_eq!(str_eq(a, b), a == b, "str_eq({a:?}, {b:?})");
    }
}

/// `drop_marker_worst_case()` really does bound the marker it describes.
///
/// The const block checks it against [`DROP_MARKER_RESERVE`], not
/// against reality -- so shrinking the estimate keeps compiling
/// (30 <= 64) while quietly ceasing to be a worst case. This measures
/// the real cost of the largest marker `PRESERVED_KEYS` can produce:
/// every member dropped at once.
#[test]
fn drop_marker_worst_case_bounds_the_real_marker() {
    let mut env = bare_envelope();
    let before = serde_json::to_vec(&serde_json::Value::Object(env.clone())).unwrap().len();
    env.insert(DROPPED_PRESERVED_KEY.to_string(), serde_json::json!(PRESERVED_KEYS));
    let after = serde_json::to_vec(&serde_json::Value::Object(env)).unwrap().len();
    let actual = after - before;
    assert!(
        actual <= drop_marker_worst_case(),
        "the marker really costs {actual} bytes but the estimate is {}",
        drop_marker_worst_case()
    );
    assert!(drop_marker_worst_case() <= DROP_MARKER_RESERVE);
}

/// Different inputs at the same length must produce different
/// fingerprints. Catches a silly mistake like hashing the *length*
/// instead of the bytes.
#[test]
fn truncate_fingerprint_distinguishes_different_payloads() {
    let a = serde_json::Value::String("a".repeat(PAYLOAD_MAX_BYTES + 50));
    let b = serde_json::Value::String("b".repeat(PAYLOAD_MAX_BYTES + 50));
    let oa = truncate_payload(a);
    let ob = truncate_payload(b);
    assert_ne!(
        oa.get("sha256"),
        ob.get("sha256"),
        "different bodies must produce different SHA-256s"
    );
}

// ── The bounded request summary (issue #617) ────────────────────────────

/// The common shape of the #617 loss, and the one that motivated the issue:
/// the argv is small, the command's *output* is what pushes the row over the
/// cap. Before this, such a row recorded the guard tier's opinion of the act
/// and a digest of the whole payload — and nothing whatever about the act.
#[test]
fn truncation_records_what_ran_when_the_result_is_the_bulk() {
    let payload = serde_json::json!({
        REQ_KEY: {"argv": ["/usr/bin/env", "bash", "-c", "make test"]},
        "result": {"stdout": "o".repeat(PAYLOAD_MAX_BYTES * 2)},
        "ms": 12,
    });

    let out = truncate_payload(payload);

    assert!(is_truncation_envelope(&out), "fixture must actually be over the cap");
    let head = out[REQ_SUMMARY_KEY]["head"].as_str().expect("summary head");
    assert!(head.contains("/usr/bin/env"), "the head must name the command, got: {head}");
    assert!(head.contains("make test"), "and its arguments, got: {head}");
}

/// The case the issue names explicitly: `req` *itself* is over the cap — a
/// generated script or a long heredoc. The head cannot hold the script, but
/// it still names the interpreter, and `len` says how much is not shown.
#[test]
fn truncation_records_the_interpreter_for_an_oversized_argv() {
    let payload = serde_json::json!({
        REQ_KEY: {"argv": ["/bin/bash", "-c", "x".repeat(PAYLOAD_MAX_BYTES * 10)]},
        "ms": 3,
    });

    let out = truncate_payload(payload);

    let summary = &out[REQ_SUMMARY_KEY];
    let head = summary["head"].as_str().expect("summary head");
    assert!(head.starts_with(r#"{"argv":["/bin/bash","-c","#), "got: {head}");
    assert!(
        summary["len"].as_u64().unwrap() > head.len() as u64,
        "an elided head must be detectable from the row itself"
    );
}

/// A write site with no request claims nothing about one. Silence and an
/// empty record must not render identically — the same rule
/// `DROPPED_PRESERVED_KEY` exists for one level up.
#[test]
fn a_payload_with_no_req_gets_no_summary() {
    let payload = serde_json::json!({"note": "z".repeat(PAYLOAD_MAX_BYTES * 2)});

    let out = truncate_payload(payload);

    assert!(is_truncation_envelope(&out));
    assert!(out.get(REQ_SUMMARY_KEY).is_none(), "nothing to summarise, nothing claimed");
}

/// Under the cap the whole request is already on the row, so a summary
/// would be duplicated noise. The under-cap path must still return the
/// payload *itself*, byte for byte.
#[test]
fn an_under_cap_payload_gains_no_summary() {
    let v = serde_json::json!({REQ_KEY: {"argv": ["/bin/true"]}, "ms": 1});

    assert_eq!(truncate_payload(v.clone()), v);
}

/// The envelope's `sha256` and `len` describe the **input**, so deriving a
/// summary must not move them — otherwise two rows for one body stop
/// comparing equal, which is exactly what the fingerprint is for.
///
/// This is the derived-key twin of
/// `a_preserved_key_does_not_change_the_fingerprint`, and it is the test
/// that pins the ordering inside `truncate_payload`: the fingerprint is
/// taken over the serialised input *before* the summary is inserted.
#[test]
fn the_req_summary_does_not_change_the_envelope_fingerprint() {
    let payload = serde_json::json!({
        REQ_KEY: {"argv": ["/bin/echo", "hi"]},
        "result": "r".repeat(PAYLOAD_MAX_BYTES * 2),
    });
    let raw = serde_json::to_vec(&payload).unwrap();
    let expected_len = raw.len();
    let expected_sha = super::req_summary::sha256_hex(&raw);

    let out = truncate_payload(payload);

    assert_eq!(out["len"].as_u64().unwrap() as usize, expected_len);
    assert_eq!(out["sha256"].as_str().unwrap(), expected_sha);
    assert!(out.get(REQ_SUMMARY_KEY).is_some(), "and the summary is still there");
}

/// Both preserved keys survive together. The guard record answers "what did
/// the control think", the summary answers "of what act" — a row with one
/// and not the other answers neither question completely.
#[test]
fn truncation_keeps_both_the_guard_record_and_the_req_summary() {
    let payload = serde_json::json!({
        REQ_KEY: {"argv": ["/bin/cat", "/etc/hosts"]},
        "result": "h".repeat(PAYLOAD_MAX_BYTES * 2),
        (GUARD_KEY): {"state": "clear", "p": 0.11, "tau": 0.79552656},
    });

    let out = truncate_payload(payload);

    assert_eq!(out[GUARD_KEY]["state"], "clear");
    assert!(out[REQ_SUMMARY_KEY]["head"].as_str().unwrap().contains("/bin/cat"));
    assert!(out.get(DROPPED_PRESERVED_KEY).is_none(), "both fit, so nothing is named");
}

/// `PRESERVED_KEYS` is **priority order** — keys are admitted left to right
/// against a shared budget, so an earlier member can starve a later one.
/// That was moot at one member and is live at two.
///
/// The guard score wins: it is a handful of scalars, it is irrecoverable
/// (a cleared document writes no second row), and losing it was the measured
/// live defect that created `PRESERVED_KEYS` in the first place. The summary
/// is the larger of the two and the one that must yield — and yielding, it
/// is still *named*.
///
/// ⚠️ **The fixture creates real contention, and must keep doing so.** An
/// earlier version gave the summary a `PAYLOAD_MAX_BYTES`-sized head, which
/// does not fit in *either* slot — so the guard was admitted and the summary
/// dropped under both orders, and the test passed with the array reversed.
/// It was asserting `preserve_onto_drops_only_the_key_that_does_not_fit`
/// over again while its docstring claimed to assert the order. Here each
/// candidate fits **alone** and the two together do not, which is the only
/// shape in which "first wins" is observable.
#[test]
fn the_guard_record_is_admitted_before_the_req_summary() {
    // Each ~60 % of the budget: either fits alone, together they cannot —
    // the same sizing `preserve_onto_measures_against_the_running_envelope`
    // uses. 45 % each would leave BOTH fitting and quietly restore the
    // vacuity this test exists to remove.
    let half = PAYLOAD_MAX_BYTES * 6 / 10;
    let src = source(&[
        (GUARD_KEY, serde_json::json!({"state": "clear", "g": "g".repeat(half)})),
        (REQ_SUMMARY_KEY, serde_json::json!({"head": "x".repeat(half)})),
    ]);

    // Precondition: this is the contention the test is about. Without it
    // the assertions below hold under either order and prove nothing.
    let guard_only = preserve_onto(bare_envelope(), &src, &[GUARD_KEY]);
    let summary_only = preserve_onto(bare_envelope(), &src, &[REQ_SUMMARY_KEY]);
    assert!(guard_only.get(DROPPED_PRESERVED_KEY).is_none(), "guard must fit alone");
    assert!(summary_only.get(DROPPED_PRESERVED_KEY).is_none(), "summary must fit alone");

    let out = preserve_onto(bare_envelope(), &src, PRESERVED_KEYS);

    assert_eq!(out[GUARD_KEY]["state"], "clear", "the guard score must never be starved");
    assert_eq!(out[DROPPED_PRESERVED_KEY], serde_json::json!([REQ_SUMMARY_KEY]));
}

/// The same contention with the array reversed puts the *summary* through
/// and drops the guard — which is what makes the test above an assertion
/// about order rather than about size.
///
/// This is the control the original pair lacked: it fails if `preserve_onto`
/// ever stops being left-to-right, and together with the test above it means
/// a reversal of `PRESERVED_KEYS` cannot pass both.
#[test]
fn reversing_the_priority_order_reverses_who_survives() {
    let half = PAYLOAD_MAX_BYTES * 6 / 10;
    let src = source(&[
        (GUARD_KEY, serde_json::json!({"state": "clear", "g": "g".repeat(half)})),
        (REQ_SUMMARY_KEY, serde_json::json!({"head": "x".repeat(half)})),
    ]);

    let out = preserve_onto(bare_envelope(), &src, &[REQ_SUMMARY_KEY, GUARD_KEY]);

    assert!(out.get(REQ_SUMMARY_KEY).is_some(), "the first member survives");
    assert_eq!(out[DROPPED_PRESERVED_KEY], serde_json::json!([GUARD_KEY]));
}

/// A summary the producer wrote itself does not win over the derived one.
/// There is exactly one producer of this rule by design (see the module
/// docs), and a payload that arrived carrying the key must not be able to
/// put a different answer on the row than the one the rule computes.
#[test]
fn a_producer_supplied_summary_is_replaced_by_the_derived_one() {
    let payload = serde_json::json!({
        REQ_KEY: {"argv": ["/bin/real"]},
        REQ_SUMMARY_KEY: {"head": "{\"argv\":[\"/bin/fake\"]}", "sha256": "0".repeat(64), "len": 1},
        "result": "r".repeat(PAYLOAD_MAX_BYTES * 2),
    });

    let out = truncate_payload(payload);

    let head = out[REQ_SUMMARY_KEY]["head"].as_str().unwrap();
    assert!(head.contains("/bin/real"), "the derived summary wins, got: {head}");
    assert!(!head.contains("/bin/fake"));
}

/// ...and it does not survive by the payload simply omitting `req`.
///
/// This is the escape the conditional overwrite left open: with no `req`
/// there was nothing to derive, so nothing overwrote the supplied key, and
/// `preserve_onto` copied the payload's own forged answer onto the envelope
/// verbatim — a row asserting a request it never carried, indistinguishable
/// from a derived summary. The key is now cleared unconditionally before
/// the derivation runs, so "no request" means "no summary".
#[test]
fn a_supplied_summary_does_not_survive_a_payload_with_no_req() {
    let payload = serde_json::json!({
        REQ_SUMMARY_KEY: {"head": "{\"argv\":[\"/bin/fake\"]}", "sha256": "0".repeat(64), "len": 1},
        "result": "r".repeat(PAYLOAD_MAX_BYTES * 2),
    });

    let out = truncate_payload(payload);

    assert!(is_truncation_envelope(&out));
    assert!(
        out.get(REQ_SUMMARY_KEY).is_none(),
        "no request means no summary, whatever the payload claimed: {out}"
    );
    assert!(
        out.get(DROPPED_PRESERVED_KEY).is_none(),
        "a forged key is removed, not reported as a key that would not fit"
    );
}
