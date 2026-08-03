//! The Policy & Access Gateway's decision point (coarse RBAC).
//!
//! PR2 is **DB-authoritative** action-level RBAC. Two DB inputs, both read under
//! Row-Level Security in a `tenant_tx` at [`PolicyGateway::authorize`]:
//!   1. the caller's **role** — `principals.role` wins WHEN it names a recognized
//!      RBAC role; the `X-Role` header remains a fallback for a principal whose role
//!      is unset (NULL) or holds a non-RBAC domain value (e.g. `"manager"`), so an
//!      operator can assign roles without callers changing anything, PR1's header
//!      behavior still holds until a real RBAC role is set, and a stored domain role
//!      is never silently treated as a downgrade;
//!   2. the **permission** — a per-tenant `role_permissions` table (migration 0011):
//!      a `(tenant, role)` with ANY rows defines that role's COMPLETE allowlist
//!      (REPLACING the in-code default matrix for that role); a role with no rows
//!      falls back to [`permits_default`]. Existing tenants (no rows) keep the exact
//!      default, so nothing regresses.
//!
//! Every governed handler authorizes here (via [`enforce`]) after resolving the
//! tenant, before the action — so an unauthenticated/mismatched request never
//! reaches the DB and the DB-free tests stay DB-free.
//!
//! PR3 adds the first RESOURCE ABAC slice: opt-in **context-ownership** (when
//! `ABAC_CONTEXT_OWNERSHIP` is set, a caller may access only their OWN context — the
//! `resource`, i.e. the subject principal_id, is now AUTHORITATIVE for context, not just
//! audited), with an **Admin override**: an `Admin` (the elevated tier added for team
//! authority) may perform governed cross-principal context access. Document ACL is wired
//! into retrieval + owner/admin-scoped management; team membership is admin-authorized.

use crate::audit;
use crate::auth::Principal;
use crate::db::tenant_tx;
use crate::error::{Error, Result};
use crate::state::AppState;
use sqlx::PgPool;

/// Coarse RBAC role. The authoritative source is `principals.role`; the `X-Role`
/// header is a fallback when that is unset (see [`PolicyGateway::authorize`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Read-only.
    Viewer,
    /// Full access to everything the gateway governs (the permissive baseline).
    Member,
    /// Elevated operator: everything a `Member` may do, plus authority operations —
    /// creating a team and managing any team's membership (see [`crate::api::teams`]).
    /// The Admin-only distinctions are enforced at the resource layer in the handler,
    /// not in the coarse action matrix (Admin ⊇ Member there).
    Admin,
}

impl Role {
    /// Map a role string (the DB `principals.role` value OR the `X-Role` header) to
    /// a [`Role`]. The role dimension is fail-CLOSED, matching tenant resolution (a
    /// missing tenant is a 401, not a bypass):
    ///
    /// - **absent or blank** → `Member`: the backward-compatible baseline. A caller
    ///   with NO role asserted can do everything the default matrix governs — the
    ///   pre-existing suites send no `X-Role` and provision principals with a NULL
    ///   role, so they must keep succeeding, and an omitted role is never a surprise
    ///   deny.
    /// - explicit `viewer`/`reader` → `Viewer` (read-only).
    /// - explicit `member` → `Member`.
    /// - any OTHER non-blank value (an unrecognized role, incl. a typo such as
    ///   `viewr` or an unmapped `auditor`) → `Viewer` (least privilege). A role we
    ///   DON'T recognize must not silently grant more than a known read-only role;
    ///   only an *omitted* role opts into the permissive baseline.
    /// - `admin` is NOT honored here: this is the UNVERIFIED-hint parser, so a hinted `admin` fails
    ///   closed to `Viewer`. The elevated `Admin` role is honored only from `principals.role`
    ///   ([`Role::from_db_value`]) or a cryptographically VERIFIED signed-JWT claim
    ///   ([`Role::from_hint`] with `verified = true`) — never a plaintext `X-Role` header.
    pub fn from_str_value(raw: Option<&str>) -> Role {
        match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            None | Some("") => Role::Member,
            Some("viewer") | Some("reader") => Role::Viewer,
            Some("member") => Role::Member,
            // "admin" deliberately falls through to Viewer — an UNVERIFIED request hint (an `X-Role`
            // header) can never assert the elevated role. `admin` is honored only from
            // `principals.role` (from_db_value) or a VERIFIED signed-JWT claim (from_hint).
            Some(_) => Role::Viewer,
        }
    }

    /// Resolve the effective role from a REQUEST hint (`X-Role` header or a JWT `role` claim),
    /// honoring the elevated `admin` ONLY when the hint is cryptographically VERIFIED (a signed JWT;
    /// `verified == true`). An unverified header hint uses [`Role::from_str_value`], which fails
    /// `admin` closed to `Viewer`; a verified hint additionally trusts `admin` (via the same
    /// recognizer as the DB-authoritative [`Role::from_db_value`]) and falls back to the header
    /// semantics for a blank/unrecognized claim (so an unknown verified role is still fail-closed).
    pub fn from_hint(raw: Option<&str>, verified: bool) -> Role {
        if verified {
            if let Some(r) = Self::from_db_value(raw) {
                return r;
            }
        }
        Self::from_str_value(raw)
    }

    /// Back-compat alias: the `X-Role` header maps through the same canonical parser.
    pub fn from_header(raw: Option<&str>) -> Role {
        Role::from_str_value(raw)
    }

    /// Map a STORED `principals.role` value to an RBAC role IF it names one.
    ///
    /// `principals.role` is an identity/DOMAIN field that may legitimately hold
    /// values OUTSIDE the RBAC vocabulary (the schema's own example is `"manager"`).
    /// Only a recognized RBAC role is an authoritative assignment; anything else —
    /// a domain label, blank, or NULL — returns `None`, so [`PolicyGateway::authorize`]
    /// falls back to the header/default instead of silently DOWNGRADING such a
    /// principal to least privilege. This is deliberately STRICTER than
    /// [`Role::from_str_value`] (used for the caller-asserted `X-Role` header, which
    /// fails an *unknown* value closed to `Viewer`): a stored domain role must be
    /// inert for RBAC, exactly as it was before roles became DB-authoritative.
    ///
    /// It is one of the two TRUSTED sources of the elevated `Admin` role: an UNVERIFIED request hint
    /// ([`Role::from_str_value`], the `X-Role` header) can assert only viewer/member, so `admin` is
    /// unforgeable from a plaintext header — it must be operator-provisioned in `principals.role`
    /// here OR asserted by a cryptographically VERIFIED signed-JWT claim ([`Role::from_hint`] with
    /// `verified = true`).
    fn from_db_value(raw: Option<&str>) -> Option<Role> {
        match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("viewer") | Some("reader") => Some(Role::Viewer),
            Some("member") => Some(Role::Member),
            Some("admin") => Some(Role::Admin),
            _ => None,
        }
    }

    /// The canonical role name — the value stored in `role_permissions.role` and the
    /// key the permission lookup binds. Round-trips through [`Role::from_str_value`].
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Member => "member",
            Role::Admin => "admin",
        }
    }
}

/// A governed action. `as_str()` equals the audit `action` label, so policy and
/// audit share one vocabulary.
#[derive(Debug, Clone, Copy)]
pub enum Action {
    DocumentsIngest,
    Retrieve,
    ContextUpsert,
    ContextGet,
    SkillsRegister,
    SkillsGet,
    ToolExecute,
    ToolsRegister,
    ToolsList,
    ToolsDecide,
    ToolsRollback,
    RunsStart,
    RunsResume,
    AuditEvents,
    TeamsCreate,
    TeamsAddMember,
    TeamsRemoveMember,
    TeamsList,
    TeamsMembers,
    DocumentsGrant,
    DocumentsRevoke,
}

impl Action {
    /// The stable action label (also the audit `action`).
    pub fn as_str(self) -> &'static str {
        match self {
            Action::DocumentsIngest => "documents.ingest",
            Action::Retrieve => "retrieve",
            Action::ContextUpsert => "context.upsert",
            Action::ContextGet => "context.get",
            Action::SkillsRegister => "skills.register",
            Action::SkillsGet => "skills.get",
            Action::ToolExecute => "tool.execute",
            Action::ToolsRegister => "tools.register",
            Action::ToolsList => "tools.list",
            Action::ToolsDecide => "tools.decide",
            Action::ToolsRollback => "tools.rollback",
            Action::RunsStart => "runs.start",
            Action::RunsResume => "runs.resume",
            Action::AuditEvents => "audit.events",
            Action::TeamsCreate => "teams.create",
            Action::TeamsAddMember => "teams.add_member",
            Action::TeamsRemoveMember => "teams.remove_member",
            Action::TeamsList => "teams.list",
            Action::TeamsMembers => "teams.members",
            Action::DocumentsGrant => "documents.grant",
            Action::DocumentsRevoke => "documents.revoke",
        }
    }

    /// Every governed action. The single source of truth for exhaustive iteration — the policy
    /// default-matrix test and the `role_permissions` action-vocabulary drift-guard both walk this,
    /// so a NEW action can't silently drift from the `role_permissions_action_check` CHECK (its
    /// `as_str()` must be in that migration's vocabulary — see 0023).
    pub const ALL: [Action; 21] = [
        Action::DocumentsIngest,
        Action::Retrieve,
        Action::ContextUpsert,
        Action::ContextGet,
        Action::SkillsRegister,
        Action::SkillsGet,
        Action::ToolExecute,
        Action::ToolsRegister,
        Action::ToolsList,
        Action::ToolsDecide,
        Action::ToolsRollback,
        Action::RunsStart,
        Action::RunsResume,
        Action::AuditEvents,
        Action::TeamsCreate,
        Action::TeamsAddMember,
        Action::TeamsRemoveMember,
        Action::TeamsList,
        Action::TeamsMembers,
        Action::DocumentsGrant,
        Action::DocumentsRevoke,
    ];

    /// Read-only actions — the subset a `Viewer` may perform.
    fn is_read(self) -> bool {
        matches!(
            self,
            Action::Retrieve
                | Action::ContextGet
                | Action::SkillsGet
                | Action::ToolsList
                | Action::AuditEvents
                | Action::TeamsList
                | Action::TeamsMembers
        )
    }

    /// Actions whose `resource` is the SUBJECT principal_id of a context profile —
    /// the ones the opt-in context-ownership ABAC rule applies to.
    fn is_context_ownership(self) -> bool {
        matches!(self, Action::ContextGet | Action::ContextUpsert)
    }
}

/// The in-code DEFAULT permission matrix: whether `role` may perform `action` when
/// the tenant has NOT configured `role_permissions` for that role. Member = all;
/// Viewer = reads only.
fn permits_default(role: Role, action: Action) -> bool {
    match role {
        // Baseline (the default for role-less callers) + elevated: everything the
        // gateway governs. Admin ⊇ Member here; the Admin-only distinctions (creating a
        // team, managing another's team) are resource-layer checks in the handler.
        Role::Member | Role::Admin => true,
        // Read-only.
        Role::Viewer => action.is_read(),
    }
}

/// Central policy decision point (PDP).
#[derive(Debug, Clone, Default)]
pub struct PolicyGateway;

impl PolicyGateway {
    /// Construct a new gateway.
    pub fn new() -> Self {
        Self
    }

    /// Authorize `principal` to perform `action` on `resource` within `tenant`.
    ///
    /// DB-authoritative: reads both the role and the permission under Row-Level
    /// Security in a single `tenant_tx`, so the lookup is tenant-isolated:
    /// 1. **role** — the DB-authoritative `principals.role` (via [`Role::from_db_value`]) wins when
    ///    it names an RBAC role; otherwise the request hint via [`Role::from_hint`]. A verified
    ///    signed-JWT hint may assert the elevated `admin`; an unverified `X-Role` header cannot.
    /// 2. **permission** — if the tenant has ANY `role_permissions` rows for the
    ///    resolved role, those are that role's COMPLETE allowlist (a tenant override
    ///    that can broaden OR restrict); otherwise [`permits_default`] applies.
    ///
    /// `Ok(role)` — the caller's resolved [`Role`], which [`enforce`] uses for the ABAC
    /// admin override — when permitted; `Err(Error::Forbidden)` (→ 403) when refused. A DB
    /// failure propagates as-is (→ 500) — the action does not proceed (fail-closed).
    ///
    /// COST: this opens a dedicated read-only `tenant_tx` (one connection checkout +
    /// two round-trips) on every governed request, separate from the handler's own
    /// action transaction — an accepted price for a real decision point. A future
    /// optimization could fold the lookup into the handler's tx or cache it per
    /// `(tenant, principal)`; correctness does not depend on that.
    pub async fn authorize(
        &self,
        db: &PgPool,
        principal: &Principal,
        tenant: &str,
        action: Action,
        resource: &str,
    ) -> Result<Role> {
        let mut tx = tenant_tx(db, tenant).await?;
        let result = authorize_in_tx(
            &mut tx,
            &principal.principal_id,
            principal.role.as_deref(),
            principal.role_verified,
            tenant,
            action,
            resource,
        )
        .await;
        // Read-only decision: commit (or roll back on error) either way.
        tx.commit().await?;
        result
    }
}

/// Resolve the caller's effective RBAC [`Role`] on a caller-provided transaction: the
/// DB-authoritative `principals.role` when it names an RBAC role, else the request
/// `role_hint` via [`Role::from_hint`] — which honors the elevated `admin` only when the hint is
/// cryptographically VERIFIED (`role_verified`, a signed JWT), never a plaintext `X-Role` header.
/// Shared by [`authorize_in_tx`]
/// and by resource handlers that need the role for an ABAC decision (e.g. the team
/// authority check in [`crate::api::teams`]).
pub async fn resolve_role_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    principal_id: &str,
    role_hint: Option<&str>,
    role_verified: bool,
    tenant: &str,
) -> Result<Role> {
    // Filter tenant_id explicitly (defense-in-depth + composite-PK prefix) —
    // principal_id is unique only within a tenant.
    let stored: Option<(Option<String>,)> =
        sqlx::query_as("SELECT role FROM principals WHERE tenant_id = $1 AND principal_id = $2")
            .bind(tenant)
            .bind(principal_id)
            .fetch_optional(&mut **tx)
            .await?;
    // DB-authoritative role wins; else the request hint, which may assert `admin` ONLY when it is a
    // verified signed-JWT claim (never an unverified `X-Role` header).
    Ok(Role::from_db_value(stored.and_then(|(r,)| r).as_deref())
        .unwrap_or_else(|| Role::from_hint(role_hint, role_verified)))
}

/// The RBAC decision on a CALLER-PROVIDED transaction.
///
/// Factored out of [`PolicyGateway::authorize`] so the run executor can authorize a
/// governed Tool step **inside the run's own `tenant_tx`** (same reasoning as the
/// tool gateway's `execute_in_tx`) — so a role authorized to START a run cannot use
/// it to invoke a tool that same role is DENIED at `/tool.execute`. Loads the
/// DB-authoritative `principals.role` (falling back to `role_hint` — a request's
/// `X-Role`/JWT role — only when that names no RBAC role), then applies the
/// `role_permissions` override or the default matrix. In an executor context there
/// is no live request, so `role_hint` is `None` and the role is purely `principals.role`.
///
/// `Ok(role)` — the caller's resolved [`Role`] — when permitted (callers that only need
/// the yes/no can discard it); `Err(Error::Forbidden)` (→ 403) when refused.
pub async fn authorize_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    principal_id: &str,
    role_hint: Option<&str>,
    role_verified: bool,
    tenant: &str,
    action: Action,
    resource: &str,
) -> Result<Role> {
    // Resolve role + permission in ONE SQL statement. This keeps the DB-authoritative semantics
    // immediate (no stale cache after an operator updates principals/role_permissions) while avoiding
    // the old two-query role lookup followed by permission lookup on every governed request.
    //
    // `principals.role` wins only when it names a recognized RBAC role; otherwise the pre-resolved
    // request/JWT fallback role is used. A populated role_permissions allowlist still REPLACES the
    // in-code default matrix.
    let fallback_role = Role::from_hint(role_hint, role_verified);
    let (role_name, action_allowed, role_rows): (String, Option<bool>, i64) = sqlx::query_as(
        r#"
        WITH effective AS (
            SELECT COALESCE(
                (SELECT CASE lower(btrim(role))
                     WHEN 'viewer' THEN 'viewer'
                     WHEN 'reader' THEN 'viewer'
                     WHEN 'member' THEN 'member'
                     WHEN 'admin' THEN 'admin'
                     ELSE NULL END
                 FROM principals WHERE tenant_id = $1 AND principal_id = $2),
                $3
            ) AS role
        )
        SELECT effective.role, bool_or(rp.action = $4), count(rp.action)
        FROM effective
        LEFT JOIN role_permissions rp ON rp.tenant_id = $1 AND rp.role = effective.role
        GROUP BY effective.role
        "#,
    )
    .bind(tenant)
    .bind(principal_id)
    .bind(fallback_role.as_str())
    .bind(action.as_str())
    .fetch_one(&mut **tx)
    .await?;
    let role = Role::from_db_value(Some(&role_name)).ok_or_else(|| {
        Error::Internal(anyhow::anyhow!(
            "policy query returned unknown role {role_name:?}"
        ))
    })?;

    let allowed = if role_rows > 0 {
        action_allowed.unwrap_or(false)
    } else {
        permits_default(role, action)
    };
    tracing::debug!(
        principal = %principal_id,
        ?role,
        action = action.as_str(),
        resource,
        role_permissions_rows = role_rows,
        allowed,
        "policy check"
    );
    if allowed {
        Ok(role)
    } else {
        Err(Error::Forbidden)
    }
}

/// Authorize `action` for `principal` on `resource` within the already-resolved
/// `tenant`, auditing a `deny` on refusal.
///
/// Call this as the first governed step, right AFTER the tenant is resolved
/// (`require_tenant` / `authenticated_tenant`) and before the action runs. On
/// deny: best-effort audit `deny` under `tenant` (with the error `code`), then
/// `Err(Forbidden)` (→ 403). On allow: `Ok(Role)` returns the already-resolved
/// DB-authoritative role so resource handlers can avoid a duplicate lookup.
///
/// Because it runs after tenant resolution, an unauthenticated/mismatched request
/// never reaches it (so the DB-free paths stay DB-free — a deny audit only happens
/// once a real, authenticated caller is refused by policy).
pub async fn enforce(
    state: &AppState,
    principal: &Principal,
    tenant: &str,
    action: Action,
    resource: &str,
) -> Result<Role> {
    // 1. Coarse role x action RBAC (PR1/PR2). On allow it yields the caller's resolved
    //    role, which the ABAC step uses for the Admin cross-principal override.
    let role = match state
        .policy
        .authorize(&state.db, principal, tenant, action, resource)
        .await
    {
        Ok(role) => role,
        Err(e) => {
            // A policy refusal (403/401) audits as `deny`; a policy-evaluation failure
            // (e.g. the DB is unreachable) audits as `error` — never mislabeled a deny.
            audit::record_best_effort(
                &state.db,
                tenant,
                Some(&principal.principal_id),
                action.as_str(),
                resource,
                e.audit_outcome(),
                serde_json::json!({ "code": e.code() }),
            )
            .await;
            return Err(e);
        }
    };

    // 2. Resource ABAC (PR3, opt-in): a context profile is owned by its subject principal
    //    (the `resource`). When ABAC_CONTEXT_OWNERSHIP is enabled, a caller may access
    //    only their OWN context — EXCEPT an `Admin` (DB-authoritative), who may perform
    //    governed cross-principal access (the elevated tier from the team-authority
    //    pillar). The admin override is recorded as an explicit audit event here — the
    //    `context.get` handler does not success-audit, so this is the authoritative trail
    //    for cross-principal READS too. A non-admin attempt is denied + audited.
    if state.config.abac_context_ownership
        && action.is_context_ownership()
        && principal.principal_id.trim() != resource.trim()
    {
        if role == Role::Admin {
            tracing::info!(
                admin = %principal.principal_id,
                subject = %resource,
                action = action.as_str(),
                "admin cross-principal context override",
            );
            // Durable governance record of the privileged cross-principal access — but
            // ONLY for reads: a write (context.upsert) is already success-audited by its
            // handler, so auditing here too would double-count. context.get has no handler
            // success audit, so this is its authoritative trail.
            if action.is_read() {
                audit::record_best_effort(
                    &state.db,
                    tenant,
                    Some(&principal.principal_id),
                    action.as_str(),
                    resource,
                    "success",
                    serde_json::json!({ "abac": "context_ownership", "override": "admin" }),
                )
                .await;
            }
        } else {
            let e = Error::Forbidden;
            audit::record_best_effort(
                &state.db,
                tenant,
                Some(&principal.principal_id),
                action.as_str(),
                resource,
                e.audit_outcome(),
                serde_json::json!({ "code": e.code(), "abac": "context_ownership" }),
            )
            .await;
            return Err(e);
        }
    }
    Ok(role)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_from_header_omitted_is_member_unknown_is_viewer() {
        // Absent/blank = no role asserted = backward-compatible Member baseline.
        assert_eq!(Role::from_header(None), Role::Member);
        assert_eq!(Role::from_header(Some("")), Role::Member);
        assert_eq!(Role::from_header(Some("  ")), Role::Member);
        // Explicit recognized roles.
        assert_eq!(Role::from_header(Some("Member")), Role::Member);
        assert_eq!(Role::from_header(Some(" VIEWER ")), Role::Viewer);
        assert_eq!(Role::from_header(Some("reader")), Role::Viewer);
        // A role WAS asserted but is unrecognized -> fail closed to least privilege
        // (read-only), NOT the permissive Member baseline.
        assert_eq!(Role::from_header(Some("manager")), Role::Viewer);
        assert_eq!(Role::from_header(Some("viewr")), Role::Viewer); // typo of viewer
                                                                    // `admin` is DB-authoritative only (from_db_value); a header/hint-asserted admin
                                                                    // is NOT honored — it fails closed to Viewer, so a caller can't self-elevate.
        assert_eq!(Role::from_header(Some("admin")), Role::Viewer);
        assert_eq!(Role::from_header(Some(" ADMIN ")), Role::Viewer);
    }

    #[test]
    fn member_permits_everything_viewer_only_reads() {
        for a in Action::ALL {
            assert!(
                permits_default(Role::Member, a),
                "member must permit {}",
                a.as_str()
            );
            // Admin ⊇ Member in the coarse matrix.
            assert!(
                permits_default(Role::Admin, a),
                "admin must permit {}",
                a.as_str()
            );
            assert_eq!(
                permits_default(Role::Viewer, a),
                a.is_read(),
                "viewer permits {} iff read",
                a.as_str()
            );
        }
        assert!(!permits_default(Role::Viewer, Action::DocumentsIngest));
        assert!(permits_default(Role::Viewer, Action::Retrieve));
    }

    #[test]
    fn role_as_str_round_trips_and_is_the_stored_key() {
        // The canonical name stored in role_permissions.role and used by the DB
        // permission lookup must round-trip through the same parser that reads it.
        // viewer/member round-trip through the header/hint parser; admin is
        // DB-authoritative only, so it round-trips through from_db_value instead.
        for r in [Role::Viewer, Role::Member] {
            assert_eq!(Role::from_str_value(Some(r.as_str())), r);
        }
        assert_eq!(
            Role::from_db_value(Some(Role::Admin.as_str())),
            Some(Role::Admin)
        );
        assert_eq!(Role::Viewer.as_str(), "viewer");
        assert_eq!(Role::Member.as_str(), "member");
        assert_eq!(Role::Admin.as_str(), "admin");
    }

    #[test]
    fn from_db_value_recognizes_only_rbac_roles() {
        // A stored RBAC role is authoritative...
        assert_eq!(Role::from_db_value(Some("viewer")), Some(Role::Viewer));
        assert_eq!(Role::from_db_value(Some("reader")), Some(Role::Viewer));
        assert_eq!(Role::from_db_value(Some(" Member ")), Some(Role::Member));
        assert_eq!(Role::from_db_value(Some("admin")), Some(Role::Admin));
        // ...but a NON-RBAC domain value, blank, or NULL is inert (None -> the caller
        // falls back to the header/default rather than downgrading to Viewer). This
        // is the key difference from the header parser and the fix for the silent
        // domain-role downgrade.
        assert_eq!(Role::from_db_value(Some("manager")), None);
        assert_eq!(Role::from_db_value(Some("agent")), None);
        assert_eq!(Role::from_db_value(Some("")), None);
        assert_eq!(Role::from_db_value(None), None);
        // Contrast: the header parser fails an unknown value CLOSED to Viewer.
        assert_eq!(Role::from_str_value(Some("manager")), Role::Viewer);
    }

    #[test]
    fn from_hint_honors_admin_only_when_verified() {
        // A VERIFIED (signed-JWT) hint may assert admin...
        assert_eq!(Role::from_hint(Some("admin"), true), Role::Admin);
        // ...an UNVERIFIED (X-Role header) hint may NOT — it fails admin closed to Viewer.
        assert_eq!(Role::from_hint(Some("admin"), false), Role::Viewer);
        // Non-elevated roles resolve identically regardless of verification.
        for verified in [true, false] {
            assert_eq!(Role::from_hint(Some("viewer"), verified), Role::Viewer);
            assert_eq!(Role::from_hint(Some("member"), verified), Role::Member);
            // No role claim -> the Member baseline.
            assert_eq!(Role::from_hint(None, verified), Role::Member);
            assert_eq!(Role::from_hint(Some(""), verified), Role::Member);
            // An unknown value is fail-closed to Viewer even when verified (admin is the ONLY
            // elevation a verified hint unlocks).
            assert_eq!(Role::from_hint(Some("manager"), verified), Role::Viewer);
        }
    }
}
