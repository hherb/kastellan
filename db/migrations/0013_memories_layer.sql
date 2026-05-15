-- Phase 1 — tag every memory row with a hierarchy layer 0..=4.
--
-- The L1 layer is the "always-in-context insight index" loaded
-- unconditionally by `core::memory::layers::load_l1`; the other layers
-- are reserved writers for future slices (L0 seed rules, L3 skills,
-- L4 session digests). Each row belongs to exactly one layer.
--
-- All existing rows are stable accumulated facts → backfilled to L2.
-- That preserves "everything currently recalled stays recallable
-- post-migration" while leaving L1 empty until something explicitly
-- writes to it (premature promotion to L0/L1 would inject every
-- existing row into every prompt — token blowout).
--
-- SMALLINT (2 bytes) over INT — 5 distinct values forever; CHECK
-- constraint at the DB boundary is canonical (the Rust enum is
-- convenience). Mirrors the `tasks.state` CHECK pattern; PG ENUMs
-- carry rename pain we've already chosen against.

ALTER TABLE memories
    ADD COLUMN layer SMALLINT NOT NULL DEFAULT 2
        CHECK (layer BETWEEN 0 AND 4);

-- ADD COLUMN with DEFAULT already populates existing rows; the explicit
-- UPDATE below is a no-op on a virgin schema but documents intent and
-- is idempotent against partial-state recovery (e.g. ADD COLUMN
-- partially applied without DEFAULT).
UPDATE memories SET layer = 2 WHERE layer IS NULL;

-- (layer, created_at DESC) supports the L1 hot path
-- (WHERE layer = 1 ORDER BY created_at DESC LIMIT $cap) and any future
-- "show me everything at layer X" query.
CREATE INDEX memories_layer_idx ON memories (layer, created_at DESC);

-- No GRANT change: hhagent_runtime already has full CRUD on `memories`
-- (migration 0002); the new column is part of that table.
