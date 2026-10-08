-- 0027_tasks_reply_settled.sql
--
-- The reply claim (#825). Before this, the only trigger for a channel reply
-- was the `tasks_completed` NOTIFY, which Postgres never replays: a task that
-- finished while no bus was listening (a restart's backoff, a graceful
-- shutdown's scheduler drain, the boot crash sweep) or whose load failed
-- after the NOTIFY was consumed lost its reply with at most a WARN.
--
-- Now every route claims the task first:
--   UPDATE tasks SET reply_settled_at = now(), reply_disposition = ...
--    WHERE id = $1 AND reply_settled_at IS NULL AND state IN (terminal)
-- so exactly one router wins — across the two buses (#497), and between the
-- live NOTIFY and the catch-up sweep, which walks the partial index below.
--
-- Design: docs/superpowers/specs/2026-10-08-825-reply-catch-up-design.md
--
-- ⚠️ The terminal-state list below is a reviewed copy of
-- `notify_task_completed`'s (0012) and of `REPLIED_STATES`
-- (db/src/tasks/reply_claim.rs). `db/tests/reply_claim_e2e.rs` pins this
-- index against the const; pinning the trigger is #712.

ALTER TABLE tasks
    ADD COLUMN reply_settled_at  TIMESTAMPTZ,
    ADD COLUMN reply_disposition TEXT
        CHECK (reply_disposition IN ('routed', 'unroutable', 'backfilled')),
    ADD CONSTRAINT tasks_reply_settled_together
        CHECK ((reply_settled_at IS NULL) = (reply_disposition IS NULL));

-- History is settled, honestly labelled: whether those replies were delivered
-- is unknowable now, and without this the first sweep after deploy would
-- re-send every reply ever made.
UPDATE tasks
   SET reply_settled_at  = COALESCE(finished_at, updated_at),
       reply_disposition = 'backfilled'
 WHERE payload->>'kind' = 'channel'
   AND state IN ('completed','failed','cancelled','blocked','timed_out','crashed','refused');

-- Only the backlog is indexed, so a sweep over an empty backlog reads nothing.
CREATE INDEX tasks_unsettled_channel_replies ON tasks (id)
 WHERE reply_settled_at IS NULL
   AND payload->>'kind' = 'channel'
   AND state IN ('completed','failed','cancelled','blocked','timed_out','crashed','refused');
