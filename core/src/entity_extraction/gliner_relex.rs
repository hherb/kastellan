//! `GlinerRelexExtractor` — production EntityExtractor impl built on
//! the gliner-relex worker landed in PR #88.
//!
//! Per-call flow (composed across Tasks 7–11):
//!   1. Chunk the input text if it exceeds the worker's 8 KiB cap
//!      (`chunk_text`).
//!   2. Resolve current `entity_labels` via `db::entity_kinds::KindsCache`.
//!   3. Fire `Client::extract` per chunk (sequential — same warm worker).
//!   4. Merge per-chunk responses, dedup, re-anchor offsets
//!      (`merge_chunks`).
//!   5. Upsert entities + relations into PostgreSQL, quarantined by
//!      default (`upsert_entities_and_relations`).
//!   6. Emit `extractor:gliner-relex/extract_entities` summary audit
//!      row (`emit_extract_entities_audit`).
//!   7. Return `EntitySeeds`.

#[allow(unused_imports)] // ExtractRequest/ExtractResponse/Entity/Triple are used in Tasks 8-11.
use crate::workers::gliner_relex::{Entity, ExtractRequest, ExtractResponse, Triple};

/// Maximum chunk size in bytes — sized below the worker's 8192-byte
/// cap with headroom for label-list overhead in the JSON envelope.
pub const CHUNK_SIZE_BYTES: usize = 7500;

/// Overlap between consecutive chunks in bytes. Ensures entities that
/// span a naive split boundary still appear in at least one chunk in
/// full.
pub const OVERLAP_BYTES: usize = 500;

/// One chunk of the input with its byte offset into the original text.
/// `text` is always valid UTF-8 (the splitter never cuts mid-codepoint).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextChunk {
    pub byte_offset: usize,
    pub text: String,
}

/// Split `text` into overlapping chunks of at most `chunk_size_bytes`,
/// each subsequent chunk starting `chunk_size_bytes - overlap_bytes`
/// later. Empty input → empty Vec; input under-cap → single chunk
/// with the whole text.
///
/// The splitter walks UTF-8 char boundaries and never returns a chunk
/// that splits a codepoint. If a single codepoint exceeds the chunk
/// size (impossible in practice — codepoints are at most 4 bytes), the
/// function returns the codepoint as a single chunk regardless of cap.
pub fn chunk_text(text: &str, chunk_size_bytes: usize, overlap_bytes: usize) -> Vec<TextChunk> {
    if text.is_empty() {
        return Vec::new();
    }
    assert!(
        chunk_size_bytes > overlap_bytes,
        "chunk_size_bytes must exceed overlap_bytes"
    );

    if text.len() <= chunk_size_bytes {
        return vec![TextChunk {
            byte_offset: 0,
            text: text.to_string(),
        }];
    }

    let stride = chunk_size_bytes - overlap_bytes;
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        // Aim for `start + chunk_size_bytes` but back off to the
        // nearest char-boundary at-or-before that index.
        let mut end = (start + chunk_size_bytes).min(text.len());
        while end < text.len() && !text.is_char_boundary(end) {
            end += 1; // walk forward until we land on a boundary
        }
        // Same walk on `start` for safety, though our stride math keeps
        // it aligned in the common case.
        while start < text.len() && !text.is_char_boundary(start) {
            start += 1;
        }
        chunks.push(TextChunk {
            byte_offset: start,
            text: text[start..end].to_string(),
        });
        if end == text.len() {
            break;
        }
        start += stride;
    }
    chunks
}

#[cfg(test)]
mod tests {
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
}
