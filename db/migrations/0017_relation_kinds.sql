-- 0017_relation_kinds.sql
--
-- Pre-reqs: 0001 (relations baseline), 0015 (entity_kinds — this
-- migration intentionally mirrors that shape), 0016 (entity_kinds
-- REVOKE pattern this migration applies preemptively).
--
-- Adds the relation_kinds lookup table — symmetric to entity_kinds —
-- so the GLiNER-Relex extractor's relation-extraction pass has a
-- defined, operator-managed vocabulary instead of `vec![]` (empty,
-- which switches the worker to entity-only mode and silently drops
-- every triple).
--
-- Design choices, with reasons:
--
--   (1) Single migration (CREATE + GRANT + REVOKE all here) rather
--       than the 0015/0016 split. The split existed only because
--       0015's redundant `GRANT SELECT` masked the silent CRUD pickup
--       from 0002's `ALTER DEFAULT PRIVILEGES`, which we patched up
--       in 0016. A fresh table can skip the bug-and-fix dance and
--       write the REVOKE alongside the CREATE — same pattern as 0008
--       (`deleted_memories`) and 0002 (`audit_log`).
--
--   (2) `undefined` is the FK fallback for `ON DELETE SET DEFAULT`
--       and MUST never be removed by operator action. Mirrors
--       `entity_kinds.undefined`.
--
--   (3) Seed vocabulary is intentionally small and biased toward the
--       clinical/medical domain that the entity_kinds taxonomy
--       already favours (patient, doctor, drug, treatment, …). The
--       18 starter seeds (plus `undefined` for FK fallback, 19 total)
--       cover the common relation shapes a clinical note would
--       surface; operators extend via direct `INSERT INTO
--       relation_kinds` (no automatic widening from the extractor —
--       a foreign label coming back from GLiNER must be rejected,
--       not silently added).
--
--   (4) FK from `relations.kind` to `relation_kinds.kind` with
--       `ON DELETE SET DEFAULT` + `ON UPDATE CASCADE`. Same shape as
--       the `entities.kind` FK introduced by 0015. CASCADE on UPDATE
--       so operators can rename a relation kind (e.g. "prescribed" →
--       "prescribed_for") without breaking historical rows.
--
--   (5) `relations.kind` keeps its `NOT NULL` constraint (preserved
--       from 0001). Setting a default of `'undefined'` makes the FK's
--       `SET DEFAULT` semantics well-defined when an operator deletes
--       a kind that's still referenced.
--
--   (6) Runtime role gets SELECT only. Adding a new relation kind is
--       a deliberate operator action, not something the agent or
--       extractor does. The REVOKE undoes the silent full-CRUD pickup
--       from 0002's `ALTER DEFAULT PRIVILEGES IN SCHEMA public ...
--       GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO
--       kastellan_runtime`.

BEGIN;

-- (1) Lookup table for valid relation kinds.
CREATE TABLE relation_kinds (
    kind        TEXT        PRIMARY KEY,
    description TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- (2) Seed vocabulary. `undefined` first so it lands before the FK
--     is added (the FK's SET DEFAULT target must exist).
INSERT INTO relation_kinds (kind, description) VALUES
    ('undefined',            'Fallback kind when the original was removed (DO NOT DELETE)'),
    ('treats',               'Treatment relation: subject treats object'),
    ('prescribed',           'Prescription relation: subject prescribed object'),
    ('diagnosed with',       'Diagnosis relation: subject diagnosed with object'),
    ('has symptom',          'Clinical-presentation relation: subject has symptom object'),
    ('side effect of',       'Adverse-effect relation: subject is a side effect of object'),
    ('contraindicated with', 'Safety relation: subject contraindicated with object'),
    ('allergic to',          'Allergy relation: subject allergic to object'),
    ('located in',           'Spatial/anatomical containment'),
    ('employed by',          'Employment relation'),
    ('works at',             'Workplace relation (looser than employed by)'),
    ('member of',            'Membership in an organisation or group'),
    ('owns',                 'Possessive relation (subject owns object)'),
    ('knows',                'Social relation (subject knows object)'),
    ('identified as',        'Identification relation (subject is identified by/as object)'),
    ('refers to',            'Reference relation (record/document referring to entity)'),
    ('occurred on',          'Temporal relation (event occurred on date)'),
    ('associated with',      'Generic association (fallback when no narrower kind fits)'),
    ('relative of',          'Family/kinship relation');

-- (3) Backfill any pre-existing relations.kind values. Production
--     `relations` is empty today (extractor has been running with
--     empty relation_labels), so this is a no-op in practice but a
--     correctness safety net.
INSERT INTO relation_kinds (kind)
SELECT DISTINCT kind FROM relations
ON CONFLICT (kind) DO NOTHING;

-- (4) Default + FK from relations.kind.
ALTER TABLE relations ALTER COLUMN kind SET DEFAULT 'undefined';

ALTER TABLE relations
    ADD CONSTRAINT relations_kind_fk
    FOREIGN KEY (kind) REFERENCES relation_kinds(kind)
    ON UPDATE CASCADE
    ON DELETE SET DEFAULT;

-- (5) GRANT shape. Same as entity_kinds post-0016: runtime can read
--     but not write the operator-managed vocabulary. ALTER DEFAULT
--     PRIVILEGES from 0002 silently granted full CRUD on every
--     newly-created table, so the REVOKE is load-bearing.
GRANT  SELECT                          ON relation_kinds TO kastellan_runtime;
REVOKE INSERT, UPDATE, DELETE, TRUNCATE ON relation_kinds FROM kastellan_runtime;

COMMIT;
