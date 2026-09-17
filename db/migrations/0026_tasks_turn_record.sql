-- 0026_tasks_turn_record.sql
-- Conversational continuity for channel tasks (#701).
--
-- A channel message becomes a task carrying only its own sentence, so a
-- follow-up in the same room has no referent: measured live on 2026-09-14,
-- where a follow-up about "the last 3 flight bookings" searched from scratch
-- and settled on a different booking than the answer it was following up on.
--
-- (1) `turn_record` — what THIS task did, for the next turn to read:
--     {"calls": [{"tool","method","parameters","returns"}],
--      "_omitted_calls": <int, present only when calls were dropped>,
--      "data_class": "..."}
--     Written by `tasks::finalize` in the same UPDATE that makes the task
--     terminal, so the record cannot disagree with the task it describes.
--     NULL for: every non-channel task; every task that finished before this
--     migration; and every channel task that never reached `finalize` with a
--     record — `sweep_crashed` and `mark_cancelled` write state and
--     finished_at only, and the pre-plan denial and failure paths pass NULL.
--     The consumer treats NULL as "show that this turn happened, and nothing
--     about what was in it".
--
-- (2) The lookup index. The conversation query filters on three payload keys
--     and orders by finished_at; `tasks.payload` had no index at all, so
--     without this the lookup is a sequential scan over every task. Partial
--     on kind='channel': no other task kind is ever looked up this way, and
--     the partial index stays small.

ALTER TABLE tasks ADD COLUMN turn_record JSONB;

CREATE INDEX tasks_conversation_idx
    ON tasks ((payload->>'channel'),
              (payload->>'peer'),
              (payload->>'conversation'),
              finished_at DESC)
 WHERE payload->>'kind' = 'channel';
