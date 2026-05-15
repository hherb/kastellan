-- Phase 1 — propagate the `memories.layer` tag into the
-- `deleted_memories` audit table (migration 0008).
--
-- Without this column, post-deletion forensics cannot reconstruct
-- whether a deleted row was a load-bearing L1 routing pointer or a
-- routine L2 accumulated fact. Adding it now (before any caller deletes
-- L1 rows) keeps the audit trail honest by construction.
--
-- `CREATE OR REPLACE FUNCTION` swaps in the expanded trigger body in
-- place; PG looks up trigger functions by name at execution time, so
-- the existing `memories_after_delete_audit` binding (from 0008) picks
-- up the new body automatically. No `DROP TRIGGER` needed.
--
-- DEFAULT 2 mirrors the source `memories` column — same defensible
-- default applied to pre-existing deleted_memories rows (none today in
-- dev; production rollout runs this before any production deletion
-- path is wired). The CHECK constraint matches the source column.
--
-- GRANT shape on `deleted_memories` unchanged (SELECT + INSERT only;
-- UPDATE/DELETE/TRUNCATE remain revoked from 0008).

ALTER TABLE deleted_memories
    ADD COLUMN layer SMALLINT NOT NULL DEFAULT 2
        CHECK (layer BETWEEN 0 AND 4);

CREATE OR REPLACE FUNCTION audit_memory_delete() RETURNS trigger AS $$
BEGIN
    INSERT INTO deleted_memories (id, body, metadata, embedding, layer, created_at)
    VALUES (OLD.id, OLD.body, OLD.metadata, OLD.embedding, OLD.layer, OLD.created_at);
    RETURN OLD;
END;
$$ LANGUAGE plpgsql;
