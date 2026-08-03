-- 0017_step_retry.sql
-- synapse — migration 0017: per-step retry/backoff + the background retry driver.
--
-- A step that fails RETRIABLY is rescheduled (status back to 'pending') with a future
-- `next_attempt_at` instead of failing the run; the background worker drives it once the
-- backoff elapses. `attempts`/`max_attempts` (from 0010) bound the retries.

-- NULL = the step is ready to run now (no pending retry). A future value = a scheduled retry
-- that must not run until the backoff elapses (the executor self-guards on this, and the
-- discovery function below only surfaces DUE retries).
ALTER TABLE run_steps ADD COLUMN next_attempt_at timestamptz;

-- Helps the worker's discovery find due retries without scanning every step.
CREATE INDEX idx_run_steps_due_retry ON run_steps (next_attempt_at)
    WHERE status = 'pending' AND next_attempt_at IS NOT NULL;

-- The worker discovers runs that need driving but have no live in-request driver. This
-- GENERALIZES 0016's crash-only discovery to also surface DUE retries; it replaces
-- synapse_list_stale_running_runs. SECURITY DEFINER (RLS-bypassing, read-only) so it spans
-- tenants; the worker reconciles each run per-tenant under RLS. See 0016 for the ownership
-- requirement (must be owned by a superuser/BYPASSRLS role).
DROP FUNCTION IF EXISTS synapse_list_stale_running_runs(bigint);

CREATE OR REPLACE FUNCTION synapse_list_drivable_runs(stale_after_secs bigint)
RETURNS TABLE (run_id uuid, tenant_id text)
LANGUAGE sql
STABLE
SECURITY DEFINER
SET search_path = pg_catalog, public
AS $$
    SELECT r.run_id, r.tenant_id
    FROM runs r
    LEFT JOIN LATERAL (
        -- the leading (lowest-index) non-terminal step decides drivability (NULL if none remain)
        SELECT s.status AS step_status, s.next_attempt_at
        FROM run_steps s
        WHERE s.run_id = r.run_id AND s.status NOT IN ('completed', 'skipped')
        ORDER BY s.step_index
        LIMIT 1
    ) lead ON true
    WHERE r.status = 'running'
      AND (
          -- crashed MID tool-execution (a 'running' step whose driver died): stale by updated_at
          (lead.step_status = 'running'
              AND r.updated_at < now() - make_interval(secs => stale_after_secs))
          -- crashed BETWEEN steps (leading 'pending', no scheduled retry): stale by updated_at
          OR (lead.step_status = 'pending' AND lead.next_attempt_at IS NULL
              AND r.updated_at < now() - make_interval(secs => stale_after_secs))
          -- a retry backoff has elapsed: drive it now, regardless of updated_at staleness
          OR (lead.step_status = 'pending' AND lead.next_attempt_at IS NOT NULL
              AND lead.next_attempt_at <= now())
          -- all steps done but the run was never finalized (crashed before the terminal write)
          OR (lead.step_status IS NULL
              AND r.updated_at < now() - make_interval(secs => stale_after_secs))
      )
    ORDER BY r.updated_at
    LIMIT 100
$$;

COMMENT ON FUNCTION synapse_list_drivable_runs(bigint) IS
    'Crash-recovery + retry discovery: runs needing a background driver across ALL tenants (SECURITY DEFINER, read-only).';
