//! The sorter (#922, S1–S3, K5): one cheap decision per item — "does this
//! item contain what the question asks for?" — and, if yes, the named fields.

use async_trait::async_trait;
use serde_json::Value;

use crate::scheduler::agent::AgentError;
use crate::scheduler::inner_loop::TaskContext;

/// The most bytes of one item's text the sorter is shown (K4).
pub(crate) const SORTER_ITEM_CAP: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SortReply {
    No,
    Yes(serde_json::Map<String, Value>),
    Undecided,
}

/// The per-item decision (S3): today the local LLM; a test double or, later,
/// a dedicated fast decision model may stand in. Returns the raw reply;
/// [`parse_sort_reply`] reads it.
#[async_trait]
pub trait Sorter: Send + Sync {
    async fn sort(&self, ctx: &TaskContext, user_msg: String) -> Result<String, AgentError>;
}

/// The sorter's user message: the criterion, the field names, then the item
/// (its search hit, then each read's result, each already screened and
/// capped). Keys in that order — a `serde_json::Map` would sort them — so
/// the parts that are the same for every item lead (#943).
pub(crate) fn sorter_message(criterion: &str, fields: &[String], item: &[Value]) -> String {
    format!(
        "{{\"keep_if\":{},\"fields\":{},\"item\":{}}}",
        serde_json::json!(criterion),
        serde_json::json!(fields),
        Value::Array(item.to_vec())
    )
}

/// Read a sorter reply. `{"keep": false}` is no; `{"keep": true, "fields":
/// {…}}` is yes, with exactly the named fields (a missing one is `null`, an
/// extra one dropped), and so is `{"keep": true}` with `fields` absent or
/// `null` (every field `null`). Anything else — `fields` of another type
/// included — is `Undecided`, never a guess (K5).
pub(crate) fn parse_sort_reply(raw: &str, fields: &[String]) -> SortReply {
    let Ok(Value::Object(reply)) = crate::scheduler::plan_parser::parse_json_lenient::<Value>(raw) else {
        return SortReply::Undecided;
    };
    match (reply.get("keep"), reply.get("fields")) {
        (Some(Value::Bool(false)), _) => SortReply::No,
        (Some(Value::Bool(true)), Some(Value::Object(got))) => SortReply::Yes(pick(fields, Some(got))),
        (Some(Value::Bool(true)), None | Some(Value::Null)) => SortReply::Yes(pick(fields, None)),
        _ => SortReply::Undecided,
    }
}

fn pick(fields: &[String], got: Option<&serde_json::Map<String, Value>>) -> serde_json::Map<String, Value> {
    fields.iter().map(|f| (f.clone(), got.and_then(|g| g.get(f)).cloned().unwrap_or(Value::Null))).collect()
}

#[cfg(test)]
mod tests;
