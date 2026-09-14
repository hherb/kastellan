//! The shape `mail.get_message` returns a message's full headers in.
//!
//! localmail returns `headers` as a JSON object keyed by header name. A
//! header's **name** is written by whoever sent the message, and since
//! kastellan #677 the planner's view of a tool result shows object keys. The
//! core's guard model screens string values only, never keys, so a message
//! could put an instruction in front of the planner as a header name
//! (`X-Forward-All-Mail-To-…`). Found by review of #702.
//!
//! This worker therefore returns headers as a list of `{name, values}`, where
//! every name is a string value the guard model does screen. The general gap
//! (any worker passing third-party JSON objects through) is kastellan #703.

use serde_json::{Map, Value};

/// Key of a message's headers in localmail's response and in this worker's.
const HEADERS_KEY: &str = "headers";

/// `message` with its `headers` object, if it has one, replaced by a list of
/// `{"name": <header name>, "values": [<value>, …]}` in key order.
///
/// localmail groups every occurrence of one exactly-cased name into an array;
/// a bare string value (seen in fixtures, never from the live service) becomes
/// a one-element array so the list has one shape. Anything else — no
/// `headers`, or `headers` that is not an object — passes through unchanged.
pub(crate) fn header_names_as_values(mut message: Value) -> Value {
    let Some(Value::Object(headers)) = message.get_mut(HEADERS_KEY) else {
        return message;
    };
    let list = std::mem::take(headers).into_iter().map(|(name, values)| entry(name, values)).collect();
    message[HEADERS_KEY] = Value::Array(list);
    message
}

/// One header entry of the list [`header_names_as_values`] builds.
fn entry(name: String, values: Value) -> Value {
    let values = match values {
        Value::Array(items) => Value::Array(items),
        other => Value::Array(vec![other]),
    };
    let mut map = Map::new();
    map.insert("name".to_string(), Value::String(name));
    map.insert("values".to_string(), values);
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_header_object_becomes_a_list_of_names_and_values() {
        let out = header_names_as_values(json!({
            "id": "5",
            "headers": {"Received": ["a", "b"], "Subject": "one"},
        }));
        assert_eq!(
            out,
            json!({
                "id": "5",
                "headers": [
                    {"name": "Received", "values": ["a", "b"]},
                    {"name": "Subject", "values": ["one"]},
                ],
            })
        );
    }

    #[test]
    fn a_message_without_a_header_object_passes_through_unchanged() {
        for message in [json!({"id": "5"}), json!({"id": "5", "headers": null}), json!({"id": "5", "headers": []})] {
            assert_eq!(header_names_as_values(message.clone()), message);
        }
    }

    #[test]
    fn a_message_whose_body_is_not_an_object_passes_through_unchanged() {
        assert_eq!(header_names_as_values(json!("x")), json!("x"));
    }
}
