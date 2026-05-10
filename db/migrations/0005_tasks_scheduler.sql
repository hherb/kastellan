-- 0005_tasks_scheduler.sql
--
-- Phase 1 scheduler additions to the tasks table.
--
-- Adds:
--   • lane              — 'fast' | 'long'; the two lane runners filter on this
--   • result            — JSONB; final task output, written by finalize()
--   • started_at        — set when claim_one transitions pending → running
--   • finished_at       — set on terminal transition (any non-running state)
--   • lease_expires_at  — single clock; doubles as wall-clock deadline AND
--                         crash-liveness signal. Set at claim time, never
--                         extended. Crashed tasks sit in 'running' until this
--                         passes, then a startup sweep marks 'crashed'.
--   • plan_count        — mirrored from the inner loop; visible in CLI status
--
-- Expanded state CHECK with the new terminal states (blocked, timed_out, crashed).
--
-- Three NOTIFY triggers (mirroring 0003_audit_log_notify.sql):
--   tasks_inserted   — wakes lane runners on new pending row
--   tasks_cancelled  — wakes the inner loop's cancellation poller
--   tasks_completed  — wakes hhagent-cli ask subscribers on terminal transition

ALTER TABLE tasks
    ADD COLUMN lane TEXT NOT NULL DEFAULT 'fast'
        CHECK (lane IN ('fast', 'long')),
    ADD COLUMN result JSONB,
    ADD COLUMN started_at      TIMESTAMPTZ,
    ADD COLUMN finished_at     TIMESTAMPTZ,
    ADD COLUMN lease_expires_at TIMESTAMPTZ,
    ADD COLUMN plan_count INT NOT NULL DEFAULT 0;

ALTER TABLE tasks DROP CONSTRAINT tasks_state_check;
ALTER TABLE tasks
    ADD CONSTRAINT tasks_state_check CHECK (state IN
        ('pending','running','completed','failed','cancelled',
         'blocked','timed_out','crashed'));

DROP INDEX IF EXISTS tasks_state_created_at_idx;
CREATE INDEX tasks_lane_state_created_at_idx
    ON tasks (lane, state, created_at);

CREATE OR REPLACE FUNCTION notify_task_inserted()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
BEGIN
    PERFORM pg_notify('tasks_inserted', NEW.id::text);
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS tasks_notify_inserted ON tasks;
CREATE TRIGGER tasks_notify_inserted
    AFTER INSERT ON tasks FOR EACH ROW
    EXECUTE FUNCTION notify_task_inserted();

CREATE OR REPLACE FUNCTION notify_task_cancelled()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
BEGIN
    IF NEW.state = 'cancelled' AND OLD.state <> 'cancelled' THEN
        PERFORM pg_notify('tasks_cancelled', NEW.id::text);
    END IF;
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS tasks_notify_cancelled ON tasks;
CREATE TRIGGER tasks_notify_cancelled
    AFTER UPDATE OF state ON tasks FOR EACH ROW
    EXECUTE FUNCTION notify_task_cancelled();

CREATE OR REPLACE FUNCTION notify_task_completed()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
BEGIN
    IF NEW.state IN ('completed','failed','cancelled','blocked','timed_out','crashed')
       AND OLD.state NOT IN ('completed','failed','cancelled','blocked','timed_out','crashed') THEN
        PERFORM pg_notify('tasks_completed', NEW.id::text);
    END IF;
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS tasks_notify_completed ON tasks;
CREATE TRIGGER tasks_notify_completed
    AFTER UPDATE OF state ON tasks FOR EACH ROW
    EXECUTE FUNCTION notify_task_completed();

GRANT SELECT, INSERT, UPDATE ON tasks TO hhagent_runtime;
GRANT USAGE, SELECT ON SEQUENCE tasks_id_seq TO hhagent_runtime;

-- Tasks have no DELETE in the lifecycle (rows transition through
-- terminal states and stay). REVOKE DELETE at the role layer mirrors
-- the audit_log + agent_prompts append-only-by-GRANT pattern from
-- 0002_runtime_role.sql.
REVOKE DELETE ON tasks FROM hhagent_runtime;
