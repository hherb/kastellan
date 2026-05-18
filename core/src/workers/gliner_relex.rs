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

#[cfg(test)]
mod tests {
    #[test]
    fn placeholder_compiles() {
        // Replaced in Task 2.2 by the wire-shape tests.
        assert!(true);
    }
}
