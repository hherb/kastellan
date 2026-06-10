-- Phase 1 — per-tool argv allowlist hygiene.
--
-- Source-of-truth for which absolute `argv[0]` paths each registered
-- tool worker may exec. Replaces the previous `KASTELLAN_SHELL_EXEC_ALLOWLIST`
-- env var: env-var-driven means a host restart with a typo can silently
-- widen the allowlist with no audit trail. With this table, every change
-- writes one row in `audit_log` via the chokepoint in `core::cli_audit`.
--
-- Why composite-PK on `(tool, argv0)`:
--   * Natural "one row per allowlisted path per tool" shape
--   * PK index serves the registry-build read `WHERE tool = $1`
--   * Idempotent semantics via `INSERT … ON CONFLICT DO NOTHING`
--   * Per-entry audit rows (one row per add/remove) rather than
--     whole-list replacement diffs
--
-- GRANT shape: SELECT/INSERT/DELETE for kastellan_runtime, deliberately
-- NO UPDATE. Changing an entry means DELETE + INSERT, preserving the
-- audit trail of both the old and new shapes. Mirrors audit_log's
-- append-only discipline from migration 0002, but applied as
-- "no-update" rather than "no-update-no-delete" — operators must be
-- able to retire allowlist entries. UPDATE and TRUNCATE are both
-- REVOKEd explicitly to counteract the `ALTER DEFAULT PRIVILEGES` from
-- `0002` — without these REVOKEs the default-privilege machinery would
-- still grant them.

-- CHECK constraint scope (defense-in-depth for callers that bypass the
-- Rust validators in `db::tool_allowlists`):
--   * `argv0 LIKE '/%'`     — leading slash (absolute path)
--   * `argv0 !~ '(^|/)\.\.(/|$)'` — no `..` *segment* (between `/`s or
--     at either end). Rejects path-confusion bypasses like
--     `/usr/bin/../bin/echo` while still allowing `..` *within* a
--     filename segment (e.g. `/usr/bin/foo..bar`).
--   * NUL bytes inside `argv0` are not handled here because Postgres
--     TEXT columns reject the 0x00 byte at the protocol layer.
CREATE TABLE tool_allowlists (
    tool       TEXT NOT NULL,
    argv0      TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_by TEXT NOT NULL,
    PRIMARY KEY (tool, argv0),
    CHECK (octet_length(tool) > 0),
    CHECK (
        octet_length(argv0) > 0
        AND argv0 LIKE '/%'
        AND argv0 !~ '(^|/)\.\.(/|$)'
    )
);

GRANT SELECT, INSERT, DELETE ON tool_allowlists TO kastellan_runtime;
REVOKE UPDATE, TRUNCATE ON tool_allowlists FROM kastellan_runtime;
