//! `POST /runs.start` + `POST /runs.resume` handlers.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;

use crate::api::idempotency::parse_idempotency_key;
use crate::audit;
use crate::auth::policy::{enforce, Action};
use crate::auth::Principal;
use crate::domain::{RunResponse, RunsResumeRequest, RunsStartRequest};
use crate::error::Result;
use crate::orchestration::runs::Orchestrator;
use crate::state::AppState;

/// Start a durable workflow run (sync or async, optionally human-gated).
pub async fn start(
    State(state): State<AppState>,
    principal: Principal,
    headers: HeaderMap,
    Json(req): Json<RunsStartRequest>,
) -> Result<Json<RunResponse>> {
    // Fail closed: the RLS tenant is the authenticated one; the body's tenant_id
    // may only confirm it (mismatch -> 403; missing X-Tenant-Id -> 401).
    let tenant = principal.require_tenant(&req.tenant_id)?;
    tracing::info!(
        principal = %principal.principal_id,
        tenant = %tenant,
        run_type = %req.run_type,
        workflow_id = ?req.workflow_id,
        human_approval = req.callbacks.human_approval,
        "runs.start"
    );

    // Authorize the role: a Viewer is denied starting runs (403 + audit deny).
    enforce(
        &state,
        &principal,
        tenant,
        Action::RunsStart,
        req.run_type.trim(),
    )
    .await?;
    // Optional client idempotency key (validated); a repeat replays the original run.
    let idempotency_key = parse_idempotency_key(&headers)?;
    let orchestrator = Orchestrator::new(
        state.db.clone(),
        state.connector.clone(),
        state.config.worker_enabled,
    );
    let result = orchestrator
        .start(
            tenant,
            &principal.principal_id,
            &req,
            idempotency_key.as_deref(),
        )
        .await;

    // Audit either way in a single call. On success a suspended (awaiting-approval)
    // run is the human-in-the-loop outcome and the resource is the new run_id; on
    // failure there is no run_id, so the resource is the (trimmed) run_type.
    let run_type = req.run_type.trim();
    let (resource, outcome, metadata) = match &result {
        Ok(resp) => (
            resp.run_id.clone(),
            if resp.status == "waiting" {
                "require_approval"
            } else {
                "success"
            },
            serde_json::json!({ "status": resp.status, "run_type": run_type }),
        ),
        Err(e) => {
            let (outcome, metadata) = e.audit_report();
            (run_type.to_string(), outcome, metadata)
        }
    };
    audit::record_best_effort(
        &state.db,
        tenant,
        Some(&principal.principal_id),
        "runs.start",
        &resource,
        outcome,
        metadata,
    )
    .await;

    Ok(Json(result?))
}

/// Resume a suspended run using its opaque resume token.
pub async fn resume(
    State(state): State<AppState>,
    principal: Principal,
    Json(req): Json<RunsResumeRequest>,
) -> Result<Json<RunResponse>> {
    // Resume carries no tenant in the body; the tenant is always the authenticated
    // one (fail-closed: missing X-Tenant-Id -> 401). RLS scopes the run lookup.
    let tenant = principal.authenticated_tenant()?;
    tracing::info!(
        principal = %principal.principal_id,
        tenant = %tenant,
        run_id = %req.run_id,
        "runs.resume"
    );

    // Authorize the role: a Viewer is denied resuming runs (403 + audit deny).
    enforce(
        &state,
        &principal,
        tenant,
        Action::RunsResume,
        req.run_id.trim(),
    )
    .await?;
    let orchestrator = Orchestrator::new(
        state.db.clone(),
        state.connector.clone(),
        state.config.worker_enabled,
    );
    let result = orchestrator
        .resume(tenant, &principal.principal_id, &req)
        .await;

    let (resource, outcome, metadata) = match &result {
        Ok(resp) => (
            resp.run_id.clone(),
            // A resume that suspends again on a further approval gate is the
            // human-in-the-loop outcome, mirroring runs.start.
            if resp.status == "waiting" {
                "require_approval"
            } else {
                "success"
            },
            serde_json::json!({ "status": resp.status }),
        ),
        Err(e) => {
            let (outcome, metadata) = e.audit_report();
            (req.run_id.trim().to_string(), outcome, metadata)
        }
    };
    audit::record_best_effort(
        &state.db,
        tenant,
        Some(&principal.principal_id),
        "runs.resume",
        &resource,
        outcome,
        metadata,
    )
    .await;

    Ok(Json(result?))
}
