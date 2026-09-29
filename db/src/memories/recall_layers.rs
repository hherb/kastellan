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
//! | L1 `Index` | yes | `<l1_insights>` holds only the newest 32 rows / 4 KiB (`core::memory::layers::L1_DEFAULT_CAP_ROWS` / `L1_DEFAULT_CAP_BYTES`). Agent-raised L1 rows are embedded on promotion (#324) and operator-added ones by the #325 backfill, precisely so older insights stay reachable through recall. There is no trust gate to bypass. |
//! | L2 `Stable` | yes | Recall is L2's only door. |
//! | L3 `Skill` | **no** | `<skills>` surfaces only operator-approved (`user_approved`/`pinned`) rows. Recall would let an `untrusted` skill reach the planner — the trust gate would hold on one path and not the other. |
//! | L4 `Digest` | yes | No dedicated block yet; when #629's `<history>` block lands, revisit this row. |
//!
//! ## Where it is enforced
//!
//! Twice, from this one definition:
//!
//! 1. **In each lane's SQL** (`layer = ANY($n)`, bound from
//!    [`recallable_layer_codes`]), before the lane's `LIMIT`. This is the
//!    primary gate: an excluded row never takes one of the lane's `k`
//!    memory slots, so a layer that grows with task history (the agent
//!    can crystallise a new `untrusted` skill after any completed
//!    tool-using task) cannot crowd real candidates out of the fusion.
//!    The entity-similarity lane also applies it to its entity fan-out,
//!    so an entity linked only to L0/L3 rows does not take one of its
//!    nearest-entity slots either.
//! 2. **After hydration** in `core::memory::recall`, via
//!    [`retain_recallable`]. It drops nothing while step 1 holds; it
//!    exists for a future lane whose author forgets step 1, and when it
//!    does drop a row it **says so** (an `error!` per row, and a
//!    `debug_assert!` so any debug-build test through `recall()` fails),
//!    because a dropped row means a lane's SQL filter has regressed.
//!
//! The rule, the codes and [`split_recallable`] are pure: no I/O,
//! deterministic, unit-tested without a database. [`retain_recallable`]
//! is that split plus the breach report. The SQL side is pinned by
//! `core/tests/memory_recall_layer_gate_e2e.rs`.
//!
//! ⚠️ **If an HNSW index on `memories.embedding` is ever added,** the
//! semantic lane's `layer` predicate is applied *after* the index scan
//! (pgvector's post-filtering, bounded by `hnsw.ef_search`), so that
//! lane could return fewer than `k` rows. Re-check step 1's "before the
//! `LIMIT`" claim then (iterative scans, or a partial index per layer
//! set).

use super::{Memory, MemoryLayer};

impl MemoryLayer {
    /// Every layer, in discriminant order.
    ///
    /// This list feeds only the SQL half of the gate. A variant missing
    /// from it is excluded by every lane even if [`Self::is_recallable`]
    /// says yes — safe, but silent. The unit test
    /// `all_matches_every_decodable_layer` catches this list drifting
    /// from [`MemoryLayer::from_db`]; a variant missing from **both**
    /// passes that test and stays fail-closed (never bound, and rejected
    /// on decode).
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
pub(crate) fn recallable_layer_codes() -> Vec<i16> {
    MemoryLayer::ALL
        .into_iter()
        .filter(|layer| layer.is_recallable())
        .map(MemoryLayer::as_db)
        .collect()
}

/// Split hydrated rows into `(recallable, withheld)`, each keeping its
/// input (ranked) order. Pure; [`retain_recallable`] is the caller.
fn split_recallable(rows: Vec<Memory>) -> (Vec<Memory>, Vec<Memory>) {
    rows.into_iter().partition(|row| row.layer.is_recallable())
}

/// The post-hydration half of the gate (see the module doc): keep the
/// recallable rows in their ranked order, and **report** every row it
/// withholds.
///
/// A withheld row is never expected — every lane's SQL already applies
/// the rule — so one here means a lane's filter has regressed. Each is
/// logged at `error!` with its id and layer, and a debug build panics
/// (`debug_assert!`), so the backstop cannot quietly absorb the defect
/// it exists to catch. Release builds log and carry on: the row is still
/// withheld, and a `panic = "abort"` daemon must not die over it. The
/// caller sees fewer than `k` rows; that is intended — a leak is
/// surfaced, not back-filled.
pub fn retain_recallable(rows: Vec<Memory>) -> Vec<Memory> {
    let (kept, withheld) = split_recallable(rows);
    for row in &withheld {
        tracing::error!(
            memory_id = row.id,
            layer = ?row.layer,
            "recall layer gate (#785): a recall lane returned a non-recallable row, \
             so its SQL layer filter has regressed; the row is withheld from <recalled>",
        );
    }
    debug_assert!(
        withheld.is_empty(),
        "recall layer gate (#785): a recall lane leaked non-recallable rows {:?}",
        withheld.iter().map(|row| (row.id, row.layer)).collect::<Vec<_>>()
    );
    kept
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
    /// `from_db` accepts, and none it rejects, in ascending order. Probes
    /// the whole `i16` range, so no "wide enough?" question arises.
    #[test]
    fn all_matches_every_decodable_layer() {
        let decodable: Vec<MemoryLayer> =
            (i16::MIN..=i16::MAX).filter_map(|raw| MemoryLayer::from_db(raw).ok()).collect();
        assert_eq!(decodable, MemoryLayer::ALL.to_vec());
    }

    /// The SQL bind value: L1, L2, L4 by their stored codes.
    #[test]
    fn recallable_codes_are_1_2_4() {
        assert_eq!(recallable_layer_codes(), vec![1, 2, 4]);
    }

    fn ids(rows: &[Memory]) -> Vec<i64> {
        rows.iter().map(|m| m.id).collect()
    }

    /// The split sends L0 and L3 to `withheld` and keeps both halves in
    /// their fused (ranked) order.
    #[test]
    fn split_recallable_separates_l0_l3_and_keeps_rank_order() {
        let rows = vec![
            row(10, MemoryLayer::Skill),
            row(11, MemoryLayer::Stable),
            row(12, MemoryLayer::Meta),
            row(13, MemoryLayer::Digest),
            row(14, MemoryLayer::Index),
        ];
        let (kept, withheld) = split_recallable(rows);
        assert_eq!(ids(&kept), vec![11, 13, 14]);
        assert_eq!(ids(&withheld), vec![10, 12]);
    }

    /// Nothing in, nothing out.
    #[test]
    fn split_recallable_on_empty_is_empty() {
        let (kept, withheld) = split_recallable(Vec::new());
        assert!(kept.is_empty() && withheld.is_empty());
    }

    /// The expected case: every row recallable, so the backstop passes
    /// them all through unchanged and in order.
    #[test]
    fn retain_recallable_passes_an_all_recallable_list_through() {
        let rows = vec![
            row(21, MemoryLayer::Digest),
            row(22, MemoryLayer::Index),
            row(23, MemoryLayer::Stable),
        ];
        assert_eq!(ids(&retain_recallable(rows)), vec![21, 22, 23]);
    }

    /// A leak is loud in a debug build: the backstop panics rather than
    /// quietly absorbing a lane whose SQL filter regressed.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "recall layer gate (#785): a recall lane leaked")]
    fn retain_recallable_panics_on_a_leak_in_debug() {
        retain_recallable(vec![row(31, MemoryLayer::Stable), row(32, MemoryLayer::Skill)]);
    }

    /// In a release build the same leak is withheld (and logged), not a
    /// panic — the daemon runs `panic = "abort"`.
    #[cfg(not(debug_assertions))]
    #[test]
    fn retain_recallable_withholds_a_leak_in_release() {
        let kept = retain_recallable(vec![row(31, MemoryLayer::Stable), row(32, MemoryLayer::Skill)]);
        assert_eq!(ids(&kept), vec![31]);
    }
}
