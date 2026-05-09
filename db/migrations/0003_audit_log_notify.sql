-- 0003_audit_log_notify.sql — emit a Postgres NOTIFY on every audit_log
-- INSERT so the in-process JSONL mirror task wakes up immediately
-- instead of polling.
--
-- Architecture (Option I, HANDOVER 2026-05-10):
--
--   * The agent-core daemon runs a long-lived `audit_mirror` task that
--     holds one dedicated `PgConnection` (sqlx's `PgListener`) and
--     listens on the `audit_log_inserted` channel.
--   * Each NOTIFY carries the new row's `id` as text. The listener
--     fetches the row by id (a single-row index lookup on
--     `audit_log_pkey`) and appends it to
--     `~/.local/state/hhagent/audit-YYYY-MM-DD.jsonl` (UTC date,
--     fsync per write, daily rotation).
--   * Sole-source-of-truth: every operator-visible audit line on disk
--     comes from a row that committed in the database. There is no
--     "log first, persist later" race window — the JSONL file lags the
--     DB but never leads it.
--
-- Why a per-row AFTER INSERT trigger and not a per-statement trigger:
--
--   * Phase 0 throughput is one INSERT per tool call (tens to low
--     hundreds per minute at most); per-row overhead is invisible.
--   * Per-statement granularity would require the listener to discover
--     the new ids out of band (a SELECT for id > last_seen) — fine, but
--     loses the wake-up specificity that NOTIFY is meant to provide and
--     reintroduces a polling cadence inside the trigger.
--
-- Why payload = id::text and not the full row:
--
--   * Postgres limits NOTIFY payload to 8000 bytes (configurable by
--     `max_notify_payload_size` in PG 17+, but defaults stay 8000); a
--     payload-truncated `audit_log.payload` JSONB column would still
--     blow that limit on a 4 KiB-near-bound row plus envelope overhead.
--   * The listener is in-process with the dispatcher writer, so the
--     extra SELECT is a sub-ms UDS round-trip — cheaper than the
--     truncation logic that "ship the row in NOTIFY" would require.
--   * Decoupling the wake-up signal from the payload means the
--     listener can also catch up on rows it missed during reconnect
--     by ignoring the payload and querying `id > last_seen_id`.

-- ─── trigger function ─────────────────────────────────────────────
-- LANGUAGE plpgsql is required for `RETURN NEW;`. Inline SQL functions
-- can't return the trigger's NEW record in PG.
--
-- `SECURITY INVOKER` (the default) means the function runs with the
-- privileges of whatever role is INSERTing — `hhagent_runtime` in the
-- production path. `pg_notify()` is callable by any role; no
-- privilege escalation needed.
--
-- `SET search_path = pg_catalog, public` defends against a path
-- hijack: if a future migration creates an object named `pg_notify`
-- in `public`, this function still resolves to the catalog version.
-- The same hardening is applied to every trigger/function in the
-- Postgres docs as standard practice.
CREATE OR REPLACE FUNCTION audit_log_notify()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
BEGIN
    -- `id::text` so listeners parse a single integer, not a JSONB
    -- envelope. The listener then SELECTs the row by id to get the
    -- full record — see `core::audit_mirror`.
    PERFORM pg_notify('audit_log_inserted', NEW.id::text);
    RETURN NEW;
END;
$$;

-- ─── trigger ──────────────────────────────────────────────────────
-- AFTER INSERT (not BEFORE) so a NOTIFY fires only once the row is
-- actually committed. NOTIFYs are queued until COMMIT and discarded
-- on ROLLBACK — exactly the semantics we want (no listener wake-up
-- for a row that vanished).
--
-- FOR EACH ROW (not FOR EACH STATEMENT) — see file header for why.
CREATE TRIGGER audit_log_notify_trigger
    AFTER INSERT ON audit_log
    FOR EACH ROW
    EXECUTE FUNCTION audit_log_notify();
