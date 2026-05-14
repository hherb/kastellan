-- 0011_agent_prompts_composite_pk.sql
--
-- Schema-v2 bump for `agent_prompts`: change PK from `(sha256)` to
-- `(sha256, name)`.
--
-- Closes issue #20. Background:
--
-- Migration 0006 keyed `agent_prompts` on `sha256` alone. That deduped
-- byte-identical prompt content across files, which sounds right but
-- corrupts CASSANDRA's forensic correlation:
--
--   * A rename `prompts/agent_planner.md` → `prompts/planner.md` (content
--     unchanged) keeps the original `name` forever in the ledger.
--   * Two files with identical content (different names) collapse to one
--     row keyed by the first `name` loaded.
--   * Every `plan.formulate` audit-log row carries `(prompt_name,
--     prompt_sha256)`. A future reviewer joining on `(name, sha256)`
--     against `agent_prompts` finds the row exists by sha256 but the
--     `name` column may not match — silent join-failure, or a rename
--     that LOOKS like behavioural drift.
--
-- The composite PK `(sha256, name)` preserves a separate row per
-- (content, name) tuple. Renames keep both the old and new name's
-- correlation history.
--
-- Migration shape: drop the old PK, add the new one. Non-destructive —
-- existing rows keep their data; the new PK is satisfied immediately
-- because pre-migration there's at most one row per `sha256` (so
-- `(sha256, name)` is also unique). New inserts of the same content
-- under a different name now succeed instead of being deduped to the
-- first-seen row.
--
-- Locking: `ALTER TABLE … DROP CONSTRAINT` + `ADD PRIMARY KEY` take an
-- ACCESS EXCLUSIVE lock for the duration of the rewrite. Acceptable
-- here because `agent_prompts` is a startup-time-only writer (the
-- daemon upserts every prompt at bring-up and never again at runtime).

ALTER TABLE agent_prompts
    DROP CONSTRAINT agent_prompts_pkey;

ALTER TABLE agent_prompts
    ADD CONSTRAINT agent_prompts_pkey PRIMARY KEY (sha256, name);

-- The `agent_prompts_name_idx` from 0006 stays — `(name, first_loaded_at DESC)`
-- still serves "give me every revision of this prompt name". No change.
