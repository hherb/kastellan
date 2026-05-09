-- 0002_runtime_role.sql — split out a non-superuser runtime role.
--
-- Up to this point the agent-core daemon has connected to its own
-- cluster as the OS user, which `initdb --username=$(whoami)` made the
-- cluster superuser. Convenient for bootstrap (CREATE EXTENSION,
-- CREATE DATABASE, the migration runner all need superuser), but it
-- means the daemon's *application-level* writes also run with
-- superuser privilege — and `audit_log` is supposed to be append-only.
--
-- This migration splits a `hhagent_runtime` role out from the bootstrap
-- superuser. The split is purely SQL-level: the cluster's `pg_hba.conf`
-- still maps the OS user to the cluster superuser via peer auth, and
-- the daemon switches into the runtime role at the start of each
-- application-level transaction via `SET ROLE hhagent_runtime`.
-- Operational consequences:
--
--   * `audit_log` rows are now created by `hhagent_runtime`, which is
--     explicitly REVOKEd from UPDATE / DELETE / TRUNCATE on this table
--     — a tampering attempt by the runtime path is rejected at the DB
--     layer rather than relying on application discipline alone.
--   * Future migrations that need superuser (CREATE EXTENSION, CREATE
--     ROLE, anything reading `pg_shadow`) keep working because the
--     migrator always runs as the OS user (= cluster superuser). Only
--     the post-migration application paths drop privilege.
--   * Schema-level CREATE/DROP is impossible from the runtime role —
--     `NOCREATEROLE NOCREATEDB` plus `NOSUPERUSER` plus the absence of
--     OWNER on any object means a compromised application path cannot
--     mutate the schema.
--
-- Why `pg_ident.conf` mapping was rejected: it would mean editing
-- `pg_ident.conf` and `pg_hba.conf` inside the data dir after
-- `initdb`, which is foreign to our sqlx-migration shape (no SQL
-- equivalent for those file edits). `SET ROLE` is the
-- migration-encapsulable equivalent and operationally identical for
-- our threat model — the runtime path's privileges are bounded by
-- the GRANTs below regardless of how the role was entered.

-- ─── role ─────────────────────────────────────────────────────────
-- `NOLOGIN` because the role is only entered via `SET ROLE` from the
-- bootstrap user; nothing should ever connect to the cluster *as*
-- hhagent_runtime directly. `NOINHERIT` so the OS user (who is GRANTed
-- this role below) does NOT pick up the runtime privileges by default
-- — they have to switch in explicitly. Together those two flags make
-- privilege drops auditable: every application connection has to call
-- `SET ROLE hhagent_runtime` before doing any tampering-sensitive write.
--
-- The `IF NOT EXISTS`-style guard via DO/PL-pgSQL is needed because
-- `CREATE ROLE` itself does not support `IF NOT EXISTS` (unlike
-- `CREATE TABLE`). A bare `CREATE ROLE` would error if a previous
-- migration attempt half-applied; sqlx's checksum tracking would then
-- refuse to retry the file. The DO block makes this migration safe to
-- re-apply by hand in pathological recovery scenarios.
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'hhagent_runtime') THEN
        CREATE ROLE hhagent_runtime
            NOSUPERUSER
            NOCREATEROLE
            NOCREATEDB
            NOLOGIN
            NOINHERIT;
    END IF;
END;
$$;

-- ─── role membership ──────────────────────────────────────────────
-- Whoever ran the migration (= the OS user, = the cluster's bootstrap
-- superuser under peer auth) needs to be a member of `hhagent_runtime`
-- so `SET ROLE hhagent_runtime` works on subsequent application
-- connections. We use `format(... %I, current_user)` rather than a
-- hardcoded role name because the OS username varies per host (`hherb`
-- on the developer box, something else on CI/macOS).
--
-- `GRANT … TO …` is idempotent — re-running the migration after a
-- partial apply does no harm.
DO $$
BEGIN
    EXECUTE format('GRANT hhagent_runtime TO %I', current_user);
END;
$$;

-- ─── schema usage ─────────────────────────────────────────────────
-- Without USAGE on the schema the role cannot resolve unqualified
-- table names (e.g. `audit_log` would have to be written as
-- `public.audit_log`). USAGE alone does NOT permit creation of new
-- objects in the schema; `CREATE` would have to be granted separately
-- and is intentionally withheld here.
GRANT USAGE ON SCHEMA public TO hhagent_runtime;

-- ─── audit_log: insert + select only ──────────────────────────────
-- This is the contract pin from `0001_init.sql`'s comment block:
-- "Once a non-superuser runtime role is split out, this table will
-- gain `REVOKE UPDATE, DELETE ON audit_log FROM <runtime_role>`."
-- Now we pay that bill.
--
-- INSERT + SELECT is everything the dispatcher write-site (Phase 0
-- Option I) and the audit-tail viewer need. UPDATE / DELETE / TRUNCATE
-- are explicitly rejected so a compromised tool, LLM-issued SQL, or
-- bug in the dispatcher cannot rewrite or vanish prior rows.
--
-- The REVOKE statements are defense-in-depth (a brand-new role has no
-- prior grants on this table, so the REVOKE is a no-op today) — but
-- they pin the intent in code so a future `GRANT ALL ON audit_log TO
-- hhagent_runtime` added by mistake elsewhere is a pure regression that
-- would need to also delete these REVOKEs to take effect. Same logic
-- for `REVOKE ALL ... FROM PUBLIC`: PUBLIC has nothing on user tables
-- by default, but the line documents the intent.
GRANT  SELECT, INSERT                       ON audit_log TO hhagent_runtime;
REVOKE UPDATE, DELETE, TRUNCATE             ON audit_log FROM hhagent_runtime;
REVOKE ALL                                  ON audit_log FROM PUBLIC;

-- The id column is BIGSERIAL, which expands to
-- `nextval('audit_log_id_seq')` as the column default. INSERTs that
-- omit the id (the only sensible shape) need USAGE on the sequence to
-- call nextval(). Without this grant every INSERT from the runtime
-- role would fail with "permission denied for sequence audit_log_id_seq".
GRANT USAGE ON SEQUENCE audit_log_id_seq TO hhagent_runtime;

-- ─── application tables: full CRUD ────────────────────────────────
-- The other five tables are the agent's day-to-day state. Each of them
-- needs SELECT + INSERT + UPDATE + DELETE because the schedulers,
-- memory writers, graph writers, and secret rotation paths all mutate
-- or remove rows. We grant CRUD bulk-style here; per-table privilege
-- carving (e.g. "the memory worker doesn't need to write secrets")
-- belongs in a *separate* migration once the per-worker role split
-- materialises in Phase 1 — premature today.
GRANT SELECT, INSERT, UPDATE, DELETE
    ON tasks, memories, entities, relations, secrets
    TO hhagent_runtime;

-- Sequences are independent objects from their owning tables, so they
-- need their own grant. See the audit_log_id_seq comment above for
-- the BIGSERIAL → nextval() chain.
GRANT USAGE ON SEQUENCE
    tasks_id_seq, memories_id_seq, entities_id_seq,
    relations_id_seq, secrets_id_seq
    TO hhagent_runtime;

-- ─── default privileges for future migrations ─────────────────────
-- Without this block, every future migration that adds a table would
-- have to remember to `GRANT ... TO hhagent_runtime` or the runtime
-- daemon would silently lose access to the new table at the next
-- restart. Easy to forget; nasty to debug.
--
-- ALTER DEFAULT PRIVILEGES applies to objects created in the future
-- by `current_user` (the OS user / cluster superuser, who is the only
-- principal that runs migrations). Existing objects are unaffected,
-- which is why the explicit GRANTs above are still required for the
-- 0001-era tables.
--
-- Caveat for future authors: an insert-only table (a future
-- audit-style log) needs its own explicit
-- `REVOKE UPDATE, DELETE, TRUNCATE FROM hhagent_runtime` after creation
-- because ALTER DEFAULT PRIVILEGES will have already granted the
-- forbidden operations along with the rest.
ALTER DEFAULT PRIVILEGES IN SCHEMA public
    GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO hhagent_runtime;

ALTER DEFAULT PRIVILEGES IN SCHEMA public
    GRANT USAGE, SELECT ON SEQUENCES TO hhagent_runtime;
