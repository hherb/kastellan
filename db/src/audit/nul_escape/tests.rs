//! Unit tests for [`super`] — the NUL escape — and for its place at the head
//! of [`crate::audit::truncate_payload`]. The live-cluster proof that the
//! escaped row actually lands is `db/tests/audit_nul_e2e.rs`.

use serde_json::json;

use super::*;
use crate::audit::{
    is_truncation_envelope, stored_form, truncate_payload, DROPPED_PRESERVED_KEY, GUARD_KEY,
    PAYLOAD_MAX_BYTES, PRESERVED_KEYS, REQ_KEY, REQ_SUMMARY_KEY,
};

/// `␀`, spelled once so a fixture cannot drift from the constant.
const E: char = NUL_ESCAPE;

/// True when no string or key anywhere in `v` holds a NUL.
///
/// A walk, not a substring search of the serialisation: a string holding
/// the six characters `\u0000` serialises as `\\u0000`, which contains
/// `\u0000`, so a search would report a NUL that is not there.
fn nul_free(v: &Value) -> bool {
    match v {
        Value::String(s) => !s.contains('\0'),
        Value::Array(items) => items.iter().all(nul_free),
        Value::Object(map) => map.iter().all(|(k, v)| !k.contains('\0') && nul_free(v)),
        Value::Null | Value::Bool(_) | Value::Number(_) => true,
    }
}

/// The helper itself: it must see a NUL in a key and deep in a value, and
/// must NOT see one in the literal text `\u0000`.
#[test]
fn nul_free_finds_real_nuls_only() {
    assert!(!nul_free(&json!({"k\0": 1})));
    assert!(!nul_free(&json!({"a": [{"b": "x\0"}]})));
    assert!(nul_free(&json!({"a": "\\u0000"})));
    assert!(nul_free(&json!({"a": format!("x{E}")})));
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
    // The array holds three items and four NULs, so an array arm that
    // counted items instead of summing NULs gets a different answer.
    let out = escape_payload(json!({
        "peer": "@m\0:srv",
        "result": {"k\0ey": ["a\0\0b", 7, {"deep": "\0\0"}]},
    }));
    assert_eq!(
        out,
        json!({
            "peer": format!("@m{E}:srv"),
            "result": {format!("k{E}ey"): [format!("a{E}{E}b"), 7, {"deep": format!("{E}{E}")}]},
            NUL_ESCAPED_KEY: 6,
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
/// below). It is safe only while core spells every top-level payload key;
/// see [`NUL_ESCAPED_KEY`].
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
/// body. The request summary's head is cut from the ESCAPED request, so an
/// operator reading it sees `␀` — the same spelling as everywhere else in
/// the row — rather than the `\u0000` text the raw serialisation holds.
///
/// (The head was never a route for a real NUL: it is cut from the request's
/// *serialisation*, where a NUL is already six ASCII characters.)
#[test]
fn over_the_cap_the_count_survives_and_the_summary_head_shows_the_glyph() {
    assert!(PRESERVED_KEYS.contains(&NUL_ESCAPED_KEY));
    let out = truncate_payload(json!({
        REQ_KEY: {"argv": ["echo", "x\0y"]},
        "result": "z".repeat(PAYLOAD_MAX_BYTES),
    }));
    assert!(is_truncation_envelope(&out), "{out}");
    assert_eq!(out[NUL_ESCAPED_KEY], json!(1), "{out}");
    let head = out[REQ_SUMMARY_KEY]["head"].as_str().expect("a req_summary head was derived");
    assert!(head.contains(&format!("x{E}y")), "the head shows the glyph: {head}");
    assert!(!head.contains("\\u0000"), "the head never shows the raw escape: {head}");
    assert!(nul_free(&out), "{out}");
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

/// The other direction: one NUL saves 3 bytes, but the marker costs ~17, so
/// a payload just UNDER the cap raw can be over it escaped. It must then be
/// enveloped like any other — and the envelope must fit and keep the count.
#[test]
fn a_payload_the_escape_pushes_over_the_cap_is_enveloped() {
    let framing = r#"{"a":""}"#.len();
    let filler = PAYLOAD_MAX_BYTES - 5 - framing - 6; // raw = cap - 5
    let raw = json!({"a": format!("\0{}", "y".repeat(filler))});
    assert_eq!(serde_json::to_vec(&raw).unwrap().len(), PAYLOAD_MAX_BYTES - 5);

    let out = truncate_payload(raw);
    assert!(is_truncation_envelope(&out), "the escaped form is over the cap: {out}");
    assert_eq!(out[NUL_ESCAPED_KEY], json!(1), "{out}");
    assert!(serde_json::to_vec(&out).unwrap().len() <= PAYLOAD_MAX_BYTES);
}

/// The count is the LAST preserved key, so the first a crowded envelope
/// would starve. The worst envelope in `every_envelope_fits_the_budget` —
/// a quote-dense request (its head costs up to 2x its cap) plus a guard
/// record — must still afford it.
#[test]
fn the_count_survives_the_most_crowded_envelope() {
    let out = truncate_payload(json!({
        REQ_KEY: {"argv": vec!["\"".repeat(64); 200]},
        GUARD_KEY: {"state": "clear", "p": 0.5},
        "result": format!("\0{}", "g".repeat(PAYLOAD_MAX_BYTES)),
    }));
    assert!(is_truncation_envelope(&out), "{out}");
    assert!(out.get(REQ_SUMMARY_KEY).is_some() && out.get(GUARD_KEY).is_some(), "{out}");
    assert_eq!(out[NUL_ESCAPED_KEY], json!(1), "{out}");
    assert!(out.get(DROPPED_PRESERVED_KEY).is_none(), "nothing was starved: {out}");
    assert!(serde_json::to_vec(&out).unwrap().len() <= PAYLOAD_MAX_BYTES);
}

/// A NUL body and the same body spelled with a literal `␀` escape to the
/// same text. For an object the count keeps their fingerprints apart; a bare
/// string has nowhere to put one, and collides — the documented limit.
#[test]
fn the_count_keeps_an_object_fingerprint_apart_from_a_literal_glyph() {
    let body = "z".repeat(PAYLOAD_MAX_BYTES);
    let nul = truncate_payload(json!({"a": format!("\0{body}")}));
    let glyph = truncate_payload(json!({"a": format!("{E}{body}")}));
    assert!(is_truncation_envelope(&nul) && is_truncation_envelope(&glyph));
    assert_ne!(nul["sha256"], glyph["sha256"], "objects: {nul} vs {glyph}");

    let nul = truncate_payload(json!(format!("\0{body}")));
    let glyph = truncate_payload(json!(format!("{E}{body}")));
    assert_eq!(nul["sha256"], glyph["sha256"], "bare strings collide, as documented");
}

// ── `stored_form`: the whole row ───────────────────────────────────────────

/// `actor` and `action` are `text`, which refuses NUL too. They are escaped,
/// borrowed when clean, and [`StoredForm::columns_escaped`] says which.
#[test]
fn stored_form_escapes_actor_and_action_and_says_so() {
    let s = stored_form("tool:x\0", "ca\0ll", json!({}));
    assert_eq!(s.actor, format!("tool:x{E}"));
    assert_eq!(s.action, format!("ca{E}ll"));
    assert!(s.columns_escaped());
    // The count describes the payload only.
    assert_eq!(s.payload, json!({}));

    let clean = stored_form("core", "startup", json!({"p": "\0"}));
    assert!(matches!(clean.actor, std::borrow::Cow::Borrowed("core")));
    assert!(matches!(clean.action, std::borrow::Cow::Borrowed("startup")));
    assert!(!clean.columns_escaped());
    assert_eq!(clean.payload, truncate_payload(json!({"p": "\0"})));
}

/// The tool path applies [`stored_form`] twice: in the `AuditSink` seam, so
/// a test double sees the stored row, and again in `audit::insert`. The
/// second pass must change nothing — in any column.
#[test]
fn stored_form_is_idempotent_with_nuls_in_play() {
    let once = stored_form("tool:x\0", "c\0", json!({"result": "a\0b"}));
    let twice = stored_form(&once.actor, &once.action, once.payload.clone());
    assert_eq!(twice, once);
    assert!(!twice.columns_escaped(), "an escaped column holds no NUL to escape again");
}

/// `truncate_payload(truncate_payload(x)) == truncate_payload(x)`.
///
/// Production applies it TWICE on the tool path, inside [`stored_form`]:
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
