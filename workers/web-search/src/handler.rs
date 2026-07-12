//! JSON-RPC handler for `web.search`.
//!
//! Flow: parse params → run `search` against the configured endpoint → build
//! the result object. The endpoint is validated once at construction
//! (`from_env`); each call re-checks the host (defense in depth). Errors map
//! onto the protocol code vocabulary. No silent fallbacks.

use kastellan_protocol::{codes, server::Handler, RpcError};
use serde::Deserialize;
use url::Url;

use kastellan_worker_web_common::allowlist::HostAllowlist;
use kastellan_worker_web_common::http::{make_get, HttpGet};
use kastellan_worker_web_common::parse::Hit;

use kastellan_worker_web_common::search::{search, validate_endpoint, SearchError, DEFAULT_COUNT};

#[derive(Deserialize)]
struct SearchParams {
    query: String,
    #[serde(default)]
    count: Option<usize>,
}

/// Map a [`SearchError`] to a JSON-RPC error.
pub(crate) fn search_err_to_rpc(e: SearchError) -> RpcError {
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

/// Run a search, returning parsed hits. The single network seam (faked in tests),
/// so the direct-endpoint path and the broker path are interchangeable behind one
/// interface — mirrors the web-research `Embedder` seam.
pub trait SearchProvider {
    fn search(&self, query: &str, count: usize) -> Result<Vec<Hit>, SearchError>;
}

/// Which provider `from_env` should build, decided purely from two env values.
/// Kept separate from (I/O-bound) construction so the precedence rule is unit-
/// testable without touching env or sockets.
#[derive(Debug, PartialEq)]
pub enum SearchProviderChoice<'a> {
    /// Neither a broker UDS nor a direct endpoint is configured.
    None,
    /// Use the broker sidecar at this UDS path (takes precedence).
    Broker { uds: &'a str },
    /// Use a direct SearxNG endpoint.
    Endpoint { endpoint: &'a str },
}

/// Pick the search source. The broker UDS wins over a direct endpoint when both
/// are set; blank/whitespace values count as unset. Mirrors `choose_embedder`.
pub fn choose_search_provider<'a>(
    broker_uds: Option<&'a str>,
    endpoint: Option<&'a str>,
) -> SearchProviderChoice<'a> {
    let broker = broker_uds.map(str::trim).filter(|s| !s.is_empty());
    let endpoint = endpoint.map(str::trim).filter(|s| !s.is_empty());
    match (broker, endpoint) {
        (Some(uds), _) => SearchProviderChoice::Broker { uds },
        (None, Some(endpoint)) => SearchProviderChoice::Endpoint { endpoint },
        (None, None) => SearchProviderChoice::None,
    }
}

/// Direct provider: a validated endpoint + host allowlist + transport. Wraps the
/// existing `web_common::search::search` (today's behaviour, behind the seam).
pub struct DirectSearchProvider<T: HttpGet> {
    endpoint: Url,
    allowlist: HostAllowlist,
    transport: T,
}

impl<T: HttpGet> DirectSearchProvider<T> {
    pub fn new(endpoint: Url, allowlist: HostAllowlist, transport: T) -> Self {
        Self { endpoint, allowlist, transport }
    }
}

impl<T: HttpGet> SearchProvider for DirectSearchProvider<T> {
    fn search(&self, query: &str, count: usize) -> Result<Vec<Hit>, SearchError> {
        search(&self.transport, &self.endpoint, &self.allowlist, query, count)
    }
}

/// Result envelope decoded from the broker's `search` reply: `{results:[Hit]}`.
#[derive(serde::Deserialize)]
struct BrokerSearchResult {
    results: Vec<Hit>,
}

/// Search via the trusted search-broker sidecar over a Unix socket. Sends JSON-RPC
/// `search{query,count}` (the broker's UDS is bind-mounted into this worker's jail)
/// and decodes the returned hits. The worker needs no search egress — the broker
/// holds the only route to SearxNG. Mirrors web-research's `BrokeredEmbedder`.
pub struct BrokeredSearchProvider {
    uds: std::path::PathBuf,
}

impl BrokeredSearchProvider {
    pub fn new(uds: std::path::PathBuf) -> Self {
        Self { uds }
    }
}

impl SearchProvider for BrokeredSearchProvider {
    fn search(&self, query: &str, count: usize) -> Result<Vec<Hit>, SearchError> {
        use std::io::{BufReader, Write};
        use std::os::unix::net::UnixStream;

        let mut stream = UnixStream::connect(&self.uds)
            .map_err(|e| SearchError::Transport(format!("connect broker {:?}: {e}", self.uds)))?;
        let req = kastellan_protocol::Request {
            jsonrpc: "2.0".into(),
            id: serde_json::json!(1),
            method: "search".into(),
            params: serde_json::json!({ "query": query, "count": count }),
        };
        let mut line = serde_json::to_vec(&req)
            .map_err(|e| SearchError::Parse(format!("request encode: {e}")))?;
        line.push(b'\n');
        stream
            .write_all(&line)
            .map_err(|e| SearchError::Transport(format!("write broker request: {e}")))?;
        stream.flush().ok();

        let mut br = BufReader::new(&stream);
        let buf = match kastellan_protocol::read_capped_record(&mut br, kastellan_protocol::MAX_RECORD_BYTES)
            .map_err(|e| SearchError::Transport(format!("read broker response: {e}")))?
        {
            kastellan_protocol::Record::Line(b) => b,
            kastellan_protocol::Record::Eof => {
                return Err(SearchError::Transport("broker closed without responding".into()))
            }
            kastellan_protocol::Record::TooLarge => {
                return Err(SearchError::Parse("broker response exceeded record cap".into()))
            }
        };
        let resp: kastellan_protocol::Response = serde_json::from_slice(&buf)
            .map_err(|e| SearchError::Parse(format!("broker response: {e}")))?;
        if let Some(err) = resp.error {
            // A broker JSON-RPC error surfaces as a transport-class failure to the
            // agent (the worker cannot itself retry the backend).
            return Err(SearchError::Transport(format!("broker error {}: {}", err.code, err.message)));
        }
        let result = resp
            .result
            .ok_or_else(|| SearchError::Parse("broker response missing result".into()))?;
        let decoded: BrokerSearchResult = serde_json::from_value(result)
            .map_err(|e| SearchError::Parse(format!("result decode: {e}")))?;
        Ok(decoded.results)
    }
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
            "web.search_batch" => {
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

    #[test]
    fn choose_broker_wins_when_both_set() {
        match choose_search_provider(Some("/run/search.sock"), Some("https://searx/search")) {
            SearchProviderChoice::Broker { uds } => assert_eq!(uds, "/run/search.sock"),
            other => panic!("expected Broker, got {other:?}"),
        }
    }

    #[test]
    fn choose_endpoint_when_only_endpoint_set() {
        match choose_search_provider(None, Some("https://searx/search")) {
            SearchProviderChoice::Endpoint { endpoint } => assert_eq!(endpoint, "https://searx/search"),
            other => panic!("expected Endpoint, got {other:?}"),
        }
    }

    #[test]
    fn choose_none_when_neither_and_blank_is_unset() {
        assert!(matches!(choose_search_provider(None, None), SearchProviderChoice::None));
        assert!(matches!(choose_search_provider(Some("  "), None), SearchProviderChoice::None));
    }

    use std::io::{BufReader as StdBufReader, Write as StdWrite};
    use std::os::unix::net::UnixListener;

    /// One-shot stub broker on `sock`: reads one request line, writes `response_json`.
    fn stub_broker(sock: std::path::PathBuf, response_json: String) -> std::thread::JoinHandle<()> {
        let listener = UnixListener::bind(&sock).unwrap();
        std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut br = StdBufReader::new(conn.try_clone().unwrap());
            let _ = kastellan_protocol::read_capped_record(&mut br, 1_000_000).unwrap();
            conn.write_all(response_json.as_bytes()).unwrap();
            conn.write_all(b"\n").unwrap();
            conn.flush().unwrap();
        })
    }

    #[test]
    fn brokered_search_round_trip_returns_hits() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("search.sock");
        let h = stub_broker(
            sock.clone(),
            r#"{"jsonrpc":"2.0","id":1,"result":{"results":[{"title":"T","url":"https://x.test","snippet":"c","engine":"e"}]}}"#.to_string(),
        );
        let p = BrokeredSearchProvider::new(sock);
        let hits = p.search("germany", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://x.test");
        h.join().unwrap();
    }

    #[test]
    fn brokered_search_maps_broker_error() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("search.sock");
        let h = stub_broker(
            sock.clone(),
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32002,"message":"backend down"}}"#.to_string(),
        );
        let p = BrokeredSearchProvider::new(sock);
        let err = p.search("x", 10).unwrap_err();
        assert!(matches!(err, SearchError::Transport(_)), "got {err:?}");
        h.join().unwrap();
    }

    #[test]
    fn brokered_search_absent_socket_is_transport_error() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("nope.sock"); // never bound
        let p = BrokeredSearchProvider::new(sock);
        assert!(matches!(p.search("x", 10).unwrap_err(), SearchError::Transport(_)));
    }
}
