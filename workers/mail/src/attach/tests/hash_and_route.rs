//! Tests for two rules added by #767/#768, kept out of `tests.rs` (already
//! over the 500-line cap): where a `Picked` gets its route spellings, and the
//! one case rule for a planner-typed hash.

use super::*;

/// #767: every path a `Picked` builds comes from `localmail_contract`, the
/// file the live shape gate `include!`s. The literal spellings in `tests.rs`
/// pin what the paths ARE; this pins WHERE they come from, so a respelling
/// made in either place alone is caught here instead of leaving the gate
/// checking a route the worker no longer sends (the #500 failure mode).
#[test]
fn picked_paths_are_built_from_the_contract() {
    use crate::localmail_contract as contract;
    let atts = [att("a.pdf", SHA_A), att("b.pdf", SHA_B)];
    let by_index = pick(&atts, None, None, Some(1), id(37413)).unwrap();
    let by_sha = Picked::from_planner_sha(SHA_A).unwrap();
    let index_blob = contract::attachment_by_index_path("37413", 1);
    let sha_blob = contract::attachment_by_sha_path(SHA_A);
    assert_eq!(by_index.blob_path(), index_blob);
    assert_eq!(by_sha.blob_path(), sha_blob);
    assert_eq!(by_index.text_path(8000, 700), contract::text_page_path(&index_blob, 8000, 700));
    assert_eq!(by_sha.text_path(0, 10), contract::text_page_path(&sha_blob, 0, 10));
}

/// #768: one case rule for a planner hash, whichever form carries it. The
/// bare form used to refuse the 64 chars that the `message_id` form accepted
/// (and lowercased), so an uppercased hash worked beside a message and failed
/// on its own. Both now lowercase while the params are read, so everything
/// downstream — URL paths, prefix matching — sees one spelling.
#[test]
fn both_sha_forms_lowercase_a_planner_hash_at_parse_time() {
    let upper = SHA_A.to_uppercase();
    assert_eq!(choose(Some(upper.clone()), None, None, None).unwrap(), Selector::Sha(planner_sha(SHA_A)));
    let got = choose(Some(upper), Some(id(5)), None, None).unwrap();
    assert!(
        matches!(got, Selector::InMessage { expect_sha: Some(ref s), .. } if s.as_str() == SHA_A),
        "{got:?}"
    );
}

/// `ShaPrefix` holds only what some attachment could match: 1..=64 hex
/// chars, lowercased. The refusal must not echo a path the planner sent.
#[test]
fn a_sha_prefix_is_nonempty_bounded_hex_and_lowercased() {
    assert_eq!(ShaPrefix::parse("ABCdef12").unwrap().as_str(), "abcdef12");
    assert_eq!(ShaPrefix::parse(SHA_A).unwrap().as_str(), SHA_A);
    for bad in ["", "zz", "0123 456", "../../etc/passwd", &format!("{SHA_A}0")] {
        let e = ShaPrefix::parse(bad).unwrap_err();
        assert!(e.contains("hex prefix") && !e.contains("etc/passwd"), "{bad:?}: {e}");
    }
}

/// The handler's missing-text advice turns on who typed the hash: a
/// planner-typed one may be a transcription error, a message-resolved one
/// cannot be.
#[test]
fn only_the_bare_sha_form_is_planner_typed() {
    assert!(Selector::Sha(planner_sha(SHA_A)).is_planner_typed());
    let in_message = choose(Some(SHA_A.into()), Some(id(5)), None, None).unwrap();
    assert!(!in_message.is_planner_typed());
    assert!(!choose(None, Some(id(5)), None, Some(0)).unwrap().is_planner_typed());
}
