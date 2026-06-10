-- Phase 1 — append-only journal of deleted memories.
--
-- Phase 1 has no caller that deletes memories today, but the cascade
-- infrastructure in 0007 treats memory deletion as a real future
-- operation (e.g. GDPR-style forgetting). When that operation
-- materialises, this trigger guarantees the deleted row is preserved
-- before it vanishes.
--
-- Why a dedicated table and not an audit_log row:
--   * audit_log truncates payloads at 4 KiB; a memory body + metadata
--     + 1024-dim embedding can easily exceed that
--   * Keeping the row's full shape means a future "undelete" or
--     "show me what disappeared" query has everything it needs
--     without joining back to a row that no longer exists
--
-- Why a trigger (not app-level discipline):
--   * Contract is "every DELETE FROM memories journals to
--     deleted_memories" — enforcing at the DB layer means a future
--     contributor's bare DELETE cannot silently bypass the audit

CREATE TABLE deleted_memories (
    id          BIGINT      PRIMARY KEY,    -- preserved from memories.id
    body        TEXT        NOT NULL,
    metadata    JSONB       NOT NULL,
    embedding   vector(1024),                -- nullable, like the source
    created_at  TIMESTAMPTZ NOT NULL,        -- original creation time
    deleted_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX deleted_memories_deleted_at_idx ON deleted_memories (deleted_at);

CREATE OR REPLACE FUNCTION audit_memory_delete() RETURNS trigger AS $$
BEGIN
    INSERT INTO deleted_memories (id, body, metadata, embedding, created_at)
    VALUES (OLD.id, OLD.body, OLD.metadata, OLD.embedding, OLD.created_at);
    RETURN OLD;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER memories_after_delete_audit
    AFTER DELETE ON memories
    FOR EACH ROW
    EXECUTE FUNCTION audit_memory_delete();

-- Runtime needs SELECT (for reads) and INSERT (because the trigger
-- runs as the DELETE issuer's role, SECURITY INVOKER by default).
-- UPDATE/DELETE revoked — same append-only shape as audit_log.
GRANT  SELECT, INSERT ON deleted_memories TO kastellan_runtime;
REVOKE UPDATE, DELETE, TRUNCATE ON deleted_memories FROM kastellan_runtime;
