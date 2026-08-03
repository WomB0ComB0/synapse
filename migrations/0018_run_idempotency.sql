-- 0018_run_idempotency.sql
-- synapse — migration 0018: client idempotency key for runs.start.
--
-- Today runs.start creates a NEW run on every call, so a client that retries a failed request
-- starts a duplicate run and re-executes every step (at-least-once). An optional client-supplied
-- `Idempotency-Key` makes the retry SAFE: a repeat with the same key REPLAYS the original run
-- instead of starting a second one.

ALTER TABLE runs ADD COLUMN idempotency_key text;

-- Tenant-scoped: tenant A's key can neither collide with nor probe tenant B's (mirrors the
-- house style of a tenant-first key). PARTIAL so keyless runs (the default, NULL) are
-- unconstrained and keep today's at-least-once behavior — NULLs never conflict. `runs` is
-- already enrolled in FORCE RLS (pre-0009), so the column inherits tenant isolation.
CREATE UNIQUE INDEX uq_runs_idempotency ON runs (tenant_id, idempotency_key)
    WHERE idempotency_key IS NOT NULL;

COMMENT ON COLUMN runs.idempotency_key IS
    'Optional client Idempotency-Key (tenant-scoped); a repeat replays this run. NULL = keyless (at-least-once).';
