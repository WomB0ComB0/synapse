-- 0023_teams_read_actions.sql
-- synapse — migration 0023: register the explicit teams API actions in role_permissions.
--
-- The explicit teams API adds three governed actions — teams.create, teams.list, teams.members
-- (see src/api/teams.rs / Action::as_str) — so a tenant that configures role_permissions can grant
-- or restrict them. role_permissions uses REPLACE semantics (migration 0011): once a role has ANY
-- row, those rows are its COMPLETE allowlist and the in-code default matrix no longer applies. So
-- WITHOUT this migration, a tenant that has configured role_permissions for a role would silently
-- 403 that role on the three new endpoints AND be UNABLE to re-grant them — the action CHECK would
-- reject `INSERT ... 'teams.list'` with a 23514. The action vocabulary MUST mirror Action::as_str()
-- (a policy drift-guard test now asserts this). Auto-named role_permissions_action_check (0011,
-- last extended in 0013).
--
-- Re-add as NOT VALID then VALIDATE separately (as 0020 does): a plain ADD CONSTRAINT holds an
-- ACCESS EXCLUSIVE lock while it full-table-scans to validate existing rows, whereas ADD ... NOT
-- VALID takes only a brief lock and VALIDATE CONSTRAINT scans under a lighter SHARE UPDATE EXCLUSIVE
-- lock that permits concurrent reads/writes. The new CHECK is a strict SUPERSET of the old (only
-- adds allowed values), so every existing row already satisfies it — validation cannot fail.

ALTER TABLE role_permissions DROP CONSTRAINT role_permissions_action_check;
ALTER TABLE role_permissions ADD CONSTRAINT role_permissions_action_check
    CHECK (action IN (
        'documents.ingest', 'retrieve', 'context.upsert', 'context.get',
        'skills.register', 'skills.get', 'tool.execute', 'runs.start',
        'runs.resume', 'audit.events',
        'teams.create', 'teams.add_member', 'teams.remove_member', 'teams.list', 'teams.members',
        'documents.grant', 'documents.revoke'
    )) NOT VALID;
ALTER TABLE role_permissions VALIDATE CONSTRAINT role_permissions_action_check;
