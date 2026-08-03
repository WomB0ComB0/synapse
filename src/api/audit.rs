//! `GET /audit/events` handler.

use axum::extract::{Query, State};
use axum::Json;
use uuid::Uuid;

use crate::audit::{AuditFilter, AuditLog};
use crate::auth::policy::{enforce, Action};
use crate::auth::Principal;
use crate::domain::{AuditEventsQuery, AuditEventsResponse};
use crate::error::{Error, Result};
use crate::state::AppState;

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 200;

/// Query the tenant's append-only audit log (newest first, keyset-paginated).
pub async fn events(
    State(state): State<AppState>,
    principal: Principal,
    Query(q): Query<AuditEventsQuery>,
) -> Result<Json<AuditEventsResponse>> {
    // Fail closed: audit is tenant-scoped, so a missing X-Tenant-Id is 401.
    let tenant = principal.authenticated_tenant()?;
    // Read action: allowed for all roles today (tightening to a privileged role is
    // a deferred PR); still routed through the gateway for a single audit vocabulary.
    enforce(&state, &principal, tenant, Action::AuditEvents, "events").await?;

    let before = match q.cursor.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(c) => Some(
            Uuid::parse_str(c).map_err(|_| Error::BadRequest(format!("invalid cursor '{c}'")))?,
        ),
        None => None,
    };
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

    // Treat blank filter params as "no filter" (consistent with the cursor).
    let norm = |o: Option<String>| o.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let filter = AuditFilter {
        action: norm(q.action),
        principal_id: norm(q.principal_id),
        resource: norm(q.resource),
        before,
        limit,
    };
    tracing::info!(
        principal = %principal.principal_id,
        tenant = %tenant,
        limit,
        "audit.events"
    );

    let (events, next_cursor) = AuditLog::new(state.db.clone())
        .query(tenant, &filter)
        .await?;
    Ok(Json(AuditEventsResponse {
        events,
        next_cursor,
    }))
}
