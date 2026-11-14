use super::*;

#[test]
fn the_stop_reasons_are_the_words_the_prompt_names() {
    let words: Vec<&str> = [Stop::Time, Stop::Cancelled, Stop::SorterFailed, Stop::ReadsNotRun].map(Stop::as_str).to_vec();
    assert_eq!(words, ["time", "cancelled", "sorter_failed", "reads_not_run"]);
}

/// DGX task 322 (#960 re-measure #9): the model sifted a 27-row scrapbook
/// `from` 64 (the search's `next_from`). The sift took no items and reported
/// `looked_at: 0, remaining: 0` as a success, which the model then misread
/// twice. A `from` past the end is refused with the list's length instead;
/// an empty list at `from` 0 is an honest zero and still runs.
#[test]
fn a_from_past_the_end_of_the_list_is_refused_with_its_length() {
    let why = check_from(64, 27).unwrap_err();
    assert!(why.contains("64") && why.contains("27 items"), "{why}");
    assert!(check_from(27, 27).is_err(), "`from` equal to the length takes nothing either");
    assert_eq!(check_from(26, 27), Ok(()));
    assert_eq!(check_from(0, 0), Ok(()), "an empty list from 0 is a real, empty sift");
}

/// #1022 part 2: a bare `id` names an item only within its tool, so it is
/// written with the tool — `{"id": 3}` from two tools is two rows. A key
/// that names the item anywhere (`message_id`, `url`) is left as it is, and
/// so is a scrapbook row's own source.
#[test]
fn a_bare_id_source_is_written_with_its_tool() {
    assert_eq!(source_of(&json!({"id": 3, "title": "x"}), "kv"), json!({"id": 3, "tool": "kv"}));
    assert_ne!(source_of(&json!({"id": 3}), "kv"), source_of(&json!({"id": 3}), "notes"));
    assert_eq!(source_of(&json!({"id": "1", "url": "https://a"}), "web"), json!({"id": "1", "url": "https://a"}));
    assert_eq!(source_of(&json!({"message_id": "37477"}), "mail"), json!({"message_id": "37477"}));
    assert_eq!(source_of(&json!({"title": "x"}), "kv"), json!({}));
    let row = json!({"source": {"id": 3, "tool": "kv"}, "fields": {}});
    assert_eq!(source_of(&row, "scrapbook"), json!({"id": 3, "tool": "kv"}));
}
