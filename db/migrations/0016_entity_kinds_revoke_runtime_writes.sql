-- 0016_entity_kinds_revoke_runtime_writes.sql
--
-- Pre-reqs: 0015 (entity_kinds table exists).
--
-- 0015 intended `entity_kinds` to be an operator-managed lookup table:
-- the runtime role should only read from it, never write. The comment
-- at the bottom of 0015 even said so ("INSERT on entity_kinds is
-- operator-only by GRANT default"). But 0002's
-- `ALTER DEFAULT PRIVILEGES IN SCHEMA public ... GRANT SELECT, INSERT,
-- UPDATE, DELETE ON TABLES TO kastellan_runtime` fires automatically for
-- every new table created by the superuser running migrations, so
-- 0015's `CREATE TABLE entity_kinds` silently picked up full CRUD for
-- the runtime role. The explicit `GRANT SELECT` at the end of 0015 was
-- redundant; this migration adds the load-bearing REVOKE.
--
-- Same pattern as 0008's `deleted_memories` (insert-only audit table:
-- explicit `REVOKE UPDATE, DELETE, TRUNCATE FROM kastellan_runtime`) and
-- 0002's `audit_log` (same shape). The caveat in 0002's default-privileges
-- comment block flagged this exact scenario:
--
--   "Caveat for future authors: an insert-only table [...] needs its
--    own explicit `REVOKE UPDATE, DELETE, TRUNCATE FROM kastellan_runtime`
--    after creation because ALTER DEFAULT PRIVILEGES will have already
--    granted the forbidden operations along with the rest."
--
-- entity_kinds is "select-only" rather than "insert-only", so we revoke
-- INSERT as well. The operator (cluster superuser) still has full CRUD
-- via direct connection.

BEGIN;

REVOKE INSERT, UPDATE, DELETE, TRUNCATE ON entity_kinds FROM kastellan_runtime;

COMMIT;
