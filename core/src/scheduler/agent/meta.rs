//! [`FormulationMeta`], moved verbatim out of `agent.rs` (over the 500-line cap).

/// Returned alongside the decoded `Plan`. The inner loop writes
/// these fields into the `plan.formulate` audit-log row payload.
#[derive(Clone, Debug)]
pub struct FormulationMeta {
    pub prompt_name: String,
    pub prompt_sha256: String,
    pub llm_model: String,
    pub llm_backend: String,
    pub latency_ms: u64,
    pub retry_count: u32,
    /// SHA-256 (hex) of the *assembled* system prompt the model
    /// actually saw — distinct from `prompt_sha256`, which is the
    /// base agent_planner.md hash only.
    pub assembled_prompt_sha256: String,
    /// Number of L0 rows the assembler folded in. Operator triage:
    /// 0 here on a clinical task means the L0 seeder didn't run.
    pub l0_count: usize,
    /// Number of L1 rows the assembler folded in. Stays 0 in
    /// production until an L1 promotion writer lands.
    pub l1_count: usize,
    /// Number of L3 skill rows surfaced into the `<skills>` block. Stays
    /// 0 in production until an operator approves a crystallised skill.
    pub skill_count: usize,
    /// Memory ids the recall lane surfaced for this iteration's
    /// instruction (RRF-fused order, capped at `L_RECALL_CAP_BYTES`).
    /// Empty when recall returned nothing or degraded due to error.
    /// Written verbatim to the `recalled_memory_ids` audit-row key.
    pub recalled_memory_ids: Vec<i64>,
    /// `recalled_memory_ids.len() as u32`. Redundant but cheap to
    /// query — observation-phase SQL avoids `jsonb_array_length` for
    /// the common "did recall fire at all?" question.
    pub recall_count: u32,
    /// Hex SHA-256 of the query text (the task instruction). Lets
    /// observation phase detect when paraphrased prompts produce the
    /// same recalled-id set vs. genuine drift.
    pub recall_query_sha256: String,
    /// Slice F (entity-extraction v2, 2026-05-19): the entity ids the
    /// gliner-relex extractor (or NoOp) resolved for this query.
    /// Empty when extraction degraded or no entities matched.
    pub graph_seed_entity_ids: Vec<i64>,
    /// `graph_seed_entity_ids.len() as u32`. Cheap-to-query duplicate
    /// for observation-phase SQL.
    pub graph_seed_count: u32,
    /// Which extraction path produced the seeds. v2 production is
    /// always `SeedSource::GlinerRelex` or `SeedSource::None`.
    pub graph_seed_source: crate::entity_extraction::SeedSource,
    /// #774: what the completion cost — prompt, generated and reasoning
    /// tokens, reasoning length, finish reason — as the backend reported it.
    /// Written to the audit row's `llm_usage` key, so "why did planning get
    /// slow?" is a query rather than a benchmark on the live host.
    pub usage: kastellan_llm_router::CompletionStats,
}
