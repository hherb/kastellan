-- 0012_tasks_state_refused.sql
-- Adds 'refused' as a valid terminal value of `tasks.state` so the
-- scheduler can record an agent self-declared constitutional refusal
-- distinct from the reviewer-detected 'blocked' state.
--
-- Two CREATE OR REPLACE operations:
--   1. The CHECK constraint `tasks_state_check` (from
--      0005_tasks_scheduler.sql) gets dropped and recreated with
--      'refused' appended to the IN list.
--   2. The `notify_task_completed` trigger function (from
--      0005_tasks_scheduler.sql, line ~76-88) enumerates the same
--      terminal set in two IN clauses; the function body is replaced
--      with 'refused' appended to both.
--
-- `tasks.finished_at` is set by application-level UPDATEs in
-- `db::tasks::{finalize, mark_cancelled, sweep_crashed}` rather than
-- by a trigger, so no trigger-side `finished_at` widening is needed.

ALTER TABLE tasks DROP CONSTRAINT tasks_state_check;
ALTER TABLE tasks
    ADD CONSTRAINT tasks_state_check CHECK (state IN
        ('pending','running','completed','failed','cancelled',
         'blocked','timed_out','crashed','refused'));

CREATE OR REPLACE FUNCTION notify_task_completed()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
BEGIN
    IF NEW.state IN ('completed','failed','cancelled','blocked',
                     'timed_out','crashed','refused')
       AND OLD.state NOT IN ('completed','failed','cancelled','blocked',
                             'timed_out','crashed','refused') THEN
        PERFORM pg_notify('tasks_completed', NEW.id::text);
    END IF;
    RETURN NEW;
END;
$$;
