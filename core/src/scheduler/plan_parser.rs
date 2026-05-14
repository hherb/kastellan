//! Lenient parse of an LLM response into a [`Plan`].
//!
//! The agent's `RouterAgent::formulate_plan` originally called
//! `serde_json::from_str` directly on the LLM's raw response. That works
//! when the model emits bare JSON, but most instruction-tuned local
//! models wrap their output in a ```json … ``` markdown fence by default
//! (gemma4, Qwen3 with thinking off, llama3 instruct, etc.). Prompt
//! engineering can usually suppress the fence, but the suppression is
//! advisory — at observation time we still see captures where the model
//! ignores the directive — and the cost of being tolerant is low.
//!
//! The contract is:
//!
//! 1. **Strict path first.** If the input is already a single bare JSON
//!    value, parse it directly so the audit-log byte-for-byte shape of
//!    successful calls is unchanged. The strict path is the same code
//!    `serde_json::from_str` ran before this module existed; the
//!    lenient extraction only kicks in when strict parsing fails.
//! 2. **Lenient path on failure.** Find the first `{` in the input and
//!    let `serde_json::Deserializer::from_str(...).into_iter::<Plan>()`
//!    consume the first complete JSON value starting at that offset.
//!    Surrounding prose (model preamble, ```json fence opener, trailing
//!    explanations after the closing brace) is ignored.
//! 3. **No partial-recovery JSON repair.** We don't patch unbalanced
//!    braces, smart-quotes, or trailing commas. If the model's JSON is
//!    structurally broken, the error surfaces to the agent loop as
//!    `AgentError::Decode` (same surface as the strict path) and the
//!    inner loop counts it as a failed plan iteration.
//! 4. **First `{` wins.** The lenient path anchors on the *first* `{`
//!    in the input. If a model emits prose that contains a `{`
//!    character *before* the real JSON body (e.g. "expected shape is
//!    `{tool, method, params}`: ```json\n{…real plan…}\n```"), the
//!    stream-deserializer will try to parse from that earlier `{`,
//!    fail, and the helper re-emits the strict-path error. The failure
//!    mode is "fail decode → failed plan iteration", never "succeed
//!    with the wrong JSON value picked from later in the response".

use crate::cassandra::types::Plan;

/// Pure function: turn an LLM response into a [`Plan`].
///
/// Tries strict JSON parse first; on failure, finds the first `{` and
/// asks `serde_json::Deserializer` to stream-parse one JSON value from
/// that offset. Returns the strict-path error when neither succeeds so
/// the failure shape stays identical to the pre-lenient code path.
///
/// # Examples
///
/// ```ignore
/// // Bare JSON (the strict path):
/// let raw = r#"{"context":"…","decision":"task_complete", …}"#;
/// let plan = parse_plan_lenient(raw)?;
///
/// // Markdown-fenced (the lenient path):
/// let raw = "```json\n{\"context\":\"…\", …}\n```";
/// let plan = parse_plan_lenient(raw)?;
/// ```
pub fn parse_plan_lenient(raw: &str) -> Result<Plan, serde_json::Error> {
    // Strict path: cheap, exact, no fallback artefacts.
    if let Ok(plan) = serde_json::from_str::<Plan>(raw) {
        return Ok(plan);
    }

    // Lenient path: find first `{` and stream-parse from there. If
    // there is no `{` at all the input cannot be JSON; fall back to
    // the strict error so the caller sees the original diagnostic.
    let Some(start) = raw.find('{') else {
        return serde_json::from_str::<Plan>(raw);
    };

    let mut it =
        serde_json::Deserializer::from_str(&raw[start..]).into_iter::<Plan>();
    match it.next() {
        Some(Ok(plan)) => Ok(plan),
        // The first JSON value at or after `start` was not parseable;
        // re-emit the *strict-path* error so callers see a stable
        // error type — not the lenient path's possibly-different
        // wording — for the diagnostic.
        Some(Err(_)) | None => serde_json::from_str::<Plan>(raw),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cassandra::types::DataClass;

    /// A minimal well-formed plan in canonical wire shape.
    /// Used as the body the harness re-wraps in fences / prose to
    /// produce the input cases this helper must tolerate.
    fn canonical_plan_json() -> &'static str {
        r#"{
            "context": "user asked to say hello",
            "decision": "task_complete",
            "rationale": "no actions needed",
            "steps": [],
            "result": {"kind": "text", "body": "hello"},
            "data_ceiling": "Public"
        }"#
    }

    fn expect_terminal_plan(plan: &Plan) {
        assert_eq!(plan.decision, "task_complete");
        assert!(plan.steps.is_empty());
        assert_eq!(plan.data_ceiling, DataClass::Public);
        assert!(plan.refused.is_none());
    }

    #[test]
    fn strict_bare_json_is_accepted() {
        let plan = parse_plan_lenient(canonical_plan_json())
            .expect("strict path should accept bare JSON");
        expect_terminal_plan(&plan);
    }

    #[test]
    fn markdown_json_fence_is_stripped() {
        // The default shape gemma4 / Qwen3-instruct / llama3-instruct
        // emit when asked for a JSON object: ```json\n<body>\n```.
        let raw = format!("```json\n{}\n```", canonical_plan_json());
        let plan = parse_plan_lenient(&raw).expect("lenient path");
        expect_terminal_plan(&plan);
    }

    #[test]
    fn markdown_unlabelled_fence_is_stripped() {
        // Some models emit ``` … ``` with no language tag.
        let raw = format!("```\n{}\n```", canonical_plan_json());
        let plan = parse_plan_lenient(&raw).expect("lenient path");
        expect_terminal_plan(&plan);
    }

    #[test]
    fn leading_prose_before_fence_is_skipped() {
        // Some reasoning models emit a short preamble before the JSON
        // fence: "Here is the plan: ```json\n{...}\n```".
        let raw = format!("Here is the plan:\n```json\n{}\n```", canonical_plan_json());
        let plan = parse_plan_lenient(&raw).expect("lenient path");
        expect_terminal_plan(&plan);
    }

    #[test]
    fn trailing_prose_after_closing_brace_is_ignored() {
        // The stream-deserializer stops at the end of the first complete
        // JSON value; anything after is silently ignored by `.next()`.
        let raw = format!("{}\nHope this helps!", canonical_plan_json());
        let plan = parse_plan_lenient(&raw).expect("lenient path");
        expect_terminal_plan(&plan);
    }

    #[test]
    fn prose_with_no_json_at_all_returns_decode_error() {
        // A model that emits "I cannot do this." with no JSON gives us
        // no `Plan`; we must surface a decode error so the agent loop
        // counts the iteration as a failed plan.
        let raw = "I cannot do this.";
        let err = parse_plan_lenient(raw)
            .err()
            .expect("must error when no JSON present");
        // We don't pin the exact wording (serde_json is free to evolve
        // it across versions); we just confirm it's a parse error.
        assert!(err.to_string().to_ascii_lowercase().contains("expected"));
    }

    #[test]
    fn invalid_json_inside_fence_returns_strict_error() {
        // The lenient path tries to parse starting at the first `{`.
        // If that parse also fails, the helper re-emits the *strict
        // path's* error — not the lenient path's — so callers see a
        // stable diagnostic regardless of which path was tried.
        let raw = "```json\n{not actually JSON}\n```";
        let err = parse_plan_lenient(raw).err().expect("must error");
        // serde's strict error for non-JSON content starts with
        // "expected value at line 1 column 1" — the lenient path's
        // error (had we surfaced it) would have started at a deeper
        // line/column. Confirming the strict-path wording is what
        // pins the error-stability contract.
        let msg = err.to_string();
        assert!(
            msg.contains("line 1 column 1"),
            "expected strict-path error position; got {msg}"
        );
    }

    #[test]
    fn whitespace_only_input_returns_decode_error() {
        let raw = "   \n\t  ";
        let err = parse_plan_lenient(raw).err().expect("must error");
        // Pinned: parse fails. We don't care about the exact wording.
        let _ = err;
    }

    #[test]
    fn earlier_stray_open_brace_in_prose_yields_decode_error_not_misparse() {
        // Pins the "first `{` wins" contract documented at module top:
        // if a model emits prose that contains a `{` before the real
        // JSON body, the lenient path anchors on the *earlier* `{`,
        // fails to parse it, and re-emits the strict-path error. The
        // failure mode is "fail decode" (counted as a failed plan
        // iteration), NEVER "succeed with the wrong value extracted
        // from later in the response". A future refactor that gets
        // fancier with extraction (e.g. skipping non-JSON-looking
        // prefixes) MUST preserve this safety: silently parsing the
        // *second* `{` would let a prose-described decoy plan slip
        // past the contract this test pins.
        let raw = format!(
            "The expected shape is {{tool, method, params}}; here's the plan:\n```json\n{}\n```",
            canonical_plan_json()
        );
        let err = parse_plan_lenient(&raw)
            .err()
            .expect("must error: earlier `{` in prose poisons the lenient anchor");
        // Strict-path error position confirms the strict-path error
        // was re-emitted (not the lenient path's deeper position).
        let msg = err.to_string();
        assert!(
            msg.contains("line 1 column 1"),
            "expected strict-path error position; got {msg}"
        );
    }

    #[test]
    fn nested_braces_inside_strings_do_not_confuse_extractor() {
        // Stream-deserializer correctly tracks string boundaries; a
        // `}` inside a JSON string literal must not be mistaken for
        // the closing brace of the outer object. We seat the
        // canonical plan inside a fence, but with a step that includes
        // a `}` character in its `done_when` text.
        let raw = r#"```json
{
  "context": "x",
  "decision": "task_complete",
  "rationale": "y",
  "steps": [
    {
      "tool": "shell-exec",
      "method": "shell.exec",
      "parameters": {"argv": ["/bin/echo", "{not a real closing}"]},
      "returns": "stdout",
      "done_when": "shell exits 0 (success criterion contains }) ",
      "classification": "Public"
    }
  ],
  "result": null,
  "data_ceiling": "Public"
}
```"#;
        let plan = parse_plan_lenient(raw).expect("lenient path");
        assert_eq!(plan.steps.len(), 1);
        assert!(plan.steps[0].done_when.contains('}'));
    }
}
