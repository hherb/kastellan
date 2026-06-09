//! JSON-RPC handler for `web.search`.
//!
//! Flow: parse params → run `search` against the configured endpoint → build
//! the result object. The endpoint is validated once at construction
//! (`from_env`); each call re-checks the host (defense in depth). Errors map
//! onto the protocol code vocabulary. No silent fallbacks.

use hhagent_protocol::{codes, server::Handler, RpcError};
use serde::Deserialize;
use url::Url;

use hhagent_worker_web_common::allowlist::HostAllowlist;
use hhagent_worker_web_common::http::{HttpGet, ReqwestGet};

use crate::search::{search, validate_endpoint, SearchError, DEFAULT_COUNT};

#[derive(Deserialize)]
struct SearchParams {
    query: String,
    #[serde(default)]
    count: Option<usize>,
}

/// Map a [`SearchError`] to a JSON-RPC error.
fn search_err_to_rpc(e: SearchError) -> RpcError {
    match e {
        SearchError::EmptyQuery => {
            RpcError::new(codes::INVALID_PARAMS, "query is empty".to_string())
        }
        SearchError::BadEndpoint(m) => RpcError::new(
            codes::POLICY_DENIED,
            format!("configured endpoint invalid: {m}"),
        ),
        SearchError::SchemeDenied(s) => RpcError::new(
            codes::POLICY_DENIED,
            format!("endpoint scheme {s:?} not allowed (https, or http for loopback only)"),
        ),
        SearchError::HostDenied(h) => RpcError::new(
            codes::POLICY_DENIED,
            format!("endpoint host {h:?} not on allowlist"),
        ),
        SearchError::Transport(m) => {
            RpcError::new(codes::OPERATION_FAILED, format!("search request failed: {m}"))
        }
        SearchError::Redirected => RpcError::new(
            codes::OPERATION_FAILED,
            "search endpoint returned an unexpected redirect".to_string(),
        ),
        SearchError::BadStatus(s) => RpcError::new(
            codes::OPERATION_FAILED,
            format!("search endpoint returned status {s}"),
        ),
        SearchError::Parse(m) => {
            RpcError::new(codes::OPERATION_FAILED, format!("parsing results failed: {m}"))
        }
    }
}

/// The worker handler, generic over the transport so tests inject a fake.
pub struct WebSearchHandler<T: HttpGet> {
    endpoint: Url,
    allowlist: HostAllowlist,
    transport: T,
}

impl WebSearchHandler<ReqwestGet> {
    /// Build from env: endpoint + allowlist JSON + real reqwest transport.
    /// Validates the endpoint up front and fails closed (the worker never
    /// serves) if it is missing, unparseable, wrong-scheme, or off-allowlist.
    pub fn from_env() -> anyhow::Result<Self> {
        let endpoint_raw = std::env::var("HHAGENT_WEB_SEARCH_ENDPOINT")
            .map_err(|_| anyhow::anyhow!("HHAGENT_WEB_SEARCH_ENDPOINT not set"))?;
        let allow_raw =
            std::env::var("HHAGENT_WEB_SEARCH_ALLOWLIST").unwrap_or_else(|_| "[]".to_string());
        let allowlist = HostAllowlist::from_env_json(&allow_raw)?;
        let endpoint = validate_endpoint(&endpoint_raw, &allowlist)
            .map_err(|e| anyhow::anyhow!(search_err_to_rpc(e).message))?;
        let transport = ReqwestGet::new()?;
        Ok(Self { endpoint, allowlist, transport })
    }
}

impl<T: HttpGet> WebSearchHandler<T> {
    #[cfg(test)]
    fn with_parts(endpoint: Url, allowlist: HostAllowlist, transport: T) -> Self {
        Self { endpoint, allowlist, transport }
    }
}

impl<T: HttpGet> Handler for WebSearchHandler<T> {
    fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        if method != "web.search" {
            return Err(RpcError::new(
                codes::METHOD_NOT_FOUND,
                format!("unknown method {method}"),
            ));
        }
        let p: SearchParams = serde_json::from_value(params)
            .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}")))?;
        let count = p.count.unwrap_or(DEFAULT_COUNT);

        let hits = search(&self.transport, &self.endpoint, &self.allowlist, &p.query, count)
            .map_err(search_err_to_rpc)?;

        let hit_count = hits.len();
        Ok(serde_json::json!({
            "query": p.query,
            "results": hits,
            "count": hit_count,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hhagent_worker_web_common::http::RawResponse;
    use hhagent_worker_web_common::testing::{al, json_resp, FakeGet};

    fn handler(responses: Vec<RawResponse>) -> WebSearchHandler<FakeGet> {
        WebSearchHandler::with_parts(
            Url::parse("https://searx.example.org/search").unwrap(),
            al(&["searx.example.org"]),
            FakeGet::new(responses),
        )
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let mut h = handler(vec![]);
        let err = h.call("nope", serde_json::json!({})).unwrap_err();
        assert_eq!(err.code, codes::METHOD_NOT_FOUND);
    }

    #[test]
    fn missing_query_is_invalid_params() {
        let mut h = handler(vec![]);
        let err = h.call("web.search", serde_json::json!({})).unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
    }

    #[test]
    fn empty_query_is_invalid_params() {
        let mut h = handler(vec![]);
        let err = h
            .call("web.search", serde_json::json!({"query": "  "}))
            .unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
    }

    #[test]
    fn happy_path_returns_hits() {
        let json = r#"{"results":[{"title":"T","url":"https://x.test","content":"c","engine":"e"}]}"#;
        let mut h = handler(vec![json_resp(json)]);
        let out = h
            .call("web.search", serde_json::json!({"query": "rust"}))
            .unwrap();
        assert_eq!(out["query"], "rust");
        assert_eq!(out["count"], 1);
        assert_eq!(out["results"][0]["url"], "https://x.test");
        assert_eq!(out["results"][0]["snippet"], "c");
    }

    #[test]
    fn endpoint_failure_maps_to_operation_failed() {
        let mut h = handler(vec![RawResponse {
            status: 500,
            location: None,
            content_type: "text/plain".into(),
            body: Vec::new(),
        }]);
        let err = h
            .call("web.search", serde_json::json!({"query": "rust"}))
            .unwrap_err();
        assert_eq!(err.code, codes::OPERATION_FAILED);
    }
}
