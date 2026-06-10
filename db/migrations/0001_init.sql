-- 0001_init.sql — initial schema for kastellan.
--
-- See docs/devel/handovers/HANDOVER.md "Option C2.2" and
-- docs/devel/ROADMAP.md "Phase 0 cont. — Postgres bring-up" for the
-- design rationale behind every table below. The summary:
--
--   * No external graph DB and no external full-text-search engine —
--     committed to in 2026-05-09 (closed issues #9 + #10 won't-fix).
--     Graph traversal is plain `entities`+`relations` behind the
--     `Graph` trait in `db/src/graph.rs` (recursive CTEs); FTS is
--     native `tsvector`+GIN+`ts_rank`.
--   * Embedding columns are `vector(1024)` to match the leading
--     bge-m3 candidate. Switching embedding models later requires a
--     re-encode regardless of column type, so the dim is committed.
--   * `audit_log` is the on-disk landing zone for the dispatcher
--     chokepoint. Insert-only by convention today; a future
--     migration will REVOKE UPDATE/DELETE from a non-superuser
--     runtime role once that role is split out.
--   * `secrets` columns are sized for AES-256-GCM ciphertext + a
--     12-byte nonce; the wrapping key lives in the OS keyring
--     (libsecret on Linux, Keychain on macOS), never in the DB.

-- pgvector enables the `vector(N)` column type used by `memories`
-- and `entities`. `IF NOT EXISTS` keeps re-running the migration
-- (e.g. against a partially-bootstrapped cluster) cheap.
CREATE EXTENSION IF NOT EXISTS vector;

-- ─── audit_log ────────────────────────────────────────────────────
-- Append-only operational record of every tool call, LLM call,
-- channel I/O, and memory write. Strictly monotonic `id`
-- (`BIGSERIAL`) so a tail-since-id reader cannot miss rows; `ts`
-- is convenience for human readers, never the primary ordering.
--
-- Append-only is enforced by application discipline today (the
-- dispatcher in `core::tool_host::dispatch()` is the only writer).
-- Once a non-superuser runtime role is split out, this table will
-- gain `REVOKE UPDATE, DELETE ON audit_log FROM <runtime_role>`.
CREATE TABLE audit_log (
    id      BIGSERIAL   PRIMARY KEY,
    ts      TIMESTAMPTZ NOT NULL DEFAULT now(),
    actor   TEXT        NOT NULL,
    action  TEXT        NOT NULL,
    payload JSONB       NOT NULL DEFAULT '{}'::jsonb
);
CREATE INDEX audit_log_ts_idx       ON audit_log (ts);
CREATE INDEX audit_log_actor_ts_idx ON audit_log (actor, ts);

-- ─── tasks ────────────────────────────────────────────────────────
-- Scheduler queue. Phase 1 fills in the state-machine semantics;
-- this slice just pins the shape so the scheduler can be wired
-- without another migration.
--
-- `state` is a free-form TEXT with a CHECK constraint instead of a
-- Postgres ENUM type. ENUMs lock the value set at column-creation
-- time and adding a value requires `ALTER TYPE … ADD VALUE` in its
-- own transaction — extra friction for no benefit at this corpus
-- size. The CHECK is cheap to update via a follow-up migration.
CREATE TABLE tasks (
    id         BIGSERIAL   PRIMARY KEY,
    state      TEXT        NOT NULL DEFAULT 'pending'
               CHECK (state IN ('pending', 'running',
                                'completed', 'failed', 'cancelled')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    payload    JSONB       NOT NULL DEFAULT '{}'::jsonb
);
CREATE INDEX tasks_state_created_at_idx ON tasks (state, created_at);

-- ─── memories ────────────────────────────────────────────────────
-- Free-form recall corpus. Three independent retrieval shapes,
-- fused by `memory::recall` in Phase 1 via Reciprocal Rank Fusion:
--
--   1. embedding — semantic recall via `vector(1024)` cosine ANN
--      (bge-m3 dim; `<=>` operator from pgvector).
--   2. tsv       — lexical recall via Postgres GIN(tsvector) and
--      `ts_rank`. Generated column means we cannot forget to
--      maintain it on UPDATE.
--   3. metadata  — JSONB filters (workspace, channel, source URL).
--
-- The HNSW ANN index on `embedding` is intentionally NOT created
-- here. Build cost is dominated by the row count at index-creation
-- time; building against an empty table just to grow it row-by-row
-- is strictly worse than building once after Phase 1's first batch
-- ingest. The Phase 1 first-load step adds:
--   CREATE INDEX memories_embedding_hnsw
--   ON memories USING hnsw (embedding vector_cosine_ops);
-- (see "Phase 1 — Memory & Loop" in ROADMAP).
CREATE TABLE memories (
    id         BIGSERIAL   PRIMARY KEY,
    body       TEXT        NOT NULL,
    metadata   JSONB       NOT NULL DEFAULT '{}'::jsonb,
    embedding  vector(1024),
    -- `simple` config (no language stemming) keeps recall
    -- predictable across multilingual content; the agent corpus is
    -- not English-only. Phase 1 may add a per-row language hint and
    -- swap to a CASE expression here if recall quality demands it.
    tsv        tsvector    GENERATED ALWAYS AS
               (to_tsvector('simple', body)) STORED,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX memories_tsv_idx      ON memories USING GIN (tsv);
CREATE INDEX memories_metadata_idx ON memories USING GIN (metadata);

-- ─── entities ────────────────────────────────────────────────────
-- Nodes in the knowledge graph. (kind, name) is the natural key —
-- two facts about the same person/place/concept dedupe into one
-- row. `attrs` is JSONB so kind-specific schema lives in code, not
-- in DDL. `embedding` is optional (semantic similarity over
-- entities is useful but not required for graph traversal).
CREATE TABLE entities (
    id         BIGSERIAL   PRIMARY KEY,
    kind       TEXT        NOT NULL,
    name       TEXT        NOT NULL,
    attrs      JSONB       NOT NULL DEFAULT '{}'::jsonb,
    embedding  vector(1024),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (kind, name)
);
CREATE INDEX entities_kind_idx  ON entities (kind);
CREATE INDEX entities_attrs_idx ON entities USING GIN (attrs);

-- ─── relations ───────────────────────────────────────────────────
-- Edges in the knowledge graph. Multi-edges are allowed
-- intentionally — two different observations about the same pair
-- (`(src, kind, dst)` triple) coexist as separate rows so the
-- chronology in `created_at` is preserved.
--
-- Cascading delete from `entities` keeps the graph internally
-- consistent: removing a node never leaves a dangling edge that
-- would break recursive-CTE traversal mid-walk.
CREATE TABLE relations (
    id         BIGSERIAL   PRIMARY KEY,
    src_id     BIGINT      NOT NULL REFERENCES entities (id) ON DELETE CASCADE,
    dst_id     BIGINT      NOT NULL REFERENCES entities (id) ON DELETE CASCADE,
    kind       TEXT        NOT NULL,
    attrs      JSONB       NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX relations_src_kind_idx ON relations (src_id, kind);
CREATE INDEX relations_dst_kind_idx ON relations (dst_id, kind);

-- ─── secrets ─────────────────────────────────────────────────────
-- Encrypted at rest. Column shapes match AES-256-GCM:
--   * `ciphertext` — output of GCM (variable-length).
--   * `nonce`      — 12 bytes (the only sane GCM nonce length;
--                    enforced by the Rust encrypt path, not by a
--                    DB CHECK so future algorithms can re-use the
--                    column).
--   * `aad`        — additional-authenticated-data (e.g. the
--                    secret's `name` is bound to the ciphertext
--                    via AAD so a row swap detaches the auth tag
--                    and decryption fails).
--   * `key_id`     — string identifier for the wrapping key in the
--                    OS keyring. The wrapping key itself never
--                    enters the database.
--
-- The runtime encrypt/decrypt path lands in a later Phase 0 slice
-- (see ROADMAP "Secrets at rest"); this migration just pins the
-- columns so a schema migration is not required when that lands.
CREATE TABLE secrets (
    id         BIGSERIAL   PRIMARY KEY,
    name       TEXT        NOT NULL UNIQUE,
    ciphertext BYTEA       NOT NULL,
    nonce      BYTEA       NOT NULL,
    aad        BYTEA       NOT NULL DEFAULT ''::bytea,
    key_id     TEXT        NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
