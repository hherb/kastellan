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

#[cfg(test)]
mod tests {
    use super::*;

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
}
