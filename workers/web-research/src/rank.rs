//! Rank passages by relevance to the query.
//!
//! [`PassageRanker`] is the seam: v1 ships [`LexicalRanker`] (pure BM25, no
//! model). A future `EmbeddingRanker` (semantic, via an embedding endpoint) and
//! a `HybridRanker` (RRF-fused, mirroring `core::memory::recall`) implement the
//! same trait and drop in without touching `research.rs`. See the design spec's
//! "Extensibility" section.

/// A passage with its relevance score (higher = more relevant).
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredPassage {
    pub text: String,
    pub score: f64,
}

/// Rank passages against a query, best-first. Implementations omit passages
/// with no relevance signal (score <= 0).
pub trait PassageRanker {
    fn rank(&self, query: &str, passages: &[String]) -> Vec<ScoredPassage>;
}

/// BM25 free-parameters (Robertson/Sparck-Jones defaults).
const K1: f64 = 1.5;
const B: f64 = 0.75;

/// Lexical BM25 ranker. Treats the passage set as the corpus (each passage a
/// document) and scores each against the query terms. Pure + deterministic.
pub struct LexicalRanker;

/// Lowercase unicode-word tokens (alphanumeric runs). Punctuation is a separator.
fn tokenize(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect()
}

impl PassageRanker for LexicalRanker {
    fn rank(&self, query: &str, passages: &[String]) -> Vec<ScoredPassage> {
        let q_terms = tokenize(query);
        if q_terms.is_empty() || passages.is_empty() {
            return Vec::new();
        }
        // Tokenize each passage once.
        let docs: Vec<Vec<String>> = passages.iter().map(|p| tokenize(p)).collect();
        let n = docs.len() as f64;
        let avg_len: f64 =
            docs.iter().map(|d| d.len()).sum::<usize>() as f64 / n.max(1.0);

        // Document frequency per unique query term.
        let mut scored: Vec<ScoredPassage> = Vec::new();
        for (doc, passage) in docs.iter().zip(passages.iter()) {
            let dl = doc.len() as f64;
            let mut score = 0.0_f64;
            for term in unique(&q_terms) {
                let tf = doc.iter().filter(|t| *t == &term).count() as f64;
                if tf == 0.0 {
                    continue;
                }
                let df = docs.iter().filter(|d| d.contains(&term)).count() as f64;
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
        let r = LexicalRanker.rank("bwrap user namespaces sandbox", &passages);
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
        let r = LexicalRanker.rank("user namespaces", &passages);
        assert_eq!(r.len(), 2);
        assert!(r[0].text.starts_with("user namespaces user"), "denser match should rank first");
    }

    #[test]
    fn empty_query_or_passages_yields_empty() {
        assert!(LexicalRanker.rank("", &["anything".to_string()]).is_empty());
        assert!(LexicalRanker.rank("q", &[]).is_empty());
    }

    #[test]
    fn no_shared_terms_yields_empty() {
        let passages = vec!["completely unrelated content".to_string()];
        assert!(LexicalRanker.rank("xyzzy plugh", &passages).is_empty());
    }

    #[test]
    fn tokenization_is_case_and_punctuation_insensitive() {
        let passages = vec!["BWRAP, the sandbox!".to_string()];
        let r = LexicalRanker.rank("bwrap", &passages);
        assert_eq!(texts(&r), vec!["BWRAP, the sandbox!"]);
    }
}
