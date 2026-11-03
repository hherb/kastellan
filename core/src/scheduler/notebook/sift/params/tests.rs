use serde_json::json;

use super::*;

fn spec_example() -> serde_json::Value {
    json!({
        "over": {"step": 2},
        "items": "results",
        "from": 0,
        "read": {"tool": "mail", "method": "mail.get_attachment_text",
                 "parameters": {"message_id": "{message_id}", "filename": "{attachments.0}"}},
        "keep_if": "an invoice I received that states an amount due",
        "fields": ["sender", "date", "amount_due"]
    })
}

#[test]
fn the_specs_example_parses() {
    let p = parse_params(&spec_example()).unwrap();
    assert_eq!(p.over, Over::Step(2));
    assert_eq!(p.items.as_deref(), Some("results"));
    assert_eq!(p.from, 0);
    assert_eq!(p.read.as_ref().unwrap().method, "mail.get_attachment_text");
    assert_eq!(p.fields, vec!["sender", "date", "amount_due"]);
}

#[test]
fn over_a_scrapbook_parses_its_ref() {
    let p = parse_params(&json!({"over": {"scrapbook": "sb:301:1"}, "keep_if": "x", "fields": ["a"]})).unwrap();
    assert_eq!(p.over, Over::Scrapbook(ScrapRef { task_id: 301, sift: 1 }));
    assert!(p.read.is_none());
    assert_eq!(p.from, 0, "from defaults to the first item");
}

#[test]
fn an_unknown_key_is_refused() {
    assert!(parse_params(&json!({"over": {"step": 2}, "keep_if": "x", "fields": ["a"], "extra": 1})).is_err());
    assert!(parse_params(&json!({"over": {"step": 2, "x": 1}, "keep_if": "x", "fields": ["a"]})).is_err());
}

#[test]
fn the_criterion_and_the_fields_are_checked() {
    let with = |keep_if: &str, fields: serde_json::Value| {
        parse_params(&json!({"over": {"step": 1}, "keep_if": keep_if, "fields": fields}))
    };
    assert!(with("  ", json!(["a"])).is_err(), "a blank criterion");
    assert!(with("x", json!([])).is_err(), "no fields");
    let seventeen: Vec<String> = (0..17).map(|i| format!("f{i}")).collect();
    assert!(with("x", json!(seventeen)).is_err(), "17 fields");
    let sixteen: Vec<String> = (0..16).map(|i| format!("f{i}")).collect();
    assert!(with("x", json!(sixteen)).is_ok(), "16 fields");
    assert!(with("x", json!(["a", "a"])).is_err(), "a field named twice");
    assert!(with("x", json!(["a", " "])).is_err(), "a blank field name");
}

#[test]
fn a_scrap_ref_round_trips() {
    let r = ScrapRef::parse("sb:301:1").unwrap();
    assert_eq!(r, ScrapRef { task_id: 301, sift: 1 });
    assert_eq!(r.to_string(), "sb:301:1");
}

#[test]
fn a_malformed_scrap_ref_is_refused() {
    for bad in ["sb:1", "sb:x:1", "sb:1:2:3", "sb:-1:1", "sb:1:0", "sb:0:1", "xb:1:1", "", "sb::1", "sb:1:+2"] {
        assert!(ScrapRef::parse(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn the_read_key_is_tool_dot_method_for_a_built_in() {
    assert_eq!(read_key("handoff", "fetch"), "handoff.fetch");
    assert_eq!(read_key("mail", "mail.get_message"), "mail.get_message");
}

fn template(tool: &str, method: &str) -> ReadTemplate {
    ReadTemplate { tool: tool.into(), method: method.into(), parameters: json!({"message_id": "{id}"}) }
}

#[test]
fn a_read_must_be_on_the_allowlist() {
    let cfg = SiftConfig::default();
    assert!(check_read(&template("mail", "mail.get_message"), &cfg).is_ok());
    assert!(check_read(&template("handoff", "fetch"), &cfg).is_ok());
    assert!(check_read(&template("shell-exec", "shell.exec"), &cfg).is_err());
    assert!(check_read(&template("sift", "sift.run"), &cfg).is_err());
}

#[test]
fn a_placeholder_in_the_tool_or_method_is_refused() {
    let cfg = SiftConfig { max_reads: 64, read_methods: vec!["{tool}.{method}".into(), "mail.{m}".into()] };
    assert!(check_read(&template("{tool}", "{method}"), &cfg).is_err());
    assert!(check_read(&template("mail", "mail.{m}"), &cfg).is_err());
}

/// Review finding: a long field name was stored whole but shown to the next
/// turn cut, so a follow-up naming it found no such field; a long `keep_if`
/// rode in every sorter message unbounded.
#[test]
fn an_overlong_criterion_or_field_name_is_refused() {
    let with = |keep_if: String, field: String| json!({"over": {"step": 2}, "keep_if": keep_if, "fields": [field]});
    assert!(parse_params(&with("é".repeat(KEEP_IF_MAX_CHARS), "a".into())).is_ok(), "the bound counts characters");
    assert!(parse_params(&with("x".repeat(KEEP_IF_MAX_CHARS + 1), "a".into())).is_err());
    assert!(parse_params(&with("x".into(), "f".repeat(FIELD_NAME_MAX_CHARS))).is_ok());
    assert!(parse_params(&with("x".into(), "f".repeat(FIELD_NAME_MAX_CHARS + 1))).is_err());
}

/// The model writes back keys it is shown (memory: the planner mirrors the
/// rendered shape): a summary's `continues` or `chain`, copied into the
/// next sift's parameters, must not refuse it.
#[test]
fn a_copied_continues_or_chain_is_ignored() {
    let mut v = spec_example();
    v["continues"] = json!("sb:9:2");
    v["chain"] = json!({"sifts": 2, "looked_at": 60, "kept": 9});
    assert!(parse_params(&v).is_ok());
}
