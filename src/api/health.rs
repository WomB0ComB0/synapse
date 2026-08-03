//! Liveness + readiness probes.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;

use crate::state::AppState;

/// `GET /health` — liveness. Cheap; never touches the database.
pub async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

/// `GET /ready` — readiness. Confirms Postgres answers AND that the app's DB
/// role still enforces RLS (not superuser/bypassrls) — fail-closed on either,
/// so a privileged role that would silently void tenant isolation never serves
/// traffic. One `pg_roles` query covers both reachability and the invariant.
pub async fn ready(State(state): State<AppState>) -> impl IntoResponse {
    match crate::db::assert_rls_enforcing(&state.db).await {
        Ok(()) => (StatusCode::OK, Json(json!({ "status": "ready" }))),
        Err(err) => {
            tracing::warn!(error = %err, "readiness check failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "status": "unavailable" })),
            )
        }
    }
}
