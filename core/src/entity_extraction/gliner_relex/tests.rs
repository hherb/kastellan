//! Unit tests for the GLiNER-Relex entity-extraction adapter.
//!
//! Lifted verbatim from the parent module's inline `#[cfg(test)] mod tests`
//! (item 9b over-cap test-lift). Production logic lives in the parent
//! `gliner_relex.rs`; this file is `mod tests;` from there and is only
//! compiled under `#[cfg(test)]`.

use super::*;

#[test]
fn chunk_text_empty_returns_empty() {
    assert!(chunk_text("", 100, 10).is_empty());
}

#[test]
fn chunk_text_under_cap_returns_single_chunk() {
    let chunks = chunk_text("hello world", 100, 10);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].byte_offset, 0);
    assert_eq!(chunks[0].text, "hello world");
}

#[test]
fn chunk_text_exactly_at_cap_returns_single_chunk() {
    let text = "a".repeat(100);
    let chunks = chunk_text(&text, 100, 10);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].text.len(), 100);
}

#[test]
fn chunk_text_over_cap_produces_overlapping_chunks() {
    // 250 bytes, cap 100, overlap 20 → stride 80, so chunks at
    // [0..100], [80..180], [160..250]. Three chunks.
    let text = "x".repeat(250);
    let chunks = chunk_text(&text, 100, 20);
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks[0].byte_offset, 0);
    assert_eq!(chunks[0].text.len(), 100);
    assert_eq!(chunks[1].byte_offset, 80);
    assert_eq!(chunks[1].text.len(), 100);
    assert_eq!(chunks[2].byte_offset, 160);
    assert_eq!(chunks[2].text.len(), 250 - 160);
}

#[test]
fn chunk_text_walks_utf8_boundary() {
    // "café" is 5 bytes (é is U+00E9 = 0xC3 0xA9). Cap 4 should
    // back off so the chunk ends at the 'f' (byte 3), not split é.
    let text = "café";
    let chunks = chunk_text(text, 4, 1);
    // chunk 0 must be valid UTF-8.
    assert!(std::str::from_utf8(chunks[0].text.as_bytes()).is_ok());
    // No chunk's bytes end mid-codepoint.
    for c in &chunks {
        assert!(std::str::from_utf8(c.text.as_bytes()).is_ok());
    }
}

use crate::workers::gliner_relex::{Entity, Triple, TripleEntity, ExtractResponse};

fn ent(text: &str, label: &str, start: u32, end: u32) -> Entity {
    Entity {
        text: text.into(),
        label: label.into(),
        start, end,
        score: 0.9,
    }
}

fn tent(text: &str, ty: &str, idx: u32) -> TripleEntity {
    TripleEntity {
        text: text.into(),
        r#type: ty.into(),
        start: 0,
        end: text.len() as u32,
        entity_idx: idx,
    }
}

#[test]
fn merge_chunks_dedups_entities_by_label_and_norm() {
    let resp_a = ExtractResponse {
        entities: vec![ent("Dr Smith", "person", 0, 8)],
        triples: vec![],
    };
    let resp_b = ExtractResponse {
        // Same person, different case — must dedup.
        entities: vec![ent("DR SMITH", "person", 5, 13)],
        triples: vec![],
    };
    let merged = merge_chunks(vec![(0, resp_a), (7500, resp_b)]);
    assert_eq!(merged.entities.len(), 1, "case-insensitive dedup");
    assert_eq!(merged.entities[0].text, "Dr Smith", "first-writer-wins on display");
}

#[test]
fn merge_chunks_re_anchors_offsets_to_original_text() {
    let resp_a = ExtractResponse {
        entities: vec![ent("alpha", "concept", 0, 5)],
        triples: vec![],
    };
    let resp_b = ExtractResponse {
        entities: vec![ent("beta", "concept", 0, 4)],
        triples: vec![],
    };
    // Second chunk starts at byte 7500 in the original text.
    let merged = merge_chunks(vec![(0, resp_a), (7500, resp_b)]);
    assert_eq!(merged.entities[0].start, 0);
    assert_eq!(merged.entities[0].end, 5);
    assert_eq!(merged.entities[1].start, 7500);
    assert_eq!(merged.entities[1].end, 7500 + 4);
}

#[test]
fn merge_chunks_dedups_triples_by_head_tail_relation() {
    let triple_a = Triple {
        head: tent("Dr Smith", "person", 0),
        tail: tent("asthma", "disease", 1),
        relation: "treats".into(),
        score: 0.95,
    };
    let triple_b = Triple {
        head: tent("DR SMITH", "person", 0),  // case-insensitive same
        tail: tent("Asthma", "disease", 1),
        relation: "TREATS".into(),
        score: 0.92,
    };
    let resp_a = ExtractResponse { entities: vec![], triples: vec![triple_a] };
    let resp_b = ExtractResponse { entities: vec![], triples: vec![triple_b] };
    let merged = merge_chunks(vec![(0, resp_a), (5000, resp_b)]);
    assert_eq!(merged.triples.len(), 1, "case-insensitive triple dedup");
}

// ─── strip_undefined_label (pure helper for relation-vocab slice) ─

/// `undefined` is the FK-fallback target on `relations.kind`; it
/// must never reach GLiNER. The pure helper drops it regardless of
/// position in the list, keeping the rest verbatim.
#[test]
fn strip_undefined_label_drops_undefined_keeps_rest() {
    let input = vec![
        "associated with".to_string(),
        "treats".to_string(),
        "undefined".to_string(),
        "located in".to_string(),
    ];
    let out = strip_undefined_label(input);
    assert_eq!(
        out,
        vec![
            "associated with".to_string(),
            "treats".to_string(),
            "located in".to_string(),
        ],
    );
}

/// A list without `undefined` must pass through untouched. Pinned
/// so a future contributor doesn't accidentally make the filter
/// destructive on the common path.
#[test]
fn strip_undefined_label_is_identity_when_undefined_absent() {
    let input = vec!["treats".to_string(), "located in".to_string()];
    let out = strip_undefined_label(input.clone());
    assert_eq!(out, input);
}

/// An empty list stays empty — defends against an off-by-one
/// future regression where the helper accidentally panics or
/// inserts a sentinel on empty input.
#[test]
fn strip_undefined_label_handles_empty_input() {
    let out = strip_undefined_label(Vec::<String>::new());
    assert!(out.is_empty());
}

/// Multiple `undefined` entries (shouldn't happen — PK on the
/// table prevents it — but defends against future schema or
/// migration glitches) are all dropped, not just the first.
#[test]
fn strip_undefined_label_drops_all_undefined_occurrences() {
    let input = vec![
        "undefined".to_string(),
        "treats".to_string(),
        "undefined".to_string(),
    ];
    let out = strip_undefined_label(input);
    assert_eq!(out, vec!["treats".to_string()]);
}
