//! Unit tests for [`super`] — the OpenAI-compatible wire types.
//!
//! Moved verbatim (de-indented one level) from the inline `mod tests`
//! block at the tail of `messages.rs`.

use super::*;
use serde_json::json;

#[test]
fn chat_role_serializes_as_lowercase() {
    // Wire-shape pin: any change here rotates the contract with
    // every OpenAI-compatible backend on the planet.
    assert_eq!(serde_json::to_string(&ChatRole::System).unwrap(), "\"system\"");
    assert_eq!(serde_json::to_string(&ChatRole::User).unwrap(), "\"user\"");
    assert_eq!(serde_json::to_string(&ChatRole::Assistant).unwrap(), "\"assistant\"");
    assert_eq!(serde_json::to_string(&ChatRole::Tool).unwrap(), "\"tool\"");
}

#[test]
fn chat_role_rejects_unknown_string() {
    // Closed enum: deserialising "developer" must fail rather than
    // silently fall back. If we ever add Developer as a role this
    // test will fail at the right moment.
    let err = serde_json::from_str::<ChatRole>("\"developer\"").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("unknown variant"), "expected 'unknown variant' in {msg:?}");
}

#[test]
fn chat_message_constructors_set_the_right_role() {
    assert_eq!(ChatMessage::system("hi").role, ChatRole::System);
    assert_eq!(ChatMessage::user("hi").role, ChatRole::User);
    assert_eq!(ChatMessage::assistant("hi").role, ChatRole::Assistant);
}

#[test]
fn chat_request_omits_none_fields_on_the_wire() {
    // Some local backends (older llama.cpp builds especially) reject
    // requests that include explicit nulls. The
    // `skip_serializing_if = Option::is_none` pin guards against a
    // refactor that drops it.
    let req = ChatRequest::new("local-model", vec![ChatMessage::user("hi")]);
    let s = serde_json::to_string(&req).unwrap();
    assert!(!s.contains("max_tokens"), "max_tokens leaked: {s}");
    assert!(!s.contains("temperature"), "temperature leaked: {s}");
    assert!(s.contains("\"model\":\"local-model\""), "model missing in {s}");
}

#[test]
fn chat_request_includes_optional_fields_when_set() {
    let req = ChatRequest {
        model: "m".into(),
        messages: vec![ChatMessage::user("hi")],
        max_tokens: Some(42),
        temperature: Some(0.7),
        chat_template_kwargs: None,
        logprobs: None,
        top_logprobs: None,
    };
    let s = serde_json::to_string(&req).unwrap();
    assert!(s.contains("\"max_tokens\":42"), "max_tokens missing: {s}");
    assert!(s.contains("\"temperature\":0.7"), "temperature missing: {s}");
}

/// An untouched request must stay byte-identical on the wire — a
/// backend that has never heard of `chat_template_kwargs` must not
/// start seeing it just because the field exists in the struct.
#[test]
fn chat_template_kwargs_is_absent_unless_asked_for() {
    let req = ChatRequest::new("m", vec![ChatMessage::user("hi")]);
    let s = serde_json::to_string(&req).unwrap();
    assert!(
        !s.contains("chat_template_kwargs"),
        "chat_template_kwargs leaked into an untouched request: {s}"
    );
}

/// The exact wire shape both Ollama and vLLM look for. Pinned
/// because a typo here fails silently: the backend ignores the
/// unknown key and the model thinks anyway.
#[test]
fn without_thinking_emits_the_enable_thinking_false_kwarg() {
    let req =
        ChatRequest::new("m", vec![ChatMessage::user("hi")]).without_thinking();
    let v: serde_json::Value =
        serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
    assert_eq!(
        v["chat_template_kwargs"]["enable_thinking"],
        serde_json::Value::Bool(false),
        "unexpected wire shape: {v}"
    );
}

/// `without_thinking` must not disturb anything else the caller set.
#[test]
fn without_thinking_preserves_the_other_fields() {
    let req = ChatRequest {
        model: "m".into(),
        messages: vec![ChatMessage::user("hi")],
        max_tokens: Some(8192),
        temperature: Some(0.2),
        chat_template_kwargs: None,
        logprobs: None,
        top_logprobs: None,
    }
    .without_thinking();
    assert_eq!(req.model, "m");
    assert_eq!(req.max_tokens, Some(8192));
    assert_eq!(req.temperature, Some(0.2));
    assert_eq!(req.messages.len(), 1);
}

/// The prefix-cache block decodes when present, and its absence
/// stays absent rather than defaulting to zero.
///
/// The distinction is load-bearing: the guard probe must be able to
/// tell "this backend reports no cache" from "nothing was cached",
/// because only the second is a number it may divide by. Verbatim
/// shape from the DGX guard server, 2026-08-23.
#[test]
fn usage_decodes_prompt_tokens_details_and_tolerates_its_absence() {
    let with: Usage = serde_json::from_value(json!({
        "prompt_tokens": 810,
        "completion_tokens": 1,
        "total_tokens": 811,
        "prompt_tokens_details": {"cached_tokens": 809}
    }))
    .unwrap();
    assert_eq!(with.prompt_tokens, Some(810));
    assert_eq!(
        with.prompt_tokens_details.and_then(|d| d.cached_tokens),
        Some(809)
    );

    // A backend that reports `usage` but no cache block at all.
    let without: Usage = serde_json::from_value(json!({
        "prompt_tokens": 11, "completion_tokens": 3, "total_tokens": 14
    }))
    .unwrap();
    assert!(
        without.prompt_tokens_details.is_none(),
        "a missing block must stay None, not become Some(cached_tokens: 0)"
    );

    // Present but empty — llama.cpp omits `cached_tokens` on a
    // fully-cold request in some builds.
    let empty: Usage =
        serde_json::from_value(json!({"prompt_tokens": 810, "prompt_tokens_details": {}}))
            .unwrap();
    assert_eq!(
        empty.prompt_tokens_details.and_then(|d| d.cached_tokens),
        None
    );
}

#[test]
fn chat_response_decodes_canonical_openai_envelope() {
    // Hand-crafted to match what a vLLM 0.5+ server returns; the
    // `system_fingerprint` field is absent on purpose to prove
    // `serde(default)` fields tolerate missing keys.
    let raw = json!({
        "id": "chatcmpl-abc",
        "object": "chat.completion",
        "created": 1_700_000_000_u64,
        "model": "Qwen/Qwen2.5-7B-Instruct",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "hello back"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 11, "completion_tokens": 3, "total_tokens": 14}
    });
    let resp: ChatResponse = serde_json::from_value(raw).unwrap();
    assert_eq!(resp.id.as_deref(), Some("chatcmpl-abc"));
    assert_eq!(resp.model.as_deref(), Some("Qwen/Qwen2.5-7B-Instruct"));
    assert_eq!(resp.choices.len(), 1);
    assert_eq!(resp.choices[0].message.role, ChatRole::Assistant);
    assert_eq!(resp.choices[0].message.content, "hello back");
    assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
    let usage = resp.usage.unwrap();
    assert_eq!(usage.prompt_tokens, Some(11));
    assert_eq!(usage.total_tokens, Some(14));
}

/// The planner path must stay byte-identical to what it sends today:
/// neither logprobs field may appear on a request nobody asked to
/// score. Same guarantee `chat_template_kwargs` carries, and the same
/// reason — a backend that has never heard of the field must not start
/// seeing it merely because the struct grew.
#[test]
fn logprobs_fields_are_absent_unless_asked_for() {
    let req = ChatRequest::new("m", vec![ChatMessage::user("hi")]);
    let s = serde_json::to_string(&req).unwrap();
    assert!(!s.contains("logprobs"), "logprobs leaked: {s}");
    assert!(!s.contains("top_logprobs"), "top_logprobs leaked: {s}");
}

/// The exact pair OpenAI-compatible backends look for. `logprobs` is a
/// bool and `top_logprobs` a count, and sending only the count is a 4xx
/// on vLLM — so the builder sets both or neither.
#[test]
fn with_logprobs_emits_the_openai_wire_shape() {
    let req =
        ChatRequest::new("m", vec![ChatMessage::user("hi")]).with_logprobs(20);
    let v: serde_json::Value =
        serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
    assert_eq!(v["logprobs"], serde_json::Value::Bool(true), "shape: {v}");
    assert_eq!(v["top_logprobs"], serde_json::json!(20), "shape: {v}");
}

/// `with_logprobs` must not disturb anything else the caller set —
/// notably `chat_template_kwargs`, which the local leg stamps on
/// unconditionally.
#[test]
fn with_logprobs_preserves_the_other_fields() {
    let req = ChatRequest::new("m", vec![ChatMessage::user("hi")])
        .without_thinking()
        .with_logprobs(5);
    assert_eq!(req.model, "m");
    assert_eq!(req.messages.len(), 1);
    assert!(req.chat_template_kwargs.is_some());
    assert_eq!(req.top_logprobs, Some(5));
}

/// Decoded from a real response measured against DGX Ollama 0.22.0 on
/// 2026-08-16 (`/v1/chat/completions`, `top_logprobs: 5`) rather than
/// reconstructed by hand — a fixture written from what we believe the
/// shape to be pins our belief, not the wire ([[#566's lesson]]).
#[test]
fn chat_response_decodes_the_logprobs_envelope() {
    let raw = json!({
        "id": "chatcmpl-800",
        "model": "gemma4:26b-a4b-it-q8_0-ctx64k",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": ""},
            "finish_reason": "length",
            "logprobs": {"content": [{
                "token": "yes",
                "logprob": -0.000000004074,
                "bytes": [121, 101, 115],
                "top_logprobs": [
                    {"token": "yes", "logprob": -0.000000004074, "bytes": [121, 101, 115]},
                    {"token": "no",  "logprob": -20.2255,        "bytes": [110, 111]}
                ]
            }]}
        }],
        "usage": {"prompt_tokens": 28, "completion_tokens": 1, "total_tokens": 29}
    });
    let resp: ChatResponse = serde_json::from_value(raw).unwrap();
    let lp = resp.choices[0].logprobs.as_ref().expect("logprobs decoded");
    assert_eq!(lp.content.len(), 1);
    assert_eq!(lp.content[0].token, "yes");
    assert_eq!(lp.content[0].top_logprobs.len(), 2);
    assert_eq!(lp.content[0].top_logprobs[1].token, "no");
    assert_eq!(
        lp.content[0].top_logprobs[0].bytes.as_deref(),
        Some([121u8, 101, 115].as_slice())
    );
}

/// Every backend that returns no logprobs — which is every call the
/// planner makes — must keep decoding exactly as before.
#[test]
fn chat_response_without_logprobs_decodes_with_none() {
    let raw = json!({
        "model": "m",
        "choices": [{"message": {"role": "assistant", "content": "ok"}}]
    });
    let resp: ChatResponse = serde_json::from_value(raw).unwrap();
    assert!(resp.choices[0].logprobs.is_none());
}

#[test]
fn chat_response_decodes_minimal_ollama_envelope() {
    // Ollama's OpenAI-compat front door omits `usage` entirely when
    // the underlying GGUF runtime didn't surface it. This test pins
    // that the decoder accepts the absence rather than failing.
    let raw = json!({
        "model": "llama3.2:3b",
        "choices": [{
            "message": {"role": "assistant", "content": "ok"}
        }]
    });
    let resp: ChatResponse = serde_json::from_value(raw).unwrap();
    assert!(resp.id.is_none());
    assert!(resp.usage.is_none());
    assert!(resp.choices[0].finish_reason.is_none());
    assert_eq!(resp.choices[0].index, 0);
}
