//! Thinking suppression — how the router asks a reasoning model *not* to
//! think, in the dialect the configured backend actually understands, and
//! how it notices when the backend ignored the request (issue #773).
//!
//! ## Why a configured dialect, not one key sent to everyone
//!
//! There are two incompatible wire conventions for "don't think":
//!
//! | Dialect | Wire | Honoured by |
//! | --- | --- | --- |
//! | [`ThinkingSwitch::ChatTemplateKwargs`] | `chat_template_kwargs: {"enable_thinking": false}` | vLLM, SGLang, llama.cpp |
//! | [`ThinkingSwitch::ReasoningEffort`] | `reasoning_effort: "none"` | Ollama's `/v1/chat/completions`; vLLM ≥ 0.17 |
//!
//! **Ollama silently ignores the first.** Measured on the DGX 2026-09-27
//! with `gemma4:26b-a4b-it-q8_0`: the kwarg produced 604 completion tokens
//! and 1 289 chars of reasoning, sending nothing produced 674 and 1 396 —
//! indistinguishable — while `reasoning_effort: "none"` produced 6 tokens
//! in 0.4 s. So on the DGX the router's default-on switch had never done
//! anything.
//!
//! **Sending both keys is not safe either.** vLLM 0.15.1 (the
//! `nvcr.io/nvidia/vllm:26.02` image, read from its source 2026-09-27)
//! declares `reasoning_effort: Literal["low", "medium", "high"]`, so a
//! `"none"` is a pydantic 400 — which reaches core as a failed planning
//! call on *every* task. vLLM 0.17 added `"none"`. The operator therefore
//! names the dialect with `KASTELLAN_LLM_THINKING_SWITCH`; the default is
//! the one every non-Ollama backend we run understands, so an existing
//! deployment sends byte-identical requests.
//!
//! ## Why a leak detector as well
//!
//! A config switch that silently does nothing is the bug #773 is about,
//! and a dialect setting can be just as wrong as the old single key was.
//! The response tells us: a backend that thought anyway returns the
//! reasoning (`message.reasoning` / `reasoning_content`) or counts it
//! (`usage.completion_tokens_details.reasoning_tokens`).
//! [`detect_thinking_leak`] reads those after a call on which suppression
//! was requested, and the router logs [`leak_warning`] — once per process,
//! because it is a configuration fact, not a per-call event.

use std::borrow::Cow;

use crate::error::RouterError;
use crate::messages::{ChatRequest, ChatResponse};

/// Environment variable naming the thinking-suppression dialect.
pub const THINKING_SWITCH_ENV: &str = "KASTELLAN_LLM_THINKING_SWITCH";

/// The value `reasoning_effort` carries to mean "do not think".
pub const REASONING_EFFORT_NONE: &str = "none";

/// Ollama stamps this `system_fingerprint` on every OpenAI-compat
/// response (observed on the DGX, Ollama's `/v1/chat/completions`). Used
/// only to make the leak warning name the likely fix — never to decide
/// what to send.
pub const OLLAMA_FINGERPRINT: &str = "fp_ollama";

/// Which wire convention the router uses to suppress thinking.
///
/// See the module docs for which backend honours which, and why the
/// router cannot simply send both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThinkingSwitch {
    /// `chat_template_kwargs: {"enable_thinking": false}` — vLLM, SGLang,
    /// llama.cpp. The default: every existing deployment sent exactly this.
    #[default]
    ChatTemplateKwargs,
    /// `reasoning_effort: "none"` — Ollama (and vLLM ≥ 0.17).
    ReasoningEffort,
}

impl ThinkingSwitch {
    /// The spelling used in `KASTELLAN_LLM_THINKING_SWITCH` and in logs.
    /// It is the wire key itself, so the setting says what goes out.
    pub fn as_str(self) -> &'static str {
        match self {
            ThinkingSwitch::ChatTemplateKwargs => "chat_template_kwargs",
            ThinkingSwitch::ReasoningEffort => "reasoning_effort",
        }
    }

    /// Parse the env-var value (trimmed, case-insensitive).
    ///
    /// Anything else is a config **error**, not a fallback to the
    /// default: a typo that quietly selected the other dialect is exactly
    /// the silent no-op this setting exists to end.
    pub fn parse(raw: &str) -> Result<Self, RouterError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "chat_template_kwargs" => Ok(ThinkingSwitch::ChatTemplateKwargs),
            "reasoning_effort" => Ok(ThinkingSwitch::ReasoningEffort),
            _ => Err(RouterError::Config(format!(
                "{THINKING_SWITCH_ENV} must be `chat_template_kwargs` (vLLM, SGLang, llama.cpp) \
                 or `reasoning_effort` (Ollama), got {raw:?}"
            ))),
        }
    }
}

/// A caller's per-request say over thinking, on top of the router's
/// configured default (`KASTELLAN_LLM_DISABLE_THINKING`).
///
/// Carried on [`ChatRequest::thinking`], which is never serialised: it is
/// an instruction *to the router*, which translates it into whichever
/// dialect [`ThinkingSwitch`] names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThinkingPolicy {
    /// Do whatever the router's config says. Every caller but one uses this.
    #[default]
    RouterDefault,
    /// Suppress thinking on this request even if the config lets the
    /// model think. The forced-synthesis retry after a timeout (#774) is
    /// the motivating case: a cheaper second attempt beats no answer.
    Suppress,
    /// Let the model think on this request even if the config suppresses it.
    Allow,
}

impl ThinkingPolicy {
    /// Does this request end up asking the backend not to think?
    pub fn suppresses(self, config_disables_thinking: bool) -> bool {
        match self {
            ThinkingPolicy::RouterDefault => config_disables_thinking,
            ThinkingPolicy::Suppress => true,
            ThinkingPolicy::Allow => false,
        }
    }
}

/// Write the suppression into `request` in the configured dialect.
///
/// Pure. Borrowed (unchanged) when `suppress` is false, and also when the
/// caller already set the dialect's own key: an explicit caller value
/// always wins, the router only fills a gap it left. Only the *selected*
/// dialect's key is ever added — see the module docs for why sending the
/// other one as well would break vLLM 0.15.
pub fn apply_thinking_switch(
    request: &ChatRequest,
    suppress: bool,
    switch: ThinkingSwitch,
) -> Cow<'_, ChatRequest> {
    if !suppress {
        return Cow::Borrowed(request);
    }
    match switch {
        ThinkingSwitch::ChatTemplateKwargs if request.chat_template_kwargs.is_none() => {
            Cow::Owned(request.clone().without_thinking())
        }
        ThinkingSwitch::ReasoningEffort if request.reasoning_effort.is_none() => {
            let mut owned = request.clone();
            owned.reasoning_effort = Some(REASONING_EFFORT_NONE.to_string());
            Cow::Owned(owned)
        }
        _ => Cow::Borrowed(request),
    }
}

/// Evidence that a backend thought although it was asked not to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThinkingLeak {
    /// Characters of reasoning text the backend returned (first choice).
    pub reasoning_chars: usize,
    /// The backend's own reasoning-token count, when it reports one.
    pub reasoning_tokens: Option<u32>,
    /// The response carries Ollama's fingerprint — lets the warning name
    /// the likely fix.
    pub backend_is_ollama: bool,
}

/// Did the backend think on a call where suppression was requested?
///
/// Pure. `None` when suppression was not requested (thinking was allowed,
/// so reasoning is expected) or when the response shows no reasoning at
/// all. A reported `reasoning_tokens: 0` with no reasoning text is *not* a
/// leak — that is a backend confirming it obeyed.
pub fn detect_thinking_leak(suppressed: bool, response: &ChatResponse) -> Option<ThinkingLeak> {
    if !suppressed {
        return None;
    }
    let reasoning_chars = response
        .choices
        .first()
        .and_then(|c| c.message.reasoning_text())
        .map(|t| t.chars().count())
        .unwrap_or(0);
    let reasoning_tokens = response
        .usage
        .as_ref()
        .and_then(|u| u.completion_tokens_details.as_ref())
        .and_then(|d| d.reasoning_tokens);
    if reasoning_chars == 0 && reasoning_tokens.unwrap_or(0) == 0 {
        return None;
    }
    Some(ThinkingLeak {
        reasoning_chars,
        reasoning_tokens,
        backend_is_ollama: response.system_fingerprint.as_deref() == Some(OLLAMA_FINGERPRINT),
    })
}

/// The operator-facing line for a [`ThinkingLeak`]. Pure.
///
/// Names the setting to change. When the backend is recognisably Ollama
/// and the dialect is the one Ollama ignores, it names the exact value.
pub fn leak_warning(leak: &ThinkingLeak, switch: ThinkingSwitch) -> String {
    let tokens = leak
        .reasoning_tokens
        .map(|t| format!(", {t} reasoning tokens"))
        .unwrap_or_default();
    let hint = if leak.backend_is_ollama && switch == ThinkingSwitch::ChatTemplateKwargs {
        format!(
            "the backend is Ollama, which ignores `chat_template_kwargs`; set \
             {THINKING_SWITCH_ENV}=reasoning_effort"
        )
    } else {
        format!(
            "this backend does not honour the `{}` dialect; check {THINKING_SWITCH_ENV}, or set \
             KASTELLAN_LLM_DISABLE_THINKING=0 to make the thinking deliberate",
            switch.as_str()
        )
    };
    format!(
        "thinking suppression was requested but the backend thought anyway ({} reasoning \
         chars{tokens}): {hint}",
        leak.reasoning_chars
    )
}

#[cfg(test)]
mod tests;
