//! OpenAI-compatible chat-completion request and response types.
//!
//! These are the wire shapes for `POST <base>/chat/completions` against
//! any OpenAI-compatible HTTP endpoint:
//!
//! * vLLM and SGLang on Linux (the canonical local-backend choices).
//! * llama.cpp's `--api` server and Ollama on macOS (Ollama's
//!   `/v1/chat/completions` endpoint follows the same shape).
//! * Any frontier backend with an OpenAI-compatible front door (which
//!   today includes every commercial provider that matters).
//!
//! We deliberately model **only** the subset of fields the router
//! actually reads or writes for Phase 0. Streaming SSE, tool-call
//! arguments, function definitions, response-format JSON schemas, and
//! image/audio modalities all live behind the same endpoint but slot
//! in later. Today's contract: a list of role-tagged text messages
//! goes out, a single completion text comes back.
//!
//! ## Why we use `serde(rename_all = "lowercase")` for [`ChatRole`]
//! OpenAI's spec serialises roles as the bare lowercase strings
//! `"user"`, `"system"`, `"assistant"`, `"tool"`. A future addition
//! (e.g. `"developer"`) will require an explicit enum variant — we'd
//! rather break the build at compile time than silently round-trip
//! an unknown role as a stringly-typed escape hatch. The `Tool`
//! variant is included now even though Phase 0 does not invoke
//! function calling: keeping the enum closed-but-complete makes the
//! eventual tool-call slice a pure-Rust addition rather than a wire-
//! shape change.

use serde::{Deserialize, Serialize};

/// Role of the speaker in a chat-completion message.
///
/// Closed enum on purpose — see module docstring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatRole {
    System,
    User,
    Assistant,
    Tool,
}

/// A single role-tagged text message in a chat conversation.
///
/// We do not attempt to model multimodal `content` (the OpenAI spec
/// permits a list of `{type, text|image_url, ...}` parts). For
/// Phase 0 the router carries plain text only; widening this later
/// is a backwards-compatible enum swap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: ChatRole::System, content: content.into() }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: ChatRole::User, content: content.into() }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self { role: ChatRole::Assistant, content: content.into() }
    }
}

/// Outgoing chat-completion request.
///
/// `max_tokens` and `temperature` are `Option` so callers can defer
/// to backend defaults; serde's `skip_serializing_if = Option::is_none`
/// keeps the wire payload minimal — some local backends choke on
/// nulls in optional fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
}

impl ChatRequest {
    pub fn new(model: impl Into<String>, messages: Vec<ChatMessage>) -> Self {
        Self { model: model.into(), messages, max_tokens: None, temperature: None }
    }
}

/// One completion choice returned by the backend.
///
/// We model `index` and `finish_reason` because they're load-bearing
/// for downstream callers (Phase 1's scheduler will branch on
/// `finish_reason == "length"` to retry with a higher `max_tokens`),
/// but we do *not* require them to be present — vLLM omits
/// `finish_reason` when streaming is disabled and the response is
/// truncated mid-token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatChoice {
    #[serde(default)]
    pub index: u32,
    pub message: ChatMessage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

/// Token-accounting envelope returned by the backend.
///
/// Phase 0 forwards this through unchanged; Phase 1+ will read it for
/// budgeting decisions in the scheduler's context-manager. All three
/// fields are `Option` because Ollama and some llama.cpp builds omit
/// the `usage` block entirely when the request was a non-streaming
/// completion.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u32>,
}

/// Decoded `200 OK` response from a chat-completion call.
///
/// The OpenAI envelope also carries `id`, `object`, `created`, and
/// `model`; we keep the first three as opaque strings (or absent) and
/// echo `model` because operators want to see which model actually
/// served the call (some backends do model-fallback transparently).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub choices: Vec<ChatChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[cfg(test)]
mod tests {
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
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"max_tokens\":42"), "max_tokens missing: {s}");
        assert!(s.contains("\"temperature\":0.7"), "temperature missing: {s}");
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
}
