//! Pure unit tests for the graph module.
//!
//! Lifted from an inline `#[cfg(test)] mod tests` block in `graph.rs` to
//! keep the production file smaller. The body is byte-identical to what it
//! was inline; `use super::*` still resolves to the parent `graph` module
//! per the Rust 2018 sibling-directory module pattern. Integration tests
//! that hit a real Postgres cluster continue to live in
//! `db/tests/postgres_e2e.rs`.

use super::*;

/// Sanity-pin the value types so a future field rename trips a
/// compile error in the test before it can leak into a downstream
/// API change.
#[test]
fn entity_struct_field_shape() {
    let e = Entity {
        id: 1,
        kind: "person".into(),
        name: "alice".into(),
        attrs: serde_json::json!({"hello": "world"}),
    };
    assert_eq!(e.id, 1);
    assert_eq!(e.kind, "person");
    assert_eq!(e.name, "alice");
    assert_eq!(e.attrs["hello"], "world");
}

#[test]
fn relation_struct_field_shape() {
    let r = Relation {
        id: 1,
        src_id: 10,
        dst_id: 20,
        kind: "knows".into(),
        attrs: serde_json::json!({}),
    };
    assert_eq!(r.src_id, 10);
    assert_eq!(r.dst_id, 20);
    assert_eq!(r.kind, "knows");
}

/// Pin every field on `WalkedEdge` so a future rename (e.g. dropping
/// `src_quarantine` or renaming `edge_id` to `relation_id`) trips a
/// compile error in the test rather than silently breaking the CLI
/// renderer's column-projection assumptions.
#[test]
fn walked_edge_struct_field_shape() {
    let e = WalkedEdge {
        depth: 1,
        edge_id: 42,
        src_id: 10,
        src_kind: "person".into(),
        src_name: "alice".into(),
        src_quarantine: false,
        dst_id: 20,
        dst_kind: "object".into(),
        dst_name: "cat".into(),
        dst_quarantine: true,
        kind: "owns".into(),
    };
    assert_eq!(e.depth, 1);
    assert_eq!(e.edge_id, 42);
    assert_eq!(e.src_id, 10);
    assert_eq!(e.src_kind, "person");
    assert_eq!(e.src_name, "alice");
    assert!(!e.src_quarantine);
    assert_eq!(e.dst_id, 20);
    assert_eq!(e.dst_kind, "object");
    assert_eq!(e.dst_name, "cat");
    assert!(e.dst_quarantine);
    assert_eq!(e.kind, "owns");
}

#[test]
fn clamp_walk_depth_leaves_in_range_untouched() {
    for d in 0..=MAX_WALK_DEPTH {
        assert_eq!(clamp_walk_depth(d), d, "depth {d} must not be clamped");
    }
}

#[test]
fn clamp_walk_depth_clamps_above_cap() {
    assert_eq!(clamp_walk_depth(MAX_WALK_DEPTH + 1), MAX_WALK_DEPTH);
    assert_eq!(clamp_walk_depth(u8::MAX), MAX_WALK_DEPTH);
}

#[test]
fn max_walk_depth_constant_pin() {
    // Pin the *value* — Next-TODO Item 21's design budget is 5, and
    // a quiet bump to 10 would change the worst-case CTE row count
    // by a factor of 10^5 on a 10-fan-out graph. If a future need
    // forces a higher cap, the change should be deliberate and
    // visible in the diff.
    assert_eq!(MAX_WALK_DEPTH, 5);
}
