//! Embed passage/query text into vectors via an embedding-only endpoint.
//!
//! [`Embedder`] is the single network-touching seam (faked in tests). The real
//! [`HttpEmbedder`] POSTs the OpenAI-compatible `{model, input:[...]}` body to the
//! configured endpoint over the shared [`HttpGet`] transport (the same proxy path
//! content fetches use) and decodes `{data:[{index, embedding:[...]}]}`. Cosine
//! ranking is dimension-agnostic, so no Matryoshka truncation happens here — only
//! a count check (one vector per input). A per-vector length check is unnecessary:
//! the downstream `cosine` ranker skips any passage whose embedding length differs
//! from the query's, so a mixed-dimension response degrades gracefully there.

use std::io::{BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use serde::Deserialize;
use url::Url;

use kastellan_worker_web_common::embed_rows::{reorder_embeddings, ReorderError};
use kastellan_worker_web_common::http::HttpGet;

/// Turn texts into embedding vectors. Batches all inputs into one request.
pub trait Embedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError>;
}

/// Which embedder `from_env` should build, decided purely from two env values.
/// Kept separate from the (I/O-bound) construction so the precedence rule is
/// unit-testable without touching env or sockets.
#[derive(Debug, PartialEq)]
pub enum EmbedderChoice<'a> {
    /// No embedder configured → lexical-only ranking.
    None,
    /// Use the broker sidecar at this UDS path (takes precedence).
    Broker { uds: &'a str },
    /// Use a direct embedding endpoint (validated + built by the caller).
    Endpoint { endpoint: &'a str },
}

/// Pick the embedder source. The broker UDS wins over a direct endpoint when both
/// are set; blank/whitespace values count as unset.
pub fn choose_embedder<'a>(
    broker_uds: Option<&'a str>,
    embed_endpoint: Option<&'a str>,
) -> EmbedderChoice<'a> {
    let broker = broker_uds.map(str::trim).filter(|s| !s.is_empty());
    let endpoint = embed_endpoint.map(str::trim).filter(|s| !s.is_empty());
    match (broker, endpoint) {
        (Some(uds), _) => EmbedderChoice::Broker { uds },
        (None, Some(endpoint)) => EmbedderChoice::Endpoint { endpoint },
        (None, None) => EmbedderChoice::None,
    }
}

/// Why an embedding call failed.
#[derive(Debug)]
pub enum EmbedError {
    Transport(String),
    Status(u16),
    Decode(String),
    CountMismatch { requested: usize, returned: usize },
    /// After sorting by `index`, the rows were not contiguous (a duplicate or
    /// gapped index) — the batch could not be safely paired with its inputs.
    NonContiguous { row: usize, index: usize },
    /// The broker returned a JSON-RPC error. Carries the code + message so a
    /// client-class error (e.g. `INVALID_PARAMS` from exceeding the caps) is not
    /// mislabelled as a transport failure.
    Broker { code: i32, message: String },
}

impl std::fmt::Display for EmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EmbedError::Transport(m) => write!(f, "transport: {m}"),
            EmbedError::Status(s) => write!(f, "endpoint status {s}"),
            EmbedError::Decode(m) => write!(f, "decode: {m}"),
            EmbedError::CountMismatch { requested, returned } =>
                write!(f, "vector count mismatch: requested {requested}, returned {returned}"),
            EmbedError::NonContiguous { row, index } =>
                write!(f, "non-contiguous embedding indices (row {row} has index {index})"),
            EmbedError::Broker { code, message } =>
                write!(f, "broker error {code}: {message}"),
        }
    }
}

impl From<ReorderError> for EmbedError {
    fn from(e: ReorderError) -> Self {
        match e {
            ReorderError::CountMismatch { requested, returned } =>
                EmbedError::CountMismatch { requested, returned },
            ReorderError::NonContiguous { row, index } =>
                EmbedError::NonContiguous { row, index },
        }
    }
}

#[derive(serde::Serialize)]
struct EmbeddingRequest<'a> {
    model: &'a str,
    input: &'a [String],
}

#[derive(Deserialize)]
struct EmbeddingData {
    #[serde(default)]
    index: usize,
    embedding: Vec<f32>,
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
}

/// Real embedder over the shared transport.
pub struct HttpEmbedder<T: HttpGet> {
    transport: T,
    endpoint: Url,
    model: String,
}

impl<T: HttpGet> HttpEmbedder<T> {
    pub fn new(transport: T, endpoint: Url, model: String) -> Self {
        Self { transport, endpoint, model }
    }
}

impl<T: HttpGet> Embedder for HttpEmbedder<T> {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let req = EmbeddingRequest { model: &self.model, input: texts };
        let body = serde_json::to_vec(&req)
            .map_err(|e| EmbedError::Decode(format!("request encode: {e}")))?;
        let resp = self
            .transport
            .post(&self.endpoint, "application/json", &body)
            .map_err(EmbedError::Transport)?;
        if !(200..300).contains(&resp.status) {
            return Err(EmbedError::Status(resp.status));
        }
        let decoded: EmbeddingResponse = serde_json::from_slice(&resp.body)
            .map_err(|e| EmbedError::Decode(e.to_string()))?;
        // Reorder + count-check + contiguity via the shared helper, so this path
        // pairs each vector with `texts[i]` and fails closed on a duplicate/gapped
        // index exactly like the broker's trusted boundary.
        let rows = decoded.data.into_iter().map(|d| (d.index, d.embedding)).collect();
        reorder_embeddings(rows, texts.len()).map_err(EmbedError::from)
    }
}

/// The broker's `embed` result envelope (mirrors `kastellan-worker-embed-broker`
/// `EmbedResult`). Kept local so web-research does not depend on the broker crate.
#[derive(serde::Deserialize)]
struct BrokerEmbedRow {
    #[serde(default)]
    index: usize,
    embedding: Vec<f32>,
}

#[derive(serde::Deserialize)]
struct BrokerEmbedResult {
    data: Vec<BrokerEmbedRow>,
}

/// Embed via the trusted embedding-broker sidecar over a Unix socket.
///
/// Sends a JSON-RPC `embed{model,input}` request to the broker (whose UDS core
/// bind-mounts into this worker's jail) and decodes the returned vectors. The
/// worker needs no embed egress — the broker holds the only route to the backend.
/// Selected by [`WebResearchHandler::from_env`] when `KASTELLAN_EMBED_BROKER_UDS`
/// is set.
pub struct BrokeredEmbedder {
    uds: PathBuf,
    model: String,
}

impl BrokeredEmbedder {
    /// Build an embedder that talks to the broker at `uds`, requesting `model`.
    pub fn new(uds: PathBuf, model: String) -> Self {
        Self { uds, model }
    }
}

impl Embedder for BrokeredEmbedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut stream = UnixStream::connect(&self.uds)
            .map_err(|e| EmbedError::Transport(format!("connect broker {:?}: {e}", self.uds)))?;

        let req = kastellan_protocol::Request {
            jsonrpc: "2.0".into(),
            id: serde_json::json!(1),
            method: "embed".into(),
            params: serde_json::json!({ "model": self.model, "input": texts }),
        };
        let mut line = serde_json::to_vec(&req)
            .map_err(|e| EmbedError::Decode(format!("request encode: {e}")))?;
        line.push(b'\n');
        stream
            .write_all(&line)
            .map_err(|e| EmbedError::Transport(format!("write broker request: {e}")))?;
        stream.flush().ok();

        let mut br = BufReader::new(&stream);
        let buf = match kastellan_protocol::read_capped_record(&mut br, kastellan_protocol::MAX_RECORD_BYTES)
            .map_err(|e| EmbedError::Transport(format!("read broker response: {e}")))?
        {
            kastellan_protocol::Record::Line(b) => b,
            kastellan_protocol::Record::Eof => {
                return Err(EmbedError::Transport("broker closed without responding".into()))
            }
            kastellan_protocol::Record::TooLarge => {
                return Err(EmbedError::Decode("broker response exceeded record cap".into()))
            }
        };
        let resp: kastellan_protocol::Response = serde_json::from_slice(&buf)
            .map_err(|e| EmbedError::Decode(format!("broker response: {e}")))?;
        if let Some(err) = resp.error {
            // Carry the broker's JSON-RPC code + message rather than flattening a
            // client-class error (e.g. INVALID_PARAMS from exceeding the caps)
            // into a transport failure.
            return Err(EmbedError::Broker { code: err.code, message: err.message });
        }
        let result = resp
            .result
            .ok_or_else(|| EmbedError::Decode("broker response missing result".into()))?;
        let decoded: BrokerEmbedResult = serde_json::from_value(result)
            .map_err(|e| EmbedError::Decode(format!("result decode: {e}")))?;
        // Reconcile via the shared helper (reorder + count-check + contiguity).
        let rows = decoded.data.into_iter().map(|d| (d.index, d.embedding)).collect();
        reorder_embeddings(rows, texts.len()).map_err(EmbedError::from)
    }
}

/// Test embedder: canned vectors keyed by exact text, or a forced failure.
#[cfg(test)]
pub(crate) struct FakeEmbedder {
    /// text -> vector; any text absent yields a zero-length vector.
    pub map: std::collections::HashMap<String, Vec<f32>>,
    /// When true, every `embed` call returns an error (simulates a dead endpoint).
    pub fail: bool,
    /// Number of times `embed` was invoked (interior-mutable for assertions).
    pub calls: std::cell::Cell<usize>,
    /// Number of texts passed to the most recent `embed` call (lets a test assert
    /// how many passages a per-page embed actually requested — e.g. the cap).
    pub last_input_len: std::cell::Cell<usize>,
}

#[cfg(test)]
impl FakeEmbedder {
    pub fn new(pairs: &[(&str, Vec<f32>)]) -> Self {
        Self {
            map: pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
            fail: false,
            calls: std::cell::Cell::new(0),
            last_input_len: std::cell::Cell::new(0),
        }
    }
    pub fn failing() -> Self {
        Self {
            map: Default::default(),
            fail: true,
            calls: std::cell::Cell::new(0),
            last_input_len: std::cell::Cell::new(0),
        }
    }
}

#[cfg(test)]
impl Embedder for FakeEmbedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.calls.set(self.calls.get() + 1);
        self.last_input_len.set(texts.len());
        if self.fail {
            return Err(EmbedError::Transport("fake endpoint down".into()));
        }
        Ok(texts.iter().map(|t| self.map.get(t).cloned().unwrap_or_default()).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kastellan_worker_web_common::http::RawResponse;
    use kastellan_worker_web_common::testing::FakeGet;

    fn embed_json(vectors: &[&[f32]]) -> String {
        let data: Vec<String> = vectors.iter().enumerate().map(|(i, v)| {
            let nums: Vec<String> = v.iter().map(|x| x.to_string()).collect();
            format!(r#"{{"index":{i},"embedding":[{}]}}"#, nums.join(","))
        }).collect();
        format!(r#"{{"data":[{}]}}"#, data.join(","))
    }

    fn endpoint() -> Url { Url::parse("http://127.0.0.1:11434/v1/embeddings").unwrap() }

    #[test]
    fn decodes_envelope_in_input_order() {
        let body = embed_json(&[&[1.0, 2.0], &[3.0, 4.0]]);
        let t = FakeGet::new(vec![RawResponse {
            status: 200, location: None,
            content_type: "application/json".into(), body: body.into_bytes(),
        }]);
        let e = HttpEmbedder::new(t, endpoint(), "embeddinggemma".into());
        let out = e.embed(&["a".into(), "b".into()]).unwrap();
        assert_eq!(out, vec![vec![1.0, 2.0], vec![3.0, 4.0]]);
    }

    #[test]
    fn reorders_out_of_order_rows_back_to_input_order() {
        // A batched backend may return rows in a different order than requested.
        // Rows arrive index:1 first, index:0 second — the result MUST still pair
        // each vector with its input position (sort_by_key on `index`).
        let body = r#"{"data":[
            {"index":1,"embedding":[3.0,4.0]},
            {"index":0,"embedding":[1.0,2.0]}
        ]}"#;
        let t = FakeGet::new(vec![RawResponse {
            status: 200, location: None,
            content_type: "application/json".into(), body: body.as_bytes().to_vec(),
        }]);
        let e = HttpEmbedder::new(t, endpoint(), "m".into());
        let out = e.embed(&["a".into(), "b".into()]).unwrap();
        assert_eq!(out, vec![vec![1.0, 2.0], vec![3.0, 4.0]],
            "index:0 vector must come first regardless of wire order");
    }

    #[test]
    fn non_2xx_is_status_error() {
        let t = FakeGet::new(vec![RawResponse {
            status: 503, location: None, content_type: "text/plain".into(), body: Vec::new(),
        }]);
        let e = HttpEmbedder::new(t, endpoint(), "m".into());
        assert!(matches!(e.embed(&["a".into()]), Err(EmbedError::Status(503))));
    }

    #[test]
    fn undecodable_body_is_decode_error() {
        let t = FakeGet::new(vec![RawResponse {
            status: 200, location: None, content_type: "application/json".into(),
            body: b"not json".to_vec(),
        }]);
        let e = HttpEmbedder::new(t, endpoint(), "m".into());
        assert!(matches!(e.embed(&["a".into()]), Err(EmbedError::Decode(_))));
    }

    #[test]
    fn count_mismatch_is_error() {
        let body = embed_json(&[&[1.0]]); // 1 vector for 2 inputs
        let t = FakeGet::new(vec![RawResponse {
            status: 200, location: None, content_type: "application/json".into(),
            body: body.into_bytes(),
        }]);
        let e = HttpEmbedder::new(t, endpoint(), "m".into());
        assert!(matches!(
            e.embed(&["a".into(), "b".into()]),
            Err(EmbedError::CountMismatch { requested: 2, returned: 1 })
        ));
    }

    // Exercises FakeEmbedder here so it is not dead code before Task 4 wires it
    // into research()/handler() tests.
    #[test]
    fn fake_embedder_returns_canned_and_counts_and_fails() {
        let e = FakeEmbedder::new(&[("a", vec![1.0_f32, 0.0])]);
        assert_eq!(e.embed(&["a".into(), "b".into()]).unwrap(),
                   vec![vec![1.0, 0.0], vec![]]); // absent text -> empty vec
        assert_eq!(e.calls.get(), 1);
        assert!(FakeEmbedder::failing().embed(&["x".into()]).is_err());
    }

    // The one-shot stub broker (reads one request line, writes a response) is
    // shared with the search-provider tests — it is generic over the JSON body.
    use kastellan_worker_web_common::testing::stub_broker;

    #[test]
    fn brokered_embedder_round_trip_returns_vectors() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("embed.sock");
        // Single line: the JSON-RPC framing is line-delimited (`read_capped_record`
        // reads to the first `\n`), so the response must not contain embedded newlines.
        let h = stub_broker(
            sock.clone(),
            r#"{"jsonrpc":"2.0","id":1,"result":{"data":[{"index":1,"embedding":[3.0,4.0]},{"index":0,"embedding":[1.0,2.0]}]}}"#.to_string(),
        );
        let e = BrokeredEmbedder::new(sock, "m".into());
        let out = e.embed(&["a".into(), "b".into()]).unwrap();
        // Reordered by index back to input order.
        assert_eq!(out, vec![vec![1.0, 2.0], vec![3.0, 4.0]]);
        h.join().unwrap();
    }

    #[test]
    fn brokered_embedder_maps_broker_error() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("embed.sock");
        let h = stub_broker(
            sock.clone(),
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32002,"message":"backend down"}}"#.to_string(),
        );
        let e = BrokeredEmbedder::new(sock, "m".into());
        let err = e.embed(&["a".into()]).unwrap_err();
        // A broker JSON-RPC error keeps its code — not mislabelled as Transport.
        assert!(matches!(err, EmbedError::Broker { code: -32002, .. }), "got {err:?}");
        h.join().unwrap();
    }

    #[test]
    fn brokered_embedder_rejects_non_contiguous_rows() {
        // Two result rows, both index 0: count matches but the shared contiguity
        // guard must reject (the client path now enforces it too, matching the
        // broker's trusted boundary).
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("embed.sock");
        let h = stub_broker(
            sock.clone(),
            r#"{"jsonrpc":"2.0","id":1,"result":{"data":[{"index":0,"embedding":[1.0]},{"index":0,"embedding":[2.0]}]}}"#.to_string(),
        );
        let e = BrokeredEmbedder::new(sock, "m".into());
        let err = e.embed(&["a".into(), "b".into()]).unwrap_err();
        assert!(matches!(err, EmbedError::NonContiguous { .. }), "got {err:?}");
        h.join().unwrap();
    }

    #[test]
    fn http_embedder_rejects_non_contiguous_rows() {
        // Two rows, both index 0 (a duplicate): count matches but contiguity fails.
        let body = r#"{"data":[{"index":0,"embedding":[1.0]},{"index":0,"embedding":[2.0]}]}"#;
        let t = FakeGet::new(vec![RawResponse {
            status: 200, location: None, content_type: "application/json".into(),
            body: body.as_bytes().to_vec(),
        }]);
        let e = HttpEmbedder::new(t, endpoint(), "m".into());
        assert!(matches!(
            e.embed(&["a".into(), "b".into()]),
            Err(EmbedError::NonContiguous { .. })
        ));
    }

    #[test]
    fn brokered_embedder_absent_socket_is_transport_error() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("nope.sock"); // never bound
        let e = BrokeredEmbedder::new(sock, "m".into());
        let err = e.embed(&["a".into()]).unwrap_err();
        assert!(matches!(err, EmbedError::Transport(_)), "got {err:?}");
    }

    #[test]
    fn brokered_embedder_empty_input_makes_no_connection() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("nope.sock"); // never bound; must not be dialed
        let e = BrokeredEmbedder::new(sock, "m".into());
        assert!(e.embed(&[]).unwrap().is_empty());
    }

    #[test]
    fn choose_broker_wins_when_both_set() {
        match choose_embedder(Some("/run/embed.sock"), Some("http://x/embed")) {
            EmbedderChoice::Broker { uds } => assert_eq!(uds, "/run/embed.sock"),
            other => panic!("expected Broker, got {other:?}"),
        }
    }

    #[test]
    fn choose_endpoint_when_only_endpoint_set() {
        match choose_embedder(None, Some("http://x/embed")) {
            EmbedderChoice::Endpoint { endpoint } => assert_eq!(endpoint, "http://x/embed"),
            other => panic!("expected Endpoint, got {other:?}"),
        }
    }

    #[test]
    fn choose_none_when_neither_set() {
        assert!(matches!(choose_embedder(None, None), EmbedderChoice::None));
    }

    #[test]
    fn choose_treats_blank_as_unset() {
        assert!(matches!(choose_embedder(Some("  "), Some("  ")), EmbedderChoice::None));
        match choose_embedder(Some("   "), Some("http://x/embed")) {
            EmbedderChoice::Endpoint { .. } => {}
            other => panic!("blank broker uds must fall through to endpoint, got {other:?}"),
        }
    }
}
