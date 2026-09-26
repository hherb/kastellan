//! Unit tests for [`super`] — the pure attachment-naming half.

use super::*;
use serde_json::json;

/// What the planner actually receives. These strings are handed to
/// `RpcError::new` unprefixed, so the whole budget is theirs — unlike
/// `ids::explain`, which pays for `parse_params`' `"bad params: "`.
fn as_the_planner_sees_it(s: &str) -> String {
    s.chars().take(kastellan_protocol::STEP_ERR_DETAIL_MAX).collect()
}

#[track_caller]
fn assert_survives_the_clamp(message: &str, phrases: &[&str]) {
    let seen = as_the_planner_sees_it(message);
    for p in phrases {
        assert!(seen.contains(p), "clamped away {p:?}; planner sees: {seen:?}");
    }
}

/// A `LocalmailId` for tests. The type deliberately has no public
/// constructor (see `ids`), so tests go through the wire form it validates.
fn id(n: i64) -> LocalmailId {
    #[derive(serde::Deserialize)]
    struct P {
        #[serde(deserialize_with = "crate::ids::message_id")]
        message_id: LocalmailId,
    }
    let p: P = serde_json::from_value(json!({ "message_id": n })).unwrap();
    p.message_id
}

fn att(filename: &str, sha: &str) -> serde_json::Value {
    json!({ "filename": filename, "sha256": sha, "content_type": "application/pdf", "size": 1 })
}

const SHA_A: &str = "71aac4580932cffe7649dda9c4cc10e2997de81d80105eafd448a64763f4a73b";
const SHA_B: &str = "322baed1a46322785c6cb46395ff9975ea99424cb844afd42ec8b2726604f2cc";
/// The filename that carried `SHA_A` in the live failure.
const LIVE_NAME: &str = "Download 470989752-e-ticket-DQXK68.pdf";

// --- choose: which form was named ---

#[test]
fn a_message_id_and_filename_select_the_in_message_form() {
    let got = choose(None, Some(id(37413)), Some(LIVE_NAME.into()), None).unwrap();
    assert_eq!(
        got,
        Selector::InMessage { message_id: id(37413), filename: Some(LIVE_NAME.into()), expect_sha: None, index: None }
    );
}

#[test]
fn a_message_id_alone_is_accepted_and_defers_the_choice_to_pick() {
    // The single-attachment case — which is what the live failure was — needs
    // no filename at all.
    let got = choose(None, Some(id(37413)), None, None).unwrap();
    assert_eq!(got, Selector::InMessage { message_id: id(37413), filename: None, expect_sha: None, index: None });
}

#[test]
fn a_sha256_alone_still_works() {
    // Backward compatibility: the original form is not withdrawn, and the
    // e2e suites still drive it.
    assert_eq!(choose(Some(SHA_A.into()), None, None, None).unwrap(), Selector::Sha(SHA_A.into()));
}

#[test]
fn a_sha256_beside_a_filename_keeps_the_sha_and_ignores_the_name() {
    // `mail.get_attachment`'s `filename` means the OUTPUT name, so a planner
    // carrying it across is confused about the parameter, not about which
    // file it wants — and the sha already addresses one attachment.
    let got = choose(Some(SHA_A.into()), None, Some("whatever.pdf".into()), None).unwrap();
    assert_eq!(got, Selector::Sha(SHA_A.into()));
}

#[test]
fn naming_no_attachment_at_all_says_how_to_name_one() {
    let e = choose(None, None, None, None).unwrap_err();
    assert_survives_the_clamp(&e, &["message_id", "filename", "sha256"]);
}

/// Both together is redundant, not contradictory — so it is taken as the
/// message form carrying a hash to verify, rather than refused. Live task 4
/// paid an iteration and a badly-named file for the old refusal.
#[test]
fn a_sha_beside_a_message_id_becomes_a_hash_to_verify_against_that_message() {
    let got = choose(Some(SHA_A.into()), Some(id(37413)), None, None).unwrap();
    assert_eq!(
        got,
        Selector::InMessage {
            message_id: id(37413),
            filename: None,
            expect_sha: Some(SHA_A.into()), index: None
        }
    );
}

#[test]
fn a_verified_sha_selects_its_attachment_and_yields_the_archive_name() {
    let atts = [att("boarding-pass.pdf", SHA_B), att(LIVE_NAME, SHA_A)];
    let picked = pick(&atts, None, Some(SHA_A), None, id(37413)).unwrap();
    assert_eq!(picked.sha256(), SHA_A);
    assert_eq!(picked.save_name(), Some(LIVE_NAME), "the archive name, for saving");
}

/// The repair text lists 12-char sha prefixes, so 12-char prefixes have to
/// resolve — otherwise the advice sends the planner into a second refusal.
#[test]
fn a_unique_sha_prefix_selects_the_attachment_the_advice_listed() {
    let atts = [att("boarding-pass.pdf", SHA_B), att(LIVE_NAME, SHA_A)];
    let picked = pick(&atts, None, Some(&SHA_A[..SHA_HEAD]), None, id(37413)).unwrap();
    assert_eq!(picked.sha256(), SHA_A, "the FULL archive hash, not the prefix");
}

/// A hash the planner uppercased still resolves — a transcription slip, not
/// a different attachment.
#[test]
fn an_uppercased_sha_still_resolves_against_the_message() {
    let atts = [att(LIVE_NAME, SHA_A)];
    let picked = pick(&atts, None, Some(&SHA_A.to_uppercase()), None, id(37413)).unwrap();
    assert_eq!(picked.sha256(), SHA_A);
}

/// A prefix short enough to be a coincidence is not a selector. Without the
/// floor, `Some("7")` would silently pick whichever attachment happened to
/// start with that nibble.
#[test]
fn a_sha_prefix_below_the_floor_does_not_select() {
    let atts = [att(LIVE_NAME, SHA_A)];
    let short = &SHA_A[..SHA_PREFIX_MIN - 1];
    assert!(pick(&atts, None, Some(short), None, id(37413)).is_err(), "{short} is too short to select");
}

/// Two attachments of one message sharing a prefix: refused, not guessed.
#[test]
fn a_sha_prefix_matching_two_attachments_is_refused() {
    let shared = format!("{}ffffffffffffffff", "a".repeat(48));
    let other = format!("{}0000000000000000", "a".repeat(48));
    let atts = [att("one.pdf", &shared), att("two.pdf", &other)];
    let e = pick(&atts, None, Some(&"a".repeat(20)), None, id(37413)).unwrap_err();
    assert!(!e.is_empty(), "an ambiguous prefix must refuse");
}

/// The point of verifying rather than trusting: a hash that belongs to no
/// attachment of this message is caught here, by the message, instead of
/// reaching localmail and coming back as its ambiguous 404.
#[test]
fn a_sha_absent_from_the_message_is_refused_and_lists_the_real_attachments() {
    let atts = [att(LIVE_NAME, SHA_A)];
    let e = pick(&atts, None, Some(SHA_B), None, id(37413)).unwrap_err();
    assert_survives_the_clamp(&e, &["sha256", LIVE_NAME]);
}

/// Two selectors that agree are fine — the filename is a second opinion,
/// and a second opinion that concurs costs nothing.
#[test]
fn a_sha_and_a_filename_naming_the_same_attachment_resolve() {
    let atts = [att("boarding-pass.pdf", SHA_B), att(LIVE_NAME, SHA_A)];
    let picked = pick(&atts, Some("e-ticket-DQXK68.pdf"), Some(SHA_A), None, id(37413)).unwrap();
    assert_eq!(picked.sha256(), SHA_A);
}

/// Two selectors that *contradict* are refused rather than resolved by
/// precedence. The hash used to win silently, which meant
/// `mail.get_attachment` wrote the other document to disk — under a name
/// matching it — and reported success. That is the doctrine
/// `search_params::normalize_filters` states for a doubled id filter, and
/// here the message is already fetched so checking is free.
#[test]
fn a_sha_and_a_filename_naming_different_attachments_are_refused() {
    let atts = [att("boarding-pass.pdf", SHA_B), att(LIVE_NAME, SHA_A)];
    let e = pick(&atts, Some("boarding-pass.pdf"), Some(SHA_A), None, id(37413)).unwrap_err();
    assert_survives_the_clamp(&e, &["sha256", "filename"]);
    assert!(e.contains("boarding-pass.pdf"), "must name what the filename picked: {e}");
}

/// A filename that resolves to nothing is not a contradiction — the hash
/// still selects, and refusing here would spend an iteration for no gain.
#[test]
fn a_sha_beside_an_unresolvable_filename_still_selects_by_hash() {
    let atts = [att("boarding-pass.pdf", SHA_B), att(LIVE_NAME, SHA_A)];
    let picked = pick(&atts, Some("nope.pdf"), Some(SHA_A), None, id(37413)).unwrap();
    assert_eq!(picked.sha256(), SHA_A);
}

#[test]
fn a_filename_without_a_message_id_is_told_which_argument_is_missing() {
    // A filename alone cannot be resolved — attachments are addressed within
    // a message. Naming the *missing* parameter is the whole point (#536).
    //
    // Asserting only `contains("message_id")` did not distinguish this arm
    // from the generic "name the attachment" one, whose text also contains
    // it — so deleting the targeted arm survived a mutation run. The
    // discriminator is that this arm does NOT offer `sha256`: the planner
    // already named the file it wants, and sending it to find a hash
    // instead is the opposite of this branch's whole argument.
    let e = choose(None, None, Some(LIVE_NAME.into()), None).unwrap_err();
    assert_survives_the_clamp(&e, &["message_id", "alone"]);
    assert!(!e.contains("sha256"), "the planner named a file, not a hash: {e}");

    // ...and the no-selector-at-all arm is the one that does offer both.
    let generic = choose(None, None, None, None).unwrap_err();
    assert_survives_the_clamp(&generic, &["message_id", "filename", "sha256"]);
}

// --- pick: which attachment a filename names ---

#[test]
fn a_lone_attachment_needs_no_filename() {
    let atts = [att(LIVE_NAME, SHA_A)];
    assert_eq!(pick(&atts, None, None, None, id(37413)).unwrap().sha256(), SHA_A);
}

#[test]
fn an_exact_filename_picks_its_attachment() {
    let atts = [att("other.pdf", SHA_B), att(LIVE_NAME, SHA_A)];
    assert_eq!(pick(&atts, Some(LIVE_NAME), None, None, id(37413)).unwrap().sha256(), SHA_A);
}

#[test]
fn filename_matching_ignores_case() {
    let atts = [att(LIVE_NAME, SHA_A)];
    assert_eq!(pick(&atts, Some(&LIVE_NAME.to_lowercase()), None, None, id(37413)).unwrap().sha256(), SHA_A);
}

#[test]
fn a_unique_substring_picks_its_attachment() {
    // The live filename is `Download 470989752-e-ticket-DQXK68.pdf`; a model
    // that writes the meaningful tail rather than the archive's download
    // prefix is naming the right file, and refusing it would spend an
    // iteration on a repair that adds no information.
    let atts = [att("boarding-pass.pdf", SHA_B), att(LIVE_NAME, SHA_A)];
    assert_eq!(pick(&atts, Some("e-ticket-DQXK68.pdf"), None, None, id(37413)).unwrap().sha256(), SHA_A);
}

#[test]
fn an_exact_match_beats_a_substring_match_of_another_attachment() {
    // `receipt.pdf` is a substring of `flight-receipt.pdf`, so a tier order
    // that ran substring first would have two candidates and refuse a request
    // that names one of them exactly.
    let atts = [att("flight-receipt.pdf", SHA_B), att("receipt.pdf", SHA_A)];
    assert_eq!(pick(&atts, Some("receipt.pdf"), None, None, id(37413)).unwrap().sha256(), SHA_A);
}

/// Two parts of one message really can share a filename (`image001.png` is
/// the canonical case), and they have different shas. Taking the first would
/// be a silent wrong answer; the planner is told to disambiguate instead.
///
/// And it must be told with a key that *can* disambiguate. The old text
/// said "copy one exactly" and listed `image001.png, image001.png` — the
/// planner copies the name it was given, gets a byte-identical error, and
/// repeats to the iteration cap. Asserting only that the name appears
/// passes for that message, which is how it shipped.
#[test]
fn two_attachments_sharing_a_filename_are_refused_with_a_key_that_discriminates() {
    let atts = [att("image001.png", SHA_A), att("image001.png", SHA_B)];
    let e = pick(&atts, Some("image001.png"), None, None, id(37413)).unwrap_err();
    assert_survives_the_clamp(&e, &["`index`", ": 0, 1"]);
    assert!(
        !e.contains("copy its `filename`"),
        "a filename cannot select between two identical names: {e}"
    );
    // ...and the key it offers actually resolves, so the advice terminates.
    assert_eq!(pick(&atts, None, None, Some(1), id(37413)).unwrap().sha256(), SHA_B);
    // A sha prefix still selects too — it is advertised, just no longer listed.
    assert_eq!(pick(&atts, None, Some(&SHA_B[..SHA_HEAD]), None, id(37413)).unwrap().sha256(), SHA_B);
}

/// The same dead end reached the other way: an attachment localmail has no
/// filename for cannot be named, and used to render as a blank list entry
/// (`… exactly one of: , real.pdf`).
#[test]
fn a_nameless_attachment_is_listed_by_index_not_as_an_empty_token() {
    let atts = [json!({ "filename": null, "sha256": SHA_A }), att("real.pdf", SHA_B)];
    let e = pick(&atts, None, None, None, id(37413)).unwrap_err();
    assert_survives_the_clamp(&e, &["`index`", ": 0, 1"]);
    assert!(!e.contains(" , "), "no empty list entry: {e}");
    assert_eq!(pick(&atts, None, None, Some(0), id(37413)).unwrap().sha256(), SHA_A);
}

#[test]
fn an_ambiguous_substring_is_refused_and_lists_only_the_candidates() {
    // The third attachment is what gives this test teeth: an implementation
    // that fell through to the generic "copy one exactly" arm would list
    // every attachment in the message, which is both longer and wrong — the
    // planner would be offered a name that does not match what it asked for.
    let atts = [
        att("receipt-jan.pdf", SHA_A),
        att("receipt-feb.pdf", SHA_B),
        att("itinerary.pdf", &"c".repeat(64)),
    ];
    let e = pick(&atts, Some("receipt"), None, None, id(37413)).unwrap_err();
    assert_survives_the_clamp(&e, &["receipt-jan.pdf", "receipt-feb.pdf"]);
    assert!(
        !e.contains("itinerary.pdf"),
        "only the attachments that actually match belong in the list: {e}"
    );
}

#[test]
fn several_attachments_and_no_filename_lists_what_to_choose_from() {
    let atts = [att("a.pdf", SHA_A), att("b.pdf", SHA_B)];
    let e = pick(&atts, None, None, None, id(37413)).unwrap_err();
    assert_survives_the_clamp(&e, &["filename", "a.pdf", "b.pdf"]);
}

#[test]
fn a_filename_that_matches_nothing_lists_what_is_there() {
    let atts = [att("a.pdf", SHA_A)];
    let e = pick(&atts, Some("nope.pdf"), None, None, id(37413)).unwrap_err();
    assert_survives_the_clamp(&e, &["a.pdf"]);
}

/// A message with genuinely no attachments used to be told to "re-check the
/// mail.get_message output for one with a sha256" — output that visibly has
/// none. The planner re-runs the same step, reads the same empty list, and
/// either loops or reports the attachment unreadable.
#[test]
fn a_message_with_no_attachments_is_not_told_to_go_looking_for_one() {
    let e = pick(&[], Some("a.pdf"), None, None, id(37413)).unwrap_err();
    assert_survives_the_clamp(&e, &["37413", "no attachments"]);
    assert!(
        !e.contains("re-check") && !e.contains("sha256"),
        "there is nothing to re-check and no hash to find: {e}"
    );
}

/// Distinct from the above, and the distinction is the point: these entries
/// exist, so telling the planner the message has none would be false, and
/// telling it to pick another one is actionable only if it knows why.
#[test]
fn attachments_that_exist_but_are_unstored_say_so_rather_than_claiming_none_exist() {
    let atts = [json!({ "filename": "ghost.pdf", "sha256": null })];
    let e = pick(&atts, None, None, None, id(37413)).unwrap_err();
    assert_survives_the_clamp(&e, &["37413"]);
    assert!(e.contains("stored"), "must say why it cannot be read: {e}");
    assert!(!e.contains("no attachments"), "the message does list one: {e}");
}

/// The false statement this fixes: `ghost.pdf` is right there in the
/// `mail.get_message` output the planner is reading, so "no attachment has
/// that filename" is untrue and it will retype the same name.
#[test]
fn naming_an_unstored_attachment_is_told_why_not_that_it_does_not_exist() {
    let atts = [json!({ "filename": "ghost.pdf", "sha256": null }), att("real.pdf", SHA_A)];
    let e = pick(&atts, Some("ghost.pdf"), None, None, id(37413)).unwrap_err();
    assert!(e.contains("ghost.pdf"), "name the one that was asked for: {e}");
    assert!(e.contains("stored"), "say why: {e}");
    assert!(
        !e.contains("No attachment in message"),
        "it is in the message; that claim is false: {e}"
    );
}

#[test]
fn an_entry_without_a_usable_sha256_is_not_selected() {
    // Defensive: localmail's `_attachment_entry` can emit `"sha256": null`
    // for a part whose blob was never stored. Selecting it would build
    // `/v1/attachments/null/text`; skipping it means the lone *usable*
    // attachment is still found without a filename.
    let atts = [json!({ "filename": "ghost.pdf", "sha256": null }), att("real.pdf", SHA_A)];
    assert_eq!(pick(&atts, None, None, None, id(37413)).unwrap().sha256(), SHA_A);
}

#[test]
fn an_entry_whose_sha256_is_not_a_hash_is_not_selected() {
    // The sha reaching `pick` is interpolated into a URL path by the
    // caller. Filtering here keeps that guard structural rather than relying
    // on the caller to re-validate what a trusted service sent.
    let atts = [json!({ "filename": "evil.pdf", "sha256": "../../etc/passwd" })];
    let e = pick(&atts, None, None, None, id(37413)).unwrap_err();
    assert!(!e.contains("etc/passwd"), "must not echo the rejected path: {e}");
}

// --- the repair text for a 404 ---

#[test]
fn a_404_on_a_planner_supplied_hash_points_at_the_hash() {
    let m = missing_text_advice(SHA_B, true);
    assert_survives_the_clamp(&m, &["message_id", "filename"]);
}

#[test]
fn a_404_on_a_resolved_hash_does_not_blame_the_hash() {
    // The hash came out of the message, so it is right by construction. The
    // planner must not be sent to re-copy a parameter it never supplied.
    let m = missing_text_advice(SHA_A, false);
    assert!(
        !m.contains("message_id"),
        "a resolved hash is not repaired by re-naming the message: {m}"
    );
    assert_survives_the_clamp(&m, &["mail.get_attachment"]);
}

// --- the shared shape rule behind the URL-path guard ---

/// [`is_sha256`] is the single rule behind both the traversal guard on the
/// `{sha256}` URL segment and `pick`'s entry filter, so each clause needs a
/// fixture that fails without it. The only negative here used to be
/// `"../../etc/passwd"`, which the charset clause rejects on its own —
/// leaving the length clause untested, and `is_sha256("")` vacuously true.
#[test]
fn is_sha256_pins_both_length_and_charset() {
    assert!(is_sha256(&"a".repeat(64)));
    assert!(is_sha256(SHA_A));
    assert!(!is_sha256(""), "an empty hash would build /v1/attachments//text");
    assert!(!is_sha256(&"a".repeat(63)), "too short");
    assert!(!is_sha256(&"a".repeat(65)), "too long");
    assert!(!is_sha256(&"A".repeat(64)), "uppercase — the error text promises lowercase");
    assert!(!is_sha256(&"g".repeat(64)), "not hex");
    assert!(!is_sha256("../../etc/passwd"));
}

/// The public constructor is fallible, and its rejection carries the
/// planner-facing shape text rather than a bare `None`.
#[test]
fn a_planner_sha_of_the_wrong_shape_is_refused_with_the_shape_rule() {
    assert_eq!(Picked::from_planner_sha(SHA_A).unwrap().sha256(), SHA_A);
    assert_eq!(Picked::from_planner_sha(SHA_A).unwrap().save_name(), None);
    let e = Picked::from_planner_sha("../../etc/passwd").unwrap_err();
    assert!(e.contains("64 lowercase hex"), "got: {e}");
    assert!(!e.contains("etc/passwd"), "must not echo the rejected path: {e}");
}

// --- the property that makes every message above fit ---

#[test]
fn no_message_grows_past_the_planner_clamp_for_any_input() {
    // The worst case is derived, not hardcoded: raising `NAME_HEAD` or
    // lengthening the prose fails here rather than silently clipping the
    // advice in production. This is the `ids` lesson — its `e.g. 374` defect
    // got in because the probes were shorter than the live values.
    let long = "x".repeat(NAME_HEAD * 4);
    let many: Vec<serde_json::Value> =
        (0..12).map(|i| att(&format!("{long}-{i}.pdf"), SHA_A)).collect();
    // A 19-digit id is the widest a non-negative i64 renders to, and every
    // other fixture here pins 5 digits — the prose, not the list, is what
    // bounds the zero-listed branch, so it has to be probed at its longest.
    let wide = id(i64::MAX);

    let mut messages = vec![
        choose(None, None, None, None).unwrap_err(),
        choose(None, None, Some(long.clone()), None).unwrap_err(),
        // The new variable-length arm: a hash the message does not carry,
        // whose repair lists that message's (here, very long) filenames.
        pick(&many, None, Some(SHA_B), None, id(37413)).unwrap_err(),
        pick(&many, None, None, None, id(37413)).unwrap_err(),
        pick(&many, Some(&long), None, None, id(37413)).unwrap_err(),
        pick(&[], None, None, None, id(37413)).unwrap_err(),
        pick(&many, None, Some(SHA_B), None, wide).unwrap_err(),
        pick(&many, Some(&long), None, None, wide).unwrap_err(),
        pick(&[], None, None, None, wide).unwrap_err(),
        missing_text_advice(SHA_A, true),
        missing_text_advice(SHA_A, false),
        missing_blob_advice(SHA_A, true),
        missing_blob_advice(SHA_A, false),
    ];
    messages.push(pick(&many[..2], Some("x"), None, None, id(37413)).unwrap_err());
    // The divergence arm quotes a filename, so it grows with its input too.
    let two = [att(&format!("{long}-a.pdf"), SHA_A), att(&format!("{long}-b.pdf"), SHA_B)];
    messages.push(
        pick(&two, Some(&format!("{long}-b.pdf")), Some(SHA_A), None, wide).unwrap_err(),
    );
    // An unstored attachment named by a very long filename.
    let ghost = [json!({ "filename": format!("{long}.pdf"), "sha256": null }), att("r.pdf", SHA_A)];
    messages.push(pick(&ghost, Some(&format!("{long}.pdf")), None, None, wide).unwrap_err());
    messages.push(pick(&ghost[..1], None, None, None, wide).unwrap_err());

    for m in &messages {
        assert!(
            m.chars().count() <= kastellan_protocol::STEP_ERR_DETAIL_MAX,
            "{} chars exceeds the planner clamp, so its tail is dropped: {m}",
            m.chars().count()
        );
    }
}

/// The other half of the budget property, and the half that was missing:
/// the list must still be *there*.
///
/// `with_candidates` breaks rather than truncates, so raising [`NAME_HEAD`]
/// makes a message **shorter**, not longer — every name stops fitting and
/// the planner receives prose plus `…` with nothing to choose from. The
/// upper-bound assertion above passes throughout that, which is exactly the
/// silent clipping the derived budget is supposed to prevent.
#[test]
fn a_listing_message_always_lists_at_least_one_candidate() {
    let long = "x".repeat(NAME_HEAD * 4);
    let many: Vec<serde_json::Value> =
        (0..12).map(|i| att(&format!("{long}-{i}.pdf"), SHA_A)).collect();
    let head_of_long: String = long.chars().take(NAME_HEAD).collect();

    for (label, m) in [
        ("no filename, many attachments", pick(&many, None, None, None, id(37413)).unwrap_err()),
        ("filename matched nothing", pick(&many, Some("zzz"), None, None, id(37413)).unwrap_err()),
        ("sha absent from message", pick(&many, None, Some(SHA_B), None, id(37413)).unwrap_err()),
        ("ambiguous substring", pick(&many, Some("x"), None, None, id(37413)).unwrap_err()),
    ] {
        assert!(
            m.contains(&head_of_long),
            "{label}: no candidate survived the budget, so the actionable half is gone: {m}"
        );
    }
}

/// Names near the budget boundary — the region the long-name fixtures skip
/// entirely, and where the reserved elision room is what keeps the result
/// inside the clamp.
#[test]
fn candidate_lists_near_the_budget_boundary_stay_inside_it() {
    for n in 1..=NAME_HEAD {
        let name = "y".repeat(n);
        let atts = [att(&name, SHA_A), att(&format!("{name}-b"), SHA_B)];
        let absent = "c".repeat(64);
        for m in [
            pick(&atts, None, None, None, id(37413)).unwrap_err(),
            pick(&atts, None, Some(&absent), None, id(i64::MAX)).unwrap_err(),
        ] {
            assert!(
                m.chars().count() <= kastellan_protocol::STEP_ERR_DETAIL_MAX,
                "name length {n}: {} chars exceeds the clamp: {m}",
                m.chars().count()
            );
        }
    }
}

// --- `index`: an attachment named by its position in the message (#760) ---

#[test]
fn an_index_without_a_message_id_is_told_which_argument_is_missing() {
    for sha in [None, Some(SHA_A.to_string())] {
        let e = choose(sha, None, None, Some(0)).unwrap_err();
        assert_survives_the_clamp(&e, &["`index`", "`message_id`"]);
    }
}

#[test]
fn an_index_beside_a_message_id_selects_the_in_message_form() {
    let got = choose(None, Some(id(37413)), None, Some(2)).unwrap();
    assert_eq!(
        got,
        Selector::InMessage { message_id: id(37413), filename: None, expect_sha: None, index: Some(2) }
    );
}

/// The live shape the index exists for: one message carrying `attachment`
/// many times over different blobs, where no filename can choose.
#[test]
fn an_index_selects_among_attachments_that_share_a_name_and_fetches_by_position() {
    let atts = [att("attachment", SHA_A), att("attachment", SHA_B)];
    let picked = pick(&atts, None, None, Some(1), id(37413)).unwrap();
    assert_eq!(picked.sha256(), SHA_B);
    assert_eq!(picked.index(), Some(1));
    assert_eq!(picked.save_name(), Some("attachment"));
    assert_eq!(picked.blob_path(), "/v1/messages/37413/attachments/1");
    assert_eq!(picked.text_path(8000, 8000), "/v1/messages/37413/attachments/1/text?offset=8000&limit=8000");
}

/// Positions count every served entry, the unusable ones included, because
/// localmail's route does. Skipping the unstored entry would fetch the wrong
/// attachment for every index after it.
#[test]
fn positions_count_unstored_entries_so_they_match_localmails_route() {
    let atts = [json!({"filename": "ghost.pdf", "sha256": null}), att("real.pdf", SHA_B)];
    assert_eq!(pick(&atts, None, None, None, id(7)).unwrap().index(), Some(1));
    assert_eq!(pick(&atts, None, None, Some(1), id(7)).unwrap().sha256(), SHA_B);
    let e = pick(&atts, None, None, Some(0), id(7)).unwrap_err();
    assert_survives_the_clamp(&e, &["never stored"]);
}

#[test]
fn an_index_past_the_end_says_how_many_there_are() {
    let atts = [att("a.pdf", SHA_A), att("b.pdf", SHA_B)];
    let e = pick(&atts, None, None, Some(2), id(37413)).unwrap_err();
    assert_survives_the_clamp(&e, &["2 attachment(s)", "numbered from 0", "past the end"]);
}

#[test]
fn a_sha_or_filename_beside_an_index_must_name_the_same_attachment() {
    let atts = [att(LIVE_NAME, SHA_A), att("boarding-pass.pdf", SHA_B)];
    // Agreeing second opinions: exact sha, sha prefix, filename substring.
    assert!(pick(&atts, None, Some(SHA_A), Some(0), id(1)).is_ok());
    assert!(pick(&atts, None, Some(&SHA_A[..SHA_HEAD]), Some(0), id(1)).is_ok());
    assert!(pick(&atts, Some("e-ticket"), None, Some(0), id(1)).is_ok());
    // Disagreeing ones are refused, naming what the index points at.
    let e = pick(&atts, None, Some(SHA_B), Some(0), id(1)).unwrap_err();
    assert_survives_the_clamp(&e, &["`index` and `sha256`", "pass one"]);
    let e = pick(&atts, Some("boarding"), None, Some(0), id(1)).unwrap_err();
    assert_survives_the_clamp(&e, &["`index` and `filename`", "pass one"]);
}

/// A planner-typed hash names bytes, not a message entry, so it keeps the hash
/// routes — there is no position to fetch by.
#[test]
fn a_planner_sha_fetches_by_hash() {
    let p = Picked::from_planner_sha(SHA_A).unwrap();
    assert_eq!(p.index(), None);
    assert_eq!(p.blob_path(), format!("/v1/attachments/{SHA_A}"));
    assert_eq!(p.text_path(0, 10), format!("/v1/attachments/{SHA_A}/text?offset=0&limit=10"));
}
