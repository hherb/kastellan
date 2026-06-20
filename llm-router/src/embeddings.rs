//! OpenAI-compatible embedding request and response types.
//!
//! Wire shapes for `POST <base>/embeddings`. The endpoint contract is
//! designed to be compatible with vLLM, SGLang, Ollama's OpenAI-compat
//! front door, and `text-embeddings-inference` / Infinity — each of
//! those backends documents acceptance of the array form of `input`
//! even for a single string. We pin `Vec<String>` rather than a
//! string-or-list enum on that basis. The integration tests in
//! Task 5 (`llm-router/tests/embedding_backend_e2e.rs`) validate the
//! round-trip against canned envelopes; live-backend conformance is
//! the operator's responsibility per their `KASTELLAN_LLM_EMBEDDING_URL`.
//!
//! ## Why we omit `encoding_format` and `dimensions`
//! OpenAI's spec carries optional `encoding_format` (`"float"` or
//! `"base64"`) and `dimensions` (server-side Matryoshka-truncation
//! target). We want float arrays and request the model's native dim,
//! then Matryoshka-truncate to the storage contract *client-side*
//! (`db::memories::truncate_to_embedding_dim`) rather than rely on
//! every backend honouring `dimensions` — so we serialise neither.
//! Adding them later is a backwards-compatible additive change
//! (existing backends already treat them as optional).

use serde::{Deserialize, Serialize};

use crate::messages::Usage;

/// Outgoing embedding request.
///
/// `input` is always a JSON array. Callers with a single text wrap it
/// in `vec![text.into()]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingRequest {
    pub model: String,
    pub input: Vec<String>,
}

impl EmbeddingRequest {
    /// Common-case constructor for a single text.
    ///
    /// Prefer this over constructing `EmbeddingRequest { model, input: vec![...] }`
    /// directly — it makes explicit that the wire format is always an array, even
    /// for a single string (see module-level doc for why).
    pub fn single(model: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            input: vec![text.into()],
        }
    }
}

/// One embedding entry in the response `data` array.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingData {
    /// 0-based position matching the corresponding `input[i]`.
    /// `serde(default)` because some backends omit it for single-input
    /// requests.
    #[serde(default)]
    pub index: u32,
    /// Raw float vector at the model's native dimension
    /// (embeddinggemma: 768). The caller
    /// (`core::memory::embed_query`) Matryoshka-truncates this to
    /// `db::memories::EMBEDDING_DIM` (256) after decode — we don't pin
    /// a length here because embedding models run at different native
    /// dims and the router stays model-agnostic.
    pub embedding: Vec<f32>,
}

/// Decoded `200 OK` response from `POST /embeddings`.
///
/// `model` and `usage` are `Option` because Ollama omits them when
/// the underlying GGUF runtime didn't surface them; vLLM always
/// emits both.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingResponse {
    pub data: Vec<EmbeddingData>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn embedding_request_serializes_canonical_shape() {
        let req = EmbeddingRequest::single("bge-m3", "hello world");
        let s = serde_json::to_string(&req).unwrap();
        // Wire shape pin: exactly `model` + `input` fields. No
        // `encoding_format`, no `dimensions`, no nulls.
        assert!(s.contains("\"model\":\"bge-m3\""), "model missing: {s}");
        assert!(s.contains("\"input\":[\"hello world\"]"), "input missing: {s}");
        assert!(!s.contains("encoding_format"), "encoding_format leaked: {s}");
        assert!(!s.contains("dimensions"), "dimensions leaked: {s}");
    }

    #[test]
    fn embedding_request_input_is_array_even_for_single_string() {
        // Defensive: serde_json could theoretically be configured to
        // unwrap single-element Vec into a bare value. Pin the JSON
        // array form because not every backend handles the
        // OpenAI-spec "string-or-list" union when given a bare string.
        let req = EmbeddingRequest::single("m", "x");
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"input\":[\"x\"]"), "input not array: {s}");
    }

    #[test]
    fn embedding_response_decodes_canonical_vllm_envelope() {
        let raw = json!({
            "object": "list",
            "data": [
                {"object": "embedding", "index": 0, "embedding": [0.1, 0.2, 0.3]}
            ],
            "model": "BAAI/bge-m3",
            "usage": {"prompt_tokens": 4, "total_tokens": 4}
        });
        let resp: EmbeddingResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.data.len(), 1);
        assert_eq!(resp.data[0].index, 0);
        assert_eq!(resp.data[0].embedding, vec![0.1, 0.2, 0.3]);
        assert_eq!(resp.model.as_deref(), Some("BAAI/bge-m3"));
        let u = resp.usage.unwrap();
        assert_eq!(u.prompt_tokens, Some(4));
    }

    #[test]
    fn embedding_response_decodes_minimal_envelope_without_model_or_usage() {
        // Some backends omit `model` and `usage` entirely. Decoder
        // must tolerate (serde(default) + skip_serializing_if).
        let raw = json!({
            "data": [{"embedding": [0.0, 1.0]}]
        });
        let resp: EmbeddingResponse = serde_json::from_value(raw).unwrap();
        assert!(resp.model.is_none());
        assert!(resp.usage.is_none());
        assert_eq!(resp.data[0].index, 0); // default
    }

    #[test]
    fn embedding_response_decodes_batch_envelope_preserving_order() {
        // Multi-text request: data[].index matches input position AND
        // the per-element embedding is preserved at the right index
        // (one swapped pair would silently regress otherwise).
        let raw = json!({
            "data": [
                {"index": 0, "embedding": [0.1]},
                {"index": 1, "embedding": [0.2]},
                {"index": 2, "embedding": [0.3]}
            ]
        });
        let resp: EmbeddingResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.data.len(), 3);
        assert_eq!(resp.data[0].index, 0);
        assert_eq!(resp.data[0].embedding, vec![0.1_f32]);
        assert_eq!(resp.data[1].index, 1);
        assert_eq!(resp.data[1].embedding, vec![0.2_f32]);
        assert_eq!(resp.data[2].index, 2);
        assert_eq!(resp.data[2].embedding, vec![0.3_f32]);
    }

    #[test]
    fn embedding_data_index_defaults_to_zero_when_absent() {
        let raw = json!({"embedding": [1.0, 2.0]});
        let d: EmbeddingData = serde_json::from_value(raw).unwrap();
        assert_eq!(d.index, 0);
        assert_eq!(d.embedding, vec![1.0, 2.0]);
    }
}
