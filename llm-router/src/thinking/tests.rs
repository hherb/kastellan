//! Unit tests for [`super`] — the thinking-suppression dialect, the
//! per-request policy, and the leak detector.

use super::*;
use crate::messages::{ChatMessage, CompletionTokensDetails, Usage};

fn req() -> ChatRequest {
    ChatRequest::new("m", vec![ChatMessage::user("hi")])
}

/// A response whose first choice carries `reasoning` text and whose usage
/// carries `reasoning_tokens`, either optional.
fn resp(reasoning: Option<&str>, reasoning_tokens: Option<u32>, fp: Option<&str>) -> ChatResponse {
    let mut message = ChatMessage::assistant("{}");
    message.reasoning = reasoning.map(str::to_string);
    ChatResponse {
        id: None,
        model: None,
        system_fingerprint: fp.map(str::to_string),
        choices: vec![crate::messages::ChatChoice {
            index: 0,
            message,
            finish_reason: Some("stop".into()),
            logprobs: None,
        }],
        usage: Some(Usage {
            completion_tokens_details: reasoning_tokens
                .map(|t| CompletionTokensDetails { reasoning_tokens: Some(t) }),
            ..Default::default()
        }),
    }
}

// ── ThinkingSwitch ────────────────────────────────────────────────────────

#[test]
fn switch_default_is_the_dialect_existing_deployments_already_send() {
    // Changing this default would change the bytes every vLLM/llama.cpp
    // deployment sends — and vLLM 0.15 400s on `reasoning_effort: "none"`.
    assert_eq!(ThinkingSwitch::default(), ThinkingSwitch::ChatTemplateKwargs);
}

#[test]
fn switch_parses_both_spellings_trimmed_and_case_insensitively() {
    for (raw, want) in [
        ("chat_template_kwargs", ThinkingSwitch::ChatTemplateKwargs),
        (" Chat_Template_Kwargs ", ThinkingSwitch::ChatTemplateKwargs),
        ("reasoning_effort", ThinkingSwitch::ReasoningEffort),
        ("REASONING_EFFORT\n", ThinkingSwitch::ReasoningEffort),
    ] {
        assert_eq!(ThinkingSwitch::parse(raw).unwrap(), want, "for {raw:?}");
    }
}

#[test]
fn switch_parse_round_trips_as_str() {
    for s in [ThinkingSwitch::ChatTemplateKwargs, ThinkingSwitch::ReasoningEffort] {
        assert_eq!(ThinkingSwitch::parse(s.as_str()).unwrap(), s);
    }
}

#[test]
fn switch_rejects_anything_else_and_names_the_variable() {
    // `ollama` is the tempting wrong answer: it names a backend, not a
    // dialect, and would read as valid to anyone who knows the DGX setup.
    for raw in ["ollama", "none", "kwargs", "1"] {
        let err = ThinkingSwitch::parse(raw).expect_err(raw).to_string();
        assert!(err.contains(THINKING_SWITCH_ENV), "{err}");
        assert!(err.contains("reasoning_effort"), "must name the valid values: {err}");
    }
}

// ── ThinkingPolicy ────────────────────────────────────────────────────────

#[test]
fn policy_router_default_follows_the_config_and_overrides_win() {
    for config in [true, false] {
        assert_eq!(ThinkingPolicy::RouterDefault.suppresses(config), config);
        assert!(ThinkingPolicy::Suppress.suppresses(config));
        assert!(!ThinkingPolicy::Allow.suppresses(config));
    }
    assert_eq!(ThinkingPolicy::default(), ThinkingPolicy::RouterDefault);
}

// ── apply_thinking_switch ─────────────────────────────────────────────────

#[test]
fn apply_leaves_the_request_untouched_when_not_suppressing() {
    let r = req();
    for switch in [ThinkingSwitch::ChatTemplateKwargs, ThinkingSwitch::ReasoningEffort] {
        let out = apply_thinking_switch(&r, false, switch);
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(*out, r);
    }
}

#[test]
fn apply_kwargs_dialect_sets_only_the_kwarg() {
    let r = req();
    let out = apply_thinking_switch(&r, true, ThinkingSwitch::ChatTemplateKwargs);
    assert_eq!(
        out.chat_template_kwargs,
        Some(serde_json::json!({"enable_thinking": false}))
    );
    assert_eq!(out.reasoning_effort, None, "the other dialect's key must never ride along");
}

#[test]
fn apply_reasoning_effort_dialect_sets_only_reasoning_effort_none() {
    let r = req();
    let out = apply_thinking_switch(&r, true, ThinkingSwitch::ReasoningEffort);
    assert_eq!(out.reasoning_effort.as_deref(), Some("none"));
    assert_eq!(out.chat_template_kwargs, None, "the other dialect's key must never ride along");
    // And on the wire, where it counts.
    let wire = serde_json::to_value(&*out).unwrap();
    assert_eq!(wire["reasoning_effort"], "none");
    assert!(wire.get("chat_template_kwargs").is_none(), "{wire}");
}

#[test]
fn apply_never_overwrites_the_callers_own_key() {
    let mut r = req();
    r.chat_template_kwargs = Some(serde_json::json!({"enable_thinking": true}));
    let out = apply_thinking_switch(&r, true, ThinkingSwitch::ChatTemplateKwargs);
    assert!(matches!(out, Cow::Borrowed(_)));

    let mut r = req();
    r.reasoning_effort = Some("high".into());
    let out = apply_thinking_switch(&r, true, ThinkingSwitch::ReasoningEffort);
    assert_eq!(out.reasoning_effort.as_deref(), Some("high"));
}

// ── detect_thinking_leak / leak_warning ───────────────────────────────────

#[test]
fn no_leak_when_thinking_was_allowed() {
    assert_eq!(detect_thinking_leak(false, &resp(Some("hmm, let me think"), Some(40), None)), None);
}

#[test]
fn no_leak_when_the_response_shows_no_reasoning() {
    assert_eq!(detect_thinking_leak(true, &resp(None, None, None)), None);
    assert_eq!(detect_thinking_leak(true, &resp(Some(""), None, None)), None);
    // A backend reporting zero reasoning tokens is confirming it obeyed.
    assert_eq!(detect_thinking_leak(true, &resp(None, Some(0), None)), None);
}

#[test]
fn leak_is_detected_from_reasoning_text_alone() {
    // Ollama's shape: reasoning text, no reasoning-token count.
    let leak = detect_thinking_leak(true, &resp(Some("abcé"), None, Some("fp_ollama"))).unwrap();
    assert_eq!(leak.reasoning_chars, 4, "counts chars, not bytes");
    assert_eq!(leak.reasoning_tokens, None);
    assert!(leak.backend_is_ollama);
}

#[test]
fn leak_is_detected_from_the_token_count_alone() {
    // A backend that hides the text but counts it.
    let leak = detect_thinking_leak(true, &resp(None, Some(120), None)).unwrap();
    assert_eq!(leak.reasoning_chars, 0);
    assert_eq!(leak.reasoning_tokens, Some(120));
    assert!(!leak.backend_is_ollama);
}

#[test]
fn leak_is_detected_from_legacy_reasoning_content() {
    let mut r = resp(None, None, None);
    r.choices[0].message.reasoning_content = Some("older vLLM spelling".into());
    let leak = detect_thinking_leak(true, &r).unwrap();
    assert_eq!(leak.reasoning_chars, "older vLLM spelling".chars().count());
}

#[test]
fn warning_names_the_exact_fix_for_ollama_on_the_kwargs_dialect() {
    let leak = ThinkingLeak { reasoning_chars: 1289, reasoning_tokens: None, backend_is_ollama: true };
    let w = leak_warning(&leak, ThinkingSwitch::ChatTemplateKwargs, true);
    assert!(w.contains("1289 reasoning chars"), "{w}");
    assert!(w.contains("KASTELLAN_LLM_THINKING_SWITCH=reasoning_effort"), "{w}");
}

#[test]
fn warning_on_a_dialect_that_should_have_worked_points_at_both_settings() {
    // Ollama on the right dialect, or an unknown backend: no exact fix is
    // knowable, so name both levers rather than guess.
    for (ollama, switch) in [
        (true, ThinkingSwitch::ReasoningEffort),
        (false, ThinkingSwitch::ChatTemplateKwargs),
    ] {
        let leak = ThinkingLeak { reasoning_chars: 10, reasoning_tokens: Some(7), backend_is_ollama: ollama };
        let w = leak_warning(&leak, switch, true);
        assert!(w.contains(switch.as_str()), "{w}");
        assert!(w.contains("KASTELLAN_LLM_DISABLE_THINKING=0"), "{w}");
        assert!(w.contains("7 reasoning tokens"), "{w}");
        assert!(!w.contains("=reasoning_effort"), "must not claim an exact fix: {w}");
    }
}

/// When a caller's per-request suppress (the #774 synthesis retry) leaked,
/// the config already lets the model think — "set it to 0" is no lever.
#[test]
fn warning_for_a_per_request_suppress_does_not_suggest_the_config_it_already_has() {
    let leak = ThinkingLeak { reasoning_chars: 10, reasoning_tokens: None, backend_is_ollama: false };
    let w = leak_warning(&leak, ThinkingSwitch::ChatTemplateKwargs, false);
    assert!(w.contains(THINKING_SWITCH_ENV), "{w}");
    assert!(!w.contains("KASTELLAN_LLM_DISABLE_THINKING"), "{w}");
}
