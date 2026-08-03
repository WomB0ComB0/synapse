//! Admin token-revocation API: `POST /admin/revocations` (revoke-all for a principal) + `DELETE`
//! (clear).
//!
//! A revocation sets a per-`(tenant, principal)` `not_before` cutoff so every verified bearer token
//! that principal holds — issued (`iat`) before now — is rejected by the auth extractor (revoke-all
//! / "logout everywhere"), the mitigation for a compromised token before its short `exp`. Admin-only
//! (a non-admin is 403'd), tenant-isolated (`tenant_tx`/RLS, `tenant_id` bound explicitly), and
//! audited. ENFORCEMENT lives in [`crate::auth`] (gated by `AUTH_REVOCATION_ENABLED`); this module is
//! only the WRITE side, so the cutoff exists regardless of whether enforcement is currently enabled.

use axum::extract::State;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::audit;
use crate::auth::policy::{resolve_role_in_tx, Role};
use crate::auth::Principal;
use crate::db::tenant_tx;
use crate::error::{Error, Result};
use crate::state::AppState;

/// Revoke ALL of a principal's outstanding tokens (advance the cutoff to now).
#[derive(Debug, Deserialize)]
pub struct RevokeRequest {
    /// The principal whose tokens to revoke, in the caller's authenticated tenant.
    pub principal_id: String,
    /// Optional operator note (why); not security-bearing.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RevokeResponse {
    pub principal_id: String,
    /// Tokens issued (`iat`) strictly before this instant are now rejected.
    pub not_before: chrono::DateTime<chrono::Utc>,
    pub status: &'static str,
}

/// Clear a principal's revocation (re-allow its tokens).
#[derive(Debug, Deserialize)]
pub struct ClearRequest {
    pub principal_id: String,
}

#[derive(Debug, Serialize)]
pub struct ClearResponse {
    pub principal_id: String,
    pub status: &'static str,
}

/// `POST /admin/revocations` — admin-only. Upserts the `(tenant, principal)` cutoff to now, so every
/// token that principal holds (issued before now) is rejected. Re-revoking advances the cutoff.
pub async fn revoke(
    State(state): State<AppState>,
    principal: Principal,
    Json(req): Json<RevokeRequest>,
) -> Result<Json<RevokeResponse>> {
    let tenant = principal.authenticated_tenant()?;
    let target = req.principal_id.trim();
    if target.is_empty() {
        return Err(Error::BadRequest("principal_id is required".into()));
    }
    let reason = req
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let write: Result<chrono::DateTime<chrono::Utc>> = async {
        let mut tx = tenant_tx(&state.db, tenant).await?;
        // Admin-only — a verified `admin` role (DB-authoritative role wins over the request hint).
        if resolve_role_in_tx(
            &mut tx,
            &principal.principal_id,
            principal.role.as_deref(),
            principal.role_verified,
            tenant,
        )
        .await?
            != Role::Admin
        {
            return Err(Error::Forbidden);
        }
        let (not_before,): (chrono::DateTime<chrono::Utc>,) = sqlx::query_as(
            "INSERT INTO revocations (tenant_id, principal_id, not_before, reason) \
                 VALUES ($1, $2, now(), $3) \
             ON CONFLICT (tenant_id, principal_id) \
             DO UPDATE SET not_before = now(), reason = EXCLUDED.reason, updated_at = now() \
             RETURNING not_before",
        )
        .bind(tenant)
        .bind(target)
        .bind(reason)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| Error::db_or_conflict(e, "revocation conflict"))?;
        tx.commit().await?;
        Ok(not_before)
    }
    .await;

    let (outcome, metadata) = match &write {
        Ok(_) => ("success", serde_json::json!({ "principal_id": target })),
        Err(e) => e.audit_report(),
    };
    audit::record_best_effort(
        &state.db,
        tenant,
        Some(&principal.principal_id),
        "revocations.revoke",
        target,
        outcome,
        metadata,
    )
    .await;
    let not_before = write?;

    Ok(Json(RevokeResponse {
        principal_id: target.to_string(),
        not_before,
        status: "revoked",
    }))
}

/// `DELETE /admin/revocations` — admin-only. Clears the `(tenant, principal)` cutoff (re-allows the
/// principal's tokens). Idempotent: clearing a non-existent revocation is a success.
pub async fn clear(
    State(state): State<AppState>,
    principal: Principal,
    Json(req): Json<ClearRequest>,
) -> Result<Json<ClearResponse>> {
    let tenant = principal.authenticated_tenant()?;
    let target = req.principal_id.trim();
    if target.is_empty() {
        return Err(Error::BadRequest("principal_id is required".into()));
    }

    let write: Result<()> = async {
        let mut tx = tenant_tx(&state.db, tenant).await?;
        if resolve_role_in_tx(
            &mut tx,
            &principal.principal_id,
            principal.role.as_deref(),
            principal.role_verified,
            tenant,
        )
        .await?
            != Role::Admin
        {
            return Err(Error::Forbidden);
        }
        sqlx::query("DELETE FROM revocations WHERE tenant_id = $1 AND principal_id = $2")
            .bind(tenant)
            .bind(target)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
    .await;

    let (outcome, metadata) = match &write {
        Ok(()) => ("success", serde_json::json!({ "principal_id": target })),
        Err(e) => e.audit_report(),
    };
    audit::record_best_effort(
        &state.db,
        tenant,
        Some(&principal.principal_id),
        "revocations.clear",
        target,
        outcome,
        metadata,
    )
    .await;
    write?;

    Ok(Json(ClearResponse {
        principal_id: target.to_string(),
        status: "cleared",
    }))
}
