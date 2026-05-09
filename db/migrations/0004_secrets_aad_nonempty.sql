-- 0004_secrets_aad_nonempty.sql
--
-- Tighten the `secrets.aad` contract.
--
-- 0001_init.sql defined the column as
--     aad BYTEA NOT NULL DEFAULT ''::bytea
-- with the comment "the runtime encrypt/decrypt path lands later".
-- That runtime now exists in `db::secrets`, and every call site
-- (`db::secrets::put`) constructs AAD via `compute_aad(name, _)` —
-- which is structurally non-empty (`AAD_DOMAIN || 0x00 || name ||
-- 0x00 || extra`, minimum length = `AAD_DOMAIN.len() + 2`).
--
-- Two changes:
--
-- 1. **Drop the empty default.** No call site relies on it; an
--    accidental future `INSERT INTO secrets (...) VALUES (...)`
--    that omits `aad` would have constructed a row with a tag
--    bound to the empty byte string — almost certainly a bug. The
--    DEFAULT made that bug silent. Removing it forces every
--    insert to populate AAD explicitly (which `db::secrets::put`
--    already does).
--
-- 2. **Add a CHECK constraint** so the database itself rejects an
--    empty AAD. Defense-in-depth on top of the application-layer
--    construction. If a future migration or hand-edited SQL ever
--    tries to write `aad = ''::bytea` it fails closed at the DB.
--
-- Both changes are safe to apply on a populated cluster: there are
-- no rows in `secrets` yet at this point in the project's history
-- (the runtime that writes them only just landed), so neither the
-- DROP DEFAULT nor the ADD CONSTRAINT can fail validation. A
-- future migration on a populated table would have to backfill
-- non-empty AAD first.
--
-- Closes issue #12.

ALTER TABLE secrets ALTER COLUMN aad DROP DEFAULT;

ALTER TABLE secrets
    ADD CONSTRAINT secrets_aad_nonempty
    CHECK (octet_length(aad) > 0);
