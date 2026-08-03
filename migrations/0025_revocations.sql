-- Per-subject token revocation (revoke-all / "logout everywhere").
--
-- A single cutoff per (tenant, principal): a verified bearer token whose `iat` (issued-at) is
-- STRICTLY BEFORE `not_before` is rejected. This kills ALL of a principal's outstanding tokens at
-- once (account compromise, forced logout) without needing to know each token's id — the mitigation
-- for a leaked/compromised token before its short `exp`. Setting the row again (upsert to now())
-- re-revokes; deleting the row re-allows tokens. Rows self-obsolete once the cutoff predates the
-- oldest still-valid token, but are cheap to keep.
--
-- Enforced fail-closed in the auth extractor for the VERIFIED-JWT modes only (a token with no `iat`
-- while a cutoff exists is rejected). Tenant-scoped like every other row: RLS FORCEd, isolation by
-- `app_current_tenant_id()`, and `tenant_id` bound explicitly on every access (composite-PK prefix).

CREATE TABLE revocations (
    tenant_id    text        NOT NULL REFERENCES tenants(tenant_id) ON DELETE CASCADE,
    principal_id text        NOT NULL,
    -- Tokens issued (`iat`) strictly before this instant are revoked.
    not_before   timestamptz NOT NULL,
    -- Optional operator note (why the principal was revoked); not security-bearing.
    reason       text,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, principal_id)
);

ALTER TABLE revocations ENABLE ROW LEVEL SECURITY;
ALTER TABLE revocations FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON revocations
    USING (tenant_id = app_current_tenant_id())
    WITH CHECK (tenant_id = app_current_tenant_id());
