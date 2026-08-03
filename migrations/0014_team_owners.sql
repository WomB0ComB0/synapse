-- 0014_team_owners.sql
-- synapse — migration 0014: team-management authority (team owners).
--
-- Team management is authority-scoped (see src/api/teams.rs): CREATING a team requires
-- the elevated `admin` role (migration 0015), and MANAGING an existing team requires
-- being one of its owners OR an admin. A team is owned by the admin that creates it (the
-- first add_member sets owners = [caller]). A team with no owners is ADMIN-ONLY
-- (fail-closed — never open to a non-admin). This closes the #24 residual and
-- team-namespace squatting: a `group` ACL grant can no longer be self-joined or squatted
-- by a non-admin Member.

ALTER TABLE teams ADD COLUMN owners text[] NOT NULL DEFAULT '{}';
COMMENT ON COLUMN teams.owners IS
    'Principals who may manage this team''s membership (in addition to any admin); empty = admin-only.';
