-- Per-tenant RBAC override for the Policy & Access Gateway (PR2).
--
-- The gateway's role x action decision is now DB-authoritative:
--   * a caller's role comes from principals.role (loaded under RLS in tenant_tx);
--   * a (tenant, role) that has ANY rows here defines that role's COMPLETE
--     allowlist, REPLACING the in-code default matrix for that role. A role with
--     NO rows falls back to the in-code default. So a tenant can tighten (revoke)
--     OR broaden a role without a code change, while every existing tenant (no
--     rows) keeps the exact default behavior.
--
-- `role`   stores the canonical Role name  ("viewer" | "member").
-- `action` stores the governed action label (Action::as_str(), e.g. "retrieve").

CREATE TABLE role_permissions (
    tenant_id  text NOT NULL REFERENCES tenants(tenant_id) ON DELETE CASCADE,
    -- Constrained to the known vocabularies so a MISTYPED grant fails LOUDLY at
    -- INSERT instead of silently locking a role out (a typo'd action never matches,
    -- so under replace-semantics the role loses that action) or silently no-op'ing.
    -- These mirror Role::as_str() and Action::as_str(); adding a role/action means
    -- a migration here anyway, so the coupling is intentional.
    role       text NOT NULL CHECK (role IN ('viewer', 'member')),
    action     text NOT NULL CHECK (action IN (
        'documents.ingest', 'retrieve', 'context.upsert', 'context.get',
        'skills.register', 'skills.get', 'tool.execute', 'runs.start',
        'runs.resume', 'audit.events'
    )),
    created_at timestamptz NOT NULL DEFAULT now(),
    -- (tenant_id, role) prefix of the PK serves the RLS-scoped `WHERE role = $1`
    -- lookup, so no extra index is needed.
    PRIMARY KEY (tenant_id, role, action)
);
COMMENT ON TABLE role_permissions IS
    'Per-tenant role->action grants; a populated (tenant, role) REPLACES the in-code default matrix for that role.';

-- Tenant isolation: identical policy shape to the other tenant tables (0009/0010).
-- Its own migration because 0009's table list is fixed. FORCE so the policy applies
-- even to a table owner; the app runs as a non-owner, RLS-enforcing role regardless.
ALTER TABLE role_permissions ENABLE ROW LEVEL SECURITY;
ALTER TABLE role_permissions FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON role_permissions
    USING (tenant_id = app_current_tenant_id())
    WITH CHECK (tenant_id = app_current_tenant_id());
