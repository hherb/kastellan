//! GLiNER-Relex worker: manifest, wire-shape types, env resolution,
//! `ToolEntry` construction, and a typed dispatch client.
//!
//! See `docs/superpowers/specs/2026-05-18-gliner-relex-worker-design.md`
//! for the design, and the Slice 2 section of
//! `docs/superpowers/plans/2026-05-18-gliner-relex-worker.md` for the
//! task-level breakdown this module implements.
//!
//! ## Module layout
//!
//! This module is a thin facade; the implementation lives in cohesive
//! siblings, all re-exported here so existing `crate::workers::gliner_relex::*`
//! paths stay stable:
//!
//! - [`wire`] — serde shape types matching the Python worker's wire
//!   contract: [`ExtractRequest`] / [`ExtractResponse`] / [`Entity`] /
//!   [`TripleEntity`] / [`Triple`] plus the `MAX_*` request-size limits
//!   (see `workers/gliner-relex/src/kastellan_worker_gliner_relex/server.py`
//!   for the producing side + `workers/gliner-relex/README.md` for the
//!   field-by-field shape table).
//! - [`resolve`] — [`GlinerRelexEnv`] (resolved weights/venv/model/device
//!   config) + the pure [`resolve_env`] resolver + its
//!   [`ResolveSkipReason`] variants.
//! - [`entry`] — [`gliner_relex_entry`]: builds the
//!   [`crate::scheduler::ToolEntry`] the dispatcher's
//!   [`crate::scheduler::ToolRegistry`] holds (host-mode / container-mode
//!   branch + shared env + lifecycle helpers).
//! - [`client`] — [`Client`]: typed wrapper over [`crate::tool_host::dispatch`]
//!   for the `extract` method, plus [`ClientError`]. Added in the v2
//!   entity-extraction consumer slice — the first non-dispatcher caller.
//! - [`manifest`] — [`GlinerRelexManifest`]: the uniform
//!   [`WorkerManifest`](crate::worker_manifest::WorkerManifest) the daemon
//!   registry iterates over.

mod client;
mod entry;
mod manifest;
mod resolve;
mod wire;

pub use client::{Client, ClientError};
pub use entry::gliner_relex_entry;
pub use manifest::GlinerRelexManifest;
pub use resolve::{resolve_env, GlinerRelexEnv, ResolveSkipReason};
pub use wire::{
    Entity, ExtractRequest, ExtractResponse, Triple, TripleEntity, MAX_ENTITY_LABELS,
    MAX_RELATION_LABELS, MAX_TEXT_BYTES,
};

#[cfg(test)]
mod tests;
