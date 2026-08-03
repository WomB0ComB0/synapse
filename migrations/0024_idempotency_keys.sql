-- 0024_idempotency_keys.sql
-- synapse — migration 0024: generic idempotency-key registry with request fingerprinting.
--
-- #32 (runs.idempotency_key) / #33 (tool_executions.idempotency_key) make a keyed retry REPLAY, but
-- do NOT fingerprint the request — so reusing a key with a DIFFERENT body silently replays the
-- ORIGINAL result instead of surfacing the conflict. This generic table records the request
-- fingerprint per (tenant, scope, key); the endpoints gate a keyed write on it: same key + SAME
-- fingerprint -> the existing per-table replay proceeds; same key + DIFFERENT fingerprint -> 409
-- Conflict. It is ADDITIVE — the #32/#33 replay paths (uq_runs_idempotency / uq_tool_executions_
-- idempotency) are unchanged; this table only adds the fingerprint gate.
CREATE TABLE idempotency_keys (
    tenant_id           text NOT NULL REFERENCES tenants(tenant_id) ON DELETE CASCADE,
    scope               text NOT NULL,   -- endpoint namespace, e.g. 'runs.start' | 'tool.execute'
    idempotency_key     text NOT NULL,   -- the client Idempotency-Key (tenant + scope scoped)
    request_fingerprint text NOT NULL,   -- hex sha256 of the canonical request bytes (server-side)
    created_at          timestamptz NOT NULL DEFAULT now(),
    -- A key is unique WITHIN a (tenant, scope): runs.start and tool.execute keep SEPARATE namespaces
    -- (as the two per-table columns did), and tenant A's key can neither collide with nor probe
    -- tenant B's.
    PRIMARY KEY (tenant_id, scope, idempotency_key)
);
COMMENT ON TABLE idempotency_keys IS
    'Generic idempotency-key registry: request fingerprint per (tenant, scope, key) for reuse-with-different-body detection (409).';

-- RLS (mirror 0009): tenant isolation enforced even for the table owner.
ALTER TABLE idempotency_keys ENABLE ROW LEVEL SECURITY;
ALTER TABLE idempotency_keys FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON idempotency_keys
    USING (tenant_id = app_current_tenant_id())
    WITH CHECK (tenant_id = app_current_tenant_id());
