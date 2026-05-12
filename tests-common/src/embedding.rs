//! Deterministic SHA-256-seeded embedding helper for memory-recall
//! tests.
//!
//! The text is hashed (SHA-256) and the first 8 bytes seed a
//! Marsaglia xorshift64 generator. The output is `EMBEDDING_DIM`
//! f32 values in `[-1, 1)`, L2-normalised so cosine similarity equals
//! dot product. Two identical inputs always yield byte-identical
//! vectors; two different inputs are overwhelmingly likely to be
//! near-orthogonal.
//!
//! This stand-in is used to exercise the pgvector path without
//! dragging the real embedding-router HTTP dependency into a unit-
//! style integration test. Production embeddings come from
//! `hhagent_core::memory::embed_query` (the LLM router).

use sha2::Digest;

/// Returns a deterministic L2-normalised f32 vector of length
/// `hhagent_db::memories::EMBEDDING_DIM` seeded by `text`.
///
/// The same input always yields the same output (no randomness
/// beyond the SHA-256 of the input). A zero seed (vanishingly
/// unlikely from a real SHA-256) is OR-coerced to 1 to defend
/// xorshift64's invalid-seed corner.
pub fn text_to_embedding(text: &str) -> Vec<f32> {
    let dim = hhagent_db::memories::EMBEDDING_DIM;
    let digest = sha2::Sha256::digest(text.as_bytes());

    // Pack the first 8 bytes of the digest little-endian into u64.
    let mut seed: u64 = 0;
    for (i, b) in digest[..8].iter().enumerate() {
        seed |= (*b as u64) << (i * 8);
    }
    if seed == 0 {
        seed = 1;
    }

    let mut state = seed;
    let mut v: Vec<f32> = Vec::with_capacity(dim);
    for _ in 0..dim {
        // Marsaglia xorshift64.
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        // Top 24 bits → f32 in [-1, 1).
        let bits = (state >> 40) as u32;
        let unit = (bits as f32) / ((1u32 << 24) as f32);
        v.push(unit * 2.0 - 1.0);
    }

    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
    v
}
