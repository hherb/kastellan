//! The search-provider seam shared by web-search and web-research: one trait
//! ([`SearchProvider`]) with a direct SearxNG implementation and a brokered one
//! that reaches SearxNG only through the trusted search-broker sidecar's UDS
//! (zero worker search egress). [`choose_search_provider`] is the pure
//! precedence rule (broker UDS wins over a direct endpoint); the
//! `SearchError → RpcError` mapper lives here too so both workers share one
//! error vocabulary. Lifted verbatim from web-search's handler (2026-07-17,
//! #464) so web-research can adopt the same seam.

use kastellan_protocol::{codes, RpcError};
use url::Url;

use crate::allowlist::HostAllowlist;
use crate::http::HttpGet;
use crate::parse::Hit;
use crate::search::{search, SearchError};

/// Map a [`SearchError`] to a JSON-RPC error.
pub fn search_err_to_rpc(e: SearchError) -> RpcError {
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
/// existing [`crate::search::search`] (today's behaviour, behind the seam).
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

#[cfg(test)]
mod tests {
    use super::*;

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

    // The one-shot stub broker lives in `crate::testing` (shared with the
    // web-research handler tests). web-common's test command already enables the
    // `testing` feature alongside `search`.
    use crate::testing::stub_broker;

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
