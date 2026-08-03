-- 0027_enterprise_performance_indexes.sql
-- synapse — enterprise query/index hardening.
--
-- These indexes target the first production-scale pressure points:
--   * Gemini deployments need a model-specific HNSW graph just like the shipped
--     OpenAI default; otherwise the model-consistency filter falls back to the
--     all-model graph and can lose recall under filtered ANN traversal.
--   * ACL/group lookups repeatedly ask "which teams is this principal in?"
--     while retrieval evaluates group document grants.
--   * team owners are stored as an array and queried via array membership.
--   * audit pagination sorts by (ts, event_id); composite indexes avoid sorting
--     large tenant logs after filtering.
--   * project scope queries use `(metadata -> 'project_ids') ?| ...`, which is
--     not served well by the existing `jsonb_path_ops` whole-document index.
--
-- On very large live datasets, build these CONCURRENTLY out-of-band first, then
-- let this migration no-op via IF NOT EXISTS during deploy. sqlx migrations run
-- in a transaction by default, so this file intentionally uses normal CREATE INDEX.

CREATE INDEX IF NOT EXISTS idx_chunks_embedding_hnsw_gemini_embedding_2 ON chunks
    USING hnsw (embedding vector_cosine_ops)
    WITH (m = 16, ef_construction = 64)
    WHERE embedding_model = 'gemini-embedding-2';

CREATE INDEX IF NOT EXISTS idx_chunks_tenant_embedding_model_present ON chunks
    (tenant_id, embedding_model)
    WHERE embedding IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_team_members_tenant_principal_team ON team_members
    (tenant_id, principal_id, team_id);

CREATE INDEX IF NOT EXISTS idx_teams_owners ON teams USING gin (owners);

CREATE INDEX IF NOT EXISTS idx_documents_project_ids ON documents
    USING gin ((metadata -> 'project_ids'));

CREATE INDEX IF NOT EXISTS idx_audit_events_tenant_ts_event ON audit_events
    (tenant_id, ts DESC, event_id DESC);

CREATE INDEX IF NOT EXISTS idx_audit_events_action_ts_event ON audit_events
    (tenant_id, action, ts DESC, event_id DESC);

CREATE INDEX IF NOT EXISTS idx_audit_events_principal_ts_event ON audit_events
    (tenant_id, principal_id, ts DESC, event_id DESC);

CREATE INDEX IF NOT EXISTS idx_audit_events_resource_ts_event ON audit_events
    (tenant_id, resource, ts DESC, event_id DESC);

COMMENT ON INDEX idx_chunks_embedding_hnsw_gemini_embedding_2 IS
    'Partial HNSW (cosine) for gemini-embedding-2 deployments; preserves filtered-ANN recall with EMBEDDING_MODEL_CONSISTENCY.';
