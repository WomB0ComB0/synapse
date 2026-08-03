-- 0005_chunks.sql
-- synapse — migration 0005: chunk + embedding store powering hybrid retrieval.
-- DERIVED artifact (rebuildable from documents). Each row is stamped with embedding_model +
-- embedding_dimensions + the source document version in metadata, so a rebuild is explainable
-- (versioning principle). Mirrors the canonical chunk schema.
--
-- Canonical shape:
--   { chunk_id, doc_id, tenant_id, ordinal, section_path[], text, token_count, char_start,
--     char_end, embedding_model, embedding_dimensions, vector_ref, sparse_terms_ref, metadata{} }

CREATE TABLE chunks (
    -- chunk_id is unique PER TENANT: it is minted with the tenant + doc prefix
    -- (e.g. "acme::runbook::chunk::0007"), and the keys below include tenant_id so
    -- two tenants can both own a doc "runbook" and its chunks without colliding.
    chunk_id             text NOT NULL,             -- e.g. "acme::runbook::chunk::0007"
    doc_id               text NOT NULL,
    tenant_id            text NOT NULL REFERENCES tenants(tenant_id) ON DELETE CASCADE,
    ordinal              integer NOT NULL,          -- position within the parent document
    section_path         text[] NOT NULL DEFAULT '{}',
    text                 text NOT NULL,
    token_count          integer,
    char_start           integer,
    char_end             integer,
    embedding_model      text,                      -- e.g. "text-embedding-3-small"
    embedding_dimensions integer,                   -- e.g. 1536
    vector_ref           text,                      -- optional external vector ref, e.g. "vec://..."
    sparse_terms_ref     text,                      -- optional external BM25 ref, e.g. "bm25://..."
    -- Dense embedding for semantic ANN search. 1536 dims == text-embedding-3-small (config default).
    -- Nullable: a chunk may exist before its embedding is computed (async ingestion).
    embedding            vector(1536),
    -- Lexical vector for BM25-style / hybrid search, generated from `text`.
    tsv                  tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(text, ''))) STORED,
    metadata             jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at           timestamptz NOT NULL DEFAULT now(),
    updated_at           timestamptz NOT NULL DEFAULT now(),
    -- chunk_id is unique within a tenant; (tenant_id, doc_id, ordinal) is the
    -- per-tenant positional key back to the parent document.
    PRIMARY KEY (tenant_id, chunk_id),
    UNIQUE (tenant_id, doc_id, ordinal),
    -- Composite FK: a chunk belongs to a document identified per tenant.
    FOREIGN KEY (tenant_id, doc_id) REFERENCES documents (tenant_id, doc_id) ON DELETE CASCADE
);
COMMENT ON TABLE chunks IS 'Derived chunk/embedding store for hybrid (vector + lexical) retrieval.';

CREATE INDEX idx_chunks_tenant ON chunks(tenant_id);
CREATE INDEX idx_chunks_doc ON chunks(tenant_id, doc_id);
CREATE INDEX idx_chunks_metadata ON chunks USING gin (metadata jsonb_path_ops);

-- Lexical / hybrid search: GIN over the generated tsvector.
CREATE INDEX idx_chunks_tsv ON chunks USING gin (tsv);

-- Semantic search: HNSW ANN index over the embedding using cosine distance (the `<=>` operator).
-- Cosine matches L2-normalized OpenAI-style embeddings. m / ef_construction are pgvector
-- defaults (16 / 64); tune per corpus + recall target.
CREATE INDEX idx_chunks_embedding_hnsw ON chunks
    USING hnsw (embedding vector_cosine_ops)
    WITH (m = 16, ef_construction = 64);
-- TODO(retrieval): alternative recall/latency profile (build cheaper, tune `lists`):
--   CREATE INDEX idx_chunks_embedding_ivf ON chunks
--       USING ivfflat (embedding vector_cosine_ops) WITH (lists = 100);

CREATE TRIGGER trg_chunks_touch BEFORE UPDATE ON chunks
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
