-- 0015_admin_role.sql
-- synapse — migration 0015: register the `admin` RBAC role.
--
-- Team management becomes authority-scoped: creating a team requires the elevated
-- `admin` role, and managing an existing team requires being its owner OR an admin
-- (see src/api/teams.rs). `admin` must therefore be a value a tenant can configure in
-- role_permissions, so extend the role CHECK from 0011 (auto-named
-- role_permissions_role_check) to include it. principals.role has no CHECK (it is a
-- free domain field), so a stored role of 'admin' already resolves via Role::from_db_value.

ALTER TABLE role_permissions DROP CONSTRAINT role_permissions_role_check;
ALTER TABLE role_permissions ADD CONSTRAINT role_permissions_role_check
    CHECK (role IN ('viewer', 'member', 'admin'));
