//! HTTP API surface.
//!
//! Wires every governed brain/admin endpoint (plus liveness/readiness) to its handler,
//! adds a request-tracing layer, and injects [`AppState`].

use axum::extract::DefaultBodyLimit;
use axum::middleware;
use axum::routing::{get, post};
use axum::Router;
use tower::limit::ConcurrencyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

use crate::state::AppState;

pub mod audit;
pub mod context;
pub mod documents;
pub mod health;
pub mod idempotency;
pub mod mcp;
pub mod retrieve;
pub mod revocations;
pub mod runs;
pub mod skills;
pub mod teams;
pub mod tools;

/// Assemble the full Synapse HTTP API.
pub fn router(state: AppState) -> Router {
    let http_metrics = state.http_metrics.clone();
    Router::new()
        // --- liveness / readiness ---
        .route("/health", get(health::health))
        .route("/ready", get(health::ready))
        // Stateless MCP Streamable HTTP endpoint for coding agents.
        .route("/mcp", get(mcp::get).post(mcp::post))
        // --- governed brain endpoints (verified JWT or trusted-gateway Principal) ---
        .route("/skills.register", post(skills::register))
        .route("/skills.get", post(skills::get))
        .route("/documents.ingest", post(documents::ingest))
        .route("/documents.reembed", post(documents::reembed))
        .route("/context.upsert", post(context::upsert))
        .route("/context.get", post(context::get))
        .route("/retrieve", post(retrieve::retrieve))
        .route("/tool.execute", post(tools::execute))
        .route("/tools.register", post(tools::register))
        .route("/tools.list", post(tools::list))
        .route("/tools.decide", post(tools::decide))
        .route("/tools.rollback", post(tools::rollback))
        .route("/runs.start", post(runs::start))
        .route("/runs.resume", post(runs::resume))
        .route("/audit/events", get(audit::events))
        // --- team membership + document ACL management ---
        .route("/teams.create", post(teams::create))
        .route("/teams.add_member", post(teams::add_member))
        .route("/teams.remove_member", post(teams::remove_member))
        .route("/teams.list", post(teams::list))
        .route("/teams.members", post(teams::members))
        .route("/documents.grant", post(documents::grant))
        .route("/documents.revoke", post(documents::revoke))
        // --- admin: per-subject token revocation (revoke-all / clear) ---
        .route(
            "/admin/revocations",
            post(revocations::revoke).delete(revocations::clear),
        )
        // Apply admission controls to every route. TraceLayer is outermost so rejected,
        // timed-out, and successful requests all produce one request span.
        .layer(DefaultBodyLimit::max(state.config.max_request_body_bytes))
        .layer(ConcurrencyLimitLayer::new(
            state.config.max_in_flight_requests,
        ))
        .layer(TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            std::time::Duration::from_secs(state.config.request_timeout_secs),
        ))
        .layer(TraceLayer::new_for_http())
        // Outermost: include timeouts, admission rejections, and handler responses in SLO metrics.
        .layer(middleware::from_fn_with_state(
            http_metrics,
            crate::telemetry::record_http_metrics,
        ))
        .with_state(state)
}
