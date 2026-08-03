-- 0013_grant_management_actions.sql
-- synapse — migration 0013: register the Team + grant management actions.
--
-- The management API adds four governed actions — teams.add_member,
-- teams.remove_member, documents.grant, documents.revoke — so a tenant that wants
-- to grant (or restrict) them via role_permissions needs them in the action CHECK
-- (migration 0011). The inline CHECK from 0011 is auto-named role_permissions_action_check.

ALTER TABLE role_permissions DROP CONSTRAINT role_permissions_action_check;
ALTER TABLE role_permissions ADD CONSTRAINT role_permissions_action_check
    CHECK (action IN (
        'documents.ingest', 'retrieve', 'context.upsert', 'context.get',
        'skills.register', 'skills.get', 'tool.execute', 'runs.start',
        'runs.resume', 'audit.events',
        'teams.add_member', 'teams.remove_member', 'documents.grant', 'documents.revoke'
    ));
