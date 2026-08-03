//! `POST /context.upsert` + `POST /context.get` handlers.

use axum::extract::State;
use axum::Json;

use crate::audit;
use crate::auth::policy::{enforce, Action};
use crate::auth::Principal;
use crate::context_service::ContextService;
use crate::domain::{Context, ContextGetRequest, ContextUpsertRequest, ContextUpsertResponse};
use crate::error::{Error, Result};
use crate::state::AppState;

/// Upsert a principal's context profile (personal/team/org).
pub async fn upsert(
    State(state): State<AppState>,
    principal: Principal,
    Json(req): Json<ContextUpsertRequest>,
) -> Result<Json<ContextUpsertResponse>> {
    // Tenant is the authenticated principal's; if the body names one it may only
    // confirm it (mismatch -> 403). Missing X-Tenant-Id -> 401 (fail closed).
    let tenant = match req.tenant_id.as_deref() {
        Some(t) => principal.require_tenant(t)?,
        None => principal.authenticated_tenant()?,
    };
    tracing::info!(
        principal = %principal.principal_id,
        tenant = %tenant,
        subject = %req.principal_id,
        contains_pii = req.data_classification.contains_pii,
        "context.upsert"
    );

    // Trim the subject id once and use it consistently for the audit + response.
    let principal_id = req.principal_id.trim();
    // Authorize the role: a Viewer is denied context writes (403 + audit deny).
    enforce(
        &state,
        &principal,
        tenant,
        Action::ContextUpsert,
        principal_id,
    )
    .await?;
    let service = ContextService::new(state.db.clone());
    let result = service.upsert(tenant, &req).await;
    let (outcome, metadata) = match &result {
        Ok(()) => (
            "success",
            serde_json::json!({ "contains_pii": req.data_classification.contains_pii }),
        ),
        Err(e) => e.audit_report(),
    };
    audit::record_best_effort(
        &state.db,
        tenant,
        Some(&principal.principal_id),
        "context.upsert",
        principal_id,
        outcome,
        metadata,
    )
    .await;
    result?;

    Ok(Json(ContextUpsertResponse {
        principal_id: principal_id.to_string(),
        status: "upserted".to_string(),
    }))
}

/// Fetch a principal's context profile.
pub async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Json(req): Json<ContextGetRequest>,
) -> Result<Json<Context>> {
    let tenant = principal.authenticated_tenant()?;
    let principal_id = req.principal_id.trim();
    if principal_id.is_empty() {
        return Err(Error::BadRequest("principal_id is required".into()));
    }
    enforce(&state, &principal, tenant, Action::ContextGet, principal_id).await?;
    tracing::info!(
        principal = %principal.principal_id,
        tenant = %tenant,
        subject = %principal_id,
        "context.get"
    );

    let service = ContextService::new(state.db.clone());
    service
        .get(tenant, principal_id)
        .await?
        .map(Json)
        .ok_or_else(|| Error::NotFound(format!("context for principal '{principal_id}'")))
}
