-- 0029_task_scrapbook.sql
--
-- The scrapbook (#922 `sift`): the items one `sift` kept from a candidate
-- list — each with its source and the fields the sorter extracted — and the
-- ones the safety screen withheld, so the answer can count them.
--
-- Design: docs/superpowers/specs/2026-11-02-sift-and-scrapbook-design.md
--
-- The rows are read by code only (the answer's table, a follow-up's sift);
-- no model ever sees a row except through a screened render. `sift` is the
-- turn (`tasks.plan_count`) the sift ran on, so a task's sifts never collide
-- and a ref `sb:<task_id>:<sift>` names one of them.
--
-- The same posture as `task_notebook` (0028): the runtime role can add rows
-- and read them, never rewrite or delete one. Trimming old scrapbooks is an
-- operator-run maintenance action, as the admin role.

BEGIN;

-- `kind` is a CLOSED set, like 0028's: `kept` (the sorter said yes) or
-- `withheld` (the item's text was blocked by the safety screen and was
-- never shown to the sorter). `classification` is the class of the call
-- that read the item, in `DataClass`'s spelling.
CREATE TABLE task_scrapbook (
    task_id         BIGINT      NOT NULL REFERENCES tasks(id),
    sift            INT         NOT NULL CHECK (sift >= 1),
    row             INT         NOT NULL CHECK (row >= 1),
    kind            TEXT        NOT NULL CHECK (kind IN ('kept', 'withheld')),
    source          JSONB       NOT NULL,
    fields          JSONB       NOT NULL,
    classification  TEXT        NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (task_id, sift, row)
);

-- The `audit_log` shape (0002): add and read, never rewrite. The REVOKEs are
-- no-ops on a new table; they pin the intent so a stray GRANT ALL elsewhere
-- would also have to delete them.
GRANT  SELECT, INSERT           ON task_scrapbook TO kastellan_runtime;
REVOKE UPDATE, DELETE, TRUNCATE ON task_scrapbook FROM kastellan_runtime;
REVOKE ALL                      ON task_scrapbook FROM PUBLIC;

COMMIT;
