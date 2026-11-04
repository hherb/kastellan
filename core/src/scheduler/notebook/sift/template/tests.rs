use serde_json::json;

use super::*;
use crate::scheduler::notebook::sift::params::ReadTemplate;

fn read(parameters: serde_json::Value) -> ReadTemplate {
    ReadTemplate { tool: "mail".into(), method: "mail.get_attachment_text".into(), parameters }
}

#[test]
fn items_at_a_path() {
    assert_eq!(items_at(&json!({"results": [1, 2]}), Some("results")).unwrap(), vec![json!(1), json!(2)]);
    assert_eq!(items_at(&json!({"a": {"b": [3]}}), Some("a.b")).unwrap(), vec![json!(3)]);
}

#[test]
fn the_whole_result_when_no_path_is_given() {
    assert_eq!(items_at(&json!([1]), None).unwrap(), vec![json!(1)]);
    assert!(items_at(&json!({"results": [1]}), None).is_err(), "an object is not a list");
}

#[test]
fn a_path_that_is_not_a_list_is_refused_naming_it() {
    let e = items_at(&json!({"results": {"n": 1}}), Some("results")).unwrap_err();
    assert!(e.contains("results"), "{e}");
    let e = items_at(&json!({}), Some("hits")).unwrap_err();
    assert!(e.contains("hits"), "{e}");
}

#[test]
fn value_at_walks_objects_and_array_indices() {
    let v = json!({"a": [{"b": 3}]});
    assert_eq!(value_at(&v, "a.0.b"), Some(&json!(3)));
    assert_eq!(value_at(&v, "a.1.b"), None);
    assert_eq!(value_at(&v, "a.x"), None);
}

#[test]
fn attachments_fan_out_skipping_images() {
    let t = read(json!({"message_id": "{id}", "filename": "{attachments.*}"}));
    let item = json!({"id": "m1", "attachments": ["a.pdf", "b.PNG", "c.docx", "d.heic"]});
    assert_eq!(
        expand_reads(&t, &item),
        vec![json!({"message_id": "m1", "filename": "a.pdf"}), json!({"message_id": "m1", "filename": "c.docx"})]
    );
}

#[test]
fn a_placeholder_keeps_the_items_json_type() {
    let t = read(json!({"message_id": "{n}", "nested": {"keep": "{flag}"}}));
    assert_eq!(
        expand_reads(&t, &json!({"n": 38996, "flag": true})),
        vec![json!({"message_id": 38996, "nested": {"keep": true}})]
    );
}

#[test]
fn an_unresolved_placeholder_skips_the_items_reads() {
    let t = read(json!({"message_id": "{missing}"}));
    assert!(expand_reads(&t, &json!({"id": "m1"})).is_empty());
}

#[test]
fn two_fan_out_placeholders_are_refused() {
    let t = read(json!({"a": "{xs.*}", "b": "{ys.*}"}));
    assert!(expand_reads(&t, &json!({"xs": ["1"], "ys": ["2"]})).is_empty());
}

#[test]
fn a_placeholder_inside_a_longer_string_is_left_verbatim() {
    let t = read(json!({"q": "x {id}"}));
    assert_eq!(expand_reads(&t, &json!({"id": "m1"})), vec![json!({"q": "x {id}"})]);
}

#[test]
fn an_item_with_no_attachments_has_no_reads() {
    let t = read(json!({"filename": "{attachments.*}"}));
    assert!(expand_reads(&t, &json!({"attachments": []})).is_empty());
    assert!(expand_reads(&t, &json!({"attachments": ["x.jpg"]})).is_empty());
}

#[test]
fn image_names_are_recognised_case_insensitively() {
    for name in ["a.png", "b.JPG", "c.jpeg", "d.gif", "e.bmp", "f.WebP", "g.heic"] {
        assert!(is_image(name), "{name}");
    }
    for name in ["a.pdf", "png", "a.png.pdf", "a.docx"] {
        assert!(!is_image(name), "{name}");
    }
}

/// #960 item 3: a mail with no readable attachment (none, or images only)
/// takes the next read, the message itself; one with a PDF takes the first.
#[test]
fn an_item_takes_the_first_read_that_applies_to_it() {
    let attachments = read(json!({"message_id": "{message_id}", "filename": "{attachments.*}"}));
    let message = ReadTemplate { tool: "mail".into(), method: "mail.get_message".into(), parameters: json!({"message_id": "{message_id}"}) };
    let both = [attachments, message];
    let pdf = json!({"message_id": "m1", "attachments": ["a.pdf", "b.png"]});
    assert_eq!(first_reads(&both, &pdf), [(0, json!({"message_id": "m1", "filename": "a.pdf"}))]);
    let photo = json!({"message_id": "m2", "attachments": ["b.png"]});
    assert_eq!(first_reads(&both, &photo), [(1, json!({"message_id": "m2"}))]);
    assert_eq!(first_reads(&both, &json!({"message_id": "m3", "attachments": []})), [(1, json!({"message_id": "m3"}))]);
    assert!(first_reads(&both, &json!({"id": "x"})).is_empty(), "no read applies");
    assert!(first_reads(&[], &pdf).is_empty());
}
