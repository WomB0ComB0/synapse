//! Teams API: `teams.create`, `teams.add_member`, `teams.remove_member` (writes) +
//! `teams.list`, `teams.members` (reads).
//!
//! Team membership is the source of truth for document-ACL `group` grants (retrieval
//! resolves a `group` grant via a `team_members` join), so the write paths activate
//! group access. Tenant-isolated (`tenant_tx`/RLS), RBAC-gated (a Viewer is denied on
//! writes), and audited. The read paths are authority-scoped too: `teams.list` shows a
//! non-admin only the teams they own/belong to, and `teams.members` is owner/admin-only
//! with the same 404/403 parity as management — so neither is a team-existence oracle.
//!
//! Membership management is **authority-scoped**: CREATING a team requires the elevated
//! `admin` role, and MANAGING an existing team requires being an owner of it OR an admin
//! (a team is owned by its creating admin — the first `add_member` creates it with
//! `owners=[caller]`). A non-admin therefore can neither create a team nor join one it
//! doesn't own, closing both the #24 self-join and the team-namespace squatting vector
//! (a Member can no longer pre-create a `team_id` a doc-owner later grants to). A team
//! with no owners is admin-only (never open to a non-admin).

use axum::extract::State;
use axum::Json;

use crate::audit;
use crate::auth::policy::{enforce, resolve_role_in_tx, Action, Role};
use crate::auth::Principal;
use crate::db::tenant_tx;
use crate::domain::{
    TeamAddMemberRequest, TeamCreateRequest, TeamCreateResponse, TeamListResponse, TeamMemberEntry,
    TeamMemberResponse, TeamMembersRequest, TeamMembersResponse, TeamRemoveMemberRequest,
    TeamSummary,
};
use crate::error::{Error, Result};
use crate::state::AppState;

/// Team-management authority for an EXISTING team: an `admin` may manage any team, and a
/// team's owner may manage their own. A team with no owners is admin-only (fail-closed —
/// never open to a non-admin), so a legacy/unowned team can't be self-joined. Refuses
/// with a 403. (Team CREATION is a separate admin-only check in [`add_member`].)
///
/// Ownership persists across role changes: an admin later downgraded to Member keeps
/// managing the teams they created. This is intended and not a squatting vector — only an
/// admin could have created (and thus owned) the team in the first place.
fn assert_can_manage_team(owners: &[String], caller: &str, is_admin: bool) -> Result<()> {
    if is_admin || owners.iter().any(|o| o == caller) {
        Ok(())
    } else {
        Err(Error::Forbidden)
    }
}

/// Add (or update the team-role of) a principal in a team.
///
/// Idempotent: re-adding an existing member updates their role. Auto-provisions the
/// team and the member principal (both tenant-scoped) so the caller need not create
/// them first — mirroring how `context.upsert` self-provisions the subject principal.
/// **Authority-scoped:** CREATING a team requires the `admin` role; managing an existing
/// team requires being its owner OR an admin (else 403). The admin that creates a team
/// owns it.
pub async fn add_member(
    State(state): State<AppState>,
    principal: Principal,
    Json(req): Json<TeamAddMemberRequest>,
) -> Result<Json<TeamMemberResponse>> {
    let tenant = principal.authenticated_tenant()?;
    let team_id = req.team_id.trim();
    let member = req.principal_id.trim();
    if team_id.is_empty() || member.is_empty() {
        return Err(Error::BadRequest(
            "team_id and principal_id are required".into(),
        ));
    }
    enforce(&state, &principal, tenant, Action::TeamsAddMember, team_id).await?;

    let write: Result<()> = async {
        let mut tx = tenant_tx(&state.db, tenant).await?;
        // Resolve the caller's effective role in THIS tx (DB-authoritative principals.role,
        // else the X-Role/JWT hint) so team CREATION can be gated to admins.
        let is_admin = resolve_role_in_tx(
            &mut tx,
            &principal.principal_id,
            principal.role.as_deref(),
            principal.role_verified,
            tenant,
        )
        .await?
            == Role::Admin;
        // Authority: CREATE the team (admin-only) if it doesn't exist yet; else MANAGE an
        // existing one (owner OR admin). This closes team-namespace squatting — a
        // non-admin can neither create/own a team nor join one it doesn't own.
        let existing: Option<(Vec<String>,)> =
            sqlx::query_as("SELECT owners FROM teams WHERE tenant_id = $1 AND team_id = $2")
                .bind(tenant)
                .bind(team_id)
                .fetch_optional(&mut *tx)
                .await?;
        match existing {
            Some((owners,)) => assert_can_manage_team(&owners, &principal.principal_id, is_admin)?,
            None => {
                if !is_admin {
                    return Err(Error::Forbidden);
                }
                // Create the team OWNED BY the creating admin. ON CONFLICT DO NOTHING is a
                // no-op on the rare create race (another admin won concurrently); this
                // admin may still manage it. An unknown TENANT (FK 23503) is the only
                // client-error FK here -> 400.
                sqlx::query(
                    "INSERT INTO teams (tenant_id, team_id, owners) \
                         VALUES ($1, $2, ARRAY[$3]::text[]) \
                     ON CONFLICT (tenant_id, team_id) DO NOTHING",
                )
                .bind(tenant)
                .bind(team_id)
                .bind(&principal.principal_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| Error::db_or_conflict(e, "team already exists"))?;
            }
        }
        sqlx::query(
            "INSERT INTO principals (tenant_id, principal_id) VALUES ($1, $2) \
             ON CONFLICT (tenant_id, principal_id) DO NOTHING",
        )
        .bind(tenant)
        .bind(member)
        .execute(&mut *tx)
        .await
        .map_err(|e| Error::db_or_conflict(e, "principal already exists"))?;
        sqlx::query(
            "INSERT INTO team_members (tenant_id, team_id, principal_id, role) \
                 VALUES ($1, $2, $3, $4) \
             ON CONFLICT (tenant_id, team_id, principal_id) DO UPDATE SET role = EXCLUDED.role",
        )
        .bind(tenant)
        .bind(team_id)
        .bind(member)
        // Normalize the optional in-team role: trim, and treat empty/whitespace as
        // absent (NULL) — same hygiene as `permission` on grant/revoke.
        .bind(req.role.as_deref().map(str::trim).filter(|s| !s.is_empty()))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
    .await;

    let (outcome, metadata) = match &write {
        Ok(()) => (
            "success",
            serde_json::json!({ "team_id": team_id, "principal_id": member }),
        ),
        Err(e) => e.audit_report(),
    };
    audit::record_best_effort(
        &state.db,
        tenant,
        Some(&principal.principal_id),
        "teams.add_member",
        team_id,
        outcome,
        metadata,
    )
    .await;
    write?;

    Ok(Json(TeamMemberResponse {
        team_id: team_id.to_string(),
        principal_id: member.to_string(),
        status: "added".to_string(),
    }))
}

/// Remove a principal from a team. Authority-scoped like [`add_member`]: managing an
/// existing team requires being its owner OR an admin (else 403); an unowned team is
/// admin-only. A missing team is an honest 404 to an admin but the SAME 403 to a
/// non-admin (so the status code reveals no team-existence oracle). Removing a
/// non-member of a managed team is a 200 no-op (idempotent).
pub async fn remove_member(
    State(state): State<AppState>,
    principal: Principal,
    Json(req): Json<TeamRemoveMemberRequest>,
) -> Result<Json<TeamMemberResponse>> {
    let tenant = principal.authenticated_tenant()?;
    let team_id = req.team_id.trim();
    let member = req.principal_id.trim();
    if team_id.is_empty() || member.is_empty() {
        return Err(Error::BadRequest(
            "team_id and principal_id are required".into(),
        ));
    }
    enforce(
        &state,
        &principal,
        tenant,
        Action::TeamsRemoveMember,
        team_id,
    )
    .await?;

    let write: Result<u64> = async {
        let mut tx = tenant_tx(&state.db, tenant).await?;
        // Authority: manage an existing team only as its owner OR an admin (unowned =
        // admin-only).
        let is_admin = resolve_role_in_tx(
            &mut tx,
            &principal.principal_id,
            principal.role.as_deref(),
            principal.role_verified,
            tenant,
        )
        .await?
            == Role::Admin;
        let owners: Option<(Vec<String>,)> =
            sqlx::query_as("SELECT owners FROM teams WHERE tenant_id = $1 AND team_id = $2")
                .bind(tenant)
                .bind(team_id)
                .fetch_optional(&mut *tx)
                .await?;
        match owners {
            Some((owners,)) => assert_can_manage_team(&owners, &principal.principal_id, is_admin)?,
            None => {
                // A missing team: an admin gets an honest 404; a non-admin gets the SAME
                // 403 as an existing team they can't manage, so the status code can't be
                // used to enumerate which team slugs exist (matches add_member's parity).
                return Err(if is_admin {
                    Error::NotFound(format!("team '{team_id}'"))
                } else {
                    Error::Forbidden
                });
            }
        }
        let r = sqlx::query(
            "DELETE FROM team_members \
             WHERE tenant_id = $1 AND team_id = $2 AND principal_id = $3",
        )
        .bind(tenant)
        .bind(team_id)
        .bind(member)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(r.rows_affected())
    }
    .await;

    let (outcome, metadata) = match &write {
        Ok(removed) => (
            "success",
            serde_json::json!({ "team_id": team_id, "principal_id": member, "removed": removed }),
        ),
        Err(e) => e.audit_report(),
    };
    audit::record_best_effort(
        &state.db,
        tenant,
        Some(&principal.principal_id),
        "teams.remove_member",
        team_id,
        outcome,
        metadata,
    )
    .await;
    write?;

    Ok(Json(TeamMemberResponse {
        team_id: team_id.to_string(),
        principal_id: member.to_string(),
        status: "removed".to_string(),
    }))
}

/// Explicitly create a team, OWNED BY the caller. Admin-only — a non-admin is 403'd BEFORE the
/// existence check, so the `409 Conflict` below is never an existence oracle to a non-admin. Not
/// idempotent: re-creating an existing team is a 409 (use `teams.add_member` to manage an existing
/// one). Mirrors the create-authority of [`add_member`], but as a first-class, empty-membership
/// operation.
pub async fn create(
    State(state): State<AppState>,
    principal: Principal,
    Json(req): Json<TeamCreateRequest>,
) -> Result<Json<TeamCreateResponse>> {
    let tenant = principal.authenticated_tenant()?;
    let team_id = req.team_id.trim();
    if team_id.is_empty() {
        return Err(Error::BadRequest("team_id is required".into()));
    }
    enforce(&state, &principal, tenant, Action::TeamsCreate, team_id).await?;
    // Normalize the optional name: trim, empty/whitespace -> NULL (same hygiene as elsewhere).
    let name = req.name.as_deref().map(str::trim).filter(|s| !s.is_empty());

    let write: Result<()> = async {
        let mut tx = tenant_tx(&state.db, tenant).await?;
        let is_admin = resolve_role_in_tx(
            &mut tx,
            &principal.principal_id,
            principal.role.as_deref(),
            principal.role_verified,
            tenant,
        )
        .await?
            == Role::Admin;
        // Only an admin may create/own a team (closes team-namespace squatting). Check FIRST so a
        // non-admin gets a 403 regardless of whether the team exists (no existence oracle).
        if !is_admin {
            return Err(Error::Forbidden);
        }
        // ON CONFLICT DO NOTHING RETURNING: a NULL return means the team already existed -> 409.
        // db_or_conflict maps an unknown TENANT (FK 23503) to a 400.
        let created: Option<(String,)> = sqlx::query_as(
            "INSERT INTO teams (tenant_id, team_id, name, owners) \
                 VALUES ($1, $2, $3, ARRAY[$4]::text[]) \
             ON CONFLICT (tenant_id, team_id) DO NOTHING \
             RETURNING team_id",
        )
        .bind(tenant)
        .bind(team_id)
        .bind(name)
        .bind(&principal.principal_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| Error::db_or_conflict(e, "team already exists"))?;
        if created.is_none() {
            return Err(Error::Conflict(format!("team '{team_id}' already exists")));
        }
        tx.commit().await?;
        Ok(())
    }
    .await;

    let (outcome, metadata) = match &write {
        Ok(()) => ("success", serde_json::json!({ "team_id": team_id })),
        Err(e) => e.audit_report(),
    };
    audit::record_best_effort(
        &state.db,
        tenant,
        Some(&principal.principal_id),
        "teams.create",
        team_id,
        outcome,
        metadata,
    )
    .await;
    write?;

    Ok(Json(TeamCreateResponse {
        team_id: team_id.to_string(),
        status: "created".to_string(),
    }))
}

/// List the teams VISIBLE to the caller. An `admin` sees ALL of the tenant's teams; a non-admin
/// sees only the teams they OWN or are a MEMBER of — so a non-admin can't enumerate the tenant's
/// team namespace (consistent with the squatting/oracle posture of the mutation paths). Read-only,
/// tenant-isolated (`tenant_tx`/RLS).
pub async fn list(
    State(state): State<AppState>,
    principal: Principal,
) -> Result<Json<TeamListResponse>> {
    let tenant = principal.authenticated_tenant()?;
    enforce(&state, &principal, tenant, Action::TeamsList, "*").await?;

    let mut tx = tenant_tx(&state.db, tenant).await?;
    let is_admin = resolve_role_in_tx(
        &mut tx,
        &principal.principal_id,
        principal.role.as_deref(),
        principal.role_verified,
        tenant,
    )
    .await?
        == Role::Admin;
    // $3 = is_admin: an admin sees every team; otherwise only teams the caller owns or belongs to.
    let teams: Vec<TeamSummary> = sqlx::query_as(
        "SELECT team_id, name, owners, created_at FROM teams \
         WHERE tenant_id = $1 \
           AND ($3 \
                OR $2 = ANY(owners) \
                OR EXISTS (SELECT 1 FROM team_members m \
                           WHERE m.tenant_id = $1 AND m.team_id = teams.team_id \
                             AND m.principal_id = $2)) \
         ORDER BY team_id",
    )
    .bind(tenant)
    .bind(&principal.principal_id)
    .bind(is_admin)
    .fetch_all(&mut *tx)
    .await?;
    Ok(Json(TeamListResponse { teams }))
}

/// List the members of a team. Owner/admin only — the SAME authority as managing it — with the same
/// 404-to-admin / 403-to-non-admin parity for a missing team so the status code is not an existence
/// oracle. Read-only, tenant-isolated.
pub async fn members(
    State(state): State<AppState>,
    principal: Principal,
    Json(req): Json<TeamMembersRequest>,
) -> Result<Json<TeamMembersResponse>> {
    let tenant = principal.authenticated_tenant()?;
    let team_id = req.team_id.trim();
    if team_id.is_empty() {
        return Err(Error::BadRequest("team_id is required".into()));
    }
    enforce(&state, &principal, tenant, Action::TeamsMembers, team_id).await?;

    let mut tx = tenant_tx(&state.db, tenant).await?;
    let is_admin = resolve_role_in_tx(
        &mut tx,
        &principal.principal_id,
        principal.role.as_deref(),
        principal.role_verified,
        tenant,
    )
    .await?
        == Role::Admin;
    let owners: Option<(Vec<String>,)> =
        sqlx::query_as("SELECT owners FROM teams WHERE tenant_id = $1 AND team_id = $2")
            .bind(tenant)
            .bind(team_id)
            .fetch_optional(&mut *tx)
            .await?;
    match owners {
        Some((owners,)) => assert_can_manage_team(&owners, &principal.principal_id, is_admin)?,
        None => {
            return Err(if is_admin {
                Error::NotFound(format!("team '{team_id}'"))
            } else {
                Error::Forbidden
            });
        }
    }
    let members: Vec<TeamMemberEntry> = sqlx::query_as(
        "SELECT principal_id, role, created_at FROM team_members \
         WHERE tenant_id = $1 AND team_id = $2 ORDER BY principal_id",
    )
    .bind(tenant)
    .bind(team_id)
    .fetch_all(&mut *tx)
    .await?;
    Ok(Json(TeamMembersResponse {
        team_id: team_id.to_string(),
        members,
    }))
}

#[cfg(test)]
mod tests {
    use super::assert_can_manage_team;
    use crate::error::Error;

    #[test]
    fn unowned_team_is_admin_only() {
        // An unowned team is fail-closed: a non-admin is refused, an admin may manage it.
        assert!(matches!(
            assert_can_manage_team(&[], "anyone", false),
            Err(Error::Forbidden)
        ));
        assert!(assert_can_manage_team(&[], "an_admin", true).is_ok());
    }

    #[test]
    fn owner_or_admin_allowed_others_forbidden() {
        let owners = vec!["lead".to_string(), "cofounder".to_string()];
        // An owner may manage their own team even without the admin role.
        assert!(assert_can_manage_team(&owners, "lead", false).is_ok());
        assert!(assert_can_manage_team(&owners, "cofounder", false).is_ok());
        // Any admin may manage any team, owner or not.
        assert!(assert_can_manage_team(&owners, "outside_admin", true).is_ok());
        // A non-owner non-admin (the self-join / squatting vector) is refused with a 403.
        assert!(matches!(
            assert_can_manage_team(&owners, "mallory", false),
            Err(Error::Forbidden)
        ));
    }
}
