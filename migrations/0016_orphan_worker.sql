-- 0016_orphan_worker.sql
-- synapse — migration 0016: cross-tenant discovery for the crash-recovery worker.
--
-- The background orphan worker must find runs stuck in 'running' (a driver crashed
-- mid-drive) ACROSS tenants, but the app role is RLS-enforcing (FORCE RLS) and only sees
-- its own tenant. This SECURITY DEFINER function runs as its OWNER (the migration role,
-- which must be a superuser / BYPASSRLS role — the same privilege that provisions tenants),
-- so it can enumerate stale 'running' runs for ALL tenants. It is READ-ONLY and returns
-- only (run_id, tenant_id); the worker then RECONCILES each run in a normal tenant_tx
-- (RLS-enforced), so the cross-tenant privilege boundary is a single, auditable read.
--
-- `stale_after_secs` MUST exceed the maximum tool-call duration (mcp_timeout_secs +
-- connect_timeout) so an actively-driven run (whose updated_at is bumped by the touch
-- trigger on every step write) is never mistaken for an orphan — see the worker default.

CREATE OR REPLACE FUNCTION synapse_list_stale_running_runs(stale_after_secs bigint)
RETURNS TABLE (run_id uuid, tenant_id text)
LANGUAGE sql
STABLE
SECURITY DEFINER
SET search_path = pg_catalog, public
AS $$
    SELECT run_id, tenant_id
    FROM runs
    WHERE status = 'running'
      AND updated_at < now() - make_interval(secs => stale_after_secs)
    ORDER BY updated_at
    LIMIT 100
$$;

COMMENT ON FUNCTION synapse_list_stale_running_runs(bigint) IS
    'Crash-recovery discovery: stale running runs across ALL tenants (SECURITY DEFINER, read-only).';
