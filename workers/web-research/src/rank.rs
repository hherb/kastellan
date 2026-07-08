//! Rank passages by relevance to the query.
//!
//! Pure ranking primitives, fused by the caller (`research()`): [`bm25`] is the
//! lexical lane (no model), [`cosine`] is the semantic lane over embedding
//! vectors, and [`rrf_fuse`] combines both lanes' rankings via Reciprocal Rank
//! Fusion (mirroring `core::memory::recall`). See the design spec's
//! "Extensibility" section.

/// A passage with its relevance score (higher = more relevant).
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredPassage {
    pub text: String,
    pub score: f64,
}

/// BM25 free-parameters (Robertson/Sparck-Jones defaults).
const K1: f64 = 1.5;
const B: f64 = 0.75;

/// Lowercase unicode-word tokens (alphanumeric runs). Punctuation is a separator.
fn tokenize(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect()
}

/// Lexical BM25 lane. Pure + deterministic; treats the passage set as the corpus.
pub fn bm25(query: &str, passages: &[String]) -> Vec<ScoredPassage> {
    // Unique query terms (BM25 sums each term once regardless of repeats).
    let q_terms = unique(&tokenize(query));
    if q_terms.is_empty() || passages.is_empty() {
        return Vec::new();
    }
    // Tokenize each passage once.
    let docs: Vec<Vec<String>> = passages.iter().map(|p| tokenize(p)).collect();
    let n = docs.len() as f64;
    let avg_len: f64 = docs.iter().map(|d| d.len()).sum::<usize>() as f64 / n.max(1.0);

    // Document frequency per query term — depends only on the corpus, so
    // compute it once (not per document × term).
    let dfs: Vec<f64> = q_terms
        .iter()
        .map(|term| docs.iter().filter(|d| d.contains(term)).count() as f64)
        .collect();

    let mut scored: Vec<ScoredPassage> = Vec::new();
    for (doc, passage) in docs.iter().zip(passages.iter()) {
        let dl = doc.len() as f64;
        let mut score = 0.0_f64;
        for (term, &df) in q_terms.iter().zip(dfs.iter()) {
            let tf = doc.iter().filter(|t| *t == term).count() as f64;
            if tf == 0.0 {
                continue;
            }
            // BM25 idf with the +1 floor so it is never negative.
            let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
            let denom = tf + K1 * (1.0 - B + B * dl / avg_len.max(1.0));
            score += idf * (tf * (K1 + 1.0)) / denom;
        }
        if score > 0.0 {
            scored.push(ScoredPassage { text: passage.clone(), score });
        }
    }
    // Best-first; stable tie-break by original order via sort_by.
    scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    scored
}

/// L2 norm of a vector.
fn l2_norm(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>().sqrt()
}

/// Semantic lane: cosine similarity of each passage embedding to the query
/// embedding. Pure. Passages whose embedding is zero-norm, is a different length
/// than the query embedding, or yields a non-positive similarity are omitted
/// (mirrors `bm25`'s "no signal -> omit"). `passage_embs[i]` pairs with
/// `passages[i]`; a length mismatch between the two slices yields an empty result.
pub fn cosine(query_emb: &[f32], passages: &[String], passage_embs: &[Vec<f32>])
    -> Vec<ScoredPassage>
{
    if query_emb.is_empty() || passages.len() != passage_embs.len() {
        return Vec::new();
    }
    let qn = l2_norm(query_emb);
    if qn == 0.0 {
        return Vec::new();
    }
    let mut scored: Vec<ScoredPassage> = Vec::new();
    for (p, e) in passages.iter().zip(passage_embs.iter()) {
        if e.len() != query_emb.len() {
            continue;
        }
        let en = l2_norm(e);
        if en == 0.0 {
            continue;
        }
        let dot: f32 = query_emb.iter().zip(e.iter()).map(|(a, b)| a * b).sum();
        let sim = (dot / (qn * en)) as f64;
        if sim > 0.0 {
            scored.push(ScoredPassage { text: p.clone(), score: sim });
        }
    }
    scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    scored
}

/// RRF damping constant (classical k = 60, matching `core::memory::recall`).
pub const RRF_K: f64 = 60.0;

/// Fuse two best-first ranked lists via parameter-free Reciprocal Rank Fusion.
/// Each passage's fused score = sum over lanes of 1/(RRF_K + rank), where `rank`
/// is 1-based position in that lane. Keyed by passage text (both lanes rank the
/// same passage set). Best-first; stable tie-break by first-seen order (a passage
/// appearing in only one lane still surfaces — the union is deliberate recall).
pub fn rrf_fuse(lexical: &[ScoredPassage], semantic: &[ScoredPassage])
    -> Vec<ScoredPassage>
{
    use std::collections::HashMap;
    let mut scores: HashMap<&str, f64> = HashMap::new();
    let mut order: Vec<&str> = Vec::new(); // first-seen order for stable ties
    for lane in [lexical, semantic] {
        for (i, sp) in lane.iter().enumerate() {
            let rank = (i + 1) as f64;
            let key = sp.text.as_str();
            scores
                .entry(key)
                .and_modify(|s| *s += 1.0 / (RRF_K + rank))
                .or_insert_with(|| {
                    order.push(key);
                    1.0 / (RRF_K + rank)
                });
        }
    }
    let mut out: Vec<ScoredPassage> = order
        .iter()
        .map(|k| ScoredPassage { text: (*k).to_string(), score: scores[*k] })
        .collect();
    // Stable sort: ties keep first-seen (lexical-first) order.
    out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    out
}

/// Unique terms preserving first-seen order.
fn unique(terms: &[String]) -> Vec<String> {
    let mut seen = Vec::new();
    for t in terms {
        if !seen.contains(t) {
            seen.push(t.clone());
        }
    }
    seen
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(scored: &[ScoredPassage]) -> Vec<&str> {
        scored.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn ranks_on_topic_passage_above_off_topic() {
        let passages = vec![
            "The cat sat on the mat and slept all afternoon.".to_string(),
            "Rust uses bwrap to create unprivileged user namespaces for sandboxing.".to_string(),
        ];
        let r = bm25("bwrap user namespaces sandbox", &passages);
        assert_eq!(r.len(), 1, "off-topic passage should score 0 and be omitted");
        assert!(r[0].text.contains("bwrap"));
        assert!(r[0].score > 0.0);
    }

    #[test]
    fn orders_multiple_matches_by_relevance() {
        let passages = vec![
            "namespaces are mentioned once here.".to_string(),
            "user namespaces user namespaces user namespaces everywhere.".to_string(),
        ];
        let r = bm25("user namespaces", &passages);
        assert_eq!(r.len(), 2);
        assert!(r[0].text.starts_with("user namespaces user"), "denser match should rank first");
    }

    #[test]
    fn empty_query_or_passages_yields_empty() {
        assert!(bm25("", &["anything".to_string()]).is_empty());
        assert!(bm25("q", &[]).is_empty());
    }

    #[test]
    fn no_shared_terms_yields_empty() {
        let passages = vec!["completely unrelated content".to_string()];
        assert!(bm25("xyzzy plugh", &passages).is_empty());
    }

    #[test]
    fn tokenization_is_case_and_punctuation_insensitive() {
        let passages = vec!["BWRAP, the sandbox!".to_string()];
        let r = bm25("bwrap", &passages);
        assert_eq!(texts(&r), vec!["BWRAP, the sandbox!"]);
    }

    #[test]
    fn bm25_matches_legacy_lexical_ranker() {
        let passages = vec![
            "The cat sat on the mat.".to_string(),
            "Rust uses bwrap to create user namespaces for sandboxing.".to_string(),
        ];
        let r = bm25("bwrap user namespaces sandbox", &passages);
        assert_eq!(r.len(), 1);
        assert!(r[0].text.contains("bwrap") && r[0].score > 0.0);
    }

    #[test]
    fn cosine_ranks_similar_vector_first_and_skips_zero_norm() {
        let passages = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let embs = vec![
            vec![1.0_f32, 0.0],   // identical direction to query -> sim 1.0
            vec![0.0_f32, 1.0],   // orthogonal -> sim 0.0 -> omitted
            vec![0.0_f32, 0.0],   // zero-norm -> omitted
        ];
        let q = vec![1.0_f32, 0.0];
        let r = cosine(&q, &passages, &embs);
        assert_eq!(r.len(), 1, "only the similar passage survives");
        assert_eq!(r[0].text, "a");
        assert!((r[0].score - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_empty_inputs_yield_empty() {
        assert!(cosine(&[], &["x".to_string()], &[vec![1.0]]).is_empty());
        assert!(cosine(&[1.0], &[], &[]).is_empty());
        // length mismatch between passages and embeddings -> empty (defensive)
        assert!(cosine(&[1.0], &["x".to_string()], &[]).is_empty());
    }

    #[test]
    fn rrf_fuse_rewards_agreement_and_unions_lanes() {
        let lex = vec![
            ScoredPassage { text: "top-both".into(), score: 9.0 },
            ScoredPassage { text: "lex-only".into(), score: 1.0 },
        ];
        let sem = vec![
            ScoredPassage { text: "top-both".into(), score: 0.9 },
            ScoredPassage { text: "sem-only".into(), score: 0.5 },
        ];
        let f = rrf_fuse(&lex, &sem);
        let texts: Vec<&str> = f.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts[0], "top-both", "ranked #1 in both lanes wins");
        // union: a passage in only one lane still appears
        assert!(texts.contains(&"lex-only") && texts.contains(&"sem-only"));
    }

    #[test]
    fn rrf_fuse_with_empty_lane_equals_other_lane_order() {
        let lex = vec![
            ScoredPassage { text: "x".into(), score: 3.0 },
            ScoredPassage { text: "y".into(), score: 1.0 },
        ];
        let f = rrf_fuse(&lex, &[]);
        let texts: Vec<&str> = f.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, vec!["x", "y"]);
    }
}
