//! GLiNER-Relex worker manifest + wire-shape types.
//!
//! See `docs/superpowers/specs/2026-05-18-gliner-relex-worker-design.md`
//! for the design, and the Slice 2 section of
//! `docs/superpowers/plans/2026-05-18-gliner-relex-worker.md` for the
//! task-level breakdown this module implements.
//!
//! What this module owns:
//!
//! - [`GlinerRelexEnv`] — daemon-startup builder; carries the resolved
//!   weights/venv paths + model id + device selector.
//! - [`gliner_relex_entry`] — produces the [`crate::scheduler::ToolEntry`]
//!   that the dispatcher's [`crate::scheduler::ToolRegistry`] holds.
//! - [`ExtractRequest`] / [`ExtractResponse`] / [`Entity`] /
//!   [`TripleEntity`] / [`Triple`] — serde shape types matching the
//!   Python worker's wire contract (see
//!   `workers/gliner-relex/src/hhagent_worker_gliner_relex/server.py`
//!   for the producing side + `workers/gliner-relex/README.md` for the
//!   field-by-field shape table).
//!
//! What this module deliberately does NOT own:
//!
//! - **A typed Rust client wrapping [`crate::tool_host::dispatch`]**.
//!   The dispatcher's `report_crash` chokepoint between `dispatch` and
//!   `map_dispatch_result` makes a standalone client either duplicate
//!   crash-classifier logic or couple to a lifecycle manager; the v2
//!   entity-extraction consumer slice will pick the right shape around
//!   its actual call site. See HANDOVER's design-spec section for the
//!   rationale.

use serde::{Deserialize, Serialize};

/// Maximum number of distinct entity labels per `extract` request.
///
/// Pinned to the matching `MAX_ENTITY_LABELS` constant on the Python
/// side at
/// `workers/gliner-relex/src/hhagent_worker_gliner_relex/server.py`.
/// Bumping either side requires bumping both: the Python validator
/// will reject inputs the Rust caller could otherwise generate.
pub const MAX_ENTITY_LABELS: usize = 64;

/// Maximum number of distinct relation labels per `extract` request.
/// Empty is valid and signals entity-only mode (no relations returned).
pub const MAX_RELATION_LABELS: usize = 64;

/// Maximum UTF-8 byte length of the `text` field.
pub const MAX_TEXT_BYTES: usize = 8192;

/// Wire shape of an `extract` request's `params`.
///
/// `threshold` and `max_entities` are optional on the wire (the Python
/// server applies defaults of 0.5 and 64). `relation_threshold` is
/// captured separately per spike correction #3 — the GLiNER-Relex
/// model is noisy at low thresholds and production callers should pass
/// ≥ 0.5 for relations to suppress dense candidate-triple noise from
/// overlapping entity subspans.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractRequest {
    pub text: String,
    pub entity_labels: Vec<String>,
    pub relation_labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relation_threshold: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_entities: Option<u32>,
}

/// Wire shape of an `extract` response's `result`.
///
/// `entities` carries top-level entity dicts (see [`Entity`]); `triples`
/// carries relations whose `head` and `tail` are *nested* entity refs
/// (see [`TripleEntity`]) — a deliberately different shape with `type`
/// instead of `label` and an `entity_idx` back-pointer, no nested
/// `score`. The smoke test on real `multi-v1.0` weights established
/// this naming (see `workers/gliner-relex/README.md` "Field-key naming
/// observed on real `multi-v1.0` output" for the table).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExtractResponse {
    pub entities: Vec<Entity>,
    pub triples: Vec<Triple>,
}

/// A top-level entity in [`ExtractResponse::entities`].
///
/// Distinct from [`TripleEntity`] because the upstream GLiNER-Relex
/// envelope uses different field names + a different field set for the
/// two positions: top-level entities carry `label` + `score`; nested
/// triple head/tail carry `type` + `entity_idx` (and no `score`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entity {
    pub text: String,
    pub label: String,
    pub start: u32,
    pub end: u32,
    pub score: f32,
}

/// A nested entity reference inside [`Triple::head`] / [`Triple::tail`].
///
/// Real `knowledgator/gliner-relex-multi-v1.0` output uses `type` (NOT
/// `label`) for the entity category and adds an `entity_idx`
/// back-pointer into the top-level [`ExtractResponse::entities`]
/// array. There is no per-position `score`; consumers wanting the
/// score look up `entities[entity_idx].score`. See
/// `workers/gliner-relex/README.md` "Field-key naming observed on
/// real `multi-v1.0` output" for the empirical confirmation (smoke
/// test 2026-05-18, fixed in `1c36f56`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TripleEntity {
    pub text: String,
    /// The entity type. Named `type` on the wire (matching upstream)
    /// but Rust requires the `r#` raw-identifier prefix for the
    /// keyword. Serde's `rename` keeps the wire side clean.
    #[serde(rename = "type")]
    pub r#type: String,
    pub start: u32,
    pub end: u32,
    /// Index back into the top-level [`ExtractResponse::entities`]
    /// array. Stable for a single response only.
    pub entity_idx: u32,
}

/// A relation triple in [`ExtractResponse::triples`].
///
/// Field names match upstream's [GLiNER-Relex inference envelope][gr]:
/// `head` and `tail` (NOT `subject` / `object`) carry full nested
/// entity dicts via [`TripleEntity`]; `relation` is the predicate
/// label; `score` is the model's confidence. See spike correction #2
/// at `docs/superpowers/specs/2026-05-18-gliner-relex-spike-notes.md`.
///
/// [gr]: https://github.com/urchade/GLiNER
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Triple {
    pub head: TripleEntity,
    pub tail: TripleEntity,
    pub relation: String,
    pub score: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_request_serialises_with_expected_keys() {
        let req = ExtractRequest {
            text: "Smith treats asthma.".to_string(),
            entity_labels: vec!["person".to_string(), "disease".to_string()],
            relation_labels: vec!["treats".to_string()],
            threshold: Some(0.5),
            relation_threshold: Some(0.5),
            max_entities: Some(64),
        };
        let v = serde_json::to_value(&req).unwrap();
        let obj = v.as_object().unwrap();
        let keys: std::collections::BTreeSet<&str> =
            obj.keys().map(|s| s.as_str()).collect();
        assert_eq!(
            keys,
            std::collections::BTreeSet::from([
                "text",
                "entity_labels",
                "relation_labels",
                "threshold",
                "relation_threshold",
                "max_entities",
            ]),
        );
    }

    #[test]
    fn extract_request_omits_optional_fields_when_none() {
        let req = ExtractRequest {
            text: "x".to_string(),
            entity_labels: vec!["x".to_string()],
            relation_labels: vec![],
            threshold: None,
            relation_threshold: None,
            max_entities: None,
        };
        let v = serde_json::to_value(&req).unwrap();
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("threshold"));
        assert!(!obj.contains_key("relation_threshold"));
        assert!(!obj.contains_key("max_entities"));
    }

    #[test]
    fn extract_response_round_trips_real_wire_shape() {
        // Sampled from the operator smoke test of 2026-05-18 against
        // real `knowledgator/gliner-relex-multi-v1.0` weights — the
        // shape that landed the install.sh + README fix (commit
        // `1c36f56`). Nested head/tail use `type` (not `label`) +
        // `entity_idx`; no nested `score`.
        let canned = serde_json::json!({
            "entities": [
                {"text": "Dr Smith", "label": "person",  "start": 0,  "end": 8,  "score": 0.999},
                {"text": "asthma",   "label": "disease", "start": 16, "end": 22, "score": 0.999}
            ],
            "triples":  [
                {
                    "head":     {"text": "Dr Smith", "type": "person",  "start": 0,  "end": 8,  "entity_idx": 0},
                    "tail":     {"text": "asthma",   "type": "disease", "start": 16, "end": 22, "entity_idx": 1},
                    "relation": "treats",
                    "score":    0.995
                }
            ],
        });
        let resp: ExtractResponse =
            serde_json::from_value(canned.clone()).expect("decode real wire shape");
        assert_eq!(resp.entities.len(), 2);
        assert_eq!(resp.entities[0].text, "Dr Smith");
        assert_eq!(resp.entities[0].label, "person");
        assert_eq!(resp.triples[0].head.text, "Dr Smith");
        // CRITICAL: nested head/tail use `type`, not `label`. If a
        // future refactor renames `TripleEntity::r#type` to `label`,
        // this assertion would still compile but the from_value above
        // would fail to decode.
        assert_eq!(resp.triples[0].head.r#type, "person");
        assert_eq!(resp.triples[0].head.entity_idx, 0);
        assert_eq!(resp.triples[0].relation, "treats");
        // Round-trip back through Rust types is shape-identical
        // (`PartialEq` on the structs). We don't compare against the
        // raw `canned` Value: f32 → JSON Number → f32 widens through
        // the json::Number f64 carrier (`0.999_f32` round-trips as
        // `0.9990000128746033`), which is a serde_json artifact, not
        // a real shape drift. The decode-then-decode equality below
        // catches every field-rename or field-add bug we care about.
        let re_serialised = serde_json::to_value(&resp).unwrap();
        let resp_again: ExtractResponse = serde_json::from_value(re_serialised).unwrap();
        assert_eq!(resp, resp_again);
    }

    #[test]
    fn label_caps_match_python_side() {
        // Pinned at the values used by the Python validators (see
        // workers/gliner-relex/src/hhagent_worker_gliner_relex/server.py
        // MAX_TEXT_BYTES / MAX_ENTITY_LABELS / MAX_RELATION_LABELS).
        // A drift here would let the Rust caller generate inputs the
        // Python side immediately rejects with INVALID_INPUT.
        assert_eq!(MAX_ENTITY_LABELS, 64);
        assert_eq!(MAX_RELATION_LABELS, 64);
        assert_eq!(MAX_TEXT_BYTES, 8192);
    }
}
