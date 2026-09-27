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

use crate::thinking::ThinkingPolicy;

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

    /// The model's reasoning, when a backend returns it beside `content`
    /// (Ollama and current vLLM spell it `reasoning`). Read only to
    /// *measure* thinking — its length feeds the plan audit row and the
    /// thinking-leak check (#773, #774); the text itself is never logged.
    ///
    /// **Deserialise-only** (`skip_serializing`): a message read from a
    /// backend and handed back in a later request must not replay the
    /// model's private reasoning to it, and an audit serialisation of a
    /// response must not carry it either.
    #[serde(default, skip_serializing)]
    pub reasoning: Option<String>,

    /// The older spelling of [`ChatMessage::reasoning`] (vLLM before the
    /// rename, DeepSeek-style servers). A **separate field, not a serde
    /// `alias`**: some builds send both keys, and an alias turns a
    /// duplicate into a hard decode error — every planning call failing
    /// over a field we only count. Read through
    /// [`ChatMessage::reasoning_text`].
    #[serde(default, skip_serializing)]
    pub reasoning_content: Option<String>,
}

impl ChatMessage {
    fn with_role(role: ChatRole, content: impl Into<String>) -> Self {
        Self { role, content: content.into(), reasoning: None, reasoning_content: None }
    }
    pub fn system(content: impl Into<String>) -> Self {
        Self::with_role(ChatRole::System, content)
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self::with_role(ChatRole::User, content)
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::with_role(ChatRole::Assistant, content)
    }

    /// The reasoning text under whichever key the backend used, `None`
    /// when absent or empty. `reasoning` wins when both are present.
    pub fn reasoning_text(&self) -> Option<&str> {
        [self.reasoning.as_deref(), self.reasoning_content.as_deref()]
            .into_iter()
            .flatten()
            .find(|t| !t.is_empty())
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

    /// Extra keyword arguments forwarded to the backend's chat
    /// *template* (not to sampling). This is the de-facto OpenAI-compat
    /// extension both Ollama and vLLM honour; it is the only portable
    /// way to reach a vLLM/SGLang/llama.cpp reasoning model's
    /// `enable_thinking` switch. **Ollama ignores it** — see
    /// [`crate::thinking`] (#773).
    ///
    /// Left `None` the field is not serialised at all, so a backend
    /// that has never heard of it sees a byte-identical payload. Normally
    /// set by the router from its configured dialect, not by hand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_template_kwargs: Option<serde_json::Value>,

    /// OpenAI's reasoning-effort knob. `"none"` is how Ollama's
    /// OpenAI-compat endpoint is told not to think. Left `None` it is not
    /// serialised — and it must stay unset on vLLM 0.15, which 400s on
    /// `"none"`. Normally set by the router from its configured dialect
    /// ([`crate::thinking::ThinkingSwitch`]), not by hand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,

    /// The caller's say over thinking for this one request. **Never on the
    /// wire** (`serde(skip)`): it is an instruction to the router, which
    /// writes it in whichever dialect the backend understands. See
    /// [`ChatRequest::with_thinking`].
    #[serde(skip)]
    pub thinking: ThinkingPolicy,

    /// Ask the backend to return per-token log-probabilities.
    ///
    /// Needed by classifier models that are read from the *distribution*
    /// at the first output position rather than from their emitted text —
    /// a `yes`/`no` safety classifier renormalised into a calibrated score
    /// is the motivating case. Without it such a model degrades to a bare
    /// verdict token, i.e. a hard threshold with no confidence band.
    ///
    /// Set both this and [`ChatRequest::top_logprobs`] together via
    /// [`ChatRequest::with_logprobs`]: vLLM rejects a `top_logprobs`
    /// arriving without `logprobs: true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<bool>,

    /// How many alternatives to return at each position (OpenAI caps this
    /// at 20). Only meaningful with `logprobs: true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_logprobs: Option<u8>,
}

impl ChatRequest {
    pub fn new(model: impl Into<String>, messages: Vec<ChatMessage>) -> Self {
        Self {
            model: model.into(),
            messages,
            max_tokens: None,
            temperature: None,
            chat_template_kwargs: None,
            reasoning_effort: None,
            thinking: ThinkingPolicy::RouterDefault,
            logprobs: None,
            top_logprobs: None,
        }
    }

    /// Override the router's thinking default for this request.
    pub fn with_thinking(mut self, policy: ThinkingPolicy) -> Self {
        self.thinking = policy;
        self
    }

    /// Ask for `top_n` token alternatives at each output position.
    ///
    /// Sets `logprobs: true` as well, because the two are one decision on
    /// the wire — vLLM 4xxs on a `top_logprobs` that arrives without it,
    /// and that failure reaches core as a `RouterError::Transport`, which
    /// reads as "the backend is unreachable" rather than "your request was
    /// malformed" (the misdiagnosis that cost the whole #505 session).
    ///
    /// Measured safe to combine with [`ChatRequest::without_thinking`]:
    /// against Shieldstral on llama.cpp the `chat_template_kwargs` key is
    /// accepted and inert — both calls returned 200 with an identical
    /// 26-token prompt, the second reporting `cached_tokens: 25`, which is
    /// positive evidence the rendered prompt was byte-identical rather
    /// than merely "no error".
    pub fn with_logprobs(mut self, top_n: u8) -> Self {
        self.logprobs = Some(true);
        self.top_logprobs = Some(top_n);
        self
    }

    /// Ask the backend's chat template to skip the model's thinking
    /// block (`chat_template_kwargs: {"enable_thinking": false}`).
    ///
    /// A reasoning model that thinks freely can spend the whole
    /// generation budget — and far more wall-clock than any sane
    /// request timeout — on `reasoning` while emitting an empty
    /// `content`. Measured on the DGX with the 26B local planner and a
    /// ~16k-token prompt: 222 s and 15 094 chars of reasoning for
    /// 1 519 chars of plan, versus 51 s with this set. Both failure
    /// modes downstream (transport timeout, and a `content` so empty
    /// that plan decoding reports `expected value at line 1 column 1`)
    /// trace back to it.
    ///
    /// This is the [`crate::thinking::ThinkingSwitch::ChatTemplateKwargs`]
    /// dialect. A backend that does not implement it ignores the key
    /// *silently* — Ollama does, which is how the DGX thought on every
    /// planning call while configured not to (#773). The router picks the
    /// dialect from config; see [`crate::thinking`].
    pub fn without_thinking(mut self) -> Self {
        self.chat_template_kwargs =
            Some(serde_json::json!({ "enable_thinking": false }));
        self
    }
}

/// One alternative token the backend considered at a given position,
/// with its log-probability.
///
/// `bytes` is the token's raw UTF-8, which OpenAI-compatible backends
/// return alongside the display form. It is the *reliable* identity of a
/// token: tokenizers render the same word with family-specific markers
/// (`Ġyes` for byte-BPE, `▁yes` for SentencePiece) that no amount of
/// trimming removes, whereas the bytes decode to plain ` yes` on every
/// one of them. See [`crate::logprob_score`], which prefers it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TopLogProb {
    pub token: String,
    pub logprob: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<Vec<u8>>,
}

/// The token actually chosen at one position, plus the alternatives that
/// were in contention there.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenLogProbs {
    pub token: String,
    pub logprob: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<Vec<u8>>,
    /// Empty when the backend returned `logprobs: true` but no
    /// `top_logprobs` count — a shape that carries no distribution to
    /// renormalise, and which callers must treat as unmeasurable.
    #[serde(default)]
    pub top_logprobs: Vec<TopLogProb>,
}

/// Per-position log-probabilities for one choice.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogProbs {
    #[serde(default)]
    pub content: Vec<TokenLogProbs>,
}

/// One completion choice returned by the backend.
///
/// We model `index` and `finish_reason` because they're load-bearing
/// for downstream callers (Phase 1's scheduler will branch on
/// `finish_reason == "length"` to retry with a higher `max_tokens`),
/// but we do *not* require them to be present — vLLM omits
/// `finish_reason` when streaming is disabled and the response is
/// truncated mid-token.
///
/// Note this is deliberately **not** `Eq`: `logprobs` carries floats, and
/// a derived `Eq` on a struct holding log-probabilities would invite
/// exact-equality comparisons on values that are the output of a softmax.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatChoice {
    #[serde(default)]
    pub index: u32,
    pub message: ChatMessage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    /// Present only when the request asked for it. Every call the planner
    /// makes leaves this `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<LogProbs>,
}

/// How many of `prompt_tokens` the backend served from its prefix
/// cache instead of processing.
///
/// OpenAI defines this block; llama.cpp's OpenAI-compat endpoint serves
/// it too (measured on the DGX guard server 2026-08-23: a repeated
/// document came back `{"prompt_tokens":810,"prompt_tokens_details":
/// {"cached_tokens":809}}` in 38 ms).
///
/// **It is read as a correctness signal, not as telemetry.** The guard
/// tier's boot probe derives a prompt-processing throughput from
/// tokens-per-second, and a cached token was never processed. Dividing
/// by wall clock without subtracting them read **21,094 tok/s** against
/// the same server's true ~5,000 — a 4x over-estimate, which derives a
/// timeout 4x too short and so turns real adjudications into fail-open
/// timeouts. See `kastellan_core::cassandra::guard_model::timeout` and
/// M2 in the wiring spec.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptTokensDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u32>,
}

/// Token-accounting envelope returned by the backend.
///
/// Phase 0 forwards this through unchanged; Phase 1+ will read it for
/// budgeting decisions in the scheduler's context-manager.
///
/// Every field is `Option` because backends report the block
/// **partially** — a count they track and one they do not. (The
/// separate case of Ollama and some llama.cpp builds omitting `usage`
/// altogether is why the containing field on [`ChatResponse`] is an
/// `Option`, not why these are.)
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u32>,
    /// Absent on every backend that does not report a prefix cache.
    /// Absence means "we do not know", never "nothing was cached".
    ///
    /// The guard probe collapses a missing block to zero cached tokens
    /// and its uncached-token floor does **not** rescue that case: the
    /// subtraction happens first, and an unreported cache leaves
    /// `prompt_tokens` intact and well above the floor. The defence
    /// there is the probe's cache-buster, not the floor — see
    /// `kastellan_core::cassandra::guard_model::timeout::probe_sample`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
    /// OpenAI's breakdown of `completion_tokens`. Carries the reasoning
    /// count on backends that report one; Ollama does not (it returns the
    /// reasoning text instead — see [`ChatMessage::reasoning`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens_details: Option<CompletionTokensDetails>,
}

/// How many of `completion_tokens` were spent reasoning, when the backend
/// says. Absence means "not reported", never "zero" (#774).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionTokensDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u32>,
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
    /// Identifies the serving stack on some backends — Ollama stamps
    /// `fp_ollama`, which lets a thinking-leak warning name the fix
    /// ([`crate::thinking::leak_warning`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_fingerprint: Option<String>,
    pub choices: Vec<ChatChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[cfg(test)]
mod tests;
