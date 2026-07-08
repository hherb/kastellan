//! Embed passage/query text into vectors via an embedding-only endpoint.
//!
//! [`Embedder`] is the single network-touching seam (faked in tests). The real
//! [`HttpEmbedder`] POSTs the OpenAI-compatible `{model, input:[...]}` body to the
//! configured endpoint over the shared [`HttpGet`] transport (the same proxy path
//! content fetches use) and decodes `{data:[{index, embedding:[...]}]}`. Cosine
//! ranking is dimension-agnostic, so no Matryoshka truncation happens here — only
//! a count check (one vector per input) and a shared-length check.
//!
//! TRANSIENT: these items are exercised by this module's `#[cfg(test)]` tests but
//! not yet by production code (`research()`/`handler` wire them in Tasks 4–5). The
//! module-scoped `allow(dead_code)` below is removed once they are wired — the
//! plan's transient-allow hard bar requires zero such allows after Task 5.
#![allow(dead_code)]

use serde::Deserialize;
use url::Url;

use kastellan_worker_web_common::http::HttpGet;

/// Turn texts into embedding vectors. Batches all inputs into one request.
pub trait Embedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError>;
}

/// Why an embedding call failed.
#[derive(Debug)]
pub enum EmbedError {
    Transport(String),
    Status(u16),
    Decode(String),
    CountMismatch { requested: usize, returned: usize },
}

impl std::fmt::Display for EmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EmbedError::Transport(m) => write!(f, "transport: {m}"),
            EmbedError::Status(s) => write!(f, "endpoint status {s}"),
            EmbedError::Decode(m) => write!(f, "decode: {m}"),
            EmbedError::CountMismatch { requested, returned } =>
                write!(f, "vector count mismatch: requested {requested}, returned {returned}"),
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
        if decoded.data.len() != texts.len() {
            return Err(EmbedError::CountMismatch {
                requested: texts.len(),
                returned: decoded.data.len(),
            });
        }
        // Order by `index` so the result pairs with `texts[i]` even if the
        // backend returns rows out of order.
        let mut rows = decoded.data;
        rows.sort_by_key(|d| d.index);
        Ok(rows.into_iter().map(|d| d.embedding).collect())
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
}

#[cfg(test)]
impl FakeEmbedder {
    pub fn new(pairs: &[(&str, Vec<f32>)]) -> Self {
        Self {
            map: pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(),
            fail: false,
            calls: std::cell::Cell::new(0),
        }
    }
    pub fn failing() -> Self {
        Self { map: Default::default(), fail: true, calls: std::cell::Cell::new(0) }
    }
}

#[cfg(test)]
impl Embedder for FakeEmbedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.calls.set(self.calls.get() + 1);
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
}
