//! Compose search + fetch + rank into one research pass, pure over the
//! [`HttpGet`] seam so the whole flow is hermetic-testable with `FakeGet`.
//!
//! Flow: reject empty query → `search()` the SearxNG endpoint → for each hit in
//! rank order, if its host is on the content allowlist attempt a fetch; on a
//! 2xx that yields at least one relevant passage extract → chunk → rank; any
//! other outcome (off-allowlist, transport/redirect failure, non-2xx status, or
//! zero relevant passages) is recorded in `unfetched` with a reason, never
//! dropped silently and never a source slot. Stops once `max_sources` pages have
//! been successfully gathered.

use url::Url;

use kastellan_worker_web_common::allowlist::HostAllowlist;
use kastellan_worker_web_common::extract::extract;
use kastellan_worker_web_common::fetch::{drive, FetchError};
use kastellan_worker_web_common::http::HttpGet;
use kastellan_worker_web_common::parse::Hit;
use kastellan_worker_web_common::search::{search, SearchError};

use crate::chunk::chunk_passages;
use crate::embed::Embedder;
use crate::rank::{bm25, cosine, rrf_fuse, ScoredPassage};

/// Default / max number of pages fetched per research call.
pub const DEFAULT_MAX_SOURCES: usize = 3;
pub const MAX_MAX_SOURCES: usize = 8;
/// Default / max passages kept per source.
pub const DEFAULT_MAX_PASSAGES: usize = 3;
pub const MAX_MAX_PASSAGES: usize = 10;
/// How many search hits to consider (before allowlist filtering).
pub const SEARCH_COUNT: usize = 10;

/// A fetched source with its top-ranked passages.
#[derive(Debug)]
pub struct SourcePassages {
    pub url: String,
    pub title: String,
    pub snippet: String,
    pub passages: Vec<ScoredPassage>,
}

/// A hit that was not turned into passages, with the reason (never dropped).
#[derive(Debug)]
pub struct UnfetchedSource {
    pub url: String,
    pub title: String,
    pub snippet: String,
    pub reason: String,
}

/// How a research call ranked its passages.
#[derive(Debug, PartialEq)]
pub enum RankMode {
    Lexical,
    Hybrid,
}

/// The full research result.
#[derive(Debug)]
pub struct ResearchOutcome {
    pub sources: Vec<SourcePassages>,
    pub unfetched: Vec<UnfetchedSource>,
    /// `Hybrid` iff an embedder was configured AND the query embedded OK.
    // TRANSIENT: only inspected by this module's tests today; Task 5 wires it
    // into handler.rs's JSON surface, at which point this allow is removed.
    #[allow(dead_code)]
    pub ranking: RankMode,
    /// `Some(reason)` iff a configured semantic lane fell back to lexical for the
    /// whole call (query embed failed) or for at least one page (first reason wins).
    // TRANSIENT: see `ranking` above — same Task 5 wiring removes this allow.
    #[allow(dead_code)]
    pub embed_note: Option<String>,
}

/// Failure of the research pass. Only a *search* failure (or empty query) is an
/// error; per-page failures are recorded in `unfetched`.
#[derive(Debug)]
pub enum ResearchError {
    EmptyQuery,
    Search(SearchError),
}

fn short_fetch_reason(e: &FetchError) -> String {
    match e {
        // HostDenied/NonHttps can fire on the initial hit URL (hop 0) as well as
        // on a redirect target, so the reason says "target", not "redirect".
        FetchError::HostDenied(h) => format!("fetch-failed: target host {h} off-allowlist"),
        FetchError::NonHttps(s) => format!("fetch-failed: target scheme {s} not https"),
        FetchError::TooManyRedirects => "fetch-failed: too many redirects".to_string(),
        FetchError::MissingLocation => "fetch-failed: redirect without Location".to_string(),
        FetchError::BadUrl(m) => format!("fetch-failed: bad url: {m}"),
        FetchError::Transport(m) => format!("fetch-failed: {m}"),
    }
}

/// Rank one page's passages. Lexical always; if `query_emb` is live, add the
/// semantic lane and RRF-fuse. Returns the ranked passages and an optional
/// degrade reason (the page fell back to lexical because its passage embed failed).
fn rank_page(
    embedder: Option<&dyn Embedder>,
    query_emb: Option<&[f32]>,
    query: &str,
    passages: &[String],
) -> (Vec<ScoredPassage>, Option<String>) {
    let lexical = bm25(query, passages);
    match (embedder, query_emb) {
        (Some(e), Some(qe)) => match e.embed(passages) {
            Ok(pe) if pe.len() == passages.len() => {
                let semantic = cosine(qe, passages, &pe);
                (rrf_fuse(&lexical, &semantic), None)
            }
            Ok(pe) => (
                lexical,
                Some(format!(
                    "embed: passage vector count mismatch (got {}, want {})",
                    pe.len(),
                    passages.len()
                )),
            ),
            Err(err) => (lexical, Some(format!("embed: passage embedding failed: {err}"))),
        },
        _ => (lexical, None),
    }
}

/// Try to turn one allowlisted hit into a `SourcePassages`. `Err(reason)` on any
/// fetch/parse/extract failure, a non-2xx terminal status, or when ranking
/// yields no relevant passage — the caller records the reason in `unfetched` so
/// a dead/irrelevant page never occupies a source slot.
fn gather_source<T: HttpGet>(
    transport: &T,
    allowlist: &HostAllowlist,
    embedder: Option<&dyn Embedder>,
    query_emb: Option<&[f32]>,
    query: &str,
    hit: &Hit,
    max_passages: usize,
) -> Result<(SourcePassages, Option<String>), String> {
    let url = Url::parse(&hit.url).map_err(|e| format!("fetch-failed: bad url: {e}"))?;
    let outcome = drive(transport, allowlist, url).map_err(|e| short_fetch_reason(&e))?;
    // `drive` returns any non-3xx terminal response, including 4xx/5xx. An error
    // page (a 403 bot-challenge, a 404, a 500) is not a usable source — record it
    // rather than extracting its error HTML into bogus passages.
    if !(200..300).contains(&outcome.status) {
        return Err(format!("fetch-failed: status {}", outcome.status));
    }
    let extracted = extract(&outcome.content_type, &outcome.body)
        .map_err(|e| format!("fetch-failed: extraction: {e}"))?;
    let passages = chunk_passages(&extracted.text);
    let (mut ranked, note) = rank_page(embedder, query_emb, query, &passages);
    ranked.truncate(max_passages);
    // A page that fetched fine but shares no terms with the query yields nothing.
    // Record it (don't consume a source slot) so a later hit can be fetched.
    if ranked.is_empty() {
        return Err("no-relevant-passages".to_string());
    }
    Ok((
        SourcePassages {
            url: outcome.final_url,
            title: hit.title.clone(),
            snippet: hit.snippet.clone(),
            passages: ranked,
        },
        note,
    ))
}

/// Run the research pass. See the module doc for the flow.
pub fn research<T: HttpGet>(
    transport: &T,
    endpoint: &Url,
    allowlist: &HostAllowlist,
    embedder: Option<&dyn Embedder>,
    query: &str,
    max_sources: usize,
    max_passages: usize,
) -> Result<ResearchOutcome, ResearchError> {
    if query.trim().is_empty() {
        return Err(ResearchError::EmptyQuery);
    }
    let max_sources = max_sources.clamp(1, MAX_MAX_SOURCES);
    let max_passages = max_passages.clamp(1, MAX_MAX_PASSAGES);

    // Embed the query once up front. On failure, degrade the WHOLE call to
    // lexical and drop the embedder (fail-fast: a dead endpoint is not re-hit
    // once per page). This is the dominant failure mode (endpoint down).
    let mut embed_note: Option<String> = None;
    let query_emb: Option<Vec<f32>> = match embedder {
        Some(e) => match e.embed(&[query.to_string()]) {
            Ok(mut v) if v.len() == 1 => Some(v.remove(0)),
            Ok(v) => {
                embed_note = Some(format!(
                    "embed: query vector count {} (expected 1); ranking lexical",
                    v.len()
                ));
                None
            }
            Err(err) => {
                embed_note = Some(format!("embed: query embedding failed: {err}; ranking lexical"));
                None
            }
        },
        None => None,
    };
    // Effective embedder: only present when the query embedded successfully.
    let eff_embedder = query_emb.as_ref().and(embedder);
    let ranking = if query_emb.is_some() { RankMode::Hybrid } else { RankMode::Lexical };

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
        match gather_source(
            transport, allowlist, eff_embedder, query_emb.as_deref(),
            query, hit, max_passages,
        ) {
            Ok((src, note)) => {
                if embed_note.is_none() {
                    embed_note = note; // first page-level degrade reason wins
                }
                sources.push(src);
            }
            Err(reason) => unfetched.push(UnfetchedSource {
                url: hit.url.clone(),
                title: hit.title.clone(),
                snippet: hit.snippet.clone(),
                reason,
            }),
        }
    }
    Ok(ResearchOutcome { sources, unfetched, ranking, embed_note })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kastellan_worker_web_common::http::RawResponse;
    use kastellan_worker_web_common::testing::{al, json_resp, ok_resp, FakeGet};
    use crate::embed::FakeEmbedder;

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
        let out = research(&t, &endpoint(), &a, None, "bwrap user namespaces", 3, 3).unwrap();
        assert_eq!(out.sources.len(), 1);
        assert_eq!(out.sources[0].url, "https://docs.example.org/bwrap");
        assert!(!out.sources[0].passages.is_empty());
        assert!(out.sources[0].passages[0].text.contains("bwrap"));
        assert!(out.unfetched.is_empty());
        assert!(matches!(out.ranking, RankMode::Lexical));
        assert!(out.embed_note.is_none());
    }

    #[test]
    fn off_allowlist_hit_is_recorded_not_fetched() {
        // Only the search response is served; the off-allowlist hit must NOT
        // consume a fetch response (FakeGet would run dry otherwise).
        let t = FakeGet::new(vec![
            json_resp(&search_json(&[("Evil", "https://evil.test/x")])),
        ]);
        let a = al(&["searx.example.org", "docs.example.org"]);
        let out = research(&t, &endpoint(), &a, None, "q term", 3, 3).unwrap();
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
        let out = research(&t, &endpoint(), &a, None, "bwrap namespaces", 3, 3).unwrap();
        assert_eq!(out.sources.len(), 1, "A should succeed");
        assert_eq!(out.sources[0].url, "https://docs.example.org/a");
        assert_eq!(out.unfetched.len(), 1, "B should be recorded as failed");
        assert!(out.unfetched[0].reason.starts_with("fetch-failed:"), "{}", out.unfetched[0].reason);
    }

    #[test]
    fn non_2xx_fetch_is_recorded_not_a_source() {
        // A 404 terminal response must not become a source with error-page text.
        let t = FakeGet::new(vec![
            json_resp(&search_json(&[("A", "https://docs.example.org/a")])),
            RawResponse { status: 404, location: None,
                content_type: "text/plain".into(), body: b"not found".to_vec() },
        ]);
        let a = al(&["searx.example.org", "docs.example.org"]);
        let out = research(&t, &endpoint(), &a, None, "bwrap namespaces", 3, 3).unwrap();
        assert!(out.sources.is_empty());
        assert_eq!(out.unfetched.len(), 1);
        assert_eq!(out.unfetched[0].reason, "fetch-failed: status 404");
    }

    #[test]
    fn fetched_page_with_no_relevant_passages_is_recorded_not_a_source() {
        // 200 page whose content shares no terms with the query → BM25 scores
        // nothing → recorded in `unfetched`, does not consume a source slot.
        let t = FakeGet::new(vec![
            json_resp(&search_json(&[("A", "https://docs.example.org/a")])),
            ok_resp("completely unrelated cooking recipe content"),
        ]);
        let a = al(&["searx.example.org", "docs.example.org"]);
        let out = research(&t, &endpoint(), &a, None, "bwrap namespaces sandbox", 3, 3).unwrap();
        assert!(out.sources.is_empty());
        assert_eq!(out.unfetched.len(), 1);
        assert_eq!(out.unfetched[0].reason, "no-relevant-passages");
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
        let out = research(&t, &endpoint(), &a, None, "bwrap namespaces", 2, 3).unwrap();
        assert_eq!(out.sources.len(), 2);
    }

    #[test]
    fn empty_query_is_error() {
        let t = FakeGet::new(vec![]);
        let a = al(&["searx.example.org"]);
        let err = research(&t, &endpoint(), &a, None, "   ", 3, 3).unwrap_err();
        assert!(matches!(err, ResearchError::EmptyQuery));
    }

    #[test]
    fn search_failure_is_error() {
        let t = FakeGet::new(vec![RawResponse { status: 503, location: None,
            content_type: "text/plain".into(), body: Vec::new() }]);
        let a = al(&["searx.example.org"]);
        let err = research(&t, &endpoint(), &a, None, "q term", 3, 3).unwrap_err();
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
        let out = research(&t, &endpoint(), &a, None, "bwrap namespaces", 3, 2).unwrap();
        assert_eq!(out.sources[0].passages.len(), 2, "capped at max_passages");
    }

    #[test]
    fn hybrid_surfaces_paraphrase_passage_bm25_misses() {
        // Query shares NO surface terms with the relevant passage, but the fake
        // embedder gives them near-identical vectors -> cosine lane surfaces it.
        // FakeEmbedder keys MUST equal chunk_passages() output exactly: it splits
        // on "\n\n" and trims, so this page yields
        //   ["unrelated filler line.", "Containers isolate processes from the host."]
        // (an unmapped key -> empty vec -> cosine skips it -> the test would fail).
        let page = "unrelated filler line.\n\nContainers isolate processes from the host.";
        let t = FakeGet::new(vec![
            json_resp(&search_json(&[("Doc", "https://docs.example.org/x")])),
            RawResponse { status: 200, location: None,
                content_type: "text/plain".into(), body: page.as_bytes().to_vec() },
        ]);
        let a = al(&["searx.example.org", "docs.example.org"]);
        let query = "sandboxing";
        let emb = FakeEmbedder::new(&[
            ("sandboxing", vec![1.0_f32, 0.0]),
            ("Containers isolate processes from the host.", vec![1.0_f32, 0.05]),
            ("unrelated filler line.", vec![0.0_f32, 1.0]),
        ]);
        let out = research(&t, &endpoint(), &a, Some(&emb), query, 3, 3).unwrap();
        assert!(matches!(out.ranking, RankMode::Hybrid));
        assert!(out.embed_note.is_none());
        assert_eq!(out.sources.len(), 1);
        assert!(out.sources[0].passages.iter().any(|p| p.text.contains("Containers isolate")));
    }

    #[test]
    fn query_embed_failure_degrades_whole_call_to_lexical_and_is_fail_fast() {
        let page = "bwrap namespaces sandbox details here.";
        let t = FakeGet::new(vec![
            json_resp(&search_json(&[("Doc", "https://docs.example.org/x")])),
            RawResponse { status: 200, location: None,
                content_type: "text/plain".into(), body: page.as_bytes().to_vec() },
        ]);
        let a = al(&["searx.example.org", "docs.example.org"]);
        let emb = FakeEmbedder::failing();
        let out = research(&t, &endpoint(), &a, Some(&emb), "bwrap namespaces", 3, 3).unwrap();
        assert!(matches!(out.ranking, RankMode::Lexical));
        assert!(out.embed_note.is_some(), "degrade must be signalled");
        assert_eq!(out.sources.len(), 1, "lexical still ranks the page");
        assert_eq!(emb.calls.get(), 1, "fail-fast: only the query embed is attempted");
    }
}
