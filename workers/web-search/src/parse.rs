//! Parse SearxNG's `/search?format=json` response into a bounded list of hits.
//!
//! We deserialize only the subset we surface. The mapping is lenient: a result
//! with no `url` is dropped (a hit the agent cannot follow is useless); missing
//! `title`/`content`/`engine` default to empty strings.

/// One search result surfaced to the planner.
#[derive(serde::Serialize, Debug, PartialEq)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub engine: String,
}

#[derive(serde::Deserialize)]
struct RawSearchResponse {
    #[serde(default)]
    results: Vec<RawResult>,
}

#[derive(serde::Deserialize)]
struct RawResult {
    #[serde(default)]
    title: String,
    url: Option<String>,
    #[serde(default)]
    content: String,
    #[serde(default)]
    engine: String,
}

/// Parse a SearxNG JSON body into hits. Errors only on malformed JSON.
pub fn parse_results(body: &[u8]) -> anyhow::Result<Vec<Hit>> {
    let raw: RawSearchResponse = serde_json::from_slice(body)
        .map_err(|e| anyhow::anyhow!("malformed SearxNG JSON: {e}"))?;
    Ok(raw
        .results
        .into_iter()
        .filter_map(|r| {
            r.url.map(|url| Hit {
                title: r.title,
                url,
                snippet: r.content,
                engine: r.engine,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_results_into_hits() {
        let json = r#"{"results":[
            {"title":"Rust","url":"https://rust-lang.org","content":"systems lang","engine":"duckduckgo"},
            {"title":"Cargo","url":"https://doc.rust-lang.org/cargo","content":"build tool","engine":"google"}
        ]}"#;
        let hits = parse_results(json.as_bytes()).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0], Hit {
            title: "Rust".into(),
            url: "https://rust-lang.org".into(),
            snippet: "systems lang".into(),
            engine: "duckduckgo".into(),
        });
    }

    #[test]
    fn result_without_url_is_skipped() {
        let json = r#"{"results":[
            {"title":"no link","content":"x"},
            {"title":"ok","url":"https://example.com","content":"y"}
        ]}"#;
        let hits = parse_results(json.as_bytes()).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://example.com");
    }

    #[test]
    fn missing_optional_fields_default_to_empty() {
        let json = r#"{"results":[{"url":"https://example.com"}]}"#;
        let hits = parse_results(json.as_bytes()).unwrap();
        assert_eq!(hits[0].title, "");
        assert_eq!(hits[0].snippet, "");
        assert_eq!(hits[0].engine, "");
    }

    #[test]
    fn empty_results_is_empty_vec() {
        let hits = parse_results(br#"{"results":[]}"#).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn missing_results_key_is_empty_vec() {
        // SearxNG always sends `results`, but be defensive.
        let hits = parse_results(br#"{"query":"x"}"#).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn malformed_json_is_an_error() {
        assert!(parse_results(b"not json").is_err());
    }
}
