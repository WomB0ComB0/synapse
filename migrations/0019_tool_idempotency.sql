-- 0019_tool_idempotency.sql
-- synapse — migration 0019: client idempotency key for /tool.execute + the stuck-intent reconciler.
--
-- A tool call is a NON-idempotent side effect: today a client that retries a failed /tool.execute
-- re-invokes the connector (a duplicate write). An optional client `Idempotency-Key` makes the
-- retry SAFE — a repeat REPLAYS the original outcome (executed→result, pending→pending,
-- failed→502) and, while the original is still in-flight (`approved`), returns 409 rather than
-- ever re-calling the connector. The committed unique-index claim is the cross-request mutex.

ALTER TABLE tool_executions ADD COLUMN idempotency_key text;

-- Tenant-scoped (tenant-first key → no cross-tenant collision/probe); PARTIAL so keyless calls
-- (NULL) stay at-least-once (NULLs never conflict). tool_executions is under FORCE RLS (0009).
CREATE UNIQUE INDEX uq_tool_executions_idempotency ON tool_executions (tenant_id, idempotency_key)
    WHERE idempotency_key IS NOT NULL;

-- started_at (set just before the out-of-tx connector call) and finished_at (set when the row is
-- finalized) already exist (0007); they are now POPULATED so the reconciler below can tell
-- 'never attempted' (started_at NULL) from 'maybe ran' (started_at set, finished_at NULL).

-- Cross-tenant discovery for the ad-hoc stuck-intent reconciler: an ad-hoc call that crashes
-- between the pre-call `approved` commit and the best-effort finalize strands an `approved`,
-- run_id-NULL row forever (the RUNS reconciler, migration 0017, only sees run_id-bearing rows),
-- so a same-key duplicate would 409 forever. This SECURITY DEFINER (RLS-bypassing, read-only)
-- function lists such stale rows ACROSS tenants; the worker fails each safely, per-tenant under
-- RLS. Owned by a superuser/BYPASSRLS role (see 0016/0017). `stale_after_secs` MUST exceed the
-- max tool-call duration so a legitimately in-flight call is never reconciled.
CREATE OR REPLACE FUNCTION synapse_list_stuck_tool_intents(stale_after_secs bigint)
RETURNS TABLE (id uuid, tenant_id text)
LANGUAGE sql
STABLE
SECURITY DEFINER
SET search_path = pg_catalog, public
AS $$
    SELECT id, tenant_id
    FROM tool_executions
    WHERE run_id IS NULL
      AND status = 'approved'
      AND created_at < now() - make_interval(secs => stale_after_secs)
    ORDER BY created_at
    LIMIT 100
$$;

COMMENT ON FUNCTION synapse_list_stuck_tool_intents(bigint) IS
    'Crash-recovery discovery: stale ad-hoc (run_id NULL) approved tool intents across ALL tenants (SECURITY DEFINER, read-only).';
