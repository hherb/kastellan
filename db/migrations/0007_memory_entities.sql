-- Phase 1 — graph lane in `core::memory::recall`.
--
-- Join table linking `memories` rows to `entities` nodes. The graph
-- lane uses this to surface memories tagged with seed entities (and
-- their 1-hop outbound neighbours, expanded in core).
--
-- Why a composite-PK join table (over JSONB on memories.metadata):
--   * Higher-cardinality storage (one row per link)
--   * Clean cascade semantics — deleting an entity drops its links
--     automatically, no manual sweep
--   * Index on entity_id makes `entity_id = ANY($1)` a single index
--     scan, not a JSONB GIN intersection
--   * Lane SQL is a straightforward GROUP BY; JSONB shape would need
--     jsonb_array_elements + casts at every read
--
-- Cascade safety: both FKs are ON DELETE CASCADE. FK cascades flow
-- only from referenced row to referencing row, so a link-row deletion
-- can NEVER trigger a memory or entity deletion. See migration 0008
-- for the trigger that journals memory deletions specifically.

CREATE TABLE memory_entities (
    memory_id  BIGINT NOT NULL
        REFERENCES memories(id) ON DELETE CASCADE,
    entity_id  BIGINT NOT NULL
        REFERENCES entities(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (memory_id, entity_id)
);

-- PK already indexes (memory_id, ...). This second index supports the
-- read path which is `WHERE entity_id = ANY($1)` (no memory_id filter).
CREATE INDEX memory_entities_entity_idx
    ON memory_entities (entity_id);

-- Runtime role gets the same shape as memories/entities/relations
-- (full CRUD). audit_log's REVOKE shape does NOT apply here — this is
-- a mutable derived index, not an immutable audit trail.
GRANT SELECT, INSERT, UPDATE, DELETE ON memory_entities TO kastellan_runtime;
