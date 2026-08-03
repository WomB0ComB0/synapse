-- 0012_run_steps_tool.sql
-- synapse — migration 0012: the governed Tool step (Executor PR2).
--
-- Adds a fourth run_steps kind, 'tool', which invokes the policy-guarded tool
-- gateway (/tool.execute) from inside the run executor's transaction: an
-- auto-approved tool completes the step inline; an approval-required tool suspends
-- the run `waiting` (exactly like a human_approval gate) with the pending
-- tool_execution linked to the run, and a resume approves + finalizes it.
--
-- A Tool step's invocation config (tool_id, arguments, approval_mode) is fixed in
-- the in-code workflow catalog and materialized onto the step row here, so the
-- executor is self-contained (it never re-resolves the catalog).

-- Allow the new kind. The inline CHECK from 0010 is auto-named run_steps_kind_check.
ALTER TABLE run_steps DROP CONSTRAINT run_steps_kind_check;
ALTER TABLE run_steps ADD CONSTRAINT run_steps_kind_check
    CHECK (kind IN ('transform', 'human_approval', 'fail', 'tool'));

-- Per-step invocation config. Non-tool steps keep the '{}' default; a Tool step
-- stores { tool_id, arguments, approval_mode }.
ALTER TABLE run_steps ADD COLUMN config jsonb NOT NULL DEFAULT '{}'::jsonb;
