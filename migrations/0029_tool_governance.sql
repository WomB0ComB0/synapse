-- Server-owned tool registry and complete ad-hoc approval/rollback lifecycle.
--
-- A caller may request a tool by id, but only an enabled registry entry can
-- reach the outbound connector. The row is the tenant authoritative policy:
-- argument schema, connector scopes, mandatory approval, and compensation tool.

CREATE TABLE tool_definitions (
    tenant_id          text NOT NULL REFERENCES tenants(tenant_id) ON DELETE CASCADE,
    tool_id            text NOT NULL,
    description        text NOT NULL DEFAULT '',
    input_schema       jsonb NOT NULL DEFAULT '{"type":"object"}'::jsonb,
    required_scopes    text[] NOT NULL DEFAULT '{}',
    approval_mode      text NOT NULL DEFAULT 'required'
        CHECK (approval_mode IN ('none', 'required')),
    rollback_tool_id   text,
    enabled            boolean NOT NULL DEFAULT false,
    revision           bigint NOT NULL DEFAULT 1 CHECK (revision > 0),
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, tool_id),
    CHECK (length(btrim(tool_id)) BETWEEN 1 AND 255),
    CHECK (jsonb_typeof(input_schema) = 'object'),
    CHECK (rollback_tool_id IS NULL OR btrim(rollback_tool_id) <> tool_id),
    CHECK (array_position(required_scopes, NULL) IS NULL)
);

COMMENT ON TABLE tool_definitions IS
    'Tenant-scoped server authority for connector tool ids, schemas, scopes, approval, and rollback.';

ALTER TABLE tool_definitions ENABLE ROW LEVEL SECURITY;
ALTER TABLE tool_definitions FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON tool_definitions
    USING (tenant_id = app_current_tenant_id())
    WITH CHECK (tenant_id = app_current_tenant_id());

CREATE INDEX idx_tool_definitions_enabled
    ON tool_definitions (tenant_id, tool_id) WHERE enabled;

ALTER TABLE tool_executions
    ADD COLUMN definition_revision bigint,
    ADD COLUMN decided_by text,
    ADD COLUMN decision_reason text,
    ADD COLUMN decided_at timestamptz,
    ADD COLUMN rollback_of uuid REFERENCES tool_executions(id) ON DELETE RESTRICT;

CREATE UNIQUE INDEX uq_tool_executions_rollback_once
    ON tool_executions (tenant_id, rollback_of)
    WHERE rollback_of IS NOT NULL;

CREATE INDEX idx_tool_executions_pending_ad_hoc
    ON tool_executions (tenant_id, created_at, id)
    WHERE run_id IS NULL AND status = 'pending';

ALTER TABLE role_permissions DROP CONSTRAINT role_permissions_action_check;
ALTER TABLE role_permissions ADD CONSTRAINT role_permissions_action_check
    CHECK (action IN (
        'documents.ingest', 'retrieve', 'context.upsert', 'context.get',
        'skills.register', 'skills.get', 'tool.execute', 'runs.start',
        'runs.resume', 'audit.events',
        'teams.create', 'teams.add_member', 'teams.remove_member', 'teams.list', 'teams.members',
        'documents.grant', 'documents.revoke',
        'tools.register', 'tools.list', 'tools.decide', 'tools.rollback'
    )) NOT VALID;
ALTER TABLE role_permissions VALIDATE CONSTRAINT role_permissions_action_check;
