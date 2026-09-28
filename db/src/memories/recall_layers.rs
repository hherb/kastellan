//! Which memory layers the recall lanes may return — the `<recalled>`
//! layer gate (issue #785).
//!
//! ## The rule
//!
//! Every layer that has its **own, dedicated door** into the planner's
//! prompt is kept out of recall, so that door stays the only one:
//!
//! | Layer | Recallable | Why |
//! | --- | --- | --- |
//! | L0 `Meta` | **no** | `<l0_meta_rules>` loads the *newest* row per rule. L0 is append-only, so recall could return a **superseded** version of an edited rule — and duplicates the current one. |
//! | L1 `Index` | yes | `<l1_insights>` holds only the newest 32 rows / 4 KiB. L1 rows are embedded on promotion (and by the #325 backfill) precisely so older insights stay reachable through recall. There is no trust gate to bypass. |
//! | L2 `Stable` | yes | Recall is L2's only door. |
//! | L3 `Skill` | **no** | `<skills>` surfaces only operator-approved (`user_approved`/`pinned`) rows. Recall would let an `untrusted` skill reach the planner — the trust gate would hold on one path and not the other. |
//! | L4 `Digest` | yes | No dedicated block yet; when one exists, revisit this row. |
//!
//! ## Where it is enforced
//!
//! Twice, from this one definition:
//!
//! 1. **In each lane's SQL** (`layer = ANY($n)`, bound from
//!    [`recallable_layer_codes`]). This is the primary gate: an excluded
//!    row never takes a lane slot, so a layer that grows with task
//!    history (L3 gains a row per multi-step task) cannot crowd real
//!    candidates out of the fusion.
//! 2. **After hydration** in `core::memory::recall`, via
//!    [`retain_recallable`]. Redundant today by design — it keeps the
//!    rule true for a future fifth lane whose author forgets step 1.
//!
//! Everything here is pure: no I/O, deterministic, unit-tested without a
//! database. The SQL side is pinned by `core/tests/memory_recall_layer_gate_e2e.rs`.

use super::{Memory, MemoryLayer};

impl MemoryLayer {
    /// Every layer, in discriminant order.
    ///
    /// A new variant added without extending this list is **not**
    /// recallable (it never reaches [`recallable_layer_codes`]) — the
    /// fail-closed direction. The unit test
    /// `all_matches_every_decodable_layer` catches the drift against
    /// [`MemoryLayer::from_db`].
    pub const ALL: [MemoryLayer; 5] = [
        MemoryLayer::Meta,
        MemoryLayer::Index,
        MemoryLayer::Stable,
        MemoryLayer::Skill,
        MemoryLayer::Digest,
    ];

    /// May a row at this layer be returned by the recall lanes (and so
    /// reach the planner's `<recalled>` block)? See the module doc for
    /// the per-layer reasons.
    ///
    /// The `match` is exhaustive on purpose — no `_` arm — so adding a
    /// layer is a compile error here until someone decides this question
    /// for it.
    pub const fn is_recallable(self) -> bool {
        match self {
            MemoryLayer::Meta | MemoryLayer::Skill => false,
            MemoryLayer::Index | MemoryLayer::Stable | MemoryLayer::Digest => true,
        }
    }
}

/// The SMALLINT codes of every recallable layer, in ascending order —
/// the value each recall lane binds for its `layer = ANY($n)` predicate.
pub fn recallable_layer_codes() -> Vec<i16> {
    MemoryLayer::ALL
        .into_iter()
        .filter(|layer| layer.is_recallable())
        .map(MemoryLayer::as_db)
        .collect()
}

/// Drop every row whose layer is not recallable, keeping the order of
/// the rest — the post-hydration half of the gate (see the module doc).
pub fn retain_recallable(rows: Vec<Memory>) -> Vec<Memory> {
    rows.into_iter().filter(|row| row.layer.is_recallable()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: i64, layer: MemoryLayer) -> Memory {
        Memory {
            id,
            body: format!("row {id}"),
            metadata: serde_json::json!({}),
            layer,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    /// The decision table from the module doc, one layer per line — the
    /// test a reviewer reads to see the rule.
    #[test]
    fn only_l1_l2_l4_are_recallable() {
        assert!(!MemoryLayer::Meta.is_recallable(), "L0: own block; superseded versions");
        assert!(MemoryLayer::Index.is_recallable(), "L1: older insights reachable by recall");
        assert!(MemoryLayer::Stable.is_recallable(), "L2: recall is its only door");
        assert!(!MemoryLayer::Skill.is_recallable(), "L3: trust-gated <skills> only");
        assert!(MemoryLayer::Digest.is_recallable(), "L4: no dedicated block yet");
    }

    /// `ALL` lists exactly the layers the database can hold: every code
    /// `from_db` accepts, and none it rejects. Probes well past the
    /// current range so a new variant decoded by `from_db` but missing
    /// from `ALL` is caught.
    #[test]
    fn all_matches_every_decodable_layer() {
        let decodable: Vec<MemoryLayer> =
            (-4i16..=16).filter_map(|raw| MemoryLayer::from_db(raw).ok()).collect();
        assert_eq!(decodable, MemoryLayer::ALL.to_vec());
    }

    /// The SQL bind value: L1, L2, L4 by their stored codes.
    #[test]
    fn recallable_codes_are_1_2_4() {
        assert_eq!(recallable_layer_codes(), vec![1, 2, 4]);
    }

    /// The post-hydration filter drops L0 and L3 and keeps the rest in
    /// their fused (ranked) order.
    #[test]
    fn retain_recallable_drops_l0_l3_and_keeps_rank_order() {
        let rows = vec![
            row(10, MemoryLayer::Skill),
            row(11, MemoryLayer::Stable),
            row(12, MemoryLayer::Meta),
            row(13, MemoryLayer::Digest),
            row(14, MemoryLayer::Index),
        ];
        let kept: Vec<i64> = retain_recallable(rows).into_iter().map(|m| m.id).collect();
        assert_eq!(kept, vec![11, 13, 14]);
    }

    /// Nothing in, nothing out.
    #[test]
    fn retain_recallable_on_empty_is_empty() {
        assert!(retain_recallable(Vec::new()).is_empty());
    }
}
