-- 0010_run_steps.sql
-- synapse — migration 0010: per-run STEP state for the multi-step run executor.
--
-- A run (migration 0007) is now materialized into an ordered list of steps at
-- start; the executor advances the run through them (see src/orchestration).
-- Steps are the durable unit of progress: on a human-approval step the run
-- suspends `waiting` on a run_checkpoint and resumes into the NEXT step.
--
-- RLS: migration 0009 already ran (its table list is fixed), so this new
-- tenant-scoped table sets up its own ENABLE + FORCE + tenant_isolation policy
-- here, matching the 0009 shape exactly (fail-closed on an unset app.tenant_id).

CREATE TABLE run_steps (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    run_id       uuid NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    tenant_id    text NOT NULL REFERENCES tenants(tenant_id) ON DELETE CASCADE,
    step_index   int  NOT NULL,
    name         text NOT NULL,
    -- 'transform' (placeholder compute), 'human_approval' (suspend/resume gate),
    -- 'fail' (fault-injection). PR2 adds 'tool' (governed connector call).
    kind         text NOT NULL CHECK (kind IN ('transform','human_approval','fail')),
    status       text NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending','running','waiting','completed','failed','skipped')),
    output       jsonb,
    attempts     int  NOT NULL DEFAULT 0,
    max_attempts int  NOT NULL DEFAULT 1,
    error        text,
    -- Deterministic per (run_id, step_index); a stable key future tool steps
    -- (PR2) dedupe external side effects on. Unique within a run.
    idempotency_key text NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    UNIQUE (run_id, step_index),
    UNIQUE (run_id, idempotency_key)
);
COMMENT ON TABLE run_steps IS 'Ordered, durable step state for the run executor.';

-- (run_id, step_index) is already covered by the UNIQUE index above, which the
-- executor's "next pending step ORDER BY step_index" query uses. Only a
-- tenant_id index is added separately (RLS scans + per-tenant reporting).
CREATE INDEX idx_run_steps_tenant ON run_steps(tenant_id);

CREATE TRIGGER trg_run_steps_touch BEFORE UPDATE ON run_steps
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();

ALTER TABLE run_steps ENABLE ROW LEVEL SECURITY;
-- Comment the next line out to allow the table owner to bypass RLS.
ALTER TABLE run_steps FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON run_steps
    USING (tenant_id = app_current_tenant_id())
    WITH CHECK (tenant_id = app_current_tenant_id());
