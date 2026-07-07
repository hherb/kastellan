//! JSON-RPC handler for `web.research`.
//!
//! Flow: parse params → run `research` (search + fetch top-N allowlisted pages +
//! rank passages) → build the result object. The endpoint + allowlist are
//! operator-controlled and validated at construction (`from_env`); the LLM
//! supplies only the query + optional caps. Errors map onto the protocol code
//! vocabulary. No silent fallbacks.

use kastellan_protocol::{codes, server::Handler, RpcError};
use serde::Deserialize;
use serde_json::json;
use url::Url;

use kastellan_worker_web_common::allowlist::HostAllowlist;
use kastellan_worker_web_common::http::{make_get, HttpGet};
use kastellan_worker_web_common::search::{validate_endpoint, SearchError};

use crate::rank::LexicalRanker;
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

/// Map a [`SearchError`] to a JSON-RPC error (shared shape with web-search).
fn search_err_to_rpc(e: SearchError) -> RpcError {
    match e {
        SearchError::EmptyQuery => RpcError::new(codes::INVALID_PARAMS, "query is empty".to_string()),
        SearchError::BadEndpoint(m) => {
            RpcError::new(codes::POLICY_DENIED, format!("configured endpoint invalid: {m}"))
        }
        SearchError::SchemeDenied(s) => RpcError::new(
            codes::POLICY_DENIED,
            format!("endpoint scheme {s:?} not allowed (https, or http for loopback only)"),
        ),
        SearchError::HostDenied(h) => {
            RpcError::new(codes::POLICY_DENIED, format!("endpoint host {h:?} not on allowlist"))
        }
        SearchError::Transport(m) => {
            RpcError::new(codes::OPERATION_FAILED, format!("search request failed: {m}"))
        }
        SearchError::Redirected => RpcError::new(
            codes::OPERATION_FAILED,
            "search endpoint returned an unexpected redirect".to_string(),
        ),
        SearchError::BadStatus(s) => {
            RpcError::new(codes::OPERATION_FAILED, format!("search endpoint returned status {s}"))
        }
        SearchError::Parse(m) => {
            RpcError::new(codes::OPERATION_FAILED, format!("parsing results failed: {m}"))
        }
    }
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
    json!({
        "query": query,
        "sources": sources,
        "unfetched": unfetched,
        "sources_fetched": out.sources.len(),
        "passage_count": passage_count,
    })
}

/// The worker handler, generic over the transport so tests inject a fake.
pub struct WebResearchHandler<T: HttpGet> {
    endpoint: Url,
    allowlist: HostAllowlist,
    transport: T,
    ranker: LexicalRanker,
}

impl WebResearchHandler<Box<dyn HttpGet>> {
    /// Build from env: endpoint + allowlist JSON + env-selected transport.
    /// Validates the endpoint up front and fails closed (the worker never
    /// serves) if it is missing, unparseable, wrong-scheme, or off-allowlist.
    pub fn from_env() -> anyhow::Result<Self> {
        let endpoint_raw = std::env::var("KASTELLAN_WEB_RESEARCH_ENDPOINT")
            .map_err(|_| anyhow::anyhow!("KASTELLAN_WEB_RESEARCH_ENDPOINT not set"))?;
        let allow_raw =
            std::env::var("KASTELLAN_WEB_RESEARCH_ALLOWLIST").unwrap_or_else(|_| "[]".into());
        let allowlist = HostAllowlist::from_env_json(&allow_raw)?;
        let endpoint = validate_endpoint(&endpoint_raw, &allowlist)
            .map_err(|e| anyhow::anyhow!(search_err_to_rpc(e).message))?;
        let transport = make_get("kastellan-web-research/0")?;
        Ok(Self { endpoint, allowlist, transport, ranker: LexicalRanker })
    }
}

impl<T: HttpGet> WebResearchHandler<T> {
    #[cfg(test)]
    fn with_parts(endpoint: Url, allowlist: HostAllowlist, transport: T) -> Self {
        Self { endpoint, allowlist, transport, ranker: LexicalRanker }
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
            &self.transport, &self.endpoint, &self.allowlist, &self.ranker,
            &p.query, max_sources, max_passages,
        ).map_err(research_err_to_rpc)?;

        Ok(outcome_to_json(&p.query, out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kastellan_worker_web_common::http::RawResponse;
    use kastellan_worker_web_common::testing::{al, json_resp, FakeGet};

    fn handler(responses: Vec<RawResponse>) -> WebResearchHandler<FakeGet> {
        WebResearchHandler::with_parts(
            Url::parse("https://searx.example.org/search").unwrap(),
            al(&["searx.example.org", "docs.example.org"]),
            FakeGet::new(responses),
        )
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
}
