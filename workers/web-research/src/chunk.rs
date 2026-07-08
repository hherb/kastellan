//! Split extracted page text into passages for ranking.
//!
//! Pure and deterministic: paragraphs (blank-line separated) are the natural
//! unit; an over-long paragraph is further split on sentence boundaries so no
//! single passage blows the ranking/context budget. Empty/whitespace-only
//! passages are dropped.

/// Upper bound on a single passage's byte length. Over-long paragraphs are
/// split on sentence boundaries into chunks no larger than this.
pub const MAX_PASSAGE_BYTES: usize = 2000;

/// Chunk `text` into passages. Never returns empty/whitespace-only entries.
pub fn chunk_passages(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for para in text.split("\n\n") {
        let para = para.trim();
        if para.is_empty() {
            continue;
        }
        if para.len() <= MAX_PASSAGE_BYTES {
            out.push(para.to_string());
        } else {
            split_long(para, &mut out);
        }
    }
    out
}

/// Split an over-long paragraph into <= MAX_PASSAGE_BYTES chunks, breaking after
/// sentence terminators (`.`/`!`/`?` followed by whitespace) where possible and
/// falling back to a hard char-boundary cut when a single sentence exceeds the cap.
fn split_long(para: &str, out: &mut Vec<String>) {
    let mut cur = String::new();
    for sentence in split_sentences(para) {
        if !cur.is_empty() && cur.len() + 1 + sentence.len() > MAX_PASSAGE_BYTES {
            out.push(std::mem::take(&mut cur));
        }
        if sentence.len() > MAX_PASSAGE_BYTES {
            // A single mega-sentence: hard-cut on char boundaries.
            for piece in hard_cut(sentence) {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                out.push(piece);
            }
            continue;
        }
        if cur.is_empty() {
            cur.push_str(sentence);
        } else {
            cur.push(' ');
            cur.push_str(sentence);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
}

/// Split on sentence terminators, keeping the terminator with its sentence.
fn split_sentences(para: &str) -> Vec<&str> {
    let mut sentences = Vec::new();
    let bytes = para.as_bytes();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if (c == b'.' || c == b'!' || c == b'?')
            && i + 1 < bytes.len()
            && bytes[i + 1].is_ascii_whitespace()
        {
            let s = para[start..=i].trim();
            if !s.is_empty() {
                sentences.push(s);
            }
            start = i + 1;
        }
        i += 1;
    }
    let tail = para[start..].trim();
    if !tail.is_empty() {
        sentences.push(tail);
    }
    sentences
}

/// Hard-cut a string into MAX_PASSAGE_BYTES pieces on char boundaries.
fn hard_cut(s: &str) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut rest = s;
    while rest.len() > MAX_PASSAGE_BYTES {
        let mut end = MAX_PASSAGE_BYTES;
        while end > 0 && !rest.is_char_boundary(end) {
            end -= 1;
        }
        pieces.push(rest[..end].to_string());
        rest = &rest[end..];
    }
    if !rest.is_empty() {
        pieces.push(rest.to_string());
    }
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_blank_lines_and_trims() {
        let text = "First paragraph.\n\n  Second paragraph.  \n\n\n Third. ";
        let p = chunk_passages(text);
        assert_eq!(p, vec!["First paragraph.", "Second paragraph.", "Third."]);
    }

    #[test]
    fn drops_empty_and_whitespace_passages() {
        let p = chunk_passages("\n\n   \n\nreal\n\n\t\n");
        assert_eq!(p, vec!["real"]);
    }

    #[test]
    fn empty_input_is_empty_vec() {
        assert!(chunk_passages("").is_empty());
        assert!(chunk_passages("   \n\n  ").is_empty());
    }

    #[test]
    fn long_paragraph_splits_on_sentence_boundaries_under_cap() {
        let sentence = format!("{}. ", "word".repeat(200)); // ~1200 bytes each
        let para = sentence.repeat(3); // ~3600 bytes, one paragraph
        let p = chunk_passages(&para);
        assert!(p.len() >= 2, "expected multiple chunks, got {}", p.len());
        assert!(p.iter().all(|c| c.len() <= MAX_PASSAGE_BYTES), "a chunk exceeded the cap");
    }

    #[test]
    fn mega_sentence_is_hard_cut_on_char_boundary() {
        let para = "x".repeat(MAX_PASSAGE_BYTES + 500); // no sentence terminator
        let p = chunk_passages(&para);
        assert!(p.iter().all(|c| c.len() <= MAX_PASSAGE_BYTES));
        assert!(p.iter().all(|c| c.is_char_boundary(c.len())));
    }
}
