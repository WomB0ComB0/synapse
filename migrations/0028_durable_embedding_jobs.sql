-- 0028_durable_embedding_jobs.sql
-- Persist canonical source text and a generation-guarded embedding job on each
-- document. Chunks can be committed immediately with NULL embeddings, making
-- lexical retrieval available while Gemini is unavailable; the worker retries
-- the idempotent embedding operation later.

ALTER TABLE documents
    ADD COLUMN content text NOT NULL DEFAULT '',
    ADD COLUMN content_sha256 text,
    ADD COLUMN ingestion_status text NOT NULL DEFAULT 'ready',
    ADD COLUMN embedding_job_id uuid,
    ADD COLUMN embedding_model text,
    ADD COLUMN embedding_attempts integer NOT NULL DEFAULT 0,
    ADD COLUMN embedding_max_attempts integer NOT NULL DEFAULT 8,
    ADD COLUMN next_embedding_attempt_at timestamptz,
    ADD COLUMN embedding_started_at timestamptz,
    ADD COLUMN embedding_completed_at timestamptz,
    ADD COLUMN embedding_last_error text;

ALTER TABLE documents
    ADD CONSTRAINT documents_ingestion_status_check
        CHECK (ingestion_status IN ('pending', 'processing', 'retry', 'ready', 'failed')),
    ADD CONSTRAINT documents_embedding_attempts_check
        CHECK (embedding_attempts >= 0 AND embedding_max_attempts > 0);

CREATE INDEX idx_documents_due_embedding
    ON documents (next_embedding_attempt_at, updated_at)
    WHERE ingestion_status IN ('pending', 'processing', 'retry');

COMMENT ON COLUMN documents.content IS
    'Canonical source text retained so chunks and embeddings are reproducibly rebuildable.';
COMMENT ON COLUMN documents.embedding_job_id IS
    'Generation guard: an older embedding attempt cannot finalize over a newer re-ingest.';
COMMENT ON COLUMN documents.ingestion_status IS
    'Durable embedding lifecycle: pending, processing, retry, ready, or failed.';

-- Cross-tenant discovery is the only privileged part of the embedding worker.
-- The returned identifiers are reconciled under the normal RLS-enforcing app
-- role in a tenant-scoped transaction. Processing jobs are rediscovered only
-- after the staleness window, covering a process crash after lease acquisition.
CREATE OR REPLACE FUNCTION synapse_list_due_embedding_jobs(
    stale_after_secs bigint,
    target_embedding_model text
)
RETURNS TABLE (tenant_id text, doc_id text, embedding_job_id uuid)
LANGUAGE sql
STABLE
SECURITY DEFINER
SET search_path = pg_catalog, public
AS $$
    SELECT d.tenant_id, d.doc_id, d.embedding_job_id
    FROM documents d
    WHERE d.embedding_job_id IS NOT NULL
      AND d.embedding_model = target_embedding_model
      AND (
          d.ingestion_status = 'pending'
          OR (d.ingestion_status = 'retry'
              AND coalesce(d.next_embedding_attempt_at, now()) <= now())
          OR (d.ingestion_status = 'processing'
              AND d.embedding_started_at < now() - make_interval(secs => stale_after_secs))
      )
    ORDER BY coalesce(d.next_embedding_attempt_at, d.updated_at), d.tenant_id, d.doc_id
    LIMIT 100
$$;

COMMENT ON FUNCTION synapse_list_due_embedding_jobs(bigint, text) IS
    'Read-only cross-tenant discovery for due/stale embedding jobs; reconciliation remains RLS-scoped.';
