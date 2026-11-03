use serde_json::{json, Value};

use super::*;

fn read(parameters: Value) -> ReadTemplate {
    ReadTemplate { tool: "mail".into(), method: "mail.get_message".into(), parameters }
}

fn items(n: usize) -> Vec<Value> {
    (0..n).map(|i| json!({"id": format!("m{i}"), "files": ["a.pdf", "b.pdf", "c.pdf"]})).collect()
}

/// Review finding: an item with no read still costs a sorter call, so the
/// cap bounds the items too — or a read-less sift over a long list makes
/// one model call per element in a single turn.
#[test]
fn a_sift_without_a_read_takes_at_most_cap_items() {
    let b = plan_reads(None, &items(200), 5);
    assert_eq!(b.per_item.len(), 5);
    assert!(b.reads.is_empty());
}

#[test]
fn items_are_taken_while_their_reads_fit() {
    let t = read(json!({"id": "{id}", "file": "{files.*}"}));
    let b = plan_reads(Some(&t), &items(4), 7);
    assert_eq!(b.per_item, [0..3, 3..6], "a third item's three reads would not fit in 7");
    assert_eq!(b.cut, 0);
}

#[test]
fn a_first_item_over_the_cap_keeps_its_first_reads_and_says_how_many_were_cut() {
    let t = read(json!({"id": "{id}", "file": "{files.*}"}));
    let b = plan_reads(Some(&t), &items(2), 2);
    assert_eq!(b.per_item, vec![std::ops::Range { start: 0, end: 2 }]);
    assert_eq!(b.cut, 1);
}

#[test]
fn a_zero_cap_still_takes_one_item() {
    assert_eq!(plan_reads(None, &items(3), 0).per_item.len(), 1);
}

#[test]
fn the_stop_reasons_are_the_words_the_prompt_names() {
    let words: Vec<&str> = [Stop::Time, Stop::Cancelled, Stop::SorterFailed, Stop::ReadsNotRun].map(Stop::as_str).to_vec();
    assert_eq!(words, ["time", "cancelled", "sorter_failed", "reads_not_run"]);
}
