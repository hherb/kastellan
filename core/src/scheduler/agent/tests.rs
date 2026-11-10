//! Unit tests for [`super`] — what an [`AgentError`] says about the failed
//! completion it carries.

use super::*;

/// The failed-call row reads its cost through `completion()`. Pinned here
/// without Postgres: the row-level checks need a cluster.
#[test]
fn only_a_decode_failure_carries_its_completion() {
    let usage = kastellan_llm_router::CompletionStats {
        completion_tokens: Some(48),
        ..Default::default()
    };
    let decode = AgentError::Decode { detail: "EOF".into(), raw: "prose".into(), usage, latency_ms: 18_976 };
    let (got, latency_ms, raw) = decode.completion().expect("a decode failure has a completion");
    assert_eq!(got.completion_tokens, Some(48));
    assert_eq!(latency_ms, 18_976);
    assert_eq!(raw, "prose");
    assert!(AgentError::PromptMissing("p".into()).completion().is_none());
}

/// #923: only an answer with nothing in it is worth a plain retry; a
/// non-JSON answer would most likely come back non-JSON again.
#[test]
fn only_a_blank_decode_failure_is_an_empty_completion() {
    let decode = |raw: &str| AgentError::Decode {
        detail: "d".into(),
        raw: raw.into(),
        usage: Default::default(),
        latency_ms: 0,
    };
    assert!(decode("").is_empty_completion());
    assert!(decode(" \n\t").is_empty_completion());
    assert!(!decode("Sure! Here is the plan").is_empty_completion());
    assert!(!decode("{").is_empty_completion());
    assert!(!AgentError::PromptMissing("p".into()).is_empty_completion());
    // Out of room is not transient: the same request runs out again.
    let out_of_room = |finish_reason: Option<&str>| AgentError::Decode {
        detail: "d".into(),
        raw: String::new(),
        usage: kastellan_llm_router::CompletionStats {
            finish_reason: finish_reason.map(Into::into),
            ..Default::default()
        },
        latency_ms: 0,
    };
    assert!(!out_of_room(Some("length")).is_empty_completion());
    assert!(out_of_room(Some("stop")).is_empty_completion());
}

/// #945: what qualifies for the re-ask. The live failures were prose with no
/// `{` at all; malformed JSON qualifies too. A blank answer is #923's (a plain
/// retry), and `length` means the reply was cut off, which re-asking repeats.
#[test]
fn only_a_complete_non_blank_decode_failure_is_a_non_plan_answer() {
    let decode = |raw: &str, finish_reason: Option<&str>| AgentError::Decode {
        detail: "d".into(),
        raw: raw.into(),
        usage: kastellan_llm_router::CompletionStats {
            finish_reason: finish_reason.map(Into::into),
            ..Default::default()
        },
        latency_ms: 0,
    };
    let prose = "Based on your emails, the following payments are overdue:";
    assert_eq!(decode(prose, Some("stop")).non_plan_answer(), Some(prose));
    assert_eq!(decode("{\"steps\": [}", None).non_plan_answer(), Some("{\"steps\": [}"));
    assert_eq!(decode(" \n", Some("stop")).non_plan_answer(), None);
    assert_eq!(decode(prose, Some("length")).non_plan_answer(), None);
    assert_eq!(AgentError::PromptMissing("p".into()).non_plan_answer(), None);
}
