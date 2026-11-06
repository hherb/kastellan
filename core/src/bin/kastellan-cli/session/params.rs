//! Each session method's params, parsed and checked. Pure: no database, no
//! I/O, so every refusal is unit-tested here.
//!
//! A refusal is a JSON-RPC `INVALID_PARAMS` (-32602) error whose message
//! says what was wrong — the client is bottie or the operator at a
//! terminal, and both need the reason, not just the code. Unknown keys are
//! refused (`deny_unknown_fields`), so a misspelt `after_seq` is an error
//! rather than a silent replay from the start.

use kastellan_core::cassandra::DataClass;
use kastellan_db::tasks::Lane;
use kastellan_protocol::{codes, RpcError};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;

use crate::common::parse_classification_floor;
use crate::inbox::CHOICES;

/// How many asks `inbox.list` returns when the client names no `limit`
/// (the same default as `kastellan-cli inbox list`), and the most it may ask for.
pub(super) const DEFAULT_INBOX_LIMIT: i64 = 20;
pub(super) const MAX_INBOX_LIMIT: i64 = 1000;

fn invalid(msg: impl Into<String>) -> RpcError {
    RpcError::new(codes::INVALID_PARAMS, msg)
}

/// Decode `params` into `T`. Absent params (`null`) read as `{}`, so a
/// method whose params are all optional can be called without any.
fn decode<T: DeserializeOwned>(params: Value) -> Result<T, RpcError> {
    let params = if params.is_null() { Value::Object(Default::default()) } else { params };
    serde_json::from_value(params).map_err(|e| invalid(format!("invalid params: {e}")))
}

/// A text the operator typed must say something.
fn non_blank(field: &str, text: String) -> Result<String, RpcError> {
    if text.trim().is_empty() {
        return Err(invalid(format!("{field} must not be blank")));
    }
    Ok(text)
}

/// `task.submit {instruction, lane?, classification_floor?}`.
#[derive(Debug, PartialEq)]
pub(super) struct Submit {
    pub instruction: String,
    pub lane: Lane,
    /// `None` lets the keyword classifier infer the floor, as `ask` does
    /// without `--classification-floor`.
    pub floor: Option<DataClass>,
}

pub(super) fn submit(params: Value) -> Result<Submit, RpcError> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Raw {
        instruction: String,
        #[serde(default)]
        lane: Option<String>,
        #[serde(default)]
        classification_floor: Option<String>,
    }
    let raw: Raw = decode(params)?;
    let lane = match raw.lane.as_deref() {
        None => Lane::Fast,
        Some(l) => Lane::from_sql(l).map_err(|_| invalid(format!("lane must be \"fast\" or \"long\", got {l:?}")))?,
    };
    let floor = raw.classification_floor.as_deref().map(parse_classification_floor).transpose().map_err(invalid)?;
    Ok(Submit { instruction: non_blank("instruction", raw.instruction)?, lane, floor })
}

/// `task.follow {task_id, after_seq?}`: entries with `seq > after_seq` are
/// sent; `0` (the default) sends the whole notebook.
#[derive(Debug, PartialEq)]
pub(super) struct Follow {
    pub task_id: i64,
    pub after_seq: i64,
}

pub(super) fn follow(params: Value) -> Result<Follow, RpcError> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Raw {
        task_id: i64,
        #[serde(default)]
        after_seq: i64,
    }
    let raw: Raw = decode(params)?;
    if raw.after_seq < 0 {
        return Err(invalid("after_seq must not be negative"));
    }
    Ok(Follow { task_id: raw.task_id, after_seq: raw.after_seq })
}

/// `task.cancel {task_id}` and `task.status {task_id}`.
pub(super) fn task_id(params: Value) -> Result<i64, RpcError> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Raw {
        task_id: i64,
    }
    Ok(decode::<Raw>(params)?.task_id)
}

/// `task.correct {task_id, text}`.
pub(super) fn correct(params: Value) -> Result<(i64, String), RpcError> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Raw {
        task_id: i64,
        text: String,
    }
    let raw: Raw = decode(params)?;
    Ok((raw.task_id, non_blank("text", raw.text)?))
}

/// `inbox.list {limit?}`.
pub(super) fn inbox_limit(params: Value) -> Result<i64, RpcError> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Raw {
        #[serde(default)]
        limit: Option<i64>,
    }
    match decode::<Raw>(params)?.limit {
        None => Ok(DEFAULT_INBOX_LIMIT),
        Some(n) if (1..=MAX_INBOX_LIMIT).contains(&n) => Ok(n),
        Some(n) => Err(invalid(format!("limit must be 1..={MAX_INBOX_LIMIT}, got {n}"))),
    }
}

/// `inbox.answer {ask_id, choice, note?}`.
#[derive(Debug, PartialEq)]
pub(super) struct Answer {
    pub ask_id: i64,
    pub choice: String,
    pub note: Option<String>,
}

pub(super) fn answer(params: Value) -> Result<Answer, RpcError> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Raw {
        ask_id: i64,
        choice: String,
        #[serde(default)]
        note: Option<String>,
    }
    let raw: Raw = decode(params)?;
    if !CHOICES.contains(&raw.choice.as_str()) {
        return Err(invalid(format!("choice must be one of {CHOICES:?}, got {:?}", raw.choice)));
    }
    Ok(Answer { ask_id: raw.ask_id, choice: raw.choice, note: raw.note })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn msg(e: RpcError) -> String {
        assert_eq!(e.code, codes::INVALID_PARAMS, "every refusal is INVALID_PARAMS: {e:?}");
        e.message
    }

    #[test]
    fn a_bare_submit_is_the_fast_lane_with_an_inferred_floor() {
        let s = submit(json!({"instruction": "find the invoice"})).unwrap();
        assert_eq!(s, Submit { instruction: "find the invoice".into(), lane: Lane::Fast, floor: None });
    }

    #[test]
    fn submit_reads_the_lane_and_the_floor() {
        let s = submit(json!({"instruction": "x", "lane": "long", "classification_floor": "personal"})).unwrap();
        assert_eq!(s.lane, Lane::Long);
        assert_eq!(s.floor, Some(DataClass::Personal));
    }

    #[test]
    fn submit_refuses_a_blank_instruction_a_bad_lane_a_bad_floor_and_a_stray_key() {
        assert!(msg(submit(json!({"instruction": "  "})).unwrap_err()).contains("instruction must not be blank"));
        assert!(msg(submit(json!({"instruction": "x", "lane": "slow"})).unwrap_err()).contains("lane"));
        assert!(msg(submit(json!({"instruction": "x", "classification_floor": "TopSecret"})).unwrap_err()).contains("TopSecret"));
        assert!(msg(submit(json!({"instruction": "x", "floor": "Public"})).unwrap_err()).contains("unknown field"));
        assert!(msg(submit(json!({})).unwrap_err()).contains("instruction"));
    }

    #[test]
    fn follow_defaults_to_the_whole_notebook_and_refuses_a_negative_seq() {
        assert_eq!(follow(json!({"task_id": 7})).unwrap(), Follow { task_id: 7, after_seq: 0 });
        assert_eq!(follow(json!({"task_id": 7, "after_seq": 3})).unwrap().after_seq, 3);
        assert!(msg(follow(json!({"task_id": 7, "after_seq": -1})).unwrap_err()).contains("negative"));
        // A misspelt resume point must not silently replay from the start.
        assert!(msg(follow(json!({"task_id": 7, "after": 3})).unwrap_err()).contains("unknown field"));
    }

    #[test]
    fn a_task_id_must_be_an_integer() {
        assert_eq!(task_id(json!({"task_id": 42})).unwrap(), 42);
        assert!(msg(task_id(json!({"task_id": "42"})).unwrap_err()).contains("invalid"));
        assert!(msg(task_id(Value::Null).unwrap_err()).contains("task_id"));
    }

    #[test]
    fn a_correction_must_say_something() {
        assert_eq!(correct(json!({"task_id": 1, "text": "the other account"})).unwrap(), (1, "the other account".into()));
        assert!(msg(correct(json!({"task_id": 1, "text": "\n"})).unwrap_err()).contains("text must not be blank"));
    }

    #[test]
    fn inbox_list_takes_no_params_or_a_bounded_limit() {
        assert_eq!(inbox_limit(Value::Null).unwrap(), DEFAULT_INBOX_LIMIT);
        assert_eq!(inbox_limit(json!({"limit": 5})).unwrap(), 5);
        assert!(msg(inbox_limit(json!({"limit": 0})).unwrap_err()).contains("limit"));
        assert!(msg(inbox_limit(json!({"limit": MAX_INBOX_LIMIT + 1})).unwrap_err()).contains("limit"));
    }

    #[test]
    fn an_answer_is_one_of_the_offered_choices() {
        assert_eq!(
            answer(json!({"ask_id": 3, "choice": "deny", "note": "no"})).unwrap(),
            Answer { ask_id: 3, choice: "deny".into(), note: Some("no".into()) }
        );
        assert!(msg(answer(json!({"ask_id": 3, "choice": "maybe"})).unwrap_err()).contains("maybe"));
    }
}
