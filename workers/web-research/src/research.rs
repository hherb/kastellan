//! Compose search + fetch + rank into one research pass, pure over the
//! [`HttpGet`] seam so the whole flow is hermetic-testable with `FakeGet`.
//!
//! Flow: reject empty query → `search()` the SearxNG endpoint → fetch every
//! allowlisted hit concurrently in bounded waves (`fetch_candidates`,
//! `MAX_CONCURRENT_FETCHES`) → classify + rank the fetched pages in rank order.
//! On a 2xx that yields at least one relevant passage the page becomes a source;
//! any other outcome (off-allowlist, transport/redirect failure, non-2xx status,
//! or zero relevant passages) is recorded in `unfetched` with a reason, never
//! dropped silently and never a source slot. The parallel fetch is
//! output-identical to a sequential pass — only the network fetch pattern
//! changes; `sources`/`unfetched` contents and order (including the `max_sources`
//! break) are preserved, so surplus pages fetched past the break are discarded
//! and never ranked/embedded.

use std::collections::HashMap;

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
/// Max passages embedded in a single per-page embed POST.
///
/// A pathological page can chunk into thousands of passages; embedding them all
/// makes one embed *response* that can exceed the transport's [`MAX_BODY_BYTES`]
/// record cap, forcing the whole page to lexical. We instead embed only the
/// first `MAX_EMBED_PASSAGES` chunks (document order — web content is
/// front-loaded, and BM25-scoring chunks past the cap still surface through the
/// lexical lane, which runs over ALL passages with no network, so nothing is
/// dropped — a late chunk is only ranked lexically, not semantically). Sized so
/// a 128 × 1024-dim JSON response stays comfortably under the 5 MiB cap — a
/// bound on the passage *count*; the response-size headroom assumes an
/// embedding dimension in the usual range (the default `embeddinggemma` is
/// 768-d, well within budget). A model with an unusually large dimension would
/// need a smaller cap.
///
/// [`MAX_BODY_BYTES`]: kastellan_worker_web_common::http::MAX_BODY_BYTES
pub const MAX_EMBED_PASSAGES: usize = 128;
/// Max page fetches in flight at once during the parallel fetch phase.
///
/// Allowlisted candidates are bounded by `SEARCH_COUNT` (10), so this caps the
/// burst on the egress proxy / origin servers to a handful while collapsing the
/// common case (≤ this many candidates) into a single wave. At the 10-candidate
/// ceiling the fetch runs in ⌈10 / N⌉ waves ⇒ ~⌈10 / N⌉ × 20 s worst case — under
/// the worker budget and far below the old sequential Σ. Separate from the
/// `ProxyConnectGet` runtime worker-thread count (an unrelated internal knob).
pub const MAX_CONCURRENT_FETCHES: usize = 6;

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
    pub ranking: RankMode,
    /// `Some(reason)` (first reason wins) iff a configured semantic lane, for at
    /// least one page or the whole call, either fell back to lexical (query or
    /// passage embed failed) or embedded only a capped prefix of an oversized
    /// page (see [`MAX_EMBED_PASSAGES`]) — never silent.
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
        (Some(e), Some(qe)) => {
            // Bound the embed POST: a huge page would otherwise embed thousands of
            // chunks in one request whose response can exceed the transport record
            // cap. Embed only the first `MAX_EMBED_PASSAGES` (document order); the
            // full lexical lane above still ranks the rest. See the const's doc.
            let candidates = &passages[..passages.len().min(MAX_EMBED_PASSAGES)];
            let cap_note = (passages.len() > candidates.len()).then(|| {
                format!(
                    "embed: page has {} chunks; embedded first {} (cap)",
                    passages.len(),
                    candidates.len()
                )
            });
            match e.embed(candidates) {
                Ok(pe) if pe.len() == candidates.len() => {
                    let semantic = cosine(qe, candidates, &pe);
                    (rrf_fuse(&lexical, &semantic), cap_note)
                }
                Ok(pe) => (
                    lexical,
                    Some(format!(
                        "embed: passage vector count mismatch (got {}, want {})",
                        pe.len(),
                        candidates.len()
                    )),
                ),
                Err(err) => (lexical, Some(format!("embed: passage embedding failed: {err}"))),
            }
        }
        _ => (lexical, None),
    }
}

/// One fetched + chunked page, not yet ranked. Phase-1 output of the fetch driver.
#[derive(Debug)]
struct FetchedPage {
    final_url: String,
    passages: Vec<String>,
}

/// Is this hit's host on the content allowlist? (Shared by the candidate filter and
/// the classify walk so the two can never drift — a hit fetched in phase 1 must be
/// the same set the classify phase expects.)
fn hit_allowed(allowlist: &HostAllowlist, hit: &Hit) -> bool {
    Url::parse(&hit.url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .map(|h| allowlist.is_allowed(&h))
        .unwrap_or(false)
}

/// Phase 1: fetch one allowlisted hit and chunk it into passages. `Err(reason)` on
/// any fetch/redirect/extract failure or a non-2xx terminal status — the exact
/// reason strings the caller records in `unfetched`. Pure over the transport seam.
fn fetch_and_chunk<T: HttpGet>(
    transport: &T,
    allowlist: &HostAllowlist,
    url: &str,
) -> Result<FetchedPage, String> {
    let url = Url::parse(url).map_err(|e| format!("fetch-failed: bad url: {e}"))?;
    let outcome = drive(transport, allowlist, url).map_err(|e| short_fetch_reason(&e))?;
    // `drive` returns any non-3xx terminal response, including 4xx/5xx. An error
    // page (403 bot-challenge, 404, 500) is not a usable source — record it rather
    // than extracting its error HTML into bogus passages.
    if !(200..300).contains(&outcome.status) {
        return Err(format!("fetch-failed: status {}", outcome.status));
    }
    let extracted = extract(&outcome.content_type, &outcome.body)
        .map_err(|e| format!("fetch-failed: extraction: {e}"))?;
    let passages = chunk_passages(&extracted.text);
    Ok(FetchedPage { final_url: outcome.final_url, passages })
}

/// Phase-1 driver: fetch + chunk every allowlisted candidate concurrently, in
/// bounded waves of `MAX_CONCURRENT_FETCHES`, sharing one `&transport` across
/// scoped threads. Returns a map from each candidate's hit index to its result so
/// the sequential classify phase can consult it in rank order (completion order
/// never leaks into the output).
fn fetch_candidates<T: HttpGet>(
    transport: &T,
    allowlist: &HostAllowlist,
    candidates: &[(usize, &Hit)],
) -> HashMap<usize, Result<FetchedPage, String>> {
    let mut results = HashMap::with_capacity(candidates.len());
    for wave in candidates.chunks(MAX_CONCURRENT_FETCHES) {
        std::thread::scope(|scope| {
            let handles: Vec<_> = wave
                .iter()
                .map(|(idx, hit)| {
                    let idx = *idx;
                    let url = hit.url.clone();
                    scope.spawn(move || (idx, fetch_and_chunk(transport, allowlist, &url)))
                })
                .collect();
            for h in handles {
                let (idx, res) = h.join().expect("fetch thread panicked");
                results.insert(idx, res);
            }
        });
    }
    results
}

/// Phase 2: rank one fetched page against the query, truncate to `max_passages`,
/// and apply the empty ⇒ `no-relevant-passages` rule. `Err(reason)` when the page
/// shares no relevant passage with the query (don't consume a source slot).
/// Returns the built source and an optional per-page degrade/cap note.
fn rank_fetched_page(
    embedder: Option<&dyn Embedder>,
    query_emb: Option<&[f32]>,
    query: &str,
    hit: &Hit,
    page: &FetchedPage,
    max_passages: usize,
) -> Result<(SourcePassages, Option<String>), String> {
    let (mut ranked, note) = rank_page(embedder, query_emb, query, &page.passages);
    ranked.truncate(max_passages);
    if ranked.is_empty() {
        return Err("no-relevant-passages".to_string());
    }
    Ok((
        SourcePassages {
            url: page.final_url.clone(),
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

    // Phase 1: fetch+chunk every allowlisted candidate concurrently.
    let candidates: Vec<(usize, &Hit)> = hits
        .iter()
        .enumerate()
        .filter(|(_, hit)| hit_allowed(allowlist, hit))
        .collect();
    let fetched = fetch_candidates(transport, allowlist, &candidates);

    // Phase 2: classify + rank in rank order — output-identical to the sequential
    // loop, including the max_sources-successes break and unfetched ordering.
    let mut sources = Vec::new();
    let mut unfetched = Vec::new();
    for (idx, hit) in hits.iter().enumerate() {
        if sources.len() >= max_sources {
            break;
        }
        if !hit_allowed(allowlist, hit) {
            unfetched.push(UnfetchedSource {
                url: hit.url.clone(),
                title: hit.title.clone(),
                snippet: hit.snippet.clone(),
                reason: "off-allowlist".to_string(),
            });
            continue;
        }
        // Every allowlisted hit was fetched in phase 1.
        let fetch_result = fetched
            .get(&idx)
            .expect("allowlisted candidate must have a phase-1 result");
        let classified = match fetch_result {
            Ok(page) => rank_fetched_page(
                eff_embedder, query_emb.as_deref(), query, hit, page, max_passages,
            ),
            Err(reason) => Err(reason.clone()),
        };
        match classified {
            Ok((src, note)) => {
                if embed_note.is_none() {
                    embed_note = note; // first page-level degrade reason wins (rank order)
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
    use kastellan_worker_web_common::testing::{al, json_resp, ok_resp, redirect_to, FakeGet, KeyedFakeGet};
    use crate::embed::FakeEmbedder;

    fn endpoint() -> Url {
        Url::parse("https://searx.example.org/search").unwrap()
    }

    /// Build a KeyedFakeGet with the search endpoint + a set of page responses.
    fn keyed(search: &str, pages: Vec<(&str, RawResponse)>) -> KeyedFakeGet {
        let mut pairs = vec![("https://searx.example.org/search", json_resp(search))];
        pairs.extend(pages);
        KeyedFakeGet::new(pairs)
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
        // A succeeds; B self-redirects until TooManyRedirects. Order-independent.
        let search = search_json(&[
            ("A", "https://docs.example.org/a"),
            ("B", "https://docs.example.org/b"),
        ]);
        let t = keyed(&search, vec![
            ("https://docs.example.org/a", ok_resp("user namespaces sandbox bwrap details")),
            // 302 → itself: drive re-fetches the same host+path until MAX_REDIRECTS.
            ("https://docs.example.org/b", redirect_to("https://docs.example.org/b")),
        ]);
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
    fn max_sources_caps_result_not_fetches() {
        // Under fetch-all, all three allowlisted candidates are fetched concurrently;
        // max_sources caps the RESULT to 2 (rank order A, B; C never classified).
        let search = search_json(&[
            ("A", "https://docs.example.org/a"),
            ("B", "https://docs.example.org/b"),
            ("C", "https://docs.example.org/c"),
        ]);
        let t = keyed(&search, vec![
            ("https://docs.example.org/a", ok_resp("bwrap namespaces one")),
            ("https://docs.example.org/b", ok_resp("bwrap namespaces two")),
            ("https://docs.example.org/c", ok_resp("bwrap namespaces three")),
        ]);
        let a = al(&["searx.example.org", "docs.example.org"]);
        let out = research(&t, &endpoint(), &a, None, "bwrap namespaces", 2, 3).unwrap();
        assert_eq!(out.sources.len(), 2);
        let urls: Vec<&str> = out.sources.iter().map(|s| s.url.as_str()).collect();
        assert_eq!(urls, vec!["https://docs.example.org/a", "https://docs.example.org/b"]);
        assert!(out.unfetched.is_empty(), "C is never classified (break at max_sources)");
    }

    #[test]
    fn parallel_fetch_returns_rank_ordered_sources() {
        // Three allowlisted candidates, all relevant → sources in rank order A,B,C
        // regardless of fetch completion order.
        let search = search_json(&[
            ("A", "https://docs.example.org/a"),
            ("B", "https://docs.example.org/b"),
            ("C", "https://docs.example.org/c"),
        ]);
        let t = keyed(&search, vec![
            ("https://docs.example.org/a", ok_resp("bwrap namespaces alpha content")),
            ("https://docs.example.org/b", ok_resp("bwrap namespaces bravo content")),
            ("https://docs.example.org/c", ok_resp("bwrap namespaces charlie content")),
        ]);
        let a = al(&["searx.example.org", "docs.example.org"]);
        let out = research(&t, &endpoint(), &a, None, "bwrap namespaces", 3, 3).unwrap();
        let urls: Vec<&str> = out.sources.iter().map(|s| s.url.as_str()).collect();
        assert_eq!(urls, vec![
            "https://docs.example.org/a",
            "https://docs.example.org/b",
            "https://docs.example.org/c",
        ]);
        assert!(out.unfetched.is_empty());
    }

    #[test]
    fn mid_list_fetch_failure_still_surfaces_later_successes() {
        // B 404s; A and C succeed → sources == [A, C], B recorded in unfetched.
        let search = search_json(&[
            ("A", "https://docs.example.org/a"),
            ("B", "https://docs.example.org/b"),
            ("C", "https://docs.example.org/c"),
        ]);
        let t = keyed(&search, vec![
            ("https://docs.example.org/a", ok_resp("bwrap namespaces alpha")),
            ("https://docs.example.org/b", RawResponse { status: 404, location: None,
                content_type: "text/plain".into(), body: b"nope".to_vec() }),
            ("https://docs.example.org/c", ok_resp("bwrap namespaces charlie")),
        ]);
        let a = al(&["searx.example.org", "docs.example.org"]);
        let out = research(&t, &endpoint(), &a, None, "bwrap namespaces", 3, 3).unwrap();
        let urls: Vec<&str> = out.sources.iter().map(|s| s.url.as_str()).collect();
        assert_eq!(urls, vec!["https://docs.example.org/a", "https://docs.example.org/c"]);
        assert_eq!(out.unfetched.len(), 1);
        assert_eq!(out.unfetched[0].url, "https://docs.example.org/b");
        assert_eq!(out.unfetched[0].reason, "fetch-failed: status 404");
    }

    #[test]
    fn parallel_result_is_deterministic() {
        // Same scenario run repeatedly must yield identical source ordering — the
        // classify phase is rank-ordered, so completion order must not leak out.
        let search = search_json(&[
            ("A", "https://docs.example.org/a"),
            ("B", "https://docs.example.org/b"),
            ("C", "https://docs.example.org/c"),
        ]);
        let a = al(&["searx.example.org", "docs.example.org"]);
        let mut seen: Option<Vec<String>> = None;
        for _ in 0..5 {
            let t = keyed(&search, vec![
                ("https://docs.example.org/a", ok_resp("bwrap namespaces alpha")),
                ("https://docs.example.org/b", ok_resp("bwrap namespaces bravo")),
                ("https://docs.example.org/c", ok_resp("bwrap namespaces charlie")),
            ]);
            let out = research(&t, &endpoint(), &a, None, "bwrap namespaces", 3, 3).unwrap();
            let urls: Vec<String> = out.sources.iter().map(|s| s.url.clone()).collect();
            match &seen {
                None => seen = Some(urls),
                Some(prev) => assert_eq!(prev, &urls, "source order must be stable across runs"),
            }
        }
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
    fn rank_page_caps_embed_input_to_max_and_signals() {
        // A page with more chunks than the cap must embed only the first
        // MAX_EMBED_PASSAGES (document order) and record a degrade note, so one
        // embed response can never blow the transport's 5 MiB record cap.
        let n = MAX_EMBED_PASSAGES + 5;
        let passages: Vec<String> =
            (0..n).map(|i| format!("sandbox namespaces passage {i}")).collect();
        let emb = FakeEmbedder::new(&[]); // returns one (empty) vector per input
        let qe = [1.0_f32, 0.0];
        let (ranked, note) = rank_page(Some(&emb), Some(&qe), "sandbox namespaces", &passages);
        assert_eq!(
            emb.last_input_len.get(),
            MAX_EMBED_PASSAGES,
            "only the first {MAX_EMBED_PASSAGES} chunks are embedded"
        );
        let note = note.expect("capping must be signalled, never silent");
        assert!(
            note.contains(&n.to_string()) && note.contains(&MAX_EMBED_PASSAGES.to_string()),
            "note should name both the chunk count and the cap: {note}"
        );
        // The linchpin invariant: a chunk PAST the embed cap is un-boosted
        // semantically but never dropped — it still surfaces via the lexical
        // lane (which ranks all passages). Pin it directly here.
        let past_cap = format!("sandbox namespaces passage {}", n - 1);
        assert!(
            ranked.iter().any(|p| p.text == past_cap),
            "a chunk past the embed cap must still surface via the lexical lane"
        );
    }

    #[test]
    fn rank_page_under_cap_embeds_all_and_no_cap_note() {
        // A page at or below the cap embeds every chunk and sets no cap note.
        let passages: Vec<String> =
            (0..4).map(|i| format!("sandbox passage {i}")).collect();
        let emb = FakeEmbedder::new(&[]);
        let qe = [1.0_f32, 0.0];
        let (_ranked, note) = rank_page(Some(&emb), Some(&qe), "sandbox", &passages);
        assert_eq!(emb.last_input_len.get(), 4, "all chunks embedded under the cap");
        assert!(note.is_none(), "no cap note under the cap: {note:?}");
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
