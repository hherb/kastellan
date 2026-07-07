//! Compose search + fetch + rank into one research pass, pure over the
//! [`HttpGet`] seam so the whole flow is hermetic-testable with `FakeGet`.
//!
//! Flow: reject empty query → `search()` the SearxNG endpoint → for each hit in
//! rank order, if its host is on the content allowlist attempt a fetch; on
//! success extract → chunk → rank passages; on failure record the source in
//! `unfetched` (never drop silently). Off-allowlist hits are recorded too. Stops
//! once `max_sources` pages have been successfully gathered.

use url::Url;

use kastellan_worker_web_common::allowlist::HostAllowlist;
use kastellan_worker_web_common::extract::extract;
use kastellan_worker_web_common::fetch::{drive, FetchError};
use kastellan_worker_web_common::http::HttpGet;
use kastellan_worker_web_common::parse::Hit;
use kastellan_worker_web_common::search::{search, SearchError};

use crate::chunk::chunk_passages;
use crate::rank::{PassageRanker, ScoredPassage};

/// Default / max number of pages fetched per research call.
// Consumed by the Task 2.4 handler as the tool schema default; not yet read
// from within this crate.
#[allow(dead_code)]
pub const DEFAULT_MAX_SOURCES: usize = 3;
pub const MAX_MAX_SOURCES: usize = 8;
/// Default / max passages kept per source.
#[allow(dead_code)]
pub const DEFAULT_MAX_PASSAGES: usize = 3;
pub const MAX_MAX_PASSAGES: usize = 10;
/// How many search hits to consider (before allowlist filtering).
pub const SEARCH_COUNT: usize = 10;

/// A fetched source with its top-ranked passages.
// `title`/`snippet` are surfaced to the caller via the Task 2.4 handler's JSON
// response; not read from within this crate yet.
#[allow(dead_code)]
#[derive(Debug)]
pub struct SourcePassages {
    pub url: String,
    pub title: String,
    pub snippet: String,
    pub passages: Vec<ScoredPassage>,
}

/// A hit that was not turned into passages, with the reason (never dropped).
#[allow(dead_code)]
#[derive(Debug)]
pub struct UnfetchedSource {
    pub url: String,
    pub title: String,
    pub snippet: String,
    pub reason: String,
}

/// The full research result.
#[derive(Debug)]
pub struct ResearchOutcome {
    pub sources: Vec<SourcePassages>,
    pub unfetched: Vec<UnfetchedSource>,
}

/// Failure of the research pass. Only a *search* failure (or empty query) is an
/// error; per-page failures are recorded in `unfetched`.
#[derive(Debug)]
pub enum ResearchError {
    EmptyQuery,
    // Inner value surfaced via Debug/Task 2.4 error mapping; not read as data
    // within this crate yet.
    #[allow(dead_code)]
    Search(SearchError),
}

fn short_fetch_reason(e: &FetchError) -> String {
    match e {
        FetchError::HostDenied(h) => format!("fetch-failed: redirect host {h} off-allowlist"),
        FetchError::NonHttps(s) => format!("fetch-failed: redirect scheme {s} not https"),
        FetchError::TooManyRedirects => "fetch-failed: too many redirects".to_string(),
        FetchError::MissingLocation => "fetch-failed: redirect without Location".to_string(),
        FetchError::BadUrl(m) => format!("fetch-failed: bad url: {m}"),
        FetchError::Transport(m) => format!("fetch-failed: {m}"),
    }
}

/// Try to turn one allowlisted hit into a `SourcePassages`. `Err(reason)` on any
/// fetch/parse/extract failure — the caller records it in `unfetched`.
fn gather_source<T: HttpGet, R: PassageRanker>(
    transport: &T,
    allowlist: &HostAllowlist,
    ranker: &R,
    query: &str,
    hit: &Hit,
    max_passages: usize,
) -> Result<SourcePassages, String> {
    let url = Url::parse(&hit.url).map_err(|e| format!("fetch-failed: bad url: {e}"))?;
    let outcome = drive(transport, allowlist, url).map_err(|e| short_fetch_reason(&e))?;
    let extracted = extract(&outcome.content_type, &outcome.body)
        .map_err(|e| format!("fetch-failed: extraction: {e}"))?;
    let passages = chunk_passages(&extracted.text);
    let mut ranked = ranker.rank(query, &passages);
    ranked.truncate(max_passages);
    Ok(SourcePassages {
        url: outcome.final_url,
        title: hit.title.clone(),
        snippet: hit.snippet.clone(),
        passages: ranked,
    })
}

/// Run the research pass. See the module doc for the flow.
pub fn research<T: HttpGet, R: PassageRanker>(
    transport: &T,
    endpoint: &Url,
    allowlist: &HostAllowlist,
    ranker: &R,
    query: &str,
    max_sources: usize,
    max_passages: usize,
) -> Result<ResearchOutcome, ResearchError> {
    if query.trim().is_empty() {
        return Err(ResearchError::EmptyQuery);
    }
    let max_sources = max_sources.clamp(1, MAX_MAX_SOURCES);
    let max_passages = max_passages.clamp(1, MAX_MAX_PASSAGES);

    let hits = search(transport, endpoint, allowlist, query, SEARCH_COUNT)
        .map_err(ResearchError::Search)?;

    let mut sources = Vec::new();
    let mut unfetched = Vec::new();
    for hit in &hits {
        if sources.len() >= max_sources {
            break;
        }
        let host = Url::parse(&hit.url).ok().and_then(|u| u.host_str().map(str::to_string));
        let allowed = host.as_deref().map(|h| allowlist.is_allowed(h)).unwrap_or(false);
        if !allowed {
            unfetched.push(UnfetchedSource {
                url: hit.url.clone(),
                title: hit.title.clone(),
                snippet: hit.snippet.clone(),
                reason: "off-allowlist".to_string(),
            });
            continue;
        }
        match gather_source(transport, allowlist, ranker, query, hit, max_passages) {
            Ok(src) => sources.push(src),
            Err(reason) => unfetched.push(UnfetchedSource {
                url: hit.url.clone(),
                title: hit.title.clone(),
                snippet: hit.snippet.clone(),
                reason,
            }),
        }
    }
    Ok(ResearchOutcome { sources, unfetched })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kastellan_worker_web_common::http::RawResponse;
    use kastellan_worker_web_common::testing::{al, json_resp, ok_resp, FakeGet};
    use crate::rank::LexicalRanker;

    fn endpoint() -> Url {
        Url::parse("https://searx.example.org/search").unwrap()
    }

    // Search JSON returning the given (title, url) pairs with a fixed snippet.
    fn search_json(hits: &[(&str, &str)]) -> String {
        let items: Vec<String> = hits
            .iter()
            .map(|(t, u)| format!(r#"{{"title":"{t}","url":"{u}","content":"snippet about bwrap namespaces","engine":"e"}}"#))
            .collect();
        format!(r#"{{"results":[{}]}}"#, items.join(","))
    }

    #[test]
    fn happy_path_search_then_fetch_ranks_passages() {
        // 1 search response, then 1 fetch response (text/plain).
        let page = "Intro paragraph unrelated.\n\nbwrap creates unprivileged user namespaces to sandbox the worker.";
        let t = FakeGet::new(vec![
            json_resp(&search_json(&[("Doc", "https://docs.example.org/bwrap")])),
            RawResponse { status: 200, location: None,
                content_type: "text/plain; charset=utf-8".into(), body: page.as_bytes().to_vec() },
        ]);
        let a = al(&["searx.example.org", "docs.example.org"]);
        let out = research(&t, &endpoint(), &a, &LexicalRanker, "bwrap user namespaces", 3, 3).unwrap();
        assert_eq!(out.sources.len(), 1);
        assert_eq!(out.sources[0].url, "https://docs.example.org/bwrap");
        assert!(!out.sources[0].passages.is_empty());
        assert!(out.sources[0].passages[0].text.contains("bwrap"));
        assert!(out.unfetched.is_empty());
    }

    #[test]
    fn off_allowlist_hit_is_recorded_not_fetched() {
        // Only the search response is served; the off-allowlist hit must NOT
        // consume a fetch response (FakeGet would run dry otherwise).
        let t = FakeGet::new(vec![
            json_resp(&search_json(&[("Evil", "https://evil.test/x")])),
        ]);
        let a = al(&["searx.example.org", "docs.example.org"]);
        let out = research(&t, &endpoint(), &a, &LexicalRanker, "q term", 3, 3).unwrap();
        assert!(out.sources.is_empty());
        assert_eq!(out.unfetched.len(), 1);
        assert_eq!(out.unfetched[0].reason, "off-allowlist");
        assert_eq!(out.unfetched[0].url, "https://evil.test/x");
    }

    #[test]
    fn one_fetch_failure_is_recorded_others_returned() {
        // hit A fetch → 500 (non-3xx terminal, extract of empty body still ok →
        // ranks to nothing) ; use a hit that 200s with content and one that errors.
        // Serve: search, then A=200 with content, then B=transport is simulated
        // by a redirect-loop -> TooManyRedirects.
        let page = "user namespaces sandbox bwrap details here.";
        let mut resps = vec![
            json_resp(&search_json(&[
                ("A", "https://docs.example.org/a"),
                ("B", "https://docs.example.org/b"),
            ])),
            RawResponse { status: 200, location: None,
                content_type: "text/plain".into(), body: page.as_bytes().to_vec() },
        ];
        // B: 6+ redirects to the same allowlisted host → TooManyRedirects.
        for _ in 0..(kastellan_worker_web_common::fetch::MAX_REDIRECTS + 2) {
            resps.push(RawResponse { status: 302,
                location: Some("https://docs.example.org/loop".into()),
                content_type: String::new(), body: Vec::new() });
        }
        let t = FakeGet::new(resps);
        let a = al(&["searx.example.org", "docs.example.org"]);
        let out = research(&t, &endpoint(), &a, &LexicalRanker, "bwrap namespaces", 3, 3).unwrap();
        assert_eq!(out.sources.len(), 1, "A should succeed");
        assert_eq!(out.sources[0].url, "https://docs.example.org/a");
        assert_eq!(out.unfetched.len(), 1, "B should be recorded as failed");
        assert!(out.unfetched[0].reason.starts_with("fetch-failed:"), "{}", out.unfetched[0].reason);
    }

    #[test]
    fn max_sources_caps_fetches() {
        let hits: Vec<(&str, &str)> = vec![
            ("A", "https://docs.example.org/a"),
            ("B", "https://docs.example.org/b"),
            ("C", "https://docs.example.org/c"),
        ];
        let t = FakeGet::new(vec![
            json_resp(&search_json(&hits)),
            ok_resp("bwrap namespaces one"),
            ok_resp("bwrap namespaces two"),
            // no third fetch response — the max_sources cap must stop before a
            // 3rd fetch (else FakeGet returns Err "no more canned responses").
        ]);
        let a = al(&["searx.example.org", "docs.example.org"]);
        let out = research(&t, &endpoint(), &a, &LexicalRanker, "bwrap namespaces", 2, 3).unwrap();
        assert_eq!(out.sources.len(), 2);
    }

    #[test]
    fn empty_query_is_error() {
        let t = FakeGet::new(vec![]);
        let a = al(&["searx.example.org"]);
        let err = research(&t, &endpoint(), &a, &LexicalRanker, "   ", 3, 3).unwrap_err();
        assert!(matches!(err, ResearchError::EmptyQuery));
    }

    #[test]
    fn search_failure_is_error() {
        let t = FakeGet::new(vec![RawResponse { status: 503, location: None,
            content_type: "text/plain".into(), body: Vec::new() }]);
        let a = al(&["searx.example.org"]);
        let err = research(&t, &endpoint(), &a, &LexicalRanker, "q term", 3, 3).unwrap_err();
        assert!(matches!(err, ResearchError::Search(_)));
    }

    #[test]
    fn max_passages_truncates_per_source() {
        let page = (0..6).map(|i| format!("bwrap namespaces passage number {i}."))
            .collect::<Vec<_>>().join("\n\n");
        let t = FakeGet::new(vec![
            json_resp(&search_json(&[("A", "https://docs.example.org/a")])),
            RawResponse { status: 200, location: None, content_type: "text/plain".into(),
                body: page.into_bytes() },
        ]);
        let a = al(&["searx.example.org", "docs.example.org"]);
        let out = research(&t, &endpoint(), &a, &LexicalRanker, "bwrap namespaces", 3, 2).unwrap();
        assert_eq!(out.sources[0].passages.len(), 2, "capped at max_passages");
    }
}
