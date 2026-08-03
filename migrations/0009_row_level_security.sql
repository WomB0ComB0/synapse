-- 0009_row_level_security.sql
-- synapse — migration 0009: enable Row-Level Security + tenant-isolation policies on every
-- tenant-scoped table. This is the query-time half of the governance model; see migration 0001
-- for the app.tenant_id GUC contract and app_current_tenant_id() helper.
--
-- POLICY SHAPE (applied per table, command = ALL):
--   USING      (tenant_id = app_current_tenant_id())   -- rows visible to read/update/delete
--   WITH CHECK (tenant_id = app_current_tenant_id())   -- rows allowed on insert/update
--
-- With app.tenant_id unset, app_current_tenant_id() returns NULL and `tenant_id = NULL` is never
-- true, so NO rows are visible or writable => fail-closed / deny-by-default.
--
-- ENFORCEMENT MODEL / OPERATIONS:
--   * Run the APPLICATION as a NON-owner role. A table's OWNER bypasses RLS unless it is FORCED.
--   * `FORCE ROW LEVEL SECURITY` (below) makes policies apply even to the table owner — the safest
--     default for a governed brain. If you run migrations AND the app as the same owner role and
--     prefer owner-bypass, comment out the FORCE line in the loop.
--   * ADMIN / provisioning tasks (creating tenants, seeding, cross-tenant reports) should use a
--     dedicated role created with the BYPASSRLS attribute, or set app.tenant_id per statement.
--
-- Example app role wiring (run once by an admin; NOT part of this migration):
--   CREATE ROLE synapse_app LOGIN PASSWORD '...';
--   GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO synapse_app;
--   GRANT USAGE ON ALL SEQUENCES IN SCHEMA public TO synapse_app;
--   -- per request / transaction, before touching tenant data:
--   SELECT set_config('app.tenant_id', $1, true);   -- $1 = the request's X-Tenant-Id
--
-- Note on `tenants`: its own guard column IS tenant_id (the PK), so a request may only see/modify
-- its own tenant row. Provision new tenants via a BYPASSRLS admin role (or by setting app.tenant_id
-- to the new id inside the same transaction that inserts it).

DO $$
DECLARE
    t text;
    tenant_tables text[] := ARRAY[
        'tenants', 'principals', 'teams', 'team_members', 'context',
        'documents', 'document_acl', 'chunks', 'skills',
        'runs', 'run_events', 'run_checkpoints', 'tool_executions', 'audit_events'
    ];
BEGIN
    FOREACH t IN ARRAY tenant_tables LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY;', t);
        -- Comment the next line out to allow the table owner to bypass RLS.
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY;', t);
        EXECUTE format(
            'CREATE POLICY tenant_isolation ON %I'
            || ' USING (tenant_id = app_current_tenant_id())'
            || ' WITH CHECK (tenant_id = app_current_tenant_id());',
            t
        );
    END LOOP;
END;
$$;
