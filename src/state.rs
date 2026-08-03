//! Shared application state injected into every handler.

use std::sync::Arc;

use crate::auth::policy::PolicyGateway;
use crate::auth::JwtDecoder;
use crate::config::Config;
use crate::mcp::{default_connector, ConnectorImpl};
use crate::retrieval::default_embedder;
use crate::retrieval::embed::EmbedderImpl;

/// Application state passed to handlers via axum's `State` extractor.
///
/// Cheap to clone: the pool is internally reference-counted and the config +
/// embedder are wrapped in [`Arc`]s.
#[derive(Clone)]
pub struct AppState {
    /// Lazily-connected Postgres pool (source-of-truth + derived stores).
    pub db: sqlx::PgPool,
    /// Immutable, shared runtime configuration.
    pub config: Arc<Config>,
    /// Process-wide embedder (real provider or [`crate::retrieval::embed::MockEmbedder`]),
    /// chosen ONCE from config at startup and shared by ingest + retrieve so the
    /// same model/dimension is used everywhere.
    pub embedder: Arc<EmbedderImpl>,
    /// The Policy & Access Gateway (RBAC decision point); every governed handler
    /// authorizes through it. Cheap to clone (a unit struct in PR1).
    pub policy: PolicyGateway,
    /// Process-wide tool connector (real MCP/HTTP or [`crate::mcp::ConnectorImpl::Disabled`]),
    /// chosen ONCE from config at startup and shared by the tool gateway.
    pub connector: Arc<ConnectorImpl>,
    /// The bearer-token verifier (RS256 public key / HS256 secret / trusted-headers), built ONCE
    /// from config at startup so the RS256 PEM is not re-parsed on every authenticated request.
    pub jwt_decoder: Arc<JwtDecoder>,
    /// Process-wide, low-cardinality HTTP metric instruments. These are no-ops when no OTLP meter
    /// provider is configured, so the request middleware has one unconditional path.
    pub http_metrics: crate::telemetry::HttpMetrics,
}

impl AppState {
    /// Construct state from a pool and an owned config.
    pub fn new(db: sqlx::PgPool, config: Config) -> Self {
        // Build the embedder + connector + JWT verifier from config before moving config into the Arc.
        let embedder = Arc::new(default_embedder(&config));
        let connector = Arc::new(default_connector(&config));
        let jwt_decoder = Arc::new(JwtDecoder::from_config(&config));
        Self {
            db,
            config: Arc::new(config),
            embedder,
            policy: PolicyGateway::new(),
            connector,
            jwt_decoder,
            http_metrics: crate::telemetry::HttpMetrics::new(),
        }
    }
}
