//! [`CompletionStats`] — what one chat completion cost, in a shape fit for
//! an audit row (issue #774).
//!
//! Before this, a `plan.formulate` row carried `latency_ms` and nothing
//! else about the call, so "why did planning get slow?" took a synthetic
//! benchmark on the live host. With it, the answer is one query: prompt
//! size, generated size, and how much of the generation was hidden
//! reasoning (#773 — on the DGX it was most of it).
//!
//! **Only counts, never text.** The reasoning a backend returns restates
//! the user's data (on the mail tasks, their correspondence), and the
//! audit row is not the place for it. Its *length* is enough to see
//! thinking; its words stay in memory for the one call.

use serde::Serialize;

use crate::messages::ChatResponse;

/// Token and reasoning accounting for one completion.
///
/// Every count is `Option` because backends report `usage` partially or
/// not at all, and **absence means "not reported", never zero** — the same
/// convention as [`crate::messages::Usage`]. Serialised with explicit
/// `null`s (no `skip_serializing_if`) so an audit query can tell "the
/// backend did not say" from "the key was never written".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CompletionStats {
    /// Tokens in the prompt the backend processed (cached ones included).
    pub prompt_tokens: Option<u32>,
    /// How many of `prompt_tokens` came from the backend's prefix cache.
    pub cached_prompt_tokens: Option<u32>,
    /// Tokens generated — reasoning included, on every backend we run.
    pub completion_tokens: Option<u32>,
    /// The backend's own count of reasoning tokens, where it reports one
    /// (OpenAI-style `completion_tokens_details`). Ollama does not.
    pub reasoning_tokens: Option<u32>,
    /// Characters of reasoning text returned with the first choice. Always
    /// measurable, so not an `Option`: `0` means none came back — either
    /// the model did not think, or the backend hid it (then see
    /// `reasoning_tokens`).
    pub reasoning_chars: usize,
    /// The first choice's `finish_reason` (`"stop"`, `"length"`, …).
    pub finish_reason: Option<String>,
}

impl CompletionStats {
    /// Read the stats off a decoded response. Pure.
    pub fn from_response(resp: &ChatResponse) -> Self {
        let usage = resp.usage.as_ref();
        let first = resp.choices.first();
        Self {
            prompt_tokens: usage.and_then(|u| u.prompt_tokens),
            cached_prompt_tokens: usage
                .and_then(|u| u.prompt_tokens_details.as_ref())
                .and_then(|d| d.cached_tokens),
            completion_tokens: usage.and_then(|u| u.completion_tokens),
            reasoning_tokens: usage
                .and_then(|u| u.completion_tokens_details.as_ref())
                .and_then(|d| d.reasoning_tokens),
            reasoning_chars: first
                .and_then(|c| c.message.reasoning_text())
                .map(|t| t.chars().count())
                .unwrap_or(0),
            finish_reason: first.and_then(|c| c.finish_reason.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_every_count_from_a_full_response() {
        let resp: ChatResponse = serde_json::from_value(json!({
            "choices": [{"message": {"role": "assistant", "content": "{}",
                "reasoning": "thought é"}, "finish_reason": "length"}],
            "usage": {"prompt_tokens": 20800, "completion_tokens": 4000,
                "prompt_tokens_details": {"cached_tokens": 19000},
                "completion_tokens_details": {"reasoning_tokens": 3500}}
        }))
        .unwrap();
        assert_eq!(
            CompletionStats::from_response(&resp),
            CompletionStats {
                prompt_tokens: Some(20800),
                cached_prompt_tokens: Some(19000),
                completion_tokens: Some(4000),
                reasoning_tokens: Some(3500),
                reasoning_chars: 9,
                finish_reason: Some("length".into()),
            }
        );
    }

    #[test]
    fn a_response_without_usage_reports_unknowns_not_zeros() {
        let resp: ChatResponse = serde_json::from_value(json!({
            "choices": [{"message": {"role": "assistant", "content": "{}"}}]
        }))
        .unwrap();
        assert_eq!(CompletionStats::from_response(&resp), CompletionStats::default());
    }

    #[test]
    fn serialises_every_key_with_explicit_nulls_and_no_text() {
        let v = serde_json::to_value(CompletionStats {
            completion_tokens: Some(674),
            reasoning_chars: 1396,
            ..Default::default()
        })
        .unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 6, "{v}");
        assert_eq!(v["completion_tokens"], 674);
        assert_eq!(v["reasoning_chars"], 1396);
        assert!(v["prompt_tokens"].is_null() && v["reasoning_tokens"].is_null(), "{v}");
    }
}
