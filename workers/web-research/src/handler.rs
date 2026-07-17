//! JSON-RPC handler for `web.research`.
//!
//! Flow: parse params → run `research` (search + fetch top-N allowlisted pages +
//! rank passages) → build the result object. The search source (a direct SearxNG
//! endpoint, or the trusted search-broker UDS) + content allowlist are
//! operator-controlled and validated at construction (`from_env`); the LLM
//! supplies only the query + optional caps. Errors map onto the protocol code
//! vocabulary. No silent fallbacks.

use kastellan_protocol::{codes, server::Handler, RpcError};
use serde::Deserialize;
use serde_json::json;
// `Url` is used only by the test helpers now (the `endpoint: Url` field became a
// `Box<dyn SearchProvider>` in #464); `validate_endpoint` returns `Url` by
// inference in `from_env` without naming the type.
#[cfg(test)]
use url::Url;

use kastellan_worker_web_common::allowlist::HostAllowlist;
use kastellan_worker_web_common::http::{make_get, HttpGet};
use kastellan_worker_web_common::search::validate_endpoint;
use kastellan_worker_web_common::search_provider::{
    choose_search_provider, search_err_to_rpc, BrokeredSearchProvider, DirectSearchProvider,
    SearchProvider, SearchProviderChoice,
};

use crate::embed::{choose_embedder, BrokeredEmbedder, Embedder, EmbedderChoice, HttpEmbedder};
use crate::research::{
    research, ResearchError, ResearchOutcome, DEFAULT_MAX_PASSAGES, DEFAULT_MAX_SOURCES,
};

#[derive(Deserialize)]
struct ResearchParams {
    query: String,
    #[serde(default)]
    max_sources: Option<usize>,
    #[serde(default)]
    max_passages: Option<usize>,
}

fn research_err_to_rpc(e: ResearchError) -> RpcError {
    match e {
        ResearchError::EmptyQuery => RpcError::new(codes::INVALID_PARAMS, "query is empty".to_string()),
        ResearchError::Search(s) => search_err_to_rpc(s),
    }
}

/// Serialize a [`ResearchOutcome`] into the wire JSON (see the design spec).
fn outcome_to_json(query: &str, out: ResearchOutcome) -> serde_json::Value {
    let sources: Vec<serde_json::Value> = out
        .sources
        .iter()
        .map(|s| {
            json!({
                "url": s.url,
                "title": s.title,
                "snippet": s.snippet,
                "fetched": true,
                "passages": s.passages.iter()
                    .map(|p| json!({ "text": p.text, "score": p.score }))
                    .collect::<Vec<_>>(),
            })
        })
        .collect();
    let unfetched: Vec<serde_json::Value> = out
        .unfetched
        .iter()
        .map(|u| json!({ "url": u.url, "title": u.title, "snippet": u.snippet, "reason": u.reason }))
        .collect();
    let passage_count: usize = out.sources.iter().map(|s| s.passages.len()).sum();
    let ranking = match out.ranking {
        crate::research::RankMode::Hybrid => "hybrid",
        crate::research::RankMode::Lexical => "lexical",
    };
    let mut obj = json!({
        "query": query,
        "sources": sources,
        "unfetched": unfetched,
        "sources_fetched": out.sources.len(),
        "passage_count": passage_count,
        "ranking": ranking,
    });
    if let Some(note) = &out.embed_note {
        obj["embed_note"] = json!(note);
    }
    obj
}

/// The worker handler, generic over the fetch transport so tests inject a fake.
/// The search source is held behind the [`SearchProvider`] trait object so the
/// direct-endpoint path and the broker path are interchangeable.
pub struct WebResearchHandler<T: HttpGet> {
    search: Box<dyn SearchProvider>,
    allowlist: HostAllowlist,
    transport: T,
    embedder: Option<Box<dyn Embedder>>,
}

impl WebResearchHandler<Box<dyn HttpGet>> {
    /// Build from env: search provider + content allowlist + env-selected
    /// fetch transport + optional embedder. Fails closed (the worker never
    /// serves) on a misconfigured direct endpoint or embed endpoint.
    ///
    /// Search-provider selection mirrors web-search: the broker UDS
    /// (`KASTELLAN_SEARCH_BROKER_UDS`, injected by core at spawn) wins over a
    /// direct `KASTELLAN_WEB_RESEARCH_ENDPOINT`. In broker mode no endpoint env
    /// is required — the broker holds the only SearxNG route.
    pub fn from_env() -> anyhow::Result<Self> {
        let allow_raw =
            std::env::var("KASTELLAN_WEB_RESEARCH_ALLOWLIST").unwrap_or_else(|_| "[]".into());
        let allowlist = HostAllowlist::from_env_json(&allow_raw)?;

        // Search source: broker UDS wins; a direct endpoint is validated against
        // the operator allowlist and fails closed (#428) if off-allowlist.
        let search_broker_uds = std::env::var("KASTELLAN_SEARCH_BROKER_UDS").ok();
        let endpoint_raw = std::env::var("KASTELLAN_WEB_RESEARCH_ENDPOINT").ok();
        let search: Box<dyn SearchProvider> =
            match choose_search_provider(search_broker_uds.as_deref(), endpoint_raw.as_deref()) {
                SearchProviderChoice::Broker { uds } => {
                    Box::new(BrokeredSearchProvider::new(std::path::PathBuf::from(uds)))
                }
                SearchProviderChoice::Endpoint { endpoint } => {
                    let url = validate_endpoint(endpoint, &allowlist)
                        .map_err(|e| anyhow::anyhow!(search_err_to_rpc(e).message))?;
                    // HostAllowlist is not Clone: parse the JSON a second time for
                    // the provider's own copy (startup-only cost).
                    let search_allowlist = HostAllowlist::from_env_json(&allow_raw)?;
                    let search_transport = make_get("kastellan-web-research/0")?;
                    Box::new(DirectSearchProvider::new(url, search_allowlist, search_transport))
                }
                SearchProviderChoice::None => anyhow::bail!(
                    "web-research: neither KASTELLAN_SEARCH_BROKER_UDS nor \
                     KASTELLAN_WEB_RESEARCH_ENDPOINT set"
                ),
            };

        let transport = make_get("kastellan-web-research/0")?;
        // Embedder selection: the embed broker UDS (KASTELLAN_EMBED_BROKER_UDS)
        // wins over a direct endpoint (KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT). The
        // model is shared by both paths. UNCHANGED by the search-provider rework.
        let embed_broker_uds = std::env::var("KASTELLAN_EMBED_BROKER_UDS").ok();
        let embed_endpoint_raw = std::env::var("KASTELLAN_WEB_RESEARCH_EMBED_ENDPOINT").ok();
        let model = std::env::var("KASTELLAN_WEB_RESEARCH_EMBED_MODEL")
            .unwrap_or_else(|_| "embeddinggemma".to_string());
        let embedder: Option<Box<dyn Embedder>> =
            match choose_embedder(embed_broker_uds.as_deref(), embed_endpoint_raw.as_deref()) {
                EmbedderChoice::Broker { uds } => {
                    // No allowlist check: the broker path has no worker egress.
                    Some(Box::new(BrokeredEmbedder::new(std::path::PathBuf::from(uds), model)))
                }
                EmbedderChoice::Endpoint { endpoint } => {
                    // The embed endpoint host must be on the same allowlist (fail
                    // closed if the operator forgot to allow it).
                    let embed_endpoint = validate_endpoint(endpoint, &allowlist)
                        .map_err(|e| anyhow::anyhow!(search_err_to_rpc(e).message))?;
                    let embed_transport = make_get("kastellan-web-research/0")?;
                    Some(Box::new(HttpEmbedder::new(embed_transport, embed_endpoint, model)))
                }
                EmbedderChoice::None => None,
            };
        Ok(Self { search, allowlist, transport, embedder })
    }
}

impl<T: HttpGet> WebResearchHandler<T> {
    #[cfg(test)]
    fn with_parts(search: Box<dyn SearchProvider>, allowlist: HostAllowlist, transport: T) -> Self {
        Self { search, allowlist, transport, embedder: None }
    }
}

impl<T: HttpGet> Handler for WebResearchHandler<T> {
    fn call(&mut self, method: &str, params: serde_json::Value)
        -> Result<serde_json::Value, RpcError>
    {
        if method != "web.research" {
            return Err(RpcError::new(codes::METHOD_NOT_FOUND, format!("unknown method {method}")));
        }
        let p: ResearchParams = serde_json::from_value(params)
            .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}")))?;
        let max_sources = p.max_sources.unwrap_or(DEFAULT_MAX_SOURCES);
        let max_passages = p.max_passages.unwrap_or(DEFAULT_MAX_PASSAGES);

        let out = research(
            &*self.search, &self.transport, &self.allowlist,
            self.embedder.as_deref(), &p.query, max_sources, max_passages,
        ).map_err(research_err_to_rpc)?;

        Ok(outcome_to_json(&p.query, out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kastellan_worker_web_common::http::RawResponse;
    use kastellan_worker_web_common::search_provider::DirectSearchProvider;
    use kastellan_worker_web_common::testing::{al, json_resp, stub_broker, FakeGet};

    /// Build a handler with a direct search provider over `search_responses` and a
    /// fetch transport over `page_responses`. Search and fetch no longer share one
    /// transport, so callers split their responses across the two.
    fn handler_parts(
        search_responses: Vec<RawResponse>,
        page_responses: Vec<RawResponse>,
    ) -> WebResearchHandler<FakeGet> {
        WebResearchHandler::with_parts(
            Box::new(DirectSearchProvider::new(
                Url::parse("https://searx.example.org/search").unwrap(),
                al(&["searx.example.org", "docs.example.org"]),
                FakeGet::new(search_responses),
            )),
            al(&["searx.example.org", "docs.example.org"]),
            FakeGet::new(page_responses),
        )
    }

    /// Legacy single-list helper: `responses[0]` is the SEARCH response, the rest
    /// are page fetches (an empty list — for query-validation tests that never
    /// search — yields empty queues for both).
    fn handler(mut responses: Vec<RawResponse>) -> WebResearchHandler<FakeGet> {
        let pages = if responses.is_empty() { Vec::new() } else { responses.split_off(1) };
        handler_parts(responses, pages)
    }

    fn search_json(title: &str, url: &str) -> String {
        format!(r#"{{"results":[{{"title":"{title}","url":"{url}","content":"c","engine":"e"}}]}}"#)
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let mut h = handler(vec![]);
        let err = h.call("nope", json!({})).unwrap_err();
        assert_eq!(err.code, codes::METHOD_NOT_FOUND);
    }

    #[test]
    fn missing_query_is_invalid_params() {
        let mut h = handler(vec![]);
        let err = h.call("web.research", json!({})).unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
    }

    #[test]
    fn empty_query_is_invalid_params() {
        let mut h = handler(vec![]);
        let err = h.call("web.research", json!({"query": "  "})).unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
    }

    #[test]
    fn happy_path_returns_sources_and_passages() {
        let page = "bwrap creates user namespaces for sandboxing workers.";
        let mut h = handler(vec![
            json_resp(&search_json("Doc", "https://docs.example.org/bwrap")),
            RawResponse { status: 200, location: None, content_type: "text/plain".into(),
                body: page.as_bytes().to_vec() },
        ]);
        let out = h.call("web.research", json!({"query": "bwrap user namespaces"})).unwrap();
        assert_eq!(out["query"], "bwrap user namespaces");
        assert_eq!(out["sources_fetched"], 1);
        assert_eq!(out["sources"][0]["url"], "https://docs.example.org/bwrap");
        assert_eq!(out["sources"][0]["fetched"], true);
        assert!(out["passage_count"].as_u64().unwrap() >= 1);
        assert!(out["sources"][0]["passages"][0]["text"].as_str().unwrap().contains("bwrap"));
    }

    #[test]
    fn search_failure_maps_to_operation_failed() {
        let mut h = handler(vec![RawResponse { status: 500, location: None,
            content_type: "text/plain".into(), body: Vec::new() }]);
        let err = h.call("web.research", json!({"query": "q term"})).unwrap_err();
        assert_eq!(err.code, codes::OPERATION_FAILED);
    }

    #[test]
    fn off_allowlist_hit_shows_in_unfetched() {
        let mut h = handler(vec![
            json_resp(&search_json("Evil", "https://evil.test/x")),
        ]);
        let out = h.call("web.research", json!({"query": "q term"})).unwrap();
        assert_eq!(out["sources_fetched"], 0);
        assert_eq!(out["unfetched"][0]["reason"], "off-allowlist");
    }

    fn handler_with_embedder(
        mut responses: Vec<RawResponse>,
        embedder: Option<Box<dyn crate::embed::Embedder>>,
    ) -> WebResearchHandler<FakeGet> {
        let pages = if responses.is_empty() { Vec::new() } else { responses.split_off(1) };
        let mut h = handler_parts(responses, pages);
        h.embedder = embedder;
        h
    }

    #[test]
    fn lexical_result_reports_ranking_lexical() {
        let page = "bwrap creates user namespaces.";
        let mut h = handler(vec![
            json_resp(&search_json("Doc", "https://docs.example.org/bwrap")),
            RawResponse { status: 200, location: None, content_type: "text/plain".into(),
                body: page.as_bytes().to_vec() },
        ]);
        let out = h.call("web.research", json!({"query": "bwrap user namespaces"})).unwrap();
        assert_eq!(out["ranking"], "lexical");
        assert!(out.get("embed_note").is_none() || out["embed_note"].is_null());
    }

    #[test]
    fn hybrid_result_reports_ranking_hybrid() {
        use crate::embed::FakeEmbedder;
        let page = "bwrap creates user namespaces.";
        let emb = FakeEmbedder::new(&[
            ("bwrap user namespaces", vec![1.0_f32, 0.0]),
            ("bwrap creates user namespaces.", vec![1.0_f32, 0.0]),
        ]);
        let mut h = handler_with_embedder(
            vec![
                json_resp(&search_json("Doc", "https://docs.example.org/bwrap")),
                RawResponse { status: 200, location: None, content_type: "text/plain".into(),
                    body: page.as_bytes().to_vec() },
            ],
            Some(Box::new(emb)),
        );
        let out = h.call("web.research", json!({"query": "bwrap user namespaces"})).unwrap();
        assert_eq!(out["ranking"], "hybrid");
    }

    #[test]
    fn degraded_result_carries_embed_note() {
        use crate::embed::FakeEmbedder;
        let page = "bwrap creates user namespaces.";
        let mut h = handler_with_embedder(
            vec![
                json_resp(&search_json("Doc", "https://docs.example.org/bwrap")),
                RawResponse { status: 200, location: None, content_type: "text/plain".into(),
                    body: page.as_bytes().to_vec() },
            ],
            Some(Box::new(FakeEmbedder::failing())),
        );
        let out = h.call("web.research", json!({"query": "bwrap user namespaces"})).unwrap();
        assert_eq!(out["ranking"], "lexical");
        assert!(out["embed_note"].as_str().unwrap().contains("embed"));
    }

    #[test]
    fn brokered_search_feeds_research_pipeline() {
        // With a brokered provider the worker searches via the broker UDS (zero
        // search egress) then fetches the returned content page over the normal
        // fetch transport — the whole pipeline runs unchanged behind the seam.
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("search.sock");
        let broker = stub_broker(
            sock.clone(),
            r#"{"jsonrpc":"2.0","id":1,"result":{"results":[{"title":"Doc","url":"https://docs.example.org/bwrap","snippet":"c","engine":"e"}]}}"#.to_string(),
        );
        let page = "bwrap creates user namespaces for sandboxing workers.";
        let mut h = WebResearchHandler::with_parts(
            Box::new(BrokeredSearchProvider::new(sock)),
            al(&["searx.example.org", "docs.example.org"]),
            FakeGet::new(vec![RawResponse {
                status: 200,
                location: None,
                content_type: "text/plain".into(),
                body: page.as_bytes().to_vec(),
            }]),
        );
        let out = h.call("web.research", json!({"query": "bwrap user namespaces"})).unwrap();
        assert_eq!(out["sources_fetched"], 1);
        assert_eq!(out["sources"][0]["url"], "https://docs.example.org/bwrap");
        broker.join().unwrap();
    }
}
