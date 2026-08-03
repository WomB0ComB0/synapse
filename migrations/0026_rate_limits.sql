-- Per-tenant request rate limiting (token bucket, Postgres-backed so the limit is EXACT and SHARED
-- across replicas).
--
-- One bucket row per tenant: `tokens` is the current (fractional) balance, refilled continuously at a
-- configured rate up to a burst capacity. Each governed request refills-then-consumes one token under
-- a `FOR UPDATE` row lock inside a tenant transaction, so concurrent same-tenant requests serialize
-- and the count is exact; different tenants never contend (separate rows). A request that can't
-- consume a token is rejected 429. Enforcement is opt-in (`RATE_LIMIT_ENABLED`); the rate + burst are
-- config. Tenant-scoped like every other row: RLS FORCEd, isolation by `app_current_tenant_id()`,
-- and `tenant_id` bound explicitly on every access.

CREATE TABLE rate_limits (
    tenant_id  text             NOT NULL REFERENCES tenants(tenant_id) ON DELETE CASCADE,
    -- Current token balance (fractional). A new tenant starts with a full bucket (burst capacity).
    tokens     double precision NOT NULL,
    -- Last refill/consume instant; `now() - updated_at` is the elapsed time used to refill.
    updated_at timestamptz      NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id)
);

ALTER TABLE rate_limits ENABLE ROW LEVEL SECURITY;
ALTER TABLE rate_limits FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON rate_limits
    USING (tenant_id = app_current_tenant_id())
    WITH CHECK (tenant_id = app_current_tenant_id());
