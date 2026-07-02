-- Harden the `audit_memory_delete()` trigger function with an explicit
-- `SET search_path` (security audit 2026-07-02, finding #10).
--
-- The function was introduced in 0008 and last redefined in 0014, both
-- times WITHOUT the `SET search_path = pg_catalog, public` clause that
-- every other trigger function in this schema carries (0003, 0005, 0012).
-- It runs as the deleting role (SECURITY INVOKER, the default). A shadow
-- object-resolution hijack is not currently reachable — 0002 denies the
-- runtime role `CREATE` on schema `public`, so it cannot plant a shadowing
-- `deleted_memories`/`pg_notify` object — but pinning the search_path is
-- the codebase's own standard-practice hardening (see 0003's comment) and
-- closes the inconsistency as defense-in-depth.
--
-- `CREATE OR REPLACE FUNCTION` swaps the body in place; PG resolves trigger
-- functions by name at execution time, so the existing
-- `memories_after_delete_audit` binding picks up the hardened body with no
-- `DROP TRIGGER` needed. Body is byte-identical to 0014 apart from the added
-- options; GRANT shape on `deleted_memories` is unchanged.

CREATE OR REPLACE FUNCTION audit_memory_delete()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
BEGIN
    INSERT INTO deleted_memories (id, body, metadata, embedding, layer, created_at)
    VALUES (OLD.id, OLD.body, OLD.metadata, OLD.embedding, OLD.layer, OLD.created_at);
    RETURN OLD;
END;
$$;
