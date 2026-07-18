-- Phase 1 — tool_allowlists holds two entry kinds (#459 residual #3).
--
-- The `0009` CHECK required every argv0 to be an absolute path
-- (`argv0 LIKE '/%'`). That is correct for `shell-exec` (which stores argv[0]
-- exec paths) but it rejects the DOMAIN entries that `web-fetch`,
-- `web-research`, and `browser-driver` store — so domain allowlist rows were
-- uninsertable through any path (the Rust validator AND this CHECK both
-- refused them), leaving those workers unable to be given an operator content
-- allowlist at all.
--
-- Replace the argv0-only CHECK with a union-branch CHECK that admits either
-- shape while still rejecting malformed rows. A port-bearing row such as
-- `localhost:8888` fails EVERY branch (no leading slash; a `:` is outside the
-- domain character class; not a bracketed IPv6 literal) — which is the #459
-- residual-#3 footgun: it would otherwise map through `{host}:443` to the dead
-- net entry `localhost:8888:443`.
--
-- The Rust per-kind validators in `db::tool_allowlists` (`validate_argv0` /
-- `validate_domain`, dispatched by `validate_entry`) remain the authoritative,
-- more precise gate (label lengths, hyphen placement, the 253-byte cap, real
-- IPv6 parsing). This CHECK is the coarser shared backstop for callers that
-- bypass them — the runtime role holds direct INSERT on this table.
--
-- Deliberately coarser than the Rust gate: e.g. an empty-label `a..b` satisfies
-- the domain branch here but is rejected by `validate_domain`. That asymmetry
-- is acceptable — such a row is a dead non-localhost host, not a security
-- boundary, and the authoritative path rejects it.
--
-- The `0009` argv0 CHECK is an inline *unnamed* constraint, so Postgres
-- auto-generated its name (in practice `tool_allowlists_argv0_check`) — not
-- stable to hardcode. Match it by DEFINITION instead, and note that Postgres
-- renders `LIKE` as the `~~` operator in `pg_get_constraintdef`, so searching
-- for the literal text "argv0 LIKE" finds nothing. Drop every CHECK on this
-- table whose definition mentions `argv0` (the only other CHECK is
-- `octet_length(tool) > 0`, which does not), excluding the replacement added
-- below so a re-run is idempotent.

DO $$
DECLARE
    c_name text;
BEGIN
    FOR c_name IN
        SELECT conname
        FROM pg_constraint
        WHERE conrelid = 'tool_allowlists'::regclass
          AND contype = 'c'
          AND conname <> 'tool_allowlists_entry_shape'
          AND pg_get_constraintdef(oid) LIKE '%argv0%'
    LOOP
        EXECUTE format('ALTER TABLE tool_allowlists DROP CONSTRAINT %I', c_name);
    END LOOP;
END $$;

ALTER TABLE tool_allowlists ADD CONSTRAINT tool_allowlists_entry_shape CHECK (
    octet_length(argv0) > 0
    AND argv0 !~ '(^|/)\.\.(/|$)'          -- no '..' segment (both kinds)
    AND (
        argv0 LIKE '/%'                    -- argv0-kind: absolute exec path
        OR argv0 ~ '^\.?[A-Za-z0-9.-]+$'   -- domain-kind: bare/wildcard host or IPv4
        OR argv0 ~ '^\[[0-9A-Fa-f:]+\]$'   -- domain-kind: bracketed IPv6 literal
    )
);
