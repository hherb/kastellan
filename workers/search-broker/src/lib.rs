//! Trusted search broker sidecar.
//!
//! A force-routed jailed worker cannot reach a loopback SearxNG (the egress proxy
//! SSRF-blocks loopback). It talks JSON-RPC `search{query,count?}` to this broker
//! over a Unix socket core bind-mounts into its jail; the broker — running in the
//! host netns with `Net::Allowlist([searx host:port])` — forwards to SearxNG and
//! returns the parsed hits. All SearxNG coupling lives in web-common's `search`.

use kastellan_protocol::server::Handler;
use kastellan_protocol::{codes, RpcError};
use serde::Deserialize;
use url::Url;

use kastellan_worker_web_common::allowlist::HostAllowlist;
use kastellan_worker_web_common::http::HttpGet;
use kastellan_worker_web_common::search::{search, SearchError, DEFAULT_COUNT};

#[derive(Deserialize)]
struct SearchRpcParams {
    query: String,
    #[serde(default)]
    count: Option<usize>,
}

/// Map a web-common `SearchError` to a JSON-RPC error. A bad-config / denied
/// endpoint is `POLICY_DENIED`; an empty query is `INVALID_PARAMS`; anything else
/// (transport, status, parse, redirect) is `OPERATION_FAILED` — the broker never
/// partially succeeds.
fn search_err_to_rpc(e: SearchError) -> RpcError {
    match e {
        SearchError::EmptyQuery => {
            RpcError::new(codes::INVALID_PARAMS, "query is empty".to_string())
        }
        SearchError::BadEndpoint(m) => {
            RpcError::new(codes::POLICY_DENIED, format!("endpoint invalid: {m}"))
        }
        SearchError::SchemeDenied(s) => {
            RpcError::new(codes::POLICY_DENIED, format!("endpoint scheme {s:?} not allowed"))
        }
        SearchError::HostDenied(h) => {
            RpcError::new(codes::POLICY_DENIED, format!("endpoint host {h:?} not on allowlist"))
        }
        SearchError::Transport(m) => {
            RpcError::new(codes::OPERATION_FAILED, format!("backend transport: {m}"))
        }
        SearchError::Redirected => RpcError::new(
            codes::OPERATION_FAILED,
            "backend returned an unexpected redirect".to_string(),
        ),
        SearchError::BadStatus(s) => {
            RpcError::new(codes::OPERATION_FAILED, format!("backend status {s}"))
        }
        SearchError::Parse(m) => {
            RpcError::new(codes::OPERATION_FAILED, format!("parse failed: {m}"))
        }
    }
}

/// JSON-RPC handler for the broker's single `search` method. Forwards to the
/// SearxNG backend via web-common's `search` (which re-checks the host allowlist
/// and enforces the `MAX_COUNT` cap). Generic over the transport for tests.
pub struct SearchHandler<T: HttpGet> {
    endpoint: Url,
    allowlist: HostAllowlist,
    transport: T,
}

impl<T: HttpGet> SearchHandler<T> {
    /// Build a handler forwarding `search` to `endpoint` (re-checked against
    /// `allowlist`) over `transport`.
    pub fn new(endpoint: Url, allowlist: HostAllowlist, transport: T) -> Self {
        Self { endpoint, allowlist, transport }
    }

    #[cfg(test)]
    fn with_parts(endpoint: Url, allowlist: HostAllowlist, transport: T) -> Self {
        Self { endpoint, allowlist, transport }
    }
}

impl<T: HttpGet> Handler for SearchHandler<T> {
    fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        if method != "search" {
            return Err(RpcError::new(
                codes::METHOD_NOT_FOUND,
                format!("unknown method: {method}"),
            ));
        }
        let p: SearchRpcParams = serde_json::from_value(params)
            .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("params: {e}")))?;
        let count = p.count.unwrap_or(DEFAULT_COUNT);
        let hits = search(&self.transport, &self.endpoint, &self.allowlist, &p.query, count)
            .map_err(search_err_to_rpc)?;
        serde_json::to_value(serde_json::json!({ "results": hits }))
            .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, format!("result encode: {e}")))
    }
}

use std::os::unix::net::UnixStream;
use std::time::Duration;

/// Framing byte-cap for one JSON-RPC request record on the broker's socket. A
/// query + count is tiny; 1 MiB is ample and far below the protocol default.
pub const BROKER_MAX_RECORD_BYTES: usize = 1024 * 1024;

/// Idle read timeout for one broker connection (serial serve loop).
pub const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Write timeout for one broker connection.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Serve one accepted UDS connection until EOF / timeout / protocol fault, via the
/// transport-generic `serve_capped` at [`BROKER_MAX_RECORD_BYTES`]. Mirrors the
/// embed-broker's serve loop; protocol faults are fail-closed
/// ([`kastellan_protocol::server::OnProtocolError::Close`]).
pub fn serve_connection<T: HttpGet>(
    handler: &mut SearchHandler<T>,
    stream: UnixStream,
) -> std::io::Result<()> {
    serve_connection_capped(
        handler,
        stream,
        Some(READ_TIMEOUT),
        Some(WRITE_TIMEOUT),
        BROKER_MAX_RECORD_BYTES,
    )
}

/// [`serve_connection`] with explicit read/write timeouts and framing cap, so
/// unit tests can drive short timeouts or a tiny cap. The timeouts are applied to
/// the socket before the loop; the cloned read half shares the same socket, so
/// they cover both halves.
fn serve_connection_capped<T: HttpGet>(
    handler: &mut SearchHandler<T>,
    stream: UnixStream,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
    cap: usize,
) -> std::io::Result<()> {
    stream.set_read_timeout(read_timeout)?;
    stream.set_write_timeout(write_timeout)?;
    let mut reader = stream.try_clone()?;
    let mut writer = stream;
    kastellan_protocol::server::serve_capped(
        handler,
        &mut reader,
        &mut writer,
        cap,
        kastellan_protocol::server::OnProtocolError::Close,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use kastellan_worker_web_common::http::RawResponse;
    use kastellan_worker_web_common::testing::{al, json_resp, FakeGet};
    use kastellan_protocol::codes;
    use url::Url;

    fn handler(responses: Vec<RawResponse>) -> SearchHandler<FakeGet> {
        SearchHandler::with_parts(
            Url::parse("http://127.0.0.1:8888/search").unwrap(),
            al(&["127.0.0.1"]),
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
    fn empty_query_is_invalid_params() {
        let mut h = handler(vec![]);
        let err = h.call("search", serde_json::json!({"query": "  "})).unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
    }

    #[test]
    fn happy_path_returns_results_envelope() {
        let json = r#"{"results":[{"title":"T","url":"https://x.test","content":"c","engine":"e"}]}"#;
        let mut h = handler(vec![json_resp(json)]);
        let out = h.call("search", serde_json::json!({"query": "germany"})).unwrap();
        assert_eq!(out["results"][0]["url"], "https://x.test");
        assert_eq!(out["results"][0]["snippet"], "c");
    }

    #[test]
    fn backend_failure_maps_to_operation_failed() {
        let mut h = handler(vec![RawResponse { status: 500, location: None, content_type: "text/plain".into(), body: Vec::new() }]);
        let err = h.call("search", serde_json::json!({"query": "x"})).unwrap_err();
        assert_eq!(err.code, codes::OPERATION_FAILED);
    }
}
