-- 0001_extensions_and_helpers.sql
-- synapse — Postgres + pgvector schema, migration 0001.
-- Extensions, the multi-tenant RLS guard, and shared trigger utilities.
--
-- Run with sqlx-cli (NOT the sqlx::migrate! macro — see README / RUST CONTRACT):
--     sqlx migrate run --database-url "$DATABASE_URL"
--
-- Requires a pgvector-enabled Postgres. docker-compose.yml uses pgvector/pgvector:pg16.

-- pgvector: the `vector` type + HNSW/IVFFlat ANN indexes used by the chunk store (0005).
CREATE EXTENSION IF NOT EXISTS vector;

-- pgcrypto: gen_random_uuid() for surrogate keys. Built into PG13+, kept explicit for portability.
CREATE EXTENSION IF NOT EXISTS pgcrypto;

-- pg_trgm: trigram GIN indexes for fuzzy lexical matching on titles / identifiers.
CREATE EXTENSION IF NOT EXISTS pg_trgm;

-- ---------------------------------------------------------------------------
-- Multi-tenant Row-Level Security guard.
--
-- Every tenant-scoped table carries a plain-text `tenant_id` (matching the canonical
-- string tenant ids like "acme"). At request time the application MUST set the
-- per-connection / per-transaction GUC before issuing tenant queries:
--
--     SET app.tenant_id = 'acme';                          -- session-scoped, or:
--     SELECT set_config('app.tenant_id', 'acme', true);    -- transaction-local (preferred)
--
-- The RLS policies installed in migration 0009 compare each row's `tenant_id` against
-- app_current_tenant_id(). current_setting(..., true) returns NULL when the GUC is unset,
-- and `tenant_id = NULL` is never true, so an UNSET tenant guard sees ZERO rows.
-- That is the intended fail-closed / deny-by-default behavior for a governed brain.
-- ---------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION app_current_tenant_id()
    RETURNS text
    LANGUAGE sql
    STABLE
AS $$
    SELECT current_setting('app.tenant_id', true)
$$;

COMMENT ON FUNCTION app_current_tenant_id() IS
    'Current request tenant, read from the app.tenant_id GUC. NULL when unset => RLS denies all rows.';

-- Shared BEFORE UPDATE trigger to maintain updated_at columns.
CREATE OR REPLACE FUNCTION touch_updated_at()
    RETURNS trigger
    LANGUAGE plpgsql
AS $$
BEGIN
    NEW.updated_at := now();
    RETURN NEW;
END;
$$;

COMMENT ON FUNCTION touch_updated_at() IS
    'BEFORE UPDATE trigger: stamps NEW.updated_at = now().';
