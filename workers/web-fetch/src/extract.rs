//! Content extraction: turn a fetched body + content-type into readable text.
//!
//!   - `text/html`        → readability main-content extraction (+ <title>).
//!   - `application/pdf`  → PDF text extraction.
//!   - `text/*`, `application/json`, or a missing/empty Content-Type
//!     → decoded as-is (UTF-8 lossy).
//!   - anything else      → error (caller maps to OPERATION_FAILED).
//!
//! The extracted text is capped at [`MAX_TEXT_BYTES`]; `truncated` records
//! whether the cap fired. This keeps the planner's context budget bounded
//! until the large-result handoff cache (ROADMAP:129) lands.

/// Cap on returned extracted text (100 KiB).
pub const MAX_TEXT_BYTES: usize = 100 * 1024;

/// Result of extraction.
#[derive(Debug)]
pub struct Extracted {
    pub title: Option<String>,
    pub text: String,
    pub truncated: bool,
}

/// The bare main media type, lowercased, params stripped
/// (`"text/html; charset=utf-8"` → `"text/html"`).
pub fn main_type(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase()
}

/// Extract readable text from `body` according to `content_type`.
pub fn extract(content_type: &str, body: &[u8]) -> anyhow::Result<Extracted> {
    let mt = main_type(content_type);
    match mt.as_str() {
        "text/html" => extract_html(body),
        "application/pdf" => {
            // `pdf-extract` can panic (not just `Err`) on some malformed PDFs.
            // The body is attacker-influenced, but a panic is contained: the
            // worker is single-use, sandboxed, and cpu/wall-clock-capped, so it
            // aborts this process and surfaces as a worker error — no fallback,
            // no leak. Hardening to catch_unwind is deferred to the egress slice.
            let raw = pdf_extract::extract_text_from_mem(body)
                .map_err(|e| anyhow::anyhow!("pdf text extraction failed: {e}"))?;
            let (text, truncated) = cap_text(raw);
            Ok(Extracted { title: None, text, truncated })
        }
        // An empty `mt` means the server sent no (or a blank) Content-Type.
        // Many plain-text endpoints omit it; treat the body as text rather
        // than rejecting an otherwise-valid response.
        _ if mt.is_empty() || mt.starts_with("text/") || mt == "application/json" => {
            let raw = String::from_utf8_lossy(body).into_owned();
            let (text, truncated) = cap_text(raw);
            Ok(Extracted { title: None, text, truncated })
        }
        other => anyhow::bail!("unsupported content-type: {other}"),
    }
}

fn extract_html(body: &[u8]) -> anyhow::Result<Extracted> {
    let html = String::from_utf8_lossy(body);
    let mut readability = readable_html::Readability::new(html.as_ref(), None, None)
        .map_err(|e| anyhow::anyhow!("readability init failed: {e}"))?;
    let article = readability
        .parse()
        .map_err(|e| anyhow::anyhow!("could not extract readable content: {e}"))?;
    let title = {
        let t = article.title.trim();
        if t.is_empty() { None } else { Some(t.to_string()) }
    };
    let (text, truncated) = cap_text(article.text_content.to_string());
    Ok(Extracted { title, text, truncated })
}

/// Truncate to at most [`MAX_TEXT_BYTES`] on a char boundary.
fn cap_text(mut s: String) -> (String, bool) {
    if s.len() <= MAX_TEXT_BYTES {
        return (s, false);
    }
    let mut end = MAX_TEXT_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    (s, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A content-rich article so readability's heuristics latch onto the main
    // node. If `parse()` ever returns GrabFailed on this, lengthen the body
    // (more paragraphs) — that's a fixture-tuning change, not a logic change.
    const ARTICLE_HTML: &[u8] = br#"<!DOCTYPE html><html><head>
        <title>The Title</title></head><body>
        <nav>home about contact</nav>
        <article>
        <h1>The Title</h1>
        <p>The first paragraph of this article contains several sentences so the
        readability algorithm recognises it as the main content. We are writing
        about web fetching and content extraction in a sandboxed worker.</p>
        <p>The second paragraph continues the discussion with more substantive
        prose. Readability scores nodes by text density, so a few real sentences
        here make the body unambiguously the article content rather than the
        navigation chrome above.</p>
        <p>A third paragraph seals it, ensuring the grab succeeds deterministically
        across versions of the extraction crate.</p>
        </article>
        <footer>copyright</footer></body></html>"#;

    #[test]
    fn html_yields_title_and_main_text() {
        let e = extract("text/html; charset=utf-8", ARTICLE_HTML).unwrap();
        assert_eq!(e.title.as_deref(), Some("The Title"));
        assert!(e.text.contains("first paragraph"), "text: {}", e.text);
        assert!(!e.text.contains("home about contact"), "nav chrome leaked: {}", e.text);
        assert!(!e.truncated);
    }

    #[test]
    fn plain_text_passes_through() {
        let e = extract("text/plain; charset=utf-8", b"just some plain text").unwrap();
        assert_eq!(e.title, None);
        assert_eq!(e.text, "just some plain text");
        assert!(!e.truncated);
    }

    #[test]
    fn json_passes_through() {
        let e = extract("application/json", br#"{"k":"v"}"#).unwrap();
        assert_eq!(e.title, None);
        assert_eq!(e.text, r#"{"k":"v"}"#);
    }

    #[test]
    fn pdf_is_extracted() {
        let bytes = include_bytes!("../tests/fixtures/hello.pdf");
        let e = extract("application/pdf", bytes).unwrap();
        assert_eq!(e.title, None);
        assert!(e.text.contains("Hello"), "pdf text: {:?}", e.text);
    }

    #[test]
    fn empty_content_type_is_treated_as_text() {
        // A server that omits Content-Type (or sends a blank one) must not
        // turn an otherwise-valid text body into an error.
        let e = extract("", b"body with no content type").unwrap();
        assert_eq!(e.title, None);
        assert_eq!(e.text, "body with no content type");
        assert!(!e.truncated);
        // A bare `;`-led param with no type behaves the same.
        let e2 = extract("; charset=utf-8", b"still text").unwrap();
        assert_eq!(e2.text, "still text");
    }

    #[test]
    fn unsupported_content_type_errors() {
        let err = extract("image/png", &[0x89, 0x50]).unwrap_err();
        assert!(format!("{err}").contains("unsupported content-type"), "{err}");
    }

    #[test]
    fn text_is_capped_on_char_boundary() {
        let big = "a".repeat(MAX_TEXT_BYTES + 500);
        let (capped, truncated) = cap_text(big);
        assert!(truncated);
        assert!(capped.len() <= MAX_TEXT_BYTES);
    }

    #[test]
    fn cap_truncates_before_a_straddling_multibyte_char() {
        // A 3-byte '€' begins at byte MAX_TEXT_BYTES - 1, so the cap lands mid-char;
        // cap_text must back up to the char boundary at MAX_TEXT_BYTES - 1.
        let mut s = "x".repeat(MAX_TEXT_BYTES - 1);
        s.push('€');
        let (capped, truncated) = cap_text(s);
        assert!(truncated);
        assert_eq!(capped.len(), MAX_TEXT_BYTES - 1);
        assert!(capped.is_char_boundary(capped.len()));
        assert!(!capped.contains('€'));
    }

    #[test]
    fn main_type_strips_params() {
        assert_eq!(main_type("text/html; charset=utf-8"), "text/html");
        assert_eq!(main_type("APPLICATION/JSON"), "application/json");
    }
}
