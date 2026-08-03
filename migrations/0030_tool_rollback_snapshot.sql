-- Preserve the compensation contract selected when an external side effect is dispatched.
--
-- tool_definitions is intentionally mutable. Resolving rollback_tool_id from its current row
-- after an execution would let a later registry update silently redirect compensation for an
-- already-completed side effect. Snapshot the selected handler on the execution ledger instead.

ALTER TABLE tool_executions
    ADD COLUMN rollback_tool_id text;

COMMENT ON COLUMN tool_executions.rollback_tool_id IS
    'Immutable compensation tool selected from the governed definition at dispatch time.';

-- Migration 0029 may already have accepted governed executions. Preserve the best available
-- compensation choice for those rows from the current same-tenant definition. New writes always
-- populate the value directly from the policy validated immediately before dispatch.
UPDATE tool_executions AS execution
SET rollback_tool_id = definition.rollback_tool_id
FROM tool_definitions AS definition
WHERE execution.tenant_id = definition.tenant_id
  AND execution.tool_id = definition.tool_id
  AND execution.definition_revision IS NOT NULL
  AND execution.rollback_of IS NULL
  AND execution.rollback_tool_id IS NULL;

-- The execution ledger is evidence, not mutable workflow configuration. Identity and request
-- fields never change after insertion. Policy snapshots may be refreshed only by the approval
-- transition, where the current contract is revalidated immediately before external dispatch.
CREATE FUNCTION guard_tool_execution_ledger()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF ROW(
        NEW.tenant_id,
        NEW.principal_id,
        NEW.tool_id,
        NEW.arguments,
        NEW.run_id,
        NEW.rollback_of,
        NEW.idempotency_key
    ) IS DISTINCT FROM ROW(
        OLD.tenant_id,
        OLD.principal_id,
        OLD.tool_id,
        OLD.arguments,
        OLD.run_id,
        OLD.rollback_of,
        OLD.idempotency_key
    ) THEN
        RAISE EXCEPTION 'tool execution identity and arguments are immutable'
            USING ERRCODE = '23514';
    END IF;

    IF ROW(NEW.definition_revision, NEW.rollback_tool_id)
        IS DISTINCT FROM ROW(OLD.definition_revision, OLD.rollback_tool_id)
        AND NOT (OLD.status = 'pending' AND NEW.status = 'approved')
    THEN
        RAISE EXCEPTION 'tool execution policy snapshot is immutable after dispatch'
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;

CREATE TRIGGER trg_tool_executions_guard_ledger
    BEFORE UPDATE ON tool_executions
    FOR EACH ROW EXECUTE FUNCTION guard_tool_execution_ledger();
