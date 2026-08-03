-- 0021_embedding_model_backfill.sql
-- synapse — migration 0021: backfill chunks.embedding_model for the model-consistency filter.
--
-- The opt-in EMBEDDING_MODEL_CONSISTENCY retrieval filter compares the query's model against
-- `chunks.embedding_model` (an equality predicate on the vector arm). Ingest has always stamped
-- that column (src/api/documents.rs), but a chunk that somehow carries an embedding with a NULL
-- model would be silently EXCLUDED once the filter is on (NULL = 'x' is not true). Backfill any
-- such rows to the historical default model so enabling the flag can't hide legacy data.
--
-- NOTE: 'text-embedding-3-small' is the shipped default (EMBEDDING_MODEL). A deployment that ran
-- a DIFFERENT embedding model for these rows must adjust this value before enabling the flag.
UPDATE chunks
SET embedding_model = 'text-embedding-3-small'
WHERE embedding_model IS NULL
  AND embedding IS NOT NULL;
