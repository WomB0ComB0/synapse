-- 0002_tenancy.sql
-- synapse — migration 0002: tenants, principals, teams, team_members.
-- The identity + isolation backbone. tenant_id is the canonical string id used by the
-- RLS guard (migration 0009). Personalization/context lives in `context` (migration 0003).

-- Top-level isolation boundary.
CREATE TABLE tenants (
    tenant_id  text PRIMARY KEY,                 -- canonical string id, e.g. "acme"
    name       text NOT NULL,
    status     text NOT NULL DEFAULT 'active' CHECK (status IN ('active','suspended')),
    metadata   jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
COMMENT ON TABLE tenants IS 'Top-level tenant/org isolation boundary; tenant_id keys the RLS guard.';

CREATE TRIGGER trg_tenants_touch BEFORE UPDATE ON tenants
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();

-- Principals: identity anchor for a user or agent. Mirrors the identity subset of the
-- canonical context/principal schema (principal_id, tenant_id, role). The mutable
-- personalization document lives in `context` so personal memory can be minimized/governed.
CREATE TABLE principals (
    principal_id text NOT NULL,                  -- e.g. "user_9281"; unique WITHIN a tenant
    tenant_id    text NOT NULL REFERENCES tenants(tenant_id) ON DELETE CASCADE,
    kind         text NOT NULL DEFAULT 'user' CHECK (kind IN ('user','agent','service')),
    display_name text,
    role         text,                           -- e.g. "manager"
    metadata     jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    -- Identity is TENANT-SCOPED (composite key), consistent with documents/chunks/
    -- skills. Two tenants may independently use the same principal_id with no
    -- collision — closing the cross-tenant existence-oracle + id-squatting vectors
    -- that a global principal_id PK would expose via context.upsert.
    PRIMARY KEY (tenant_id, principal_id)
);
COMMENT ON TABLE principals IS 'User/agent identity anchor; X-Principal-Id resolves here.';

CREATE INDEX idx_principals_tenant ON principals(tenant_id);

CREATE TRIGGER trg_principals_touch BEFORE UPDATE ON principals
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();

-- Teams: tenant-scoped groups. (tenant_id, team_id) is the natural key referenced by ACLs
-- and team_members; a surrogate uuid keeps joins cheap.
CREATE TABLE teams (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id  text NOT NULL REFERENCES tenants(tenant_id) ON DELETE CASCADE,
    team_id    text NOT NULL,                    -- canonical team slug, e.g. "finance"
    name       text,
    metadata   jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, team_id)
);
COMMENT ON TABLE teams IS 'Tenant-scoped teams; (tenant_id, team_id) is the ACL grantee/group key.';

CREATE INDEX idx_teams_tenant ON teams(tenant_id);

CREATE TRIGGER trg_teams_touch BEFORE UPDATE ON teams
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();

-- Team membership (principals <-> teams), kept queryable for ACL evaluation at retrieval time
-- (design principle: make ACLs queryable, not UI-only).
CREATE TABLE team_members (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id    text NOT NULL REFERENCES tenants(tenant_id) ON DELETE CASCADE,
    team_id      text NOT NULL,
    principal_id text NOT NULL,
    role         text,                           -- role within the team, e.g. 'member' | 'lead'
    created_at   timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, team_id, principal_id),
    FOREIGN KEY (tenant_id, team_id) REFERENCES teams(tenant_id, team_id) ON DELETE CASCADE,
    FOREIGN KEY (tenant_id, principal_id) REFERENCES principals(tenant_id, principal_id) ON DELETE CASCADE
);
COMMENT ON TABLE team_members IS 'Queryable membership edges for permission-aware retrieval.';

CREATE INDEX idx_team_members_principal ON team_members(principal_id);
CREATE INDEX idx_team_members_team ON team_members(tenant_id, team_id);
