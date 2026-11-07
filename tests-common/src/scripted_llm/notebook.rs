//! Notebook-shaped planner replies for the daemon e2es (#993).
//!
//! The daemon plans on the notebook loop by default (#922 slice 4), so a
//! scripted model must answer each turn with one notebook reply: a JSON
//! object holding `steps` on the first turn, an optional `tick`, and exactly
//! one of `call`, `answer` or `infeasible` (`prompts/agent_notebook.md`).
//! These builders return that object as a string, ready for
//! [`super::envelope_for`].
//!
//! Every script here has one step, id 1. The loop's parser
//! (`core::scheduler::notebook::reply`) refuses unknown keys, so these
//! builders spell exactly the keys it reads — the unit tests below pin them.

use serde_json::{json, Value};

/// The one step every builder here plans: id 1, to be done with `tool`.
fn step_one(tool: &str) -> Value {
    json!({"id": 1, "purpose": "test step", "tool": tool, "done_when": "test done"})
}

/// A reply that lists step 1 and calls `tool`'s `method` for it with
/// `parameters`; the data it returns is classified `Public`.
///
/// Fine for every turn of a script, not just the first: re-listing a step
/// that **failed** reopens it, so the same reply retries a denied call (the
/// failure-path tests send it until the turn cap). A step that succeeded is
/// done and is never reopened — answer after it instead.
pub fn notebook_call_json(tool: &str, method: &str, parameters: Value) -> String {
    json!({
        "steps": [step_one(tool)],
        "call": {
            "step": 1,
            "tool": tool,
            "method": method,
            "parameters": parameters,
            "returns": "the result",
            "done_when": "test done",
            "classification": "Public",
        },
    })
    .to_string()
}

/// A reply that ends the task with `body` as its answer (no `tick`).
///
/// ⚠️ On the daemon's default config (`KASTELLAN_LLM_DISABLE_THINKING`
/// unset, so thinking is off) the answer a routine turn writes is set aside
/// (a `plan.discarded` row) and the next turn writes it again, thinking — so
/// a script after a call needs **two** of these: the draft, then the answer
/// the task ends with. The rewrite turn must not tick, which is why this
/// reply never does.
pub fn notebook_answer_json(body: &str) -> String {
    json!({"answer": {"body": body}}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(v: &Value) -> Vec<&str> {
        let mut k: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        k.sort_unstable();
        k
    }

    #[test]
    fn a_call_reply_lists_step_one_and_calls_it() {
        let v: Value = serde_json::from_str(&notebook_call_json("mail", "mail.search", json!({"q": "x"}))).unwrap();
        assert_eq!(keys(&v), ["call", "steps"]);
        assert_eq!(keys(&v["steps"][0]), ["done_when", "id", "purpose", "tool"]);
        assert_eq!(
            keys(&v["call"]),
            ["classification", "done_when", "method", "parameters", "returns", "step", "tool"]
        );
        assert_eq!(v["steps"][0]["id"], v["call"]["step"]);
        assert_eq!(v["steps"][0]["tool"], "mail");
        assert_eq!(v["call"]["method"], "mail.search");
        assert_eq!(v["call"]["parameters"], json!({"q": "x"}));
    }

    #[test]
    fn an_answer_reply_holds_only_the_answer() {
        let v: Value = serde_json::from_str(&notebook_answer_json("the body")).unwrap();
        assert_eq!(v, json!({"answer": {"body": "the body"}}));
    }
}
