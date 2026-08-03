-- 0006_skills.sql
-- synapse — migration 0006: versioned skill registry.
-- Skills are versioned (skill_id + semver) so prompt/schema/behavior changes are explicit and
-- rebuildable. Written by POST /skills.register. Mirrors the canonical skill schema.
--
-- Canonical shape (required: skill_id, version, name):
--   { skill_id, version, name, summary, owners[], triggers[], input_schema(JSON Schema),
--     output_schema(JSON Schema), required_tools[], policy_tags[], examples[] }

CREATE TABLE skills (
    id             uuid NOT NULL DEFAULT gen_random_uuid(),
    tenant_id      text NOT NULL REFERENCES tenants(tenant_id) ON DELETE CASCADE,
    skill_id       text NOT NULL,                         -- e.g. "skill.approval.draft_response"
    version        text NOT NULL,                         -- semver, e.g. "1.2.0"
    name           text NOT NULL,
    summary        text,
    owners         text[] NOT NULL DEFAULT '{}',
    triggers       text[] NOT NULL DEFAULT '{}',
    input_schema   jsonb NOT NULL DEFAULT '{}'::jsonb,    -- JSON Schema
    output_schema  jsonb NOT NULL DEFAULT '{}'::jsonb,    -- JSON Schema
    required_tools text[] NOT NULL DEFAULT '{}',
    policy_tags    text[] NOT NULL DEFAULT '{}',          -- e.g. "human_approval_required"
    examples       jsonb NOT NULL DEFAULT '[]'::jsonb,    -- array of { input, output }
    is_latest      boolean NOT NULL DEFAULT true,         -- newest version flag for a skill_id
    metadata       jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, skill_id, version)
);
COMMENT ON TABLE skills IS 'Versioned skill manifests (JSON Schema contracts + policy tags).';

-- Stable surrogate id so runs / tool_executions can reference a specific skill version.
CREATE UNIQUE INDEX uq_skills_id ON skills(id);
CREATE INDEX idx_skills_tenant ON skills(tenant_id);
CREATE INDEX idx_skills_lookup ON skills(tenant_id, skill_id);
CREATE INDEX idx_skills_policy_tags ON skills USING gin (policy_tags);
CREATE INDEX idx_skills_triggers ON skills USING gin (triggers);
CREATE INDEX idx_skills_required_tools ON skills USING gin (required_tools);
-- At most one version per (tenant, skill) may be flagged latest.
CREATE UNIQUE INDEX uq_skills_latest ON skills(tenant_id, skill_id) WHERE is_latest;

CREATE TRIGGER trg_skills_touch BEFORE UPDATE ON skills
    FOR EACH ROW EXECUTE FUNCTION touch_updated_at();
