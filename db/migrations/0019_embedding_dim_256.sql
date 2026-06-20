-- 0019_embedding_dim_256.sql
--
-- Pre-reqs: 0001 (baseline — created `memories` + `entities` with
-- `vector(1024)` embedding columns for the original bge-m3 candidate),
-- 0008 (`deleted_memories` audit table, also `vector(1024)`).
--
-- Narrow every embedding column from `vector(1024)` to `vector(256)`.
--
-- WHY 256: the active embedding model is **embeddinggemma**, a
-- Matryoshka-representation-learning (MRL) model. Its leading 256
-- components are an information-dense, self-contained embedding, so the
-- application truncates the model's native 768-dim output to 256 and
-- renormalizes (see `db::memories::truncate_to_embedding_dim`). Storing
-- 256 instead of 1024 cuts embedding storage ~4× and makes cosine ANN
-- proportionally faster, with negligible retrieval-quality loss for an
-- MRL model. (The old `vector(1024)` width was never satisfied in
-- practice — embeddinggemma returns 768, so every embed previously
-- failed the dim gate and recall ran with an empty semantic lane.)
--
-- EXISTING DATA: an embedding stored under the old contract cannot be
-- cast to a different dimension (pgvector rejects the cast), and a
-- 1024-dim vector is not a valid 256-dim Matryoshka prefix anyway.
-- Stale embeddings are therefore discarded (set NULL) before the type
-- change; the rows themselves (body, metadata, graph) are untouched and
-- get a fresh 256-dim embedding the next time they are (re)written. All
-- three columns are nullable, so NULL is a valid resting state. No ANN
-- index exists on any of these columns (0001 deliberately defers it),
-- so there is nothing to drop/rebuild.
--
-- Runs as superuser inside `probe::run`'s migrate step, before SET ROLE
-- to kastellan_runtime — the runtime role never needs DDL rights.

-- memories.embedding (semantic recall lane)
UPDATE memories SET embedding = NULL WHERE embedding IS NOT NULL;
ALTER TABLE memories
    ALTER COLUMN embedding TYPE vector(256) USING embedding::vector(256);

-- entities.embedding (optional entity-similarity; nullable)
UPDATE entities SET embedding = NULL WHERE embedding IS NOT NULL;
ALTER TABLE entities
    ALTER COLUMN embedding TYPE vector(256) USING embedding::vector(256);

-- deleted_memories.embedding (audit copy; nullable, mirrors the source)
UPDATE deleted_memories SET embedding = NULL WHERE embedding IS NOT NULL;
ALTER TABLE deleted_memories
    ALTER COLUMN embedding TYPE vector(256) USING embedding::vector(256);
