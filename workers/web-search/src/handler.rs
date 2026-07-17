//! JSON-RPC handler for `web.search`.
//!
//! Flow: parse params → run `search` against the configured endpoint → build
//! the result object. The endpoint is validated once at construction
//! (`from_env`); each call re-checks the host (defense in depth). Errors map
//! onto the protocol code vocabulary. No silent fallbacks.

use kastellan_protocol::{codes, server::Handler, RpcError};
use serde::Deserialize;

use kastellan_worker_web_common::allowlist::HostAllowlist;
use kastellan_worker_web_common::http::make_get;
use kastellan_worker_web_common::search::{validate_endpoint, DEFAULT_COUNT};
use kastellan_worker_web_common::search_provider::{
    choose_search_provider, search_err_to_rpc, BrokeredSearchProvider, DirectSearchProvider,
    SearchProvider, SearchProviderChoice,
};

// `Url` and `HttpGet` are used only by the test-only `with_parts*` constructors:
// the `DirectSearchProvider` that referenced them at module scope moved to
// web-common's `search_provider` in #464, leaving them test-only here.
#[cfg(test)]
use kastellan_worker_web_common::http::HttpGet;
#[cfg(test)]
use url::Url;

#[derive(Deserialize)]
struct SearchParams {
    query: String,
    #[serde(default)]
    count: Option<usize>,
}

/// The worker handler. Holds a single [`SearchProvider`] behind a trait object so
/// the direct-endpoint path and the broker path are interchangeable — `from_env`
/// picks one at startup.
pub struct WebSearchHandler {
    provider: Box<dyn SearchProvider>,
    /// Max queries accepted by `web.search_batch` (operator-tunable via
    /// `KASTELLAN_WEB_SEARCH_MAX_BATCH_QUERIES`; default 8).
    max_batch: usize,
}

impl WebSearchHandler {
    /// Build from env, selecting the provider (broker UDS wins, else direct
    /// endpoint). Fails closed (the worker never serves) if neither is set, or if
    /// the direct endpoint is unparseable / wrong-scheme / off-allowlist.
    ///
    /// * `KASTELLAN_SEARCH_BROKER_UDS` set → [`BrokeredSearchProvider`] (no direct
    ///   endpoint required — the broker holds the SearxNG route).
    /// * else `KASTELLAN_WEB_SEARCH_ENDPOINT` + `KASTELLAN_WEB_SEARCH_ALLOWLIST` →
    ///   [`DirectSearchProvider`] over the env-selected transport (`ProxyConnectGet`
    ///   when `KASTELLAN_EGRESS_PROXY_UDS` is set, else `ReqwestGet`).
    pub fn from_env() -> anyhow::Result<Self> {
        let broker_uds = std::env::var("KASTELLAN_SEARCH_BROKER_UDS").ok();
        let endpoint_raw = std::env::var("KASTELLAN_WEB_SEARCH_ENDPOINT").ok();
        let provider: Box<dyn SearchProvider> =
            match choose_search_provider(broker_uds.as_deref(), endpoint_raw.as_deref()) {
                SearchProviderChoice::Broker { uds } => {
                    Box::new(BrokeredSearchProvider::new(std::path::PathBuf::from(uds)))
                }
                SearchProviderChoice::Endpoint { endpoint } => {
                    let allow_raw = std::env::var("KASTELLAN_WEB_SEARCH_ALLOWLIST")
                        .unwrap_or_else(|_| "[]".to_string());
                    let allowlist = HostAllowlist::from_env_json(&allow_raw)?;
                    let url = validate_endpoint(endpoint, &allowlist)
                        .map_err(|e| anyhow::anyhow!(search_err_to_rpc(e).message))?;
                    let transport = make_get("kastellan-web-search/0")?;
                    Box::new(DirectSearchProvider::new(url, allowlist, transport))
                }
                SearchProviderChoice::None => anyhow::bail!(
                    "web-search: neither KASTELLAN_SEARCH_BROKER_UDS nor \
                     KASTELLAN_WEB_SEARCH_ENDPOINT set"
                ),
            };
        let max_batch = crate::batch::resolve_max_batch(
            std::env::var(crate::batch::MAX_BATCH_QUERIES_ENV).ok().as_deref(),
        );
        Ok(Self { provider, max_batch })
    }

    #[cfg(test)]
    fn with_parts<T: HttpGet + 'static>(
        endpoint: Url,
        allowlist: HostAllowlist,
        transport: T,
    ) -> Self {
        Self {
            provider: Box::new(DirectSearchProvider::new(endpoint, allowlist, transport)),
            max_batch: crate::batch::DEFAULT_MAX_BATCH_QUERIES,
        }
    }

    #[cfg(test)]
    fn with_parts_and_max_batch<T: HttpGet + 'static>(
        endpoint: Url,
        allowlist: HostAllowlist,
        transport: T,
        max_batch: usize,
    ) -> Self {
        Self {
            provider: Box::new(DirectSearchProvider::new(endpoint, allowlist, transport)),
            max_batch,
        }
    }
}

impl Handler for WebSearchHandler {
    fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        match method {
            "web.search" => {
                let p: SearchParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}")))?;
                let count = p.count.unwrap_or(DEFAULT_COUNT);
                let hits = self.provider.search(&p.query, count).map_err(search_err_to_rpc)?;
                let hit_count = hits.len();
                Ok(serde_json::json!({ "query": p.query, "results": hits, "count": hit_count }))
            }
            kastellan_worker_web_common::WEB_SEARCH_BATCH_METHOD => {
                let p: crate::batch::BatchParams = serde_json::from_value(params)
                    .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("bad params: {e}")))?;
                crate::batch::validate_batch(&p.queries, self.max_batch)
                    .map_err(|m| RpcError::new(codes::INVALID_PARAMS, m))?;
                let count = p.count.unwrap_or(DEFAULT_COUNT);
                // Soft-bound the sequential batch so it cannot outrun the worker's
                // hard wall-clock watchdog and lose already-completed queries.
                let deadline = std::time::Instant::now() + crate::batch::BATCH_SOFT_DEADLINE;
                let elements =
                    crate::batch::run_batch(&*self.provider, &p.queries, count, Some(deadline));
                Ok(serde_json::json!({ "results": elements }))
            }
            other => Err(RpcError::new(
                codes::METHOD_NOT_FOUND,
                format!("unknown method {other}"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kastellan_worker_web_common::http::RawResponse;
    use kastellan_worker_web_common::testing::{al, json_resp, FakeGet};

    fn handler(responses: Vec<RawResponse>) -> WebSearchHandler {
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

    #[test]
    fn batch_returns_per_query_results_in_order() {
        let good = r#"{"results":[{"title":"T","url":"https://x.test","content":"c","engine":"e"}]}"#;
        let mut h = handler(vec![json_resp(good), json_resp(good)]);
        let out = h
            .call("web.search_batch", serde_json::json!({"queries": ["a", "b"]}))
            .unwrap();
        let arr = out["results"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["query"], "a");
        assert_eq!(arr[0]["results"][0]["url"], "https://x.test");
        assert_eq!(arr[1]["query"], "b");
    }

    #[test]
    fn batch_one_bad_query_is_error_element_not_whole_failure() {
        let good = r#"{"results":[{"title":"T","url":"https://x.test","content":"c","engine":"e"}]}"#;
        let mut h = handler(vec![
            json_resp(good),
            RawResponse { status: 500, location: None, content_type: "text/plain".into(), body: Vec::new() },
        ]);
        let out = h
            .call("web.search_batch", serde_json::json!({"queries": ["a", "b"]}))
            .unwrap();
        let arr = out["results"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["results"][0]["url"], "https://x.test");
        assert!(arr[1]["error"].is_string(), "b should be an error element: {out}");
    }

    #[test]
    fn batch_empty_queries_is_invalid_params() {
        let mut h = handler(vec![]);
        let err = h
            .call("web.search_batch", serde_json::json!({"queries": []}))
            .unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
    }

    #[test]
    fn batch_over_cap_is_invalid_params() {
        let mut h = WebSearchHandler::with_parts_and_max_batch(
            Url::parse("https://searx.example.org/search").unwrap(),
            al(&["searx.example.org"]),
            FakeGet::new(vec![]),
            2,
        );
        let err = h
            .call("web.search_batch", serde_json::json!({"queries": ["a", "b", "c"]}))
            .unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
    }

    #[test]
    fn single_search_still_byte_identical() {
        // Regression pin: web.search is unchanged by the batch arm.
        let json = r#"{"results":[{"title":"T","url":"https://x.test","content":"c","engine":"e"}]}"#;
        let mut h = handler(vec![json_resp(json)]);
        let out = h.call("web.search", serde_json::json!({"query": "rust"})).unwrap();
        assert_eq!(out["query"], "rust");
        assert_eq!(out["count"], 1);
        assert_eq!(out["results"][0]["snippet"], "c");
    }

    // The SearchProvider seam tests (choose_* precedence + the brokered client's
    // UDS round-trip/error mapping) moved to web-common with the seam itself:
    // `kastellan_worker_web_common::search_provider::tests` (#464).
}
