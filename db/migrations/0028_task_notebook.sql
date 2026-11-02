-- 0028_task_notebook.sql
--
-- The task notebook (#922): an append-only, per-task log of the planner's
-- working memory — its step list, each step ticked off with a note and the
-- facts found, operator corrections, and how the task ended.
--
-- Design: docs/superpowers/specs/2026-11-01-task-notebook-design.md
--
-- One row per event. The notebook's current state is a pure fold over a
-- task's rows (`core::scheduler::notebook::state::fold`), so the loop, the
-- CLI and a live watcher all read one shape.
--
-- It is the *performance record* of a task, not the security record (that
-- stays `audit_log`), but it is kept with the same posture: the runtime role
-- can add rows and read them, never rewrite or delete one. Trimming old
-- notebooks is an operator-run maintenance action, as the admin role.

BEGIN;

-- `seq` is gap-free from 1 per task and allocated by the writer
-- (`db::notebook::append`, MAX + 1 under a per-task advisory lock), because
-- two writers exist: the planner loop and an operator correction.
--
-- `kind` is a CLOSED set; a new kind is a migration, deliberately — the fold
-- refuses a kind it does not know, so an unlisted one would fail a task.
CREATE TABLE task_notebook (
    task_id     BIGINT      NOT NULL REFERENCES tasks(id),
    seq         BIGINT      NOT NULL CHECK (seq >= 1),
    kind        TEXT        NOT NULL
                CHECK (kind IN ('steps', 'tick', 'failed_step', 'correction',
                                'answer', 'infeasible', 'cancelled')),
    payload     JSONB       NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (task_id, seq)
);

-- A watcher follows a notebook without polling: the payload is
-- `<task_id>:<seq>` and the listener SELECTs the row. AFTER INSERT, so the
-- NOTIFY is delivered only once the row is committed (and dropped on a
-- ROLLBACK). `search_path` pinned against a `pg_notify` hijack, as in 0003.
CREATE OR REPLACE FUNCTION task_notebook_notify()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
BEGIN
    PERFORM pg_notify('task_notebook', NEW.task_id::text || ':' || NEW.seq::text);
    RETURN NEW;
END;
$$;

CREATE TRIGGER task_notebook_notify_trigger
    AFTER INSERT ON task_notebook
    FOR EACH ROW
    EXECUTE FUNCTION task_notebook_notify();

-- The `audit_log` shape (0002): add and read, never rewrite. The REVOKEs are
-- no-ops on a new table; they pin the intent so a stray GRANT ALL elsewhere
-- would also have to delete them.
GRANT  SELECT, INSERT           ON task_notebook TO kastellan_runtime;
REVOKE UPDATE, DELETE, TRUNCATE ON task_notebook FROM kastellan_runtime;
REVOKE ALL                      ON task_notebook FROM PUBLIC;

COMMIT;
