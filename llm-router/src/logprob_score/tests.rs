//! Tests for [`super`] — the two-way logprob renormalisation.
//!
//! The centre of gravity here is the `None` cases, not the arithmetic.
//! Getting `0.87` slightly wrong is a calibration question; returning a
//! number at all when nothing was observed is a security defect, and it
//! is the one this module exists to make unrepresentable.

use super::*;
use crate::messages::{ChatChoice, ChatMessage, ChatResponse, LogProbs, TokenLogProbs};

/// One alternative. `bytes` is separate from `token` on purpose — the
/// divergence between them is what several of these tests are about.
fn alt(token: &str, logprob: f64, bytes: Option<&[u8]>) -> TopLogProb {
    TopLogProb {
        token: token.to_string(),
        logprob,
        bytes: bytes.map(<[u8]>::to_vec),
    }
}

/// Naive reference implementation, written the obvious way. Used to pin
/// that the stable form is a refactor and not a behaviour change.
fn naive_softmax(z_yes: f64, z_no: f64) -> f64 {
    z_yes.exp() / (z_yes.exp() + z_no.exp())
}

#[test]
fn renormalises_two_observed_alternatives() {
    let alts = [alt("yes", -0.1, None), alt("no", -2.3, None)];
    let p = binary_token_probability(&alts, YES_FORMS, NO_FORMS).expect("both forms present");
    let want = naive_softmax(-0.1, -2.3) as f32;
    assert!((p - want).abs() < 1e-6, "got {p}, want {want}");
}

#[test]
fn is_none_when_only_the_yes_form_is_present() {
    // A sentinel floor for the missing side would manufacture ~0.9999
    // here — a confident verdict from a single observation.
    let alts = [alt("yes", -0.1, None), alt("maybe", -4.0, None)];
    assert_eq!(binary_token_probability(&alts, YES_FORMS, NO_FORMS), None);
}

#[test]
fn is_none_when_only_the_no_form_is_present() {
    let alts = [alt("no", -0.1, None), alt("maybe", -4.0, None)];
    assert_eq!(binary_token_probability(&alts, YES_FORMS, NO_FORMS), None);
}

#[test]
fn is_none_when_neither_form_is_present() {
    // The defect this pins: with both logits on a shared sentinel floor
    // the softmax is exactly 0.5, which reads as "below τ" — i.e. safe.
    let alts = [alt("perhaps", -1.0, None), alt("<|channel|>", -2.0, None)];
    assert_eq!(binary_token_probability(&alts, YES_FORMS, NO_FORMS), None);
}

#[test]
fn is_none_for_an_empty_alternatives_slice() {
    assert_eq!(binary_token_probability(&[], YES_FORMS, NO_FORMS), None);
}

#[test]
fn agrees_with_the_naive_softmax_on_ordinary_values() {
    for (y, n) in [(-0.0001, -9.2), (-3.0, -3.0), (-0.7, -0.7), (-12.0, -0.2)] {
        let alts = [alt("yes", y, None), alt("no", n, None)];
        let got = binary_token_probability(&alts, YES_FORMS, NO_FORMS).unwrap();
        let want = naive_softmax(y, n) as f32;
        assert!((got - want).abs() < 1e-6, "y={y} n={n}: got {got}, want {want}");
    }
}

#[test]
fn survives_logprobs_the_naive_softmax_turns_into_nan() {
    // exp(-800) underflows to 0.0, so the naive form evaluates 0/0 = NaN,
    // and a NaN score compares false against every threshold — silently
    // unreachable in whichever direction the caller tests.
    let alts = [alt("yes", -800.0, None), alt("no", -820.0, None)];
    let got = binary_token_probability(&alts, YES_FORMS, NO_FORMS).expect("measurable");
    assert!(naive_softmax(-800.0, -820.0).is_nan(), "reference should be NaN here");
    assert!(got.is_finite(), "stable form went non-finite: {got}");
    assert!(got > 0.99, "yes dominates by 20 nats, got {got}");
}

#[test]
fn identifies_tokens_from_bytes_when_the_display_form_is_unusable() {
    // The display forms here are mojibake that `normalize_token` cannot
    // rescue — only `bytes` identifies these tokens.
    //
    // Deliberately NOT the obvious `Ġyes`/`bytes: " yes"` pairing: that
    // one passes whether or not `token_text` consults `bytes` at all,
    // because `normalize_token` strips `Ġ` by itself. A test whose
    // subject is unreachable from its assertion proves nothing, so the
    // display form has to be something no folding rule recovers.
    let alts = [
        alt("ÃŠyeÅ¡", -0.1, Some(b" yes")),
        alt("Ã±Ã¸", -2.3, Some(b" no")),
    ];
    assert!(
        binary_token_probability(&alts, YES_FORMS, NO_FORMS).is_some(),
        "bytes were not consulted — the display forms cannot match"
    );
}

#[test]
fn falls_back_to_the_display_form_when_bytes_are_absent() {
    // Same markers, no bytes: normalize_token must strip them itself, or
    // a backend that omits `bytes` floors every case at once.
    let alts = [
        alt("\u{0120}yes", -0.1, None),
        alt("\u{2581}No", -2.3, None),
    ];
    assert!(binary_token_probability(&alts, YES_FORMS, NO_FORMS).is_some());
}

#[test]
fn invalid_utf8_bytes_fall_back_to_the_display_form() {
    // Half a multi-byte character is a real thing at a truncation
    // boundary. Lossy-decoding it could only invent a spurious match.
    let alts = [
        alt("yes", -0.1, Some(&[0xff, 0xfe])),
        alt("no", -2.3, None),
    ];
    assert!(binary_token_probability(&alts, YES_FORMS, NO_FORMS).is_some());
}

#[test]
fn takes_the_highest_logprob_when_a_spelling_appears_twice() {
    // Two encodings of the same word: the model's mass is split across
    // them, so the stronger one is the honest representative.
    let low = [alt("yes", -5.0, None), alt("no", -1.0, None)];
    let both = [alt("yes", -5.0, None), alt("Yes", -0.5, None), alt("no", -1.0, None)];
    let p_low = binary_token_probability(&low, YES_FORMS, NO_FORMS).unwrap();
    let p_both = binary_token_probability(&both, YES_FORMS, NO_FORMS).unwrap();
    assert!(p_both > p_low, "max not taken: {p_both} !> {p_low}");
    let want = naive_softmax(-0.5, -1.0) as f32;
    assert!((p_both - want).abs() < 1e-6, "got {p_both}, want {want}");
}

#[test]
fn normalize_token_folds_the_documented_variants() {
    for raw in [" Yes.", "\u{0120}yes", "\u{2581}YES", "\"yes\"", "yes!", "'Yes'"] {
        assert_eq!(normalize_token(raw), "yes", "failed to fold {raw:?}");
    }
}

#[test]
fn normalize_token_does_not_fold_unrelated_tokens_onto_a_verdict() {
    // A false match here fabricates a score, so the folding stays narrow.
    for raw in ["yesterday", "no_way", "yes-man", "nope"] {
        let folded = normalize_token(raw);
        assert!(
            !YES_FORMS.contains(&folded.as_str()) && !NO_FORMS.contains(&folded.as_str()),
            "{raw:?} folded onto a verdict spelling: {folded:?}"
        );
    }
}

/// Build a response carrying `alts` at the first position.
fn response_with(alts: Vec<TopLogProb>) -> ChatResponse {
    ChatResponse {
        id: None,
        model: None,
        system_fingerprint: None,
        usage: None,
        choices: vec![ChatChoice {
            index: 0,
            message: ChatMessage::assistant(""),
            finish_reason: None,
            logprobs: Some(LogProbs {
                content: vec![TokenLogProbs {
                    token: "yes".into(),
                    logprob: -0.1,
                    bytes: None,
                    top_logprobs: alts,
                }],
            }),
        }],
    }
}

#[test]
fn first_position_alternatives_returns_the_first_positions_alternatives() {
    let resp = response_with(vec![alt("yes", -0.1, None), alt("no", -2.3, None)]);
    let alts = first_position_alternatives(&resp).expect("alternatives present");
    assert_eq!(alts.len(), 2);
    assert_eq!(alts[0].token, "yes");
}

#[test]
fn first_position_alternatives_is_none_without_a_logprobs_block() {
    // Every call the planner makes has this shape.
    let resp = ChatResponse {
        id: None,
        model: None,
        system_fingerprint: None,
        usage: None,
        choices: vec![ChatChoice {
            index: 0,
            message: ChatMessage::assistant("hello"),
            finish_reason: None,
            logprobs: None,
        }],
    };
    assert!(first_position_alternatives(&resp).is_none());
}

#[test]
fn first_position_alternatives_is_none_for_an_empty_top_logprobs() {
    // What a backend returns for `logprobs: true` with no count: a block
    // that exists but carries no distribution.
    let resp = response_with(vec![]);
    assert!(first_position_alternatives(&resp).is_none());
}

#[test]
fn first_position_alternatives_is_none_when_there_are_no_choices() {
    let resp = ChatResponse { id: None, model: None, system_fingerprint: None, usage: None, choices: vec![] };
    assert!(first_position_alternatives(&resp).is_none());
}
