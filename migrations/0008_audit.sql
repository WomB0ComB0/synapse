-- 0008_audit.sql
-- synapse — migration 0008: append-only audit event log.
-- Every governed action (retrieval, tool execution, skill registration, run transition,
-- approval decision) should append a row here. Queried by GET /audit/events.
-- Mirrors the canonical audit event schema.
--
-- Canonical shape:
--   { event_id, tenant_id, principal_id, action, resource, outcome, ts, metadata{} }

CREATE TABLE audit_events (
    event_id     uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id    text NOT NULL REFERENCES tenants(tenant_id) ON DELETE CASCADE,
    -- Deliberately NOT a FK: audit must survive principal deletion / external actors.
    principal_id text,
    action       text NOT NULL,                 -- e.g. "retrieve", "tool.execute", "skills.register"
    resource     text,                          -- e.g. "doc_8742", "tool.create_draft"
    outcome      text NOT NULL,                 -- e.g. "allow" | "deny" | "success" | "error"
    ts           timestamptz NOT NULL DEFAULT now(),
    metadata     jsonb NOT NULL DEFAULT '{}'::jsonb
);
COMMENT ON TABLE audit_events IS 'Append-only audit trail for governed actions (GET /audit/events).';

CREATE INDEX idx_audit_events_tenant_ts ON audit_events(tenant_id, ts DESC);
CREATE INDEX idx_audit_events_principal ON audit_events(tenant_id, principal_id);
CREATE INDEX idx_audit_events_action ON audit_events(tenant_id, action);
CREATE INDEX idx_audit_events_resource ON audit_events(tenant_id, resource);
CREATE INDEX idx_audit_events_metadata ON audit_events USING gin (metadata jsonb_path_ops);
