-- 0004_documents.sql
-- synapse — migration 0004: canonical document manifests + queryable ACLs.
-- documents is the source-of-truth metadata store; chunks/embeddings (0005) are DERIVED and
-- rebuildable. Written by POST /documents.ingest. Mirrors the canonical document schema.
--
-- Canonical shape:
--   { doc_id, tenant_id, team_scope[], source_system, source_uri, title, content_type,
--     language, version, created_at, updated_at, owners[],
--     acl:{users[], groups[], inherit_from_source}, metadata{} }

CREATE TABLE documents (
    -- doc_id is unique PER TENANT, not globally: the natural key is the composite
    -- (tenant_id, doc_id) (see PRIMARY KEY at the end of the table). A globally
    -- unique doc_id would let one tenant's ids collide with — or probe for — another
    -- tenant's (cross-tenant namespace collisions, DoS, existence-probing leaks).
    doc_id        text NOT NULL,                  -- e.g. "runbook" (unique WITHIN its tenant)
    tenant_id     text NOT NULL REFERENCES tenants(tenant_id) ON DELETE CASCADE,
    team_scope    text[] NOT NULL DEFAULT '{}',   -- team slugs the doc is scoped to
    source_system text,                           -- e.g. "google_drive"
    source_uri    text,                           -- e.g. "gdrive://file/abc"
    title         text,
    content_type  text,                           -- e.g. "application/pdf"
    language      text,                           -- e.g. "en"
    version       text,                           -- source version stamp, e.g. "sha256:..."
    owners        text[] NOT NULL DEFAULT '{}',
    -- Inline ACL snapshot matching the canonical acl object. The normalized document_acl
    -- table below makes the same grants queryable at row level.
    acl           jsonb NOT NULL DEFAULT '{"users": [], "groups": [], "inherit_from_source": true}'::jsonb,
    metadata      jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now(),
    -- Composite natural key: a doc_id is unique WITHIN a tenant, so two tenants can
    -- each own a document called "runbook" without colliding.
    PRIMARY KEY (tenant_id, doc_id)
);
COMMENT ON TABLE documents IS 'Canonical, source-of-truth document metadata + ACL snapshot.';

CREATE INDEX idx_documents_tenant ON documents(tenant_id);
CREATE INDEX idx_documents_source ON documents(tenant_id, source_system);
CREATE INDEX idx_documents_team_scope ON documents USING gin (team_scope);
CREATE INDEX idx_documents_owners ON documents USING gin (owners);
CREATE INDEX idx_documents_acl ON documents USING gin (acl jsonb_path_ops);
CREATE INDEX idx_documents_metadata ON documents USING gin (metadata jsonb_path_ops);
CREATE INDEX idx_documents_title_trgm ON documents USING gin (title gin_trgm_ops);

CREATE TRIGGER trg_documents_touch BEFORE UPDATE ON documents
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();

-- Normalized, queryable ACL grants (in addition to the inline documents.acl JSONB).
-- Design principle: make ACLs queryable (row-level), not UI-only. Each grant targets either
-- a user (principal_id) or a group (team_id); `inherited` marks grants mirrored from source.
CREATE TABLE document_acl (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id    text NOT NULL REFERENCES tenants(tenant_id) ON DELETE CASCADE,
    doc_id       text NOT NULL,
    grantee_type text NOT NULL CHECK (grantee_type IN ('user','group')),
    grantee_id   text NOT NULL,                   -- principal_id when 'user', team_id when 'group'
    permission   text NOT NULL DEFAULT 'read' CHECK (permission IN ('read','write','admin')),
    inherited    boolean NOT NULL DEFAULT false,  -- true when mirrored from the source system
    created_at   timestamptz NOT NULL DEFAULT now(),
    -- doc_id is only unique within a tenant, so the grant key includes tenant_id.
    UNIQUE (tenant_id, doc_id, grantee_type, grantee_id, permission),
    -- Composite FK to the per-tenant document key (tenant_id, doc_id).
    FOREIGN KEY (tenant_id, doc_id) REFERENCES documents (tenant_id, doc_id) ON DELETE CASCADE
);
COMMENT ON TABLE document_acl IS 'Row-level, queryable document access grants (users + groups).';

CREATE INDEX idx_document_acl_doc ON document_acl(tenant_id, doc_id);
CREATE INDEX idx_document_acl_grantee ON document_acl(tenant_id, grantee_type, grantee_id);
