//! Entity-review + entity/relation-kind audit-row payload builders.
//!
//! Split out of the parent `scheduler/audit.rs` (500-LOC cap); the
//! parent re-exports everything here via `pub use`, so the public paths
//! `scheduler::audit::{ACTION_ENTITIES_*, ACTION_ENTITY_KINDS_*,
//! ACTION_RELATION_KINDS_*, build_entities_*}` are unchanged. Function
//! bodies and doc comments are verbatim moves.
//!
//! These are the `actor='cli'` rows behind the operator's entity
//! quarantine-review workflow (`kastellan-cli entities
//! approve/reject/merge`) and the entity/relation kind-label registry
//! (`entities kinds` / `relations kinds` add/remove). The
//! `extractor:gliner-relex` *summary* row is a different family — see
//! the sibling `extract_entities` module.

/// `actor='cli' action='entities.approved'` — operator flipped a
/// quarantined entity to approved. Payload: {entity_id, kind, name}.
pub const ACTION_ENTITIES_APPROVED: &str = "entities.approved";

/// `actor='cli' action='entities.rejected'` — operator deleted a
/// quarantined entity. Payload:
/// {entity_id, kind, name, mentions_dropped}. The `mentions_dropped`
/// field is the number of `memory_entities` rows cascaded by the FK.
pub const ACTION_ENTITIES_REJECTED: &str = "entities.rejected";

/// `actor='cli' action='entities.merged'` — operator consolidated near-
/// duplicate entities. Payload: {kept_id, kept_kind, kept_name,
/// dropped_ids, links_retargeted, links_dropped_as_duplicate}.
pub const ACTION_ENTITIES_MERGED: &str = "entities.merged";

/// `actor='cli' action='entity_kinds.add'` — operator added a new
/// entity-kind label via `kastellan-cli entities kinds add`. Payload:
/// `{kind, description}` where `description` is `null` when omitted.
/// Emitted only on a real INSERT (`Ok(true)`); idempotent re-adds and
/// validation errors write no row. Symmetric to
/// [`ACTION_RELATION_KINDS_ADD`].
pub const ACTION_ENTITY_KINDS_ADD: &str = "entity_kinds.add";

/// `actor='cli' action='entity_kinds.remove'` — operator removed an
/// entity-kind label via `kastellan-cli entities kinds remove`. Payload:
/// `{kind}`. Emitted only on a real DELETE (`Ok(true)`); idempotent
/// no-ops, validation errors, and the explicit
/// `RemovalOfUndefinedRejected` write no row. Symmetric to
/// [`ACTION_RELATION_KINDS_REMOVE`].
pub const ACTION_ENTITY_KINDS_REMOVE: &str = "entity_kinds.remove";

/// `actor='cli' action='relation_kinds.add'` — operator added a new
/// relation-kind label via `kastellan-cli relations kinds add`. Payload:
/// `{kind, description}` where `description` is `null` when omitted.
/// Emitted only on a real INSERT (`Ok(true)`); idempotent re-adds and
/// validation errors write no row. Symmetric to
/// [`super::ACTION_TOOLS_ALLOWLIST_ADD`].
pub const ACTION_RELATION_KINDS_ADD: &str = "relation_kinds.add";

/// `actor='cli' action='relation_kinds.remove'` — operator removed a
/// relation-kind label via `kastellan-cli relations kinds remove`.
/// Payload: `{kind}`. Emitted only on a real DELETE (`Ok(true)`);
/// idempotent no-ops, validation errors, and the explicit
/// `RemovalOfUndefinedRejected` write no row. Symmetric to
/// [`super::ACTION_TOOLS_ALLOWLIST_REMOVE`].
pub const ACTION_RELATION_KINDS_REMOVE: &str = "relation_kinds.remove";

/// Build the wire-stable payload for `actor='cli' action='entities.approved'`.
/// Keys: {entity_id, kind, name} (3 keys, BTreeSet-pinned in tests).
pub fn build_entities_approved_payload(
    entity_id: i64,
    kind: &str,
    name: &str,
) -> serde_json::Value {
    serde_json::json!({
        "entity_id": entity_id,
        "kind":      kind,
        "name":      name,
    })
}

/// Build the wire-stable payload for `actor='cli' action='entities.rejected'`.
/// Keys: {entity_id, kind, name, mentions_dropped} (4 keys).
pub fn build_entities_rejected_payload(
    entity_id: i64,
    kind: &str,
    name: &str,
    mentions_dropped: i64,
) -> serde_json::Value {
    serde_json::json!({
        "entity_id":        entity_id,
        "kind":             kind,
        "name":             name,
        "mentions_dropped": mentions_dropped,
    })
}

/// Build the wire-stable payload for `actor='cli' action='entities.merged'`.
/// Keys: {kept_id, kept_kind, kept_name, dropped_ids, links_retargeted,
/// links_dropped_as_duplicate} (6 keys).
pub fn build_entities_merged_payload(
    kept_id: i64,
    kept_kind: &str,
    kept_name: &str,
    dropped_ids: &[i64],
    links_retargeted: i64,
    links_dropped_as_duplicate: i64,
) -> serde_json::Value {
    serde_json::json!({
        "kept_id":                     kept_id,
        "kept_kind":                   kept_kind,
        "kept_name":                   kept_name,
        "dropped_ids":                 dropped_ids,
        "links_retargeted":            links_retargeted,
        "links_dropped_as_duplicate":  links_dropped_as_duplicate,
    })
}
