//! Unit tests for [`super`] — the bounded request summary (issue #617).

use super::*;
use serde_json::json;

// ── char_boundary_prefix ────────────────────────────────────────────────

/// A string already within the cap comes back untouched, cap exactly equal
/// included — the bound is inclusive, like the payload cap it serves.
#[test]
fn char_boundary_prefix_returns_the_whole_string_when_it_fits() {
    assert_eq!(char_boundary_prefix("abc", 3), "abc");
    assert_eq!(char_boundary_prefix("abc", 100), "abc");
}

/// When the cap lands on a boundary the walk has nothing to do and the cut
/// is exactly at the cap.
#[test]
fn char_boundary_prefix_cuts_at_the_cap_when_the_cap_is_a_boundary() {
    assert_eq!(char_boundary_prefix("abcdef", 3), "abc");
}

/// The case issue #591 records as the one every duplicate of this idiom got
/// wrong: the copy that shipped used a 2-byte character against an even cap,
/// so the cap always landed on a boundary, the decrementing branch never ran,
/// and deleting the walk entirely left the suite green.
///
/// `好` is 3 bytes and the cap lands one byte inside it, so walking back is
/// the only way to produce an answer at all — without it, `&s[..cap]` panics.
#[test]
fn char_boundary_prefix_walks_back_off_a_straddling_multibyte_char() {
    const CAP: usize = 10;
    let s = format!("{}好", "a".repeat(CAP - 1));
    assert_eq!(s.len(), CAP - 1 + 3, "fixture must actually straddle the cap");

    let out = char_boundary_prefix(&s, CAP);

    assert_eq!(out, "a".repeat(CAP - 1), "must not slice into the character");
    assert!(out.len() < CAP, "walking back necessarily gives up bytes");
}

/// The production cap is not accidentally exempt from the walk.
///
/// The fixture is built **relative to the cap** — `cap - 2` filler then a
/// three-byte character — so byte `cap` is that character's third byte for
/// any `cap >= 2`. The straddle therefore holds whatever `HEAD_MAX_BYTES`
/// becomes, and this test keeps meaning the same thing after a cap change.
///
/// It does **not** depend on 512 not being a multiple of 3, and an earlier
/// version of this comment claiming otherwise had it backwards: a
/// cap-relative fixture straddles regardless of divisibility. Divisibility
/// matters only for a fixture built as a uniform repeat from offset 0,
/// which is `a_multibyte_request_head_is_never_cut_mid_character` below —
/// that is where the `512 = 3·170 + 2` argument belongs and is correct.
///
/// No `is_char_boundary` guard here: it could never fire, so it was a
/// positive control that controlled nothing. The construction is the
/// guarantee, and the length assertion is what would catch a wrong walk.
#[test]
fn char_boundary_prefix_walks_back_at_the_production_cap() {
    let s = format!("{}好", "a".repeat(HEAD_MAX_BYTES - 2));

    assert_eq!(char_boundary_prefix(&s, HEAD_MAX_BYTES).len(), HEAD_MAX_BYTES - 2);
}

/// Index 0 is always a boundary, so a zero cap terminates immediately with
/// an empty prefix rather than underflowing.
#[test]
fn char_boundary_prefix_of_zero_is_empty() {
    assert_eq!(char_boundary_prefix("好", 0), "");
}

// ── summarize_req ───────────────────────────────────────────────────────

/// A payload that never carried a request claims nothing about one. This is
/// what keeps "no request was recorded" distinct from "the request was
/// summarised", which is the same distinction `DROPPED_PRESERVED_KEY` draws
/// one level up.
#[test]
fn a_payload_with_no_req_has_no_summary() {
    assert!(summarize_req(&json!({"result": "x", "ms": 3})).is_none());
}

/// A payload that is not an object has no keys at all, so there is nothing
/// to look up and nothing to claim.
#[test]
fn a_non_object_payload_has_no_summary() {
    assert!(summarize_req(&json!("just a string")).is_none());
    assert!(summarize_req(&json!([1, 2, 3])).is_none());
}

/// A request small enough to fit survives whole, and `len` equal to the
/// head's byte length is how a reader knows that.
#[test]
fn a_small_request_survives_whole() {
    let req = json!({"argv": ["/usr/bin/env", "true"]});

    let summary = summarize_req(&json!({"req": req.clone(), "result": "x"})).expect("req present");

    let head = summary["head"].as_str().expect("head is a string");
    assert_eq!(head, serde_json::to_string(&req).unwrap());
    assert_eq!(
        summary["len"].as_u64().unwrap() as usize,
        head.len(),
        "len == head length is the reader's signal that nothing was cut"
    );
}

/// The case issue #617 names: the request itself is over the cap. The head
/// cannot hold the script, but it still names the interpreter — and `len`
/// says how much is not being shown.
#[test]
fn an_oversized_request_keeps_a_bounded_head_that_still_names_the_command() {
    let req = json!({"argv": ["/bin/bash", "-c", "s".repeat(40_000)]});
    let full = serde_json::to_string(&req).unwrap();

    let summary = summarize_req(&json!({"req": req})).expect("req present");

    let head = summary["head"].as_str().expect("head is a string");
    assert!(head.len() <= HEAD_MAX_BYTES, "head is bounded by construction");
    assert!(
        head.starts_with(r#"{"argv":["/bin/bash","-c","#),
        "the head must still name what ran, got: {head}"
    );
    assert_eq!(summary["len"].as_u64().unwrap() as usize, full.len());
    assert!(
        summary["len"].as_u64().unwrap() as usize > head.len(),
        "a cut head must be detectable from the record itself"
    );

    // The digest covers the WHOLE request, not the head. Every other
    // digest assertion in this file uses a request small enough that
    // `head == text`, so all of them pass unchanged if the digest is taken
    // over the head instead — a mutation that leaves the suite green while
    // destroying the one property the field exists for.
    assert_eq!(summary["sha256"].as_str().unwrap(), sha256_hex(full.as_bytes()));
    assert_ne!(
        summary["sha256"].as_str().unwrap(),
        sha256_hex(head.as_bytes()),
        "digesting the head instead of the request must not go unnoticed"
    );
}

/// Two over-cap requests that share a head must not share a digest.
///
/// This is the case the `sha256` field is *for*: an agent-generated
/// `bash -c` heredoc is identical for hundreds of bytes and differs deep in
/// the body, so a digest taken over the 512-byte head would collide for two
/// genuinely different commands — and two rows that must be distinguishable
/// would silently compare equal. The whole-request assertion above pins the
/// value; this pins the consequence.
#[test]
fn two_over_cap_requests_sharing_a_head_do_not_share_a_digest() {
    let shared = "s".repeat(40_000);
    let a = json!({"argv": ["/bin/bash", "-c", format!("{shared}A")]});
    let b = json!({"argv": ["/bin/bash", "-c", format!("{shared}B")]});

    let sa = summarize_req(&json!({"req": a})).unwrap();
    let sb = summarize_req(&json!({"req": b})).unwrap();

    assert_eq!(sa["head"], sb["head"], "the fixture must actually share a head");
    assert_ne!(
        sa["sha256"], sb["sha256"],
        "different requests must not collide just because their heads match"
    );
}

/// A request whose serialisation lands exactly on the cap is not cut, and
/// says so: `len` equal to the head's byte length is the reader's signal
/// that nothing was elided. One byte more and the signal flips.
#[test]
fn the_len_signal_flips_exactly_at_the_cap() {
    // `{"a":"<pad>"}` is 8 bytes of envelope plus the padding.
    let exact = json!({"a": "p".repeat(HEAD_MAX_BYTES - 8)});
    let text = serde_json::to_string(&exact).unwrap();
    assert_eq!(text.len(), HEAD_MAX_BYTES, "fixture must sit exactly on the cap");

    let at = summarize_req(&json!({"req": exact})).unwrap();
    assert_eq!(at["len"].as_u64().unwrap() as usize, HEAD_MAX_BYTES);
    assert_eq!(at["head"].as_str().unwrap().len(), HEAD_MAX_BYTES, "nothing cut at the cap");

    let over = json!({"a": "p".repeat(HEAD_MAX_BYTES - 7)});
    let so = summarize_req(&json!({"req": over})).unwrap();
    assert_eq!(so["len"].as_u64().unwrap() as usize, HEAD_MAX_BYTES + 1);
    assert!(
        (so["len"].as_u64().unwrap() as usize) > so["head"].as_str().unwrap().len(),
        "one byte over the cap must be detectable"
    );
}

/// The digest describes the request, not the payload the request sat in.
/// Without that, two rows for the same command would fail to compare
/// whenever their outputs differed — which is every interesting case.
#[test]
fn the_summary_digests_the_request_not_the_payload() {
    let req = json!({"argv": ["/bin/echo", "hi"]});

    let a = summarize_req(&json!({"req": req.clone(), "result": "one"})).unwrap();
    let b = summarize_req(&json!({"req": req, "result": "two"})).unwrap();
    let c = summarize_req(&json!({"req": {"argv": ["/bin/echo", "bye"]}})).unwrap();

    assert_eq!(a["sha256"], b["sha256"], "same request, same digest, whatever else changed");
    assert_ne!(a["sha256"], c["sha256"], "a different request must not collide");
}

/// Pinned against an independent computation, so a change to *what* is
/// digested — the value versus its serialisation, say — fails here rather
/// than silently making rows written before and after incomparable.
#[test]
fn the_summary_digest_is_the_sha256_of_the_serialised_request() {
    use sha2::Digest;
    let req = json!({"argv": ["/bin/echo", "hi"]});
    let expected: String = sha2::Sha256::digest(serde_json::to_vec(&req).unwrap())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();

    let summary = summarize_req(&json!({"req": req})).unwrap();

    assert_eq!(summary["sha256"].as_str().unwrap(), expected);
}

/// A request made of three-byte characters guarantees the cap lands inside
/// one. The real assertion is that this does not panic; the length pins
/// *where* the walk stopped so a silently wrong walk is caught too.
///
/// The key is one character long on purpose: `{"c":"` is 6 bytes, and
/// 512 − 6 = 506 = 3·168 + 2, so the cap straddles. A longer key could make
/// the offset divide evenly and quietly disarm the test.
#[test]
fn a_multibyte_request_head_is_never_cut_mid_character() {
    let summary = summarize_req(&json!({"req": {"c": "好".repeat(1_000)}})).unwrap();

    let head = summary["head"].as_str().unwrap();
    assert_eq!(head.len(), 6 + 3 * 168, "cut at the last whole character before the cap");
    assert!(head.ends_with('好'));
}

/// A request that is present but `null` is summarised rather than skipped:
/// this judges the key, not the shape, exactly as preserved-key handling
/// does one level up. "The producer wrote a null request" is a fact worth
/// keeping, and it is not the same fact as "there was no request key".
#[test]
fn a_null_request_is_summarised_rather_than_skipped() {
    let summary = summarize_req(&json!({"req": serde_json::Value::Null})).expect("key present");

    assert_eq!(summary["head"].as_str().unwrap(), "null");
    assert_eq!(summary["len"].as_u64().unwrap(), 4);
}

/// Same input, same output, every call — the property the whole audit
/// fingerprint story rests on.
#[test]
fn summarize_req_is_deterministic() {
    let payload = json!({"req": {"argv": ["/bin/ls", "-la"]}, "result": "x".repeat(9_000)});

    assert_eq!(summarize_req(&payload), summarize_req(&payload));
}

/// The summary's own sub-keys are pinned, because readers outside this
/// crate index them by literal.
#[test]
fn summary_sub_key_literals_are_pinned() {
    assert_eq!(REQ_KEY, "req");
    assert_eq!(REQ_SUMMARY_KEY, "req_summary");
    assert_eq!(HEAD_KEY, "head");
    assert_eq!(SHA256_KEY, "sha256");
    assert_eq!(LEN_KEY, "len");
}
