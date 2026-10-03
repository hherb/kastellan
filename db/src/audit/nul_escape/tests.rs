//! Unit tests for [`super`] — the NUL escape — and for its place at the head
//! of [`crate::audit::truncate_payload`]. The live-cluster proof that the
//! escaped row actually lands is `db/tests/audit_nul_e2e.rs`.

use serde_json::json;

use super::*;
use crate::audit::{
    is_truncation_envelope, truncate_payload, PAYLOAD_MAX_BYTES, PRESERVED_KEYS, REQ_SUMMARY_KEY,
};

/// `␀`, spelled once so a fixture cannot drift from the constant.
const E: char = NUL_ESCAPE;

/// True when no string or key anywhere in `v` holds a NUL. The serialised
/// form is the right place to look: `serde_json` writes a NUL as `\u0000`,
/// which is exactly what Postgres refuses.
fn nul_free(v: &Value) -> bool {
    !serde_json::to_string(v).unwrap().contains("\\u0000")
}

/// [`escape_str`] with its result owned, so fixtures compare plain strings.
fn esc(s: &str) -> (String, u64) {
    let (out, n) = escape_str(s);
    (out.into_owned(), n)
}

/// A clean string is BORROWED, not copied: every string in every audit
/// payload passes through here, and nearly all of them are clean.
#[test]
fn a_clean_string_is_borrowed_with_a_zero_count() {
    let (out, n) = escape_str("matrix");
    assert!(matches!(out, std::borrow::Cow::Borrowed("matrix")), "{out:?}");
    assert_eq!(n, 0);
    assert_eq!(esc(""), (String::new(), 0));
}

/// Replaced, never deleted: deleting would join `a` and `b` into a token the
/// worker never sent. Adjacent NULs stay distinguishable from one.
#[test]
fn every_nul_is_replaced_by_the_glyph_and_counted() {
    assert_eq!(esc("a\0b"), (format!("a{E}b"), 1));
    assert_eq!(esc("\0\0x\0"), (format!("{E}{E}x{E}"), 3));
    // Multi-byte neighbours survive: a NUL is never part of a longer
    // UTF-8 sequence, so a byte count is a character count.
    assert_eq!(esc("é\0中"), (format!("é{E}中"), 1));
}

/// The glyph is three bytes of UTF-8 and outside every range `serde_json`
/// escapes, so it is stored as itself, and a reader sees `␀`.
#[test]
fn the_glyph_serialises_raw_not_as_another_escape() {
    assert_eq!(NUL_ESCAPE, '\u{2400}');
    assert_eq!(serde_json::to_string(&json!(NUL_ESCAPE.to_string())).unwrap(), "\"\u{2400}\"");
}

/// A payload with nothing to escape must reach the cap byte-for-byte, so
/// every existing row is stored exactly as before #816.
#[test]
fn a_clean_payload_is_unchanged_and_gets_no_marker() {
    let v = json!({"channel": "matrix", "n": 3, "ok": true, "x": null, "list": ["a", {"b": "c"}]});
    assert_eq!(escape_payload(v.clone()), v);
}

#[test]
fn nested_strings_and_keys_are_escaped_and_counted_together() {
    let out = escape_payload(json!({
        "peer": "@m\0:srv",
        "result": {"k\0ey": ["a\0\0b", 7, {"deep": "\0"}]},
    }));
    assert_eq!(
        out,
        json!({
            "peer": format!("@m{E}:srv"),
            "result": {format!("k{E}ey"): [format!("a{E}{E}b"), 7, {"deep": E.to_string()}]},
            NUL_ESCAPED_KEY: 5,
        })
    );
    assert!(nul_free(&out));
}

/// The collision a hostile worker can arrange: `k\0` escapes onto an existing
/// `k␀`. A plain re-insert would keep one value and lose the other with no
/// trace. Both must survive, and the key the worker spelled without a NUL
/// keeps its spelling.
#[test]
fn an_escaped_key_never_overwrites_an_existing_one() {
    let k_glyph = format!("k{E}");
    let out = escape_payload(json!({"k\0": "evil", (k_glyph.clone()): "benign"}));
    assert_eq!(out[&k_glyph], json!("benign"), "the NUL-free key keeps its slot: {out}");
    assert_eq!(out[format!("k{E}{E}")], json!("evil"), "the escaped key moves aside: {out}");
    // One NUL was sent; the appended disambiguating glyph replaced nothing.
    assert_eq!(out[NUL_ESCAPED_KEY], json!(1));
}

/// Several keys contending for the same escaped spelling all survive, each
/// pushed one glyph further along.
#[test]
fn a_chain_of_collisions_still_overwrites_nothing() {
    let out = escape_payload(json!({
        "k\0": 1,
        (format!("k{E}")): 2,
        (format!("k{E}{E}")): 3,
    }));
    let obj = out.as_object().unwrap();
    // Three values plus the marker: nothing was lost.
    assert_eq!(obj.len(), 4, "{out}");
    assert_eq!(out[format!("k{E}")], json!(2));
    assert_eq!(out[format!("k{E}{E}")], json!(3));
    assert_eq!(out[format!("k{E}{E}{E}")], json!(1));
}

/// Two NUL-bearing keys can escape to the same spelling with no NUL-free key
/// involved: `a\0␀` and `a␀\0` both become `a␀␀`. The second must not land
/// on the first.
#[test]
fn two_nul_keys_escaping_alike_both_survive() {
    let out = escape_payload(json!({
        (format!("a\0{E}")): 1,
        (format!("a{E}\0")): 2,
    }));
    let obj = out.as_object().unwrap();
    assert_eq!(obj.len(), 3, "two values and the marker: {out}");
    let mut landed = vec![out[format!("a{E}{E}")].clone(), out[format!("a{E}{E}{E}")].clone()];
    landed.sort_by_key(|v| v.as_i64());
    assert_eq!(landed, vec![json!(1), json!(2)], "{out}");
    assert_eq!(out[NUL_ESCAPED_KEY], json!(2));
    assert!(nul_free(&out));
}

/// When the payload holds NULs, the count is the escape's own: a marker the
/// payload arrived with is overwritten by the true number, never added to.
#[test]
fn a_payload_with_nuls_gets_the_true_count_whatever_it_carried() {
    assert_eq!(
        escape_payload(json!({NUL_ESCAPED_KEY: 99, "a": "\0"})),
        json!({"a": E.to_string(), NUL_ESCAPED_KEY: 1})
    );
}

/// With no NUL to escape the payload is left alone — marker included. That
/// is what makes the escape idempotent (see the `truncate_payload` test
/// below), and it costs nothing a worker can use: top-level payload keys are
/// spelled by core's payload builders, and worker data sits beneath them.
#[test]
fn a_payload_without_nuls_keeps_whatever_marker_it_carried() {
    let v = json!({NUL_ESCAPED_KEY: 2, "a": format!("x{E}{E}")});
    assert_eq!(escape_payload(v.clone()), v);
}

/// No object, nowhere to put a count — but the row must still land.
#[test]
fn a_non_object_payload_is_escaped_without_a_marker() {
    assert_eq!(escape_payload(json!("a\0")), json!(format!("a{E}")));
    assert_eq!(escape_payload(json!(["\0", 1])), json!([E.to_string(), 1]));
}

// ── Its place in `truncate_payload` ────────────────────────────────────────

/// Under the cap, `truncate_payload` is now the escape — and remains the one
/// function that says what `audit::insert` will store, which is how
/// `core::channel::skipped` and the dispatcher tests use it.
#[test]
fn truncate_payload_escapes_a_payload_under_the_cap() {
    let out = truncate_payload(json!({"peer": "@m\0:srv"}));
    assert_eq!(out, json!({"peer": format!("@m{E}:srv"), NUL_ESCAPED_KEY: 1}));
}

/// Over the cap, the count is a [`PRESERVED_KEYS`] member, so it outlives the
/// body; and the request summary's head is cut from the ESCAPED request, so
/// no NUL reaches the envelope that way either.
#[test]
fn over_the_cap_the_count_survives_and_the_summary_head_is_escaped() {
    assert!(PRESERVED_KEYS.contains(&NUL_ESCAPED_KEY));
    let out = truncate_payload(json!({
        "req": {"argv": ["echo", "x\0y"]},
        "result": "z".repeat(PAYLOAD_MAX_BYTES),
    }));
    assert!(is_truncation_envelope(&out), "{out}");
    assert_eq!(out[NUL_ESCAPED_KEY], json!(1), "{out}");
    assert!(out.get(REQ_SUMMARY_KEY).is_some(), "a req_summary was derived: {out}");
    assert!(nul_free(&out), "no NUL may reach the envelope: {out}");
}

/// The cap measures the stored form. A NUL serialises as `\u0000` (6 bytes)
/// and `␀` as 3, so a payload just over the cap only because of its NULs
/// now fits — measured AFTER the escape, as Postgres will see it.
#[test]
fn the_cap_is_measured_on_the_escaped_payload() {
    // `{"a":"<s>"}` costs 8 bytes of framing; the marker adds
    // `,"_nul_escaped":N` once escaped. Size the body so the RAW form is over
    // the cap and the escaped form is under it.
    let nuls = 200;
    let framing = r#"{"a":""}"#.len();
    let filler = PAYLOAD_MAX_BYTES - framing - nuls * 6 + 1; // raw = cap + 1
    let s = format!("{}{}", "\0".repeat(nuls), "y".repeat(filler));
    let raw = json!({"a": s});
    assert_eq!(serde_json::to_vec(&raw).unwrap().len(), PAYLOAD_MAX_BYTES + 1);

    let out = truncate_payload(raw);
    assert!(!is_truncation_envelope(&out), "the stored form fits, so it is not truncated");
    assert_eq!(out[NUL_ESCAPED_KEY], json!(nuls));
    assert!(serde_json::to_vec(&out).unwrap().len() <= PAYLOAD_MAX_BYTES);
}

/// `truncate_payload(truncate_payload(x)) == truncate_payload(x)`.
///
/// Production applies it TWICE on the tool path:
/// `core::tool_host::audit_sink::AuditSink::insert` computes the stored form
/// so a test double sees it, and `PgAuditSink` then calls `audit::insert`,
/// which applies it again. The first draft of #816 broke this: the second
/// pass found no NUL, stripped the marker the first pass wrote, and stored
/// `␀` with nothing to say it had been a NUL — on the path that carries
/// almost every worker-written string. Each fixture is one shape the
/// escape treats differently.
#[test]
fn truncate_payload_is_idempotent_with_nuls_in_play() {
    let fixtures = [
        json!({"result": "a\0b"}),
        json!({"k\0": 1, (format!("k{E}")): 2}),
        json!({"req": {"argv": ["x\0"]}, "result": "z".repeat(PAYLOAD_MAX_BYTES)}),
        json!({NUL_ESCAPED_KEY: 7, "a": "\0"}),
        json!({"plain": true}),
        json!("bare\0string"),
    ];
    for x in fixtures {
        let once = truncate_payload(x.clone());
        let twice = truncate_payload(once.clone());
        assert_eq!(twice, once, "not idempotent for {x}");
    }
}
