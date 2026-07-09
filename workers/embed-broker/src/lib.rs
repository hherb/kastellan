//! Trusted embedding broker sidecar.
//!
//! A jailed worker cannot reach the operator's embedding backend directly (no
//! egress). Instead it talks JSON-RPC `embed{model,input}` to this broker over a
//! Unix socket that core bind-mounts into its jail; the broker forwards the
//! request to the backend as an OpenAI-compatible POST and returns the vectors.
//! All OpenAI-compat coupling lives here, in one place.

use serde::{Deserialize, Serialize};

/// Max passages accepted in one `embed` batch (defense-in-depth; bounds the
/// backend POST). Web-research caps per-page embeds at 128, so 256 leaves margin.
pub const MAX_INPUTS: usize = 256;

/// Max total input-text bytes accepted in one `embed` batch (fail-closed above).
pub const MAX_REQUEST_BYTES: usize = 1_000_000;

/// The `embed` method params, parsed from the JSON-RPC request.
#[derive(Debug, Deserialize)]
pub struct EmbedParams {
    /// The embedding model name (forwarded verbatim to the backend).
    pub model: String,
    /// The texts to embed, one vector returned per input.
    pub input: Vec<String>,
}

/// One row of the `embed` result: a vector at its input position.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct EmbedData {
    /// Position in the request's `input` array (0-based).
    pub index: usize,
    /// The embedding vector.
    pub embedding: Vec<f32>,
}

/// The `embed` method result: one [`EmbedData`] per input, in input order.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct EmbedResult {
    pub data: Vec<EmbedData>,
}

use kastellan_protocol::{codes, RpcError};
use kastellan_worker_web_common::http::HttpGet;
use url::Url;

/// The OpenAI-compatible request body sent to the backend.
#[derive(Serialize)]
struct BackendReq<'a> {
    model: &'a str,
    input: &'a [String],
}

/// One row of the backend's OpenAI-compatible response.
#[derive(Deserialize)]
struct BackendRow {
    #[serde(default)]
    index: usize,
    embedding: Vec<f32>,
}

/// The backend's OpenAI-compatible response envelope.
#[derive(Deserialize)]
struct BackendResp {
    data: Vec<BackendRow>,
}

/// Forward one `embed` request to the backend and normalise the response.
///
/// POSTs `{model, input}` (OpenAI-compatible) to `endpoint` over `transport`,
/// decodes `{data:[{index,embedding}]}`, reorders rows by `index` so each vector
/// pairs with its input position, and count-checks (one vector per input). Any
/// transport error, non-2xx status, decode failure, or count mismatch becomes an
/// `OPERATION_FAILED` [`RpcError`] — the broker never partially succeeds.
pub fn forward_embed<T: HttpGet>(
    transport: &T,
    endpoint: &Url,
    params: &EmbedParams,
) -> Result<EmbedResult, RpcError> {
    if params.input.is_empty() {
        return Ok(EmbedResult { data: Vec::new() });
    }
    let body = serde_json::to_vec(&BackendReq { model: &params.model, input: &params.input })
        .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, format!("request encode: {e}")))?;
    let resp = transport
        .post(endpoint, "application/json", &body)
        .map_err(|e| RpcError::new(codes::OPERATION_FAILED, format!("backend transport: {e}")))?;
    if !(200..300).contains(&resp.status) {
        return Err(RpcError::new(
            codes::OPERATION_FAILED,
            format!("backend status {}", resp.status),
        ));
    }
    let decoded: BackendResp = serde_json::from_slice(&resp.body)
        .map_err(|e| RpcError::new(codes::OPERATION_FAILED, format!("backend decode: {e}")))?;
    if decoded.data.len() != params.input.len() {
        return Err(RpcError::new(
            codes::OPERATION_FAILED,
            format!(
                "vector count mismatch: requested {}, returned {}",
                params.input.len(),
                decoded.data.len()
            ),
        ));
    }
    let mut rows = decoded.data;
    rows.sort_by_key(|d| d.index);
    // A compliant backend returns exactly one row per input position, so after
    // sorting, row `i` carries `index == i`. Reject duplicate or gapped indices
    // (e.g. `[0, 0]` or `[0, 2]` for two inputs): those pass the count check
    // above yet would silently pair a vector with the wrong input position.
    // The broker is the trusted boundary — it fails closed rather than forward a
    // mispaired batch.
    if let Some((i, d)) = rows.iter().enumerate().find(|(i, d)| d.index != *i) {
        return Err(RpcError::new(
            codes::OPERATION_FAILED,
            format!("backend returned non-contiguous embedding indices (row {i} has index {})", d.index),
        ));
    }
    let data = rows
        .into_iter()
        .enumerate()
        .map(|(i, d)| EmbedData { index: i, embedding: d.embedding })
        .collect();
    Ok(EmbedResult { data })
}

use kastellan_protocol::server::Handler;

/// JSON-RPC handler for the broker's single `embed` method.
///
/// Enforces the batch caps ([`MAX_INPUTS`], [`MAX_REQUEST_BYTES`]) fail-closed
/// *before* any backend call, then delegates to [`forward_embed`]. Generic over
/// the transport so tests inject a `FakeGet`.
pub struct EmbedHandler<T: HttpGet> {
    transport: T,
    endpoint: Url,
}

impl<T: HttpGet> EmbedHandler<T> {
    /// Build a handler that forwards `embed` calls to `endpoint` over `transport`.
    pub fn new(transport: T, endpoint: Url) -> Self {
        Self { transport, endpoint }
    }
}

impl<T: HttpGet> Handler for EmbedHandler<T> {
    fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        if method != "embed" {
            return Err(RpcError::new(
                codes::METHOD_NOT_FOUND,
                format!("unknown method: {method}"),
            ));
        }
        let p: EmbedParams = serde_json::from_value(params)
            .map_err(|e| RpcError::new(codes::INVALID_PARAMS, format!("params: {e}")))?;
        if p.input.len() > MAX_INPUTS {
            return Err(RpcError::new(
                codes::INVALID_PARAMS,
                format!("too many inputs: {} > {}", p.input.len(), MAX_INPUTS),
            ));
        }
        let total: usize = p.input.iter().map(|s| s.len()).sum();
        if total > MAX_REQUEST_BYTES {
            return Err(RpcError::new(
                codes::INVALID_PARAMS,
                format!("request too large: {total} > {MAX_REQUEST_BYTES} bytes"),
            ));
        }
        let result = forward_embed(&self.transport, &self.endpoint, &p)?;
        serde_json::to_value(result)
            .map_err(|e| RpcError::new(codes::INTERNAL_ERROR, format!("result encode: {e}")))
    }
}

use std::os::unix::net::UnixStream;

/// Serve one accepted UDS connection: run the JSON-RPC loop until the peer
/// closes the socket (EOF). Reuses the transport-generic
/// [`kastellan_protocol::server::serve`] over the two cloned halves of the stream.
///
/// A client connects, sends one or more `embed` requests, and reads each
/// response; when it drops the socket the loop returns `Ok`.
pub fn serve_connection<T: HttpGet>(
    handler: &mut EmbedHandler<T>,
    stream: UnixStream,
) -> std::io::Result<()> {
    let mut reader = stream.try_clone()?;
    let mut writer = stream;
    kastellan_protocol::server::serve(handler, &mut reader, &mut writer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kastellan_worker_web_common::http::RawResponse;
    use kastellan_worker_web_common::testing::FakeGet;
    use url::Url;

    fn endpoint() -> Url { Url::parse("http://127.0.0.1:11434/v1/embeddings").unwrap() }

    fn ok_body(rows: &[(usize, &[f32])]) -> Vec<u8> {
        let data: Vec<String> = rows.iter().map(|(i, v)| {
            let nums: Vec<String> = v.iter().map(|x| x.to_string()).collect();
            format!(r#"{{"index":{i},"embedding":[{}]}}"#, nums.join(","))
        }).collect();
        format!(r#"{{"data":[{}]}}"#, data.join(",")).into_bytes()
    }

    fn resp(status: u16, body: Vec<u8>) -> RawResponse {
        RawResponse { status, location: None, content_type: "application/json".into(), body }
    }

    fn params(model: &str, input: &[&str]) -> EmbedParams {
        EmbedParams { model: model.into(), input: input.iter().map(|s| s.to_string()).collect() }
    }

    #[test]
    fn forward_returns_vectors_in_input_order() {
        let t = FakeGet::new(vec![resp(200, ok_body(&[(0, &[1.0, 2.0]), (1, &[3.0, 4.0])]))]);
        let out = forward_embed(&t, &endpoint(), &params("m", &["a", "b"])).unwrap();
        assert_eq!(out, EmbedResult { data: vec![
            EmbedData { index: 0, embedding: vec![1.0, 2.0] },
            EmbedData { index: 1, embedding: vec![3.0, 4.0] },
        ]});
    }

    #[test]
    fn forward_reorders_out_of_order_backend_rows() {
        // Backend returns index:1 first, index:0 second — result must be input-ordered.
        let t = FakeGet::new(vec![resp(200, ok_body(&[(1, &[3.0, 4.0]), (0, &[1.0, 2.0])]))]);
        let out = forward_embed(&t, &endpoint(), &params("m", &["a", "b"])).unwrap();
        assert_eq!(out.data[0].embedding, vec![1.0, 2.0]);
        assert_eq!(out.data[1].embedding, vec![3.0, 4.0]);
    }

    #[test]
    fn forward_count_mismatch_is_error() {
        let t = FakeGet::new(vec![resp(200, ok_body(&[(0, &[1.0])]))]); // 1 row for 2 inputs
        let err = forward_embed(&t, &endpoint(), &params("m", &["a", "b"])).unwrap_err();
        assert_eq!(err.code, kastellan_protocol::codes::OPERATION_FAILED);
    }

    #[test]
    fn forward_duplicate_index_is_error() {
        // Count matches (2 rows for 2 inputs) but both claim index 0 — the
        // contiguity check must reject rather than silently mispair.
        let t = FakeGet::new(vec![resp(200, ok_body(&[(0, &[1.0]), (0, &[2.0])]))]);
        let err = forward_embed(&t, &endpoint(), &params("m", &["a", "b"])).unwrap_err();
        assert_eq!(err.code, kastellan_protocol::codes::OPERATION_FAILED);
    }

    #[test]
    fn forward_gapped_index_is_error() {
        // Count matches (2 rows for 2 inputs) but indices are {0, 2} — position 1
        // is unfilled; reject rather than pair input[1] with the index-2 vector.
        let t = FakeGet::new(vec![resp(200, ok_body(&[(0, &[1.0]), (2, &[2.0])]))]);
        let err = forward_embed(&t, &endpoint(), &params("m", &["a", "b"])).unwrap_err();
        assert_eq!(err.code, kastellan_protocol::codes::OPERATION_FAILED);
    }

    #[test]
    fn forward_non_2xx_is_error() {
        let t = FakeGet::new(vec![resp(503, b"upstream down".to_vec())]);
        let err = forward_embed(&t, &endpoint(), &params("m", &["a"])).unwrap_err();
        assert_eq!(err.code, kastellan_protocol::codes::OPERATION_FAILED);
    }

    #[test]
    fn forward_transport_failure_is_error() {
        let t = FakeGet::new(vec![]); // empty queue -> post() errors "no more canned responses"
        let err = forward_embed(&t, &endpoint(), &params("m", &["a"])).unwrap_err();
        assert_eq!(err.code, kastellan_protocol::codes::OPERATION_FAILED);
    }

    #[test]
    fn forward_empty_input_makes_no_call() {
        let t = FakeGet::new(vec![]); // would error if called
        let out = forward_embed(&t, &endpoint(), &params("m", &[])).unwrap();
        assert!(out.data.is_empty());
    }

    #[test]
    fn embed_params_parse_from_json() {
        let v = serde_json::json!({ "model": "m", "input": ["a", "b"] });
        let p: EmbedParams = serde_json::from_value(v).unwrap();
        assert_eq!(p.model, "m");
        assert_eq!(p.input, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn embed_result_serializes_index_and_embedding() {
        let r = EmbedResult { data: vec![EmbedData { index: 0, embedding: vec![1.0, 2.0] }] };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v, serde_json::json!({ "data": [{ "index": 0, "embedding": [1.0, 2.0] }] }));
    }

    fn handler(responses: Vec<RawResponse>) -> EmbedHandler<FakeGet> {
        EmbedHandler::new(FakeGet::new(responses), endpoint())
    }

    #[test]
    fn call_embed_returns_result_value() {
        let mut h = handler(vec![resp(200, ok_body(&[(0, &[1.0, 2.0])]))]);
        let out = h.call("embed", serde_json::json!({ "model": "m", "input": ["a"] })).unwrap();
        assert_eq!(out, serde_json::json!({ "data": [{ "index": 0, "embedding": [1.0, 2.0] }] }));
    }

    #[test]
    fn call_unknown_method_is_method_not_found() {
        let mut h = handler(vec![]);
        let err = h.call("bogus", serde_json::json!({})).unwrap_err();
        assert_eq!(err.code, kastellan_protocol::codes::METHOD_NOT_FOUND);
    }

    #[test]
    fn call_bad_params_is_invalid_params() {
        let mut h = handler(vec![]);
        let err = h.call("embed", serde_json::json!({ "model": 5 })).unwrap_err();
        assert_eq!(err.code, kastellan_protocol::codes::INVALID_PARAMS);
    }

    #[test]
    fn call_too_many_inputs_rejected_before_backend() {
        // Empty response queue: if the cap did NOT fire first, forward_embed would
        // error OPERATION_FAILED. Asserting INVALID_PARAMS proves the cap fired
        // before any backend call.
        let big: Vec<String> = (0..(MAX_INPUTS + 1)).map(|i| i.to_string()).collect();
        let mut h = handler(vec![]);
        let err = h.call("embed", serde_json::json!({ "model": "m", "input": big })).unwrap_err();
        assert_eq!(err.code, kastellan_protocol::codes::INVALID_PARAMS);
    }

    #[test]
    fn call_oversized_request_rejected_before_backend() {
        let huge = "x".repeat(MAX_REQUEST_BYTES + 1);
        let mut h = handler(vec![]);
        let err = h.call("embed", serde_json::json!({ "model": "m", "input": [huge] })).unwrap_err();
        assert_eq!(err.code, kastellan_protocol::codes::INVALID_PARAMS);
    }

    use std::io::{BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};

    /// Drive the broker's serve loop over a real UDS with a fake backend, from a
    /// client that speaks the JSON-RPC `embed` protocol. Proves the on-wire path
    /// end to end (framing + dispatch + response), not just the in-process call.
    #[test]
    fn uds_round_trip_embeds_over_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("embed.sock");
        let listener = UnixListener::bind(&sock).unwrap();

        // Broker side: accept ONE connection, serve it with a fake backend.
        let server = std::thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            let mut h = EmbedHandler::new(
                FakeGet::new(vec![resp(200, ok_body(&[(0, &[7.0, 8.0])]))]),
                endpoint(),
            );
            serve_connection(&mut h, conn).unwrap();
        });

        // Client side: send one embed request, read the response.
        let mut client = UnixStream::connect(&sock).unwrap();
        let req = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "embed",
            "params": { "model": "m", "input": ["hello"] }
        });
        let mut line = serde_json::to_vec(&req).unwrap();
        line.push(b'\n');
        client.write_all(&line).unwrap();
        client.flush().unwrap();

        let mut br = BufReader::new(&client);
        let rec = kastellan_protocol::read_capped_record(&mut br, kastellan_protocol::MAX_RECORD_BYTES).unwrap();
        let buf = match rec {
            kastellan_protocol::Record::Line(b) => b,
            other => panic!("expected a response line, got {other:?}"),
        };
        let resp: kastellan_protocol::Response = serde_json::from_slice(&buf).unwrap();
        assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
        assert_eq!(
            resp.result.unwrap(),
            serde_json::json!({ "data": [{ "index": 0, "embedding": [7.0, 8.0] }] })
        );

        drop(client); // let the serve loop see EOF and return
        server.join().unwrap();
    }
}
