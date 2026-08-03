-- 0022_partial_hnsw_per_model.sql
-- synapse — migration 0022: per-model PARTIAL HNSW index for the model-consistency filter (#35).
--
-- WHY (recall, not correctness):
--   The opt-in EMBEDDING_MODEL_CONSISTENCY filter (#35) restricts the VECTOR arm to chunks embedded
--   by the CURRENT model (`c.embedding_model = <model>`). After a model migration the corpus is
--   mixed, but the single full index `idx_chunks_embedding_hnsw` (0005) is one HNSW graph over ALL
--   models' vectors. A filtered ANN over that graph traverses (and spends its ef_search budget on)
--   neighbours that the model predicate then discards, so it can under-return matching rows — the
--   classic "filtered-HNSW recall cliff". A PARTIAL index whose graph contains ONLY the current
--   model's vectors has no foreign neighbours to waste budget on, so recall is preserved.
--
--   This is a pure recall/perf optimisation: results are always correct (the WHERE predicate is
--   applied regardless of which index is chosen); a matching partial index only improves how many
--   of the true nearest same-model neighbours the ANN actually surfaces.
--
-- PLANNER CONTRACT: Postgres only chooses a partial index when the query predicate PROVABLY implies
--   the index predicate. A bound parameter (`embedding_model = $n`) under a generic plan does NOT —
--   it falls back to the full index + a post-filter. So the VECTOR arm inlines the model as a
--   validated literal (`embedding_model = 'text-embedding-3-small'`) when the filter is on; that
--   exact-equality literal matches this index's predicate. (Verified via EXPLAIN.)
--
-- The full index `idx_chunks_embedding_hnsw` is intentionally KEPT: it serves the consistency-OFF
-- path (no model predicate) and any model without its own partial index.
--
-- 'text-embedding-3-small' is the shipped default (EMBEDDING_MODEL / config default / 0021 backfill).
-- A deployment that runs a DIFFERENT active model should add an analogous partial index for it, e.g.
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_chunks_embedding_hnsw_<slug>
--       ON chunks USING hnsw (embedding vector_cosine_ops) WITH (m = 16, ef_construction = 64)
--       WHERE embedding_model = '<that-model>';
-- (On a large existing corpus, build it CONCURRENTLY out of band to avoid locking writes; IF NOT
--  EXISTS then makes redeploys a no-op. This migration is non-concurrent to match 0005.)

CREATE INDEX IF NOT EXISTS idx_chunks_embedding_hnsw_default ON chunks
    USING hnsw (embedding vector_cosine_ops)
    WITH (m = 16, ef_construction = 64)
    WHERE embedding_model = 'text-embedding-3-small';

COMMENT ON INDEX idx_chunks_embedding_hnsw_default IS
    'Partial HNSW (cosine) over the default embedding model — recall-preserving ANN for the model-consistency filter (#35). See 0022.';
