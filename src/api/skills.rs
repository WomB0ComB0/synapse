//! `POST /skills.register` + `POST /skills.get` handlers.

use axum::extract::State;
use axum::Json;

use crate::audit;
use crate::auth::policy::{enforce, Action};
use crate::auth::Principal;
use crate::domain::{
    SkillGetRequest, SkillGetResponse, SkillRegisterRequest, SkillRegisterResponse,
};
use crate::error::{Error, Result};
use crate::skills::registry::SkillRegistry;
use crate::state::AppState;

/// Register (and version) a skill in the registry.
pub async fn register(
    State(state): State<AppState>,
    principal: Principal,
    Json(req): Json<SkillRegisterRequest>,
) -> Result<Json<SkillRegisterResponse>> {
    // Fail closed: skills are tenant-scoped and the manifest carries no tenant,
    // so the tenant is ALWAYS the authenticated principal's (missing -> 401).
    let tenant = principal.authenticated_tenant()?;
    tracing::info!(
        principal = %principal.principal_id,
        tenant = %tenant,
        skill_id = %req.skill_id,
        version = %req.version,
        "skills.register"
    );

    // Authorize the role: a Viewer is denied skill registration (403 + audit deny).
    enforce(
        &state,
        &principal,
        tenant,
        Action::SkillsRegister,
        req.skill_id.trim(),
    )
    .await?;
    let registry = SkillRegistry::new(state.db.clone());
    let result = registry.register(tenant, &req).await;
    let (outcome, metadata) = match &result {
        Ok(is_latest) => (
            "success",
            serde_json::json!({ "version": req.version.clone(), "is_latest": is_latest }),
        ),
        Err(e) => e.audit_report(),
    };
    audit::record_best_effort(
        &state.db,
        tenant,
        Some(&principal.principal_id),
        "skills.register",
        req.skill_id.trim(),
        outcome,
        metadata,
    )
    .await;
    let is_latest = result?;

    Ok(Json(SkillRegisterResponse {
        skill_id: req.skill_id.trim().to_string(),
        version: req.version.trim().to_string(),
        status: if is_latest {
            "registered (latest)".to_string()
        } else {
            "registered".to_string()
        },
    }))
}

/// Fetch a versioned skill — the current latest, or a specific version.
pub async fn get(
    State(state): State<AppState>,
    principal: Principal,
    Json(req): Json<SkillGetRequest>,
) -> Result<Json<SkillGetResponse>> {
    let tenant = principal.authenticated_tenant()?;
    if req.skill_id.trim().is_empty() {
        return Err(Error::BadRequest("skill_id is required".into()));
    }
    tracing::info!(
        principal = %principal.principal_id,
        tenant = %tenant,
        skill_id = %req.skill_id,
        version = req.version.as_deref().unwrap_or("<latest>"),
        "skills.get"
    );

    enforce(
        &state,
        &principal,
        tenant,
        Action::SkillsGet,
        req.skill_id.trim(),
    )
    .await?;
    let registry = SkillRegistry::new(state.db.clone());
    let found = registry
        .get(tenant, &req.skill_id, req.version.as_deref())
        .await?;

    found.map(Json).ok_or_else(|| {
        let which = req
            .version
            .as_deref()
            .map(|v| format!(" version '{v}'"))
            .unwrap_or_default();
        Error::NotFound(format!("skill '{}'{which}", req.skill_id))
    })
}
