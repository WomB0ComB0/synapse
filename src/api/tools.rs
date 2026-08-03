//! Governed tool registry and execution lifecycle handlers.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;

use crate::api::idempotency::parse_idempotency_key;
use crate::audit;
use crate::auth::policy::{enforce, Action, Role};
use crate::auth::Principal;
use crate::domain::{
    ToolDecisionRequest, ToolExecuteRequest, ToolExecuteResponse, ToolLifecycleResponse,
    ToolListResponse, ToolRegisterRequest, ToolRegisterResponse, ToolRollbackRequest,
};
use crate::error::{Error, Result};
use crate::state::AppState;
use crate::tools::gateway::ToolGateway;
use crate::tools::registry;

async fn require_admin(
    state: &AppState,
    principal: &Principal,
    tenant: &str,
    role: Role,
    action: &str,
    resource: &str,
) -> Result<()> {
    if role != Role::Admin {
        audit::record_best_effort(
            &state.db,
            tenant,
            Some(&principal.principal_id),
            action,
            resource,
            "deny",
            serde_json::json!({ "reason": "admin role required" }),
        )
        .await;
        return Err(Error::Forbidden);
    }
    Ok(())
}

/// Execute a tool/connector call through the policy-guarded gateway.
pub async fn execute(
    State(state): State<AppState>,
    principal: Principal,
    headers: HeaderMap,
    Json(req): Json<ToolExecuteRequest>,
) -> Result<Json<ToolExecuteResponse>> {
    // Fail closed: the RLS tenant is the authenticated one; the body's tenant_id
    // may only confirm it (mismatch -> 403; missing X-Tenant-Id -> 401).
    let tenant = principal.require_tenant(&req.tenant_id)?;
    // Authorize the role: a Viewer is denied tool execution (403 + audit deny).
    // Runs before the ToolGateway's approval gate.
    enforce(
        &state,
        &principal,
        tenant,
        Action::ToolExecute,
        req.tool_id.trim(),
    )
    .await?;
    tracing::info!(
        principal = %principal.principal_id,
        tenant = %tenant,
        tool = %req.tool_id,
        approval_mode = ?req.policy.approval_mode,
        "tool.execute"
    );

    // Optional client idempotency key (validated); a repeat replays the original outcome and
    // never re-invokes the non-idempotent connector.
    let idempotency_key = parse_idempotency_key(&headers)?;
    let gateway = ToolGateway::new(state.db.clone(), state.connector.clone());
    let result = gateway
        .execute(
            tenant,
            &principal.principal_id,
            &req,
            idempotency_key.as_deref(),
        )
        .await;

    // Audit the governed action either way: success/require_approval on the
    // human-in-the-loop outcome, or deny/error on failure.
    let (outcome, metadata) = match &result {
        Ok(resp) => (
            if resp.requires_approval {
                "require_approval"
            } else {
                "success"
            },
            serde_json::json!({ "status": resp.status, "requires_approval": resp.requires_approval }),
        ),
        Err(e) => e.audit_report(),
    };
    audit::record_best_effort(
        &state.db,
        tenant,
        Some(&principal.principal_id),
        "tool.execute",
        req.tool_id.trim(),
        outcome,
        metadata,
    )
    .await;

    Ok(Json(result?))
}

/// Create or update a tenant-owned tool contract. Admin-only.
pub async fn register(
    State(state): State<AppState>,
    principal: Principal,
    Json(req): Json<ToolRegisterRequest>,
) -> Result<Json<ToolRegisterResponse>> {
    let tenant = principal.authenticated_tenant()?;
    let tool_id = req.tool_id.trim();
    let role = enforce(&state, &principal, tenant, Action::ToolsRegister, tool_id).await?;
    require_admin(&state, &principal, tenant, role, "tools.register", tool_id).await?;

    let result = registry::register(&state.db, tenant, &req).await;
    let (outcome, metadata) = match &result {
        Ok(tool) => (
            "success",
            serde_json::json!({ "revision": tool.revision, "enabled": tool.enabled }),
        ),
        Err(error) => error.audit_report(),
    };
    audit::record_best_effort(
        &state.db,
        tenant,
        Some(&principal.principal_id),
        "tools.register",
        tool_id,
        outcome,
        metadata,
    )
    .await;
    let tool = result?;
    Ok(Json(ToolRegisterResponse {
        tool,
        status: "registered".into(),
    }))
}

/// List the authenticated tenant's registered tool contracts.
pub async fn list(
    State(state): State<AppState>,
    principal: Principal,
) -> Result<Json<ToolListResponse>> {
    let tenant = principal.authenticated_tenant()?;
    enforce(&state, &principal, tenant, Action::ToolsList, tenant).await?;
    let tools = registry::list(&state.db, tenant).await?;
    Ok(Json(ToolListResponse { tools }))
}

/// Approve or deny a standalone pending tool execution. Admin-only.
pub async fn decide(
    State(state): State<AppState>,
    principal: Principal,
    Json(req): Json<ToolDecisionRequest>,
) -> Result<Json<ToolLifecycleResponse>> {
    let tenant = principal.authenticated_tenant()?;
    let execution_id = uuid::Uuid::parse_str(req.execution_id.trim())
        .map_err(|_| Error::BadRequest("execution_id must be a UUID".into()))?;
    let role = enforce(
        &state,
        &principal,
        tenant,
        Action::ToolsDecide,
        req.execution_id.trim(),
    )
    .await?;
    require_admin(
        &state,
        &principal,
        tenant,
        role,
        "tools.decide",
        req.execution_id.trim(),
    )
    .await?;

    let gateway = ToolGateway::new(state.db.clone(), state.connector.clone());
    let result = gateway
        .decide(
            tenant,
            &principal.principal_id,
            execution_id,
            req.decision,
            req.reason.as_deref(),
        )
        .await;
    let (outcome, metadata) = match &result {
        Ok(response) => (
            "success",
            serde_json::json!({ "status": response.status, "tool_id": response.tool_id }),
        ),
        Err(error) => error.audit_report(),
    };
    audit::record_best_effort(
        &state.db,
        tenant,
        Some(&principal.principal_id),
        "tools.decide",
        req.execution_id.trim(),
        outcome,
        metadata,
    )
    .await;
    Ok(Json(result?))
}

/// Invoke the registered compensation tool for an executed call. Admin-only and idempotent.
pub async fn rollback(
    State(state): State<AppState>,
    principal: Principal,
    Json(req): Json<ToolRollbackRequest>,
) -> Result<Json<ToolLifecycleResponse>> {
    let tenant = principal.authenticated_tenant()?;
    let execution_id = uuid::Uuid::parse_str(req.execution_id.trim())
        .map_err(|_| Error::BadRequest("execution_id must be a UUID".into()))?;
    let role = enforce(
        &state,
        &principal,
        tenant,
        Action::ToolsRollback,
        req.execution_id.trim(),
    )
    .await?;
    require_admin(
        &state,
        &principal,
        tenant,
        role,
        "tools.rollback",
        req.execution_id.trim(),
    )
    .await?;

    let gateway = ToolGateway::new(state.db.clone(), state.connector.clone());
    let result = gateway
        .rollback(
            tenant,
            &principal.principal_id,
            execution_id,
            req.reason.as_deref(),
        )
        .await;
    let (outcome, metadata) = match &result {
        Ok(response) => (
            "success",
            serde_json::json!({ "status": response.status, "tool_id": response.tool_id }),
        ),
        Err(error) => error.audit_report(),
    };
    audit::record_best_effort(
        &state.db,
        tenant,
        Some(&principal.principal_id),
        "tools.rollback",
        req.execution_id.trim(),
        outcome,
        metadata,
    )
    .await;
    Ok(Json(result?))
}
