-- 0006_agent_prompts.sql
--
-- Prompt-traceability ledger.
--
-- Source of truth for prompt CONTENT is git (`prompts/*.md`); this table
-- is a runtime ledger that records every prompt SHA-256 the daemon has
-- ever loaded. Every plan.formulate audit row carries the prompt name +
-- sha256 in its payload, so CASSANDRA's reviewer (when real impls land)
-- can correlate behavioural drift to specific prompt versions via this
-- table.
--
-- Append-only by GRANT, same shape as audit_log:
--   • SELECT, INSERT granted to kastellan_runtime
--   • UPDATE, DELETE never granted — old rows persist forever.

CREATE TABLE agent_prompts (
    sha256          CHAR(64) PRIMARY KEY,
    name            TEXT NOT NULL,
    content         TEXT NOT NULL,
    first_loaded_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX agent_prompts_name_idx
    ON agent_prompts (name, first_loaded_at DESC);

GRANT SELECT, INSERT ON agent_prompts TO kastellan_runtime;
-- Intentionally NO UPDATE, DELETE grants. Append-only by GRANT.
