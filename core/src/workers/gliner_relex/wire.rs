//! Wire-shape types for the gliner-relex worker's `extract` method.
//!
//! These serde structs match, field-for-field, the JSON the Python
//! worker produces/consumes (see
//! `workers/gliner-relex/src/kastellan_worker_gliner_relex/server.py`
//! for the producing side + `workers/gliner-relex/README.md` for the
//! field-by-field shape table). They carry no behaviour beyond
//! (de)serialisation, so they live in their own leaf module with the
//! smallest possible dependency surface.

use serde::{Deserialize, Serialize};

/// Maximum number of distinct entity labels per `extract` request.
///
/// Pinned to the matching `MAX_ENTITY_LABELS` constant on the Python
/// side at
/// `workers/gliner-relex/src/kastellan_worker_gliner_relex/server.py`.
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
