//! Unit tests for [`super`] — the planner's pruned view of a tool result.

use super::*;
use serde_json::json;

fn limits(leaf: usize, items: usize, keys: usize) -> PruneLimits {
    PruneLimits { leaf, items, keys }
}

// ── prune: the three things the old flattened view destroyed ──────────

#[test]
fn prune_keeps_object_keys() {
    let v = json!({"message_id": "3327", "subject": "Itinerary"});
    assert_eq!(prune(&v, PruneLimits::DEFAULT), v);
}

#[test]
fn prune_keeps_booleans() {
    let v = json!({"has_attachments": true, "is_read": false});
    assert_eq!(prune(&v, PruneLimits::DEFAULT), v);
}

#[test]
fn prune_keeps_numbers_and_null() {
    let v = json!({"score": 0.0618, "message_count": 38052, "folder": null});
    assert_eq!(prune(&v, PruneLimits::DEFAULT), v);
}

// ── prune: strings ────────────────────────────────────────────────────

#[test]
fn prune_cuts_an_over_cap_string_and_marks_it() {
    assert_eq!(prune(&json!("abcdefghij"), limits(4, 20, 64)), json!("abcd…"));
}

#[test]
fn prune_never_grows_a_string_barely_over_the_cap() {
    // Cutting 6 bytes to 4 and adding the 3-byte ellipsis would make 7.
    assert_eq!(prune(&json!("abcdef"), limits(4, 20, 64)), json!("abcdef"));
}

#[test]
fn prune_walks_back_off_a_straddling_multibyte_char() {
    // "日" is 3 bytes, so a cap of 2 lands inside the first one. The cut must
    // step back to byte 1; slicing at byte 2 would panic. (#591 records that
    // copies of this idiom keep shipping without this case.)
    let s = format!("a{}", "日".repeat(4)); // 13 bytes
    assert_eq!(prune(&json!(s), limits(2, 20, 64)), json!("a…"));
}

// ── prune: containers and depth ───────────────────────────────────────

#[test]
fn prune_caps_an_array_and_names_how_many_were_dropped() {
    let v = json!([1, 2, 3, 4, 5]);
    assert_eq!(prune(&v, limits(usize::MAX, 2, 64)), json!([1, 2, "…3 more items omitted"]));
}

#[test]
fn prune_caps_an_object_and_counts_the_dropped_keys() {
    // `serde_json::Map` is a BTreeMap here, so the kept keys are the first
    // two alphabetically. If this fails with "d" kept, someone enabled
    // serde_json's `preserve_order` feature.
    let v = json!({"d": 4, "a": 1, "c": 3, "b": 2});
    assert_eq!(prune(&v, limits(usize::MAX, 20, 2)), json!({"a": 1, "b": 2, "_omitted_keys": 2}));
}

#[test]
fn prune_replaces_a_subtree_nested_past_the_walk_depth() {
    let mut v = json!("leaf");
    for _ in 0..(MAX_WALK_DEPTH + 10) {
        v = json!([v]);
    }
    let out = prune(&v, PruneLimits::DEFAULT);
    let mut cur = &out;
    for _ in 0..MAX_WALK_DEPTH {
        cur = &cur[0];
    }
    assert_eq!(cur, &json!(DEPTH_MARKER));
}
