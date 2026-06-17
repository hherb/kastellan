//! Unit tests for three-lane RRF-fused recall (`RecallModes`, the RRF damping
//! constant, lane fusion, graph fan-out caps, and empty-seed degrade).
//!
//! Lifted verbatim from the parent module's inline `#[cfg(test)] mod tests`
//! block (Rust-2018 sibling-module pattern; precedents: `capture/tests.rs`,
//! `l0_seed/tests.rs`, `inner_loop/tests.rs`, `injection_guard/tests.rs`).
//! `use super::*` resolves to the parent `recall` module, so every item the
//! tests exercise is reachable exactly as before the lift.

use super::*;

/// `k = 60` is the canonical RRF damping; pinning it here makes a
/// future "tune the constant" PR explicit.
#[test]
fn rrf_k_constant_is_sixty() {
    assert!((RRF_K_CONSTANT - 60.0).abs() < f64::EPSILON);
}

/// `RecallModes::default` enables every lane — the configuration
/// the scheduler will use 99% of the time.
#[test]
fn recall_modes_default_runs_every_lane() {
    let m = RecallModes::default();
    assert!(m.semantic);
    assert!(m.lexical);
    assert!(m.graph);
}

#[test]
fn recall_modes_all_is_every_lane_on() {
    assert_eq!(
        RecallModes::ALL,
        RecallModes { semantic: true, lexical: true, graph: true }
    );
}

#[test]
fn recall_modes_semantic_only_disables_lexical() {
    let m = RecallModes::SEMANTIC_ONLY;
    assert!(m.semantic);
    assert!(!m.lexical);
    assert!(!m.graph);
}

#[test]
fn recall_modes_lexical_only_disables_semantic() {
    let m = RecallModes::LEXICAL_ONLY;
    assert!(!m.semantic);
    assert!(m.lexical);
    assert!(!m.graph);
}

/// Empty input → empty output. RRF over no lists must not produce
/// phantom scores.
#[test]
fn rrf_over_empty_lane_set_is_empty() {
    let out = reciprocal_rank_fusion(&[], 60.0);
    assert!(out.is_empty());
}

/// One lane with N items: output ranks match the input order, and
/// the score sequence is strictly decreasing (rank 1 > rank 2).
#[test]
fn rrf_single_lane_preserves_order() {
    let lane: &[i64] = &[10, 20, 30];
    let out = reciprocal_rank_fusion(&[lane], 60.0);
    let ids: Vec<i64> = out.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, vec![10, 20, 30]);
    assert!(out[0].1 > out[1].1);
    assert!(out[1].1 > out[2].1);
}

/// Two lanes, full agreement: the same top item wins, and its
/// score is exactly the sum of the per-lane contributions.
#[test]
fn rrf_two_lanes_agreeing_sums_scores() {
    let a: &[i64] = &[10, 20, 30];
    let b: &[i64] = &[10, 20, 30];
    let out = reciprocal_rank_fusion(&[a, b], 60.0);
    let top = out[0];
    assert_eq!(top.0, 10);
    // Doc 10 is rank 1 in both lanes: 2 / (60 + 1) = 0.0327...
    let expected = 2.0 / 61.0;
    assert!(
        (top.1 - expected).abs() < 1e-9,
        "expected {}, got {}",
        expected,
        top.1
    );
}

/// Two lanes, one item in lane A, a *different* item in lane B —
/// both at rank 1 in their lane. The fused list's top is the one
/// with the smaller id (tiebreaker), and both have identical
/// scores.
#[test]
fn rrf_two_lanes_disagreeing_ties_break_on_smaller_id() {
    let a: &[i64] = &[42];
    let b: &[i64] = &[7];
    let out = reciprocal_rank_fusion(&[a, b], 60.0);
    // Both have identical scores 1 / 61. Tiebreaker is smaller id.
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].0, 7);
    assert_eq!(out[1].0, 42);
    assert!((out[0].1 - out[1].1).abs() < 1e-12);
}

/// Items absent from a lane contribute 0, not a penalty. So a
/// top-of-list-in-lane-A item that's absent from lane B still
/// outranks a mid-list item that's only in lane A.
#[test]
fn rrf_absent_items_contribute_zero() {
    let a: &[i64] = &[1, 2, 3];
    let b: &[i64] = &[]; // empty lane
    let out = reciprocal_rank_fusion(&[a, b], 60.0);
    // Same as single-lane.
    let ids: Vec<i64> = out.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, vec![1, 2, 3]);
}

/// A document at rank 1 in one lane and rank 1 in the other beats
/// a document at rank 1 in only one lane — the value of fusion.
#[test]
fn rrf_two_lane_winner_beats_single_lane_winner() {
    // Lane A: doc 1 first, doc 2 second.
    // Lane B: doc 1 second, doc 9 first.
    // Doc 1 is the only doc that appears in both lanes near the
    // top; doc 9 only appears in B (at rank 1); doc 2 only appears
    // in A (at rank 2). Doc 1's two-lane score must be higher than
    // doc 9's single-lane top score.
    let a: &[i64] = &[1, 2];
    let b: &[i64] = &[9, 1];
    let out = reciprocal_rank_fusion(&[a, b], 60.0);
    assert_eq!(out[0].0, 1, "two-lane appearer must rank first: {out:?}");
}

/// Smaller `k` puts more weight on rank 1 — sanity check that the
/// constant is plumbed through and not hardcoded inside.
#[test]
fn rrf_smaller_k_weights_top_more() {
    let lane: &[i64] = &[10, 20];
    let out_60 = reciprocal_rank_fusion(&[lane], 60.0);
    let out_1 = reciprocal_rank_fusion(&[lane], 1.0);
    // Top score with k=60: 1/61 ≈ 0.0164
    // Top score with k=1: 1/2 = 0.5
    assert!(out_1[0].1 > out_60[0].1);
    // Ratio between rank 1 and rank 2 widens as k shrinks.
    let ratio_60 = out_60[0].1 / out_60[1].1;
    let ratio_1 = out_1[0].1 / out_1[1].1;
    assert!(ratio_1 > ratio_60);
}

/// `RecallModes::ALL` now includes the graph lane (third lane
/// added in Option P). If a future fourth lane lands without
/// updating `ALL`, this trips loudly.
// `ALL.*` are associated consts, so these assertions are
// const-foldable — that is exactly the point: a runtime drift pin
// that fails the moment `ALL` stops enabling a lane.
#[allow(clippy::assertions_on_constants)]
#[test]
fn recall_modes_all_includes_graph() {
    assert!(RecallModes::ALL.graph);
    assert!(RecallModes::ALL.semantic);
    assert!(RecallModes::ALL.lexical);
}

/// `RecallModes::GRAPH_ONLY` exact shape pin.
#[test]
fn recall_modes_graph_only_is_only_graph() {
    let m = RecallModes::GRAPH_ONLY;
    assert!(!m.semantic);
    assert!(!m.lexical);
    assert!(m.graph);
}

/// `RecallModes::SEMANTIC_AND_LEXICAL` is the default for
/// [`RecallParams::new`]: both text-bearing lanes on, graph off.
/// Pinned because the docs and call-site contract depend on it.
#[test]
fn recall_modes_semantic_and_lexical_is_two_text_lanes() {
    let m = RecallModes::SEMANTIC_AND_LEXICAL;
    assert!(m.semantic);
    assert!(m.lexical);
    assert!(!m.graph);
}

/// `RecallParams::new(text, emb)` leaves `seed_entity_ids = None`
/// and uses [`RecallModes::SEMANTIC_AND_LEXICAL`] — graph lane is
/// off by default because the no-seeds constructor cannot
/// populate it. Issue #40 pin.
#[test]
fn recall_params_new_default_is_semantic_and_lexical_no_seeds() {
    let emb: Vec<f32> = vec![0.0; 1024];
    let params = RecallParams::new("query text", &emb);
    assert!(params.seed_entity_ids.is_none());
    assert_eq!(params.modes, RecallModes::SEMANTIC_AND_LEXICAL);
    // Specifically: graph is OFF by default. If a future change
    // re-enables graph in `new()`, every prod caller starts
    // warn-and-skipping on every call — pin against that.
    assert!(!params.modes.graph);
}

/// `RecallParams::with_seeds` wires the graph lane on by populating
/// `seed_entity_ids` and switching `modes` to ALL. The seed-bearing
/// constructor is the only way to get graph contributions from the
/// canonical constructors. Issue #40 pin.
#[test]
fn recall_params_with_seeds_enables_all_three_lanes() {
    let emb: Vec<f32> = vec![0.0; 1024];
    let seeds: &[i64] = &[7, 42];
    let params = RecallParams::with_seeds("query", &emb, seeds);
    assert_eq!(params.seed_entity_ids, Some(seeds));
    assert_eq!(params.modes, RecallModes::ALL);
    assert!(params.modes.graph);
    assert!(params.modes.semantic);
    assert!(params.modes.lexical);
}

/// Pin `GRAPH_FANOUT_CAP_PER_SEED = 32` so a future tune is an
/// explicit PR.
#[test]
fn graph_fanout_cap_per_seed_is_thirty_two() {
    assert_eq!(GRAPH_FANOUT_CAP_PER_SEED, 32);
}

/// Issue #17 — the missing-input policy is *hybrid*, and this pins its sharp
/// edge: a lane whose input is absent warn-and-skips, BUT when **every**
/// enabled lane skipped, `recall` returns an `Err` instead of a silent
/// `Ok(vec![])` that would look like "no matches" and mask the caller bug.
/// Here the only enabled lane is semantic and `query_embedding` is `None`
/// — exactly the worst case called out in the issue. The error path returns
/// before any SQL runs, so a lazy (never-connected) pool is enough.
#[tokio::test]
async fn recall_errors_when_only_enabled_lane_lacks_its_input() {
    let pool = PgPool::connect_lazy("postgres://localhost/kastellan_recall_unit_unused")
        .expect("construct a lazy pool (no connection is attempted on the all-skip path)");
    let params = RecallParams {
        query_text: None,
        query_embedding: None,
        seed_entity_ids: None,
        k: 5,
        modes: RecallModes::SEMANTIC_ONLY,
    };
    let err = recall(&pool, &params)
        .await
        .expect_err("semantic-only with no embedding must error, not return an empty Ok");
    let msg = err.to_string();
    assert!(
        msg.contains("no lanes ran"),
        "error should explain the missing-input cause, got: {msg}"
    );
}
