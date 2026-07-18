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
-- Fix: make the row carry its own `kind`, and branch the CHECK on it.
--
-- Why a `kind` column rather than a tool-name list in SQL: the entry shape is a
-- property of the tool, and the tool roster GROWS. Encoding the roster in the
-- constraint (`CASE WHEN tool IN ('web-fetch', …)`) would make every future
-- network worker pay a schema migration just to be added to a hardcoded list,
-- and would leave that list to drift against `WorkerManifest::allowlist_kind`.
-- With the kind in the row, SQL never needs to know a tool name: adding a tool
-- is a pure Rust manifest change (no migration), and only a genuinely new KIND
-- needs schema work. `db::tool_allowlists::add` writes the value from the single
-- Rust source of truth, so the column is consistent with the tool by
-- construction, and the row becomes self-describing for operators
-- (`SELECT tool, kind, argv0`).
--
-- A port-bearing row such as `localhost:8888` fails the domain branch (the `:`
-- is outside the domain character class and it is not a bracketed IPv6
-- literal) — that is the #459 residual-#3 footgun, which would otherwise map
-- through `{host}:443` to the dead net entry `localhost:8888:443`. A relative
-- argv0 such as `echo` still fails the argv0 branch, preserving the `0009`
-- guarantee that `shell-exec` entries are absolute.
--
-- The Rust per-kind validators (`validate_argv0` / `validate_domain`, dispatched
-- by `validate_entry`) remain the authoritative, more precise gate (label
-- lengths, hyphen placement, the 253-byte cap, real IPv6 parsing). This CHECK is
-- the coarser shared backstop for callers that bypass them — the runtime role
-- holds direct INSERT on this table. Deliberately coarser: e.g. an empty-label
-- `a..b` satisfies the domain branch here but is rejected by `validate_domain`.
-- That asymmetry is acceptable — such a row is a dead non-localhost host, not a
-- security boundary.
--
-- Existing rows are all `shell-exec` argv0 paths, so the `DEFAULT 'argv0'`
-- backfills them correctly and they still satisfy the argv0 branch unchanged.

ALTER TABLE tool_allowlists
    ADD COLUMN kind TEXT NOT NULL DEFAULT 'argv0';

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
    AND kind IN ('argv0', 'domain')
    AND CASE kind
        -- argv0-kind: absolute exec path (the 0009 guarantee, preserved).
        WHEN 'argv0'  THEN argv0 LIKE '/%'
        -- domain-kind: bare/wildcard host or IPv4, or a bracketed IPv6 literal.
        WHEN 'domain' THEN argv0 ~ '^\.?[A-Za-z0-9.-]+$'
                        OR argv0 ~ '^\[[0-9A-Fa-f:]+\]$'
        ELSE false
    END
);
