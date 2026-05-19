-- 0015_entity_kinds_and_quarantine.sql
--
-- Pre-reqs: 0001 (entities/relations baseline).
-- Adds the entity_kinds lookup table seeded with default kinds, the
-- entities.quarantine flag (DEFAULT TRUE so newly-extracted entities
-- stay out of graph results until operator review), and the
-- name_norm dedup key replacing the byte-exact (kind, name) uniqueness.

BEGIN;

-- (1) Lookup table for valid entity kinds. Operator extends via INSERT.
CREATE TABLE entity_kinds (
    kind        TEXT        PRIMARY KEY,
    description TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- (2) Seed taxonomy.
--
--     `undefined` is the FK fallback for ON DELETE SET DEFAULT and
--     must never be removed by operator action.
INSERT INTO entity_kinds (kind, description) VALUES
    ('undefined',     'Fallback kind when the original was removed (DO NOT DELETE)'),
    ('person',        'A specific named individual'),
    ('patient',       'A clinical-context individual receiving care'),
    ('doctor',        'A medical practitioner'),
    ('nurse',         'A nursing practitioner'),
    ('organization',  'A named institution or organisation'),
    ('place',         'A geographic or physical location'),
    ('address',       'A postal or street address'),
    ('phone number',  'A telephone number'),
    ('identifier',    'A reference identifier (case number, patient id, ticket id, etc.)'),
    ('drug',          'A medication, pharmaceutical agent, or substance'),
    ('treatment',     'A procedure, intervention, or therapy'),
    ('disease',       'A diagnosis, disorder, or medical condition'),
    ('infection',     'A specific infectious disease or pathogen'),
    ('symptom',       'A clinical sign or complaint'),
    ('system',        'A software system, service, or technical component'),
    ('file',          'A file, document, or path'),
    ('object',        'A physical or virtual object (device, vehicle, artefact)'),
    ('concept',       'An abstract concept, topic, or idea'),
    ('date',          'A calendar date or time reference');

-- (3) Backfill any pre-existing entities.kind values.
INSERT INTO entity_kinds (kind)
SELECT DISTINCT kind FROM entities
ON CONFLICT (kind) DO NOTHING;

-- (4) Default + FK from entities.kind.
ALTER TABLE entities ALTER COLUMN kind SET DEFAULT 'undefined';

ALTER TABLE entities
    ADD CONSTRAINT entities_kind_fk
    FOREIGN KEY (kind) REFERENCES entity_kinds(kind)
    ON UPDATE CASCADE
    ON DELETE SET DEFAULT;

-- (5) Quarantine flag.
ALTER TABLE entities
    ADD COLUMN quarantine BOOLEAN NOT NULL DEFAULT TRUE;

-- (6) Normalized name column for case/whitespace-insensitive dedup.
--     SQL backfill is best-effort for ASCII; the Rust normalize is the
--     source of truth going forward. `entities` is empty in production
--     today so the backfill is a no-op in practice.
ALTER TABLE entities ADD COLUMN name_norm TEXT;
UPDATE entities SET name_norm =
    lower(regexp_replace(trim(name), '\s+', ' ', 'g'));
ALTER TABLE entities ALTER COLUMN name_norm SET NOT NULL;

ALTER TABLE entities DROP CONSTRAINT entities_kind_name_key;
CREATE UNIQUE INDEX entities_kind_name_norm_idx
    ON entities (kind, name_norm);

-- (7) Partial index for the production hot path.
CREATE INDEX entities_unquarantined_idx
    ON entities (kind, name)
    WHERE quarantine = FALSE;

-- (8) GRANT shape. Runtime role needs SELECT on entity_kinds for the
--     extractor's startup label-list resolution. INSERT on entity_kinds
--     is operator-only by GRANT default — adding a kind is a deliberate
--     act, not something the agent or extractor does.
GRANT SELECT ON entity_kinds TO hhagent_runtime;

COMMIT;
