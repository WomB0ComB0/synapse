-- 0003_context.sql
-- synapse — migration 0003: principal context (the Context Service store).
-- Implements the canonical context/principal schema. Kept separate from org knowledge so
-- personal memory / PII is minimized and governed independently. Written by POST /context.upsert.
--
-- Canonical shape:
--   { principal_id, tenant_id, team_ids[], role, location, approval_limit_usd(number),
--     preferred_tools[], active_projects[], policy_overrides[],
--     data_classification:{contains_pii, special_category}, updated_at }

CREATE TABLE context (
    principal_id        text NOT NULL,
    tenant_id           text NOT NULL REFERENCES tenants(tenant_id) ON DELETE CASCADE,
    team_ids            text[] NOT NULL DEFAULT '{}',
    role                text,
    location            text,
    approval_limit_usd  numeric(14,2),
    preferred_tools     text[] NOT NULL DEFAULT '{}',
    active_projects     text[] NOT NULL DEFAULT '{}',
    policy_overrides    text[] NOT NULL DEFAULT '{}',
    -- { contains_pii: bool, special_category: bool } — drives PII-minimization policy.
    data_classification jsonb NOT NULL DEFAULT '{"contains_pii": false, "special_category": false}'::jsonb,
    metadata            jsonb NOT NULL DEFAULT '{}'::jsonb,
    updated_at          timestamptz NOT NULL DEFAULT now(),
    -- Tenant-scoped identity; the composite FK guarantees a context row can only
    -- reference a principal in the SAME tenant (closes the tenant_id-consistency gap).
    PRIMARY KEY (tenant_id, principal_id),
    FOREIGN KEY (tenant_id, principal_id) REFERENCES principals(tenant_id, principal_id) ON DELETE CASCADE
);
COMMENT ON TABLE context IS 'Per-principal context document (personalization, routing, policy overrides).';

CREATE INDEX idx_context_tenant ON context(tenant_id);
CREATE INDEX idx_context_team_ids ON context USING gin (team_ids);
CREATE INDEX idx_context_active_projects ON context USING gin (active_projects);

CREATE TRIGGER trg_context_touch BEFORE UPDATE ON context
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
