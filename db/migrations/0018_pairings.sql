-- 0018_pairings.sql
--
-- Pre-reqs: 0001 (baseline), 0002 (kastellan_runtime role + the
-- ALTER DEFAULT PRIVILEGES that auto-grants full CRUD on new tables).
--
-- Comms slice #3 — DM pairing. Two tables:
--
--   (1) `pairings` — the channel bus's authorization source of truth.
--       A row binds a channel-native identity (channel, peer) that the
--       operator deliberately authorized. The DB-backed PeerAuthorizer
--       reads it on every inbound message; a missing/ revoked row =>
--       the peer is unrecognised and its messages are dropped before
--       any processing (fail-closed). "Static contact allowlists
--       rejected (forgeable)" — a peer only lands here by proving
--       control of the account via a one-time operator-issued code.
--
--   (2) `pairing_codes` — pending operator-issued codes. Only the
--       SHA-256 *hash* of the code is stored; the plaintext is printed
--       once by `kastellan-cli pair issue` and never persisted. Codes
--       are single-use (claimed via a conditional UPDATE so two racing
--       claims cannot both win) and short-lived (expires_at).
--
-- Grants (least-privilege; same REVOKE pattern as 0016/0017, because
-- 0002's `ALTER DEFAULT PRIVILEGES IN SCHEMA public ... GRANT SELECT,
-- INSERT, UPDATE, DELETE ON TABLES TO kastellan_runtime` fires for
-- every superuser-created table):
--
--   * pairings: runtime may SELECT (authorize) and INSERT (bind a peer
--     on a successful code, in-daemon) but NOT UPDATE/DELETE — revoking
--     a pairing is a deliberate operator action over the admin
--     connection, never something the daemon (or a compromised worker
--     path) can do.
--   * pairing_codes: runtime may SELECT + UPDATE (find + atomically
--     consume a code) but NOT INSERT — minting codes is operator-only
--     (admin INSERT via `pair issue`). So the daemon can complete a
--     pairing the operator authorized, but cannot mint authorization
--     for itself.

BEGIN;

-- (1) Authorization source of truth.
CREATE TABLE pairings (
    id          BIGSERIAL   PRIMARY KEY,
    channel     TEXT        NOT NULL,
    peer        TEXT        NOT NULL,
    method      TEXT        NOT NULL DEFAULT 'code',
    paired_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at  TIMESTAMPTZ           -- NULL = active
);

-- At most one *active* pairing per (channel, peer); revoked rows are
-- kept for the audit trail and don't block re-pairing.
CREATE UNIQUE INDEX pairings_active_uniq
    ON pairings (channel, peer)
    WHERE revoked_at IS NULL;

-- (2) Pending operator-issued codes (hash only).
CREATE TABLE pairing_codes (
    id          BIGSERIAL   PRIMARY KEY,
    code_sha256 TEXT        NOT NULL,
    label       TEXT,                 -- operator note (who it's for)
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at  TIMESTAMPTZ NOT NULL,
    consumed_at TIMESTAMPTZ,          -- NULL = still claimable
    consumed_by TEXT                  -- "<channel>/<peer>" that claimed it
);

CREATE INDEX pairing_codes_claimable
    ON pairing_codes (expires_at)
    WHERE consumed_at IS NULL;

-- (3) Grants. The REVOKEs are load-bearing (see header).
GRANT  SELECT, INSERT             ON pairings      TO kastellan_runtime;
REVOKE UPDATE, DELETE, TRUNCATE   ON pairings      FROM kastellan_runtime;

GRANT  SELECT, UPDATE             ON pairing_codes TO kastellan_runtime;
REVOKE INSERT, DELETE, TRUNCATE   ON pairing_codes FROM kastellan_runtime;

COMMIT;
