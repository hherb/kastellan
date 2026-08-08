use super::*;

#[test]
fn a_key_present_in_old_and_absent_in_new_is_lost() {
    let d = diff_env_files("KASTELLAN_MAIL_ENDPOINT=https://h:8443\nKASTELLAN_DATA_DIR=/d\n",
                           "KASTELLAN_DATA_DIR=/d\n");
    assert_eq!(d.lost, vec!["KASTELLAN_MAIL_ENDPOINT"]);
    assert!(d.changed.is_empty());
    assert!(!d.is_empty());
}

#[test]
fn a_key_whose_value_differs_is_changed() {
    // The exact live shape: install reverts a hand-tuned model tag to the
    // flag default, and nothing says so.
    let d = diff_env_files("KASTELLAN_LLM_LOCAL_MODEL=gemma4:26b-a4b-it-q8_0-ctx64k\n",
                           "KASTELLAN_LLM_LOCAL_MODEL=gemma4:26b-a4b-it-q8_0\n");
    assert!(d.lost.is_empty());
    assert_eq!(d.changed, vec!["KASTELLAN_LLM_LOCAL_MODEL"]);
}

#[test]
fn an_identical_file_diffs_to_nothing() {
    let s = "KASTELLAN_DATA_DIR=/d\nKASTELLAN_LLM_LOCAL_URL=http://h/v1\n";
    let d = diff_env_files(s, s);
    assert!(d.is_empty(), "{d:?}");
}

#[test]
fn a_key_only_in_new_is_not_reported() {
    // The installer adding a key is it doing its job, not a loss.
    let d = diff_env_files("A=1\n", "A=1\nB=2\n");
    assert!(d.is_empty(), "{d:?}");
}

#[test]
fn commented_and_malformed_lines_are_ignored_on_both_sides() {
    // `render_env_file` emits commented defaults (# KASTELLAN_TIMEZONE=...);
    // a comment is not a key, on either side.
    let d = diff_env_files("# KASTELLAN_TIMEZONE=Australia/Sydney\nnokey\n\nA=1\n",
                           "# KASTELLAN_TIMEZONE=Australia/Sydney\nA=1\n");
    assert!(d.is_empty(), "{d:?}");
}

#[test]
fn a_key_the_operator_uncommented_is_reported_as_lost() {
    // Old file has it live; the freshly rendered file has it commented out.
    // The value really is being lost, so `lost` is the honest answer.
    let d = diff_env_files("KASTELLAN_TIMEZONE=Australia/Sydney\n",
                           "# KASTELLAN_TIMEZONE=Australia/Sydney\n");
    assert_eq!(d.lost, vec!["KASTELLAN_TIMEZONE"]);
}

#[test]
fn reported_keys_follow_the_old_files_order_and_do_not_repeat() {
    let d = diff_env_files("Z=1\nA=2\nZ=3\n", "");
    // Deterministic for a stable operator-facing message; a duplicated key in
    // the source file is reported once.
    assert_eq!(d.lost, vec!["Z", "A"]);
}

#[test]
fn a_repeated_key_is_compared_on_its_last_value_not_its_first() {
    // systemd takes the last occurrence, and env_file::merge_env already
    // behaves that way. Comparing the first would report a change that did
    // not happen.
    let d = diff_env_files("A=old\nA=real\n", "A=real\n");
    assert!(d.is_empty(), "nothing changed; operative old value is `real`: {d:?}");
}

#[test]
fn a_revert_of_a_repeated_keys_last_value_is_reported() {
    // The dangerous direction: comparing first values makes this look
    // unchanged, and a real revert is silently unreported.
    let d = diff_env_files("A=1\nA=2\n", "A=1\n");
    assert_eq!(d.changed, vec!["A"]);
    assert!(d.lost.is_empty());
}
