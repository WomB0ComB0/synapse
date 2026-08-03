-- 0007_runs.sql
-- synapse — migration 0007: durable workflow orchestration + the policy-guarded tool gateway log.
--   runs             — durable run state machine (POST /runs.start, /runs.resume)
--   run_events       — append-only event history (episodic run memory)
--   run_checkpoints  — pause/resume + human-approval interrupt points
--   tool_executions  — policy-guarded tool/connector (MCP) gateway invocations (POST /tool.execute)
--
-- Canonical shapes:
--   runs.start:   { tenant_id, run_type, workflow_id, input{}, callbacks:{human_approval, webhook} }
--   runs.resume:  { run_id, token, resume_input{} }
--   tool.execute: { tenant_id, principal_id, tool_id, arguments{}, policy:{approval_mode, reason} }

CREATE TABLE runs (
    run_id       uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id    text NOT NULL REFERENCES tenants(tenant_id) ON DELETE CASCADE,
    principal_id text,                                                          -- who started it (tenant-scoped)
    run_type     text NOT NULL,
    workflow_id  text NOT NULL,
    status       text NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending','running','waiting','suspended','completed','failed','cancelled')),
    input        jsonb NOT NULL DEFAULT '{}'::jsonb,
    output       jsonb,
    callbacks    jsonb NOT NULL DEFAULT '{}'::jsonb,   -- { human_approval, webhook }
    error        text,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    -- Composite FK to the tenant-scoped principal; on delete null ONLY principal_id
    -- (tenant_id is NOT NULL), which needs the PG15+ column-list SET NULL form.
    FOREIGN KEY (tenant_id, principal_id) REFERENCES principals(tenant_id, principal_id) ON DELETE SET NULL (principal_id)
);
COMMENT ON TABLE runs IS 'Durable run state machine for the workflow orchestrator.';

CREATE INDEX idx_runs_tenant_status ON runs(tenant_id, status);
CREATE INDEX idx_runs_workflow ON runs(tenant_id, workflow_id);

CREATE TRIGGER trg_runs_touch BEFORE UPDATE ON runs
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();

-- Append-only event history (episodic run memory / orchestration audit). seq orders events per run.
CREATE TABLE run_events (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    run_id     uuid NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    tenant_id  text NOT NULL REFERENCES tenants(tenant_id) ON DELETE CASCADE,
    seq        bigint NOT NULL,
    event_type text NOT NULL,
    payload    jsonb NOT NULL DEFAULT '{}'::jsonb,
    ts         timestamptz NOT NULL DEFAULT now(),
    UNIQUE (run_id, seq)
);
COMMENT ON TABLE run_events IS 'Append-only, ordered event history per run.';

CREATE INDEX idx_run_events_run ON run_events(run_id, seq);
CREATE INDEX idx_run_events_tenant ON run_events(tenant_id);

-- Durable checkpoints for pause/resume + human-approval interrupts. runs.resume matches
-- (run_id, token); resume_input is applied to the stored state on resume.
CREATE TABLE run_checkpoints (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    run_id      uuid NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
    tenant_id   text NOT NULL REFERENCES tenants(tenant_id) ON DELETE CASCADE,
    token       text NOT NULL,                       -- opaque resume token handed to the caller
    state       jsonb NOT NULL DEFAULT '{}'::jsonb,  -- serialized run state at the checkpoint
    status      text NOT NULL DEFAULT 'open' CHECK (status IN ('open','consumed','expired')),
    created_at  timestamptz NOT NULL DEFAULT now(),
    consumed_at timestamptz,
    UNIQUE (run_id, token)
);
COMMENT ON TABLE run_checkpoints IS 'Resume/interrupt checkpoints; token drives POST /runs.resume.';

CREATE UNIQUE INDEX uq_run_checkpoints_token ON run_checkpoints(token);
CREATE INDEX idx_run_checkpoints_run ON run_checkpoints(run_id);
CREATE INDEX idx_run_checkpoints_tenant ON run_checkpoints(tenant_id);

-- Policy-guarded tool/connector (MCP) gateway invocations, with approval + execution state.
-- Audit + approval are required before autonomous writes (design principle).
CREATE TABLE tool_executions (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id       text NOT NULL REFERENCES tenants(tenant_id) ON DELETE CASCADE,
    principal_id    text,
    run_id          uuid REFERENCES runs(run_id) ON DELETE SET NULL,   -- null for ad-hoc calls
    tool_id         text NOT NULL,
    arguments       jsonb NOT NULL DEFAULT '{}'::jsonb,
    approval_mode   text NOT NULL DEFAULT 'none' CHECK (approval_mode IN ('none','required')),
    approval_reason text,
    status          text NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending','approved','denied','executed','failed')),
    result          jsonb,
    error           text,
    created_at      timestamptz NOT NULL DEFAULT now(),
    started_at      timestamptz,
    finished_at     timestamptz,
    -- Composite FK to the tenant-scoped principal (see runs); null only principal_id.
    FOREIGN KEY (tenant_id, principal_id) REFERENCES principals(tenant_id, principal_id) ON DELETE SET NULL (principal_id)
);
COMMENT ON TABLE tool_executions IS 'Governed tool/connector invocations with approval + audit state.';

CREATE INDEX idx_tool_executions_tenant_status ON tool_executions(tenant_id, status);
CREATE INDEX idx_tool_executions_run ON tool_executions(run_id);
CREATE INDEX idx_tool_executions_tool ON tool_executions(tenant_id, tool_id);
