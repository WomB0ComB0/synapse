//! Runtime configuration, loaded purely from environment variables.
//!
//! We intentionally avoid a config crate here to keep startup configuration
//! dependency-light and 12-factor friendly.

use anyhow::Context as _;
use std::path::PathBuf;

/// Supported embedding providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingProvider {
    /// Deterministic local hashing embedder for dev/CI or explicit degraded mode.
    Mock,
    /// OpenAI-compatible `/embeddings` API.
    OpenAi,
    /// Google Gemini native embeddings API.
    Gemini,
}

impl EmbeddingProvider {
    /// Parse an env value for `EMBEDDING_PROVIDER`.
    fn from_env_value(raw: &str) -> anyhow::Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "mock" | "local" | "none" => Ok(Self::Mock),
            "openai" | "openai-compatible" | "compatible" => Ok(Self::OpenAi),
            "gemini" | "google" => Ok(Self::Gemini),
            other => anyhow::bail!(
                "EMBEDDING_PROVIDER must be one of gemini, openai, mock; got {other:?}"
            ),
        }
    }

    /// Stable provider label for logs/config dumps.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mock => "mock",
            Self::OpenAi => "openai",
            Self::Gemini => "gemini",
        }
    }

    /// Default model for this provider.
    pub fn default_model(self) -> &'static str {
        match self {
            Self::Mock | Self::OpenAi => "text-embedding-3-small",
            Self::Gemini => "gemini-embedding-2",
        }
    }

    /// Default base URL for this provider.
    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::Mock | Self::OpenAi => "https://api.openai.com/v1",
            Self::Gemini => "https://generativelanguage.googleapis.com/v1beta",
        }
    }

    /// Provider-specific API key env var.
    pub fn api_key_env_var(self) -> Option<&'static str> {
        match self {
            Self::Mock => None,
            Self::OpenAi => Some("OPENAI_API_KEY"),
            Self::Gemini => Some("GEMINI_API_KEY"),
        }
    }
}

/// Service configuration.
///
/// `Debug` is implemented manually to REDACT secrets (`database_url`, which
/// carries a password, and `openai_api_key`) so a `{:?}` of the config — in a
/// log line or error context — never leaks credentials.
#[derive(Clone)]
pub struct Config {
    /// Production posture gate (`SYNAPSE_ENV=production`). Startup refuses insecure auth,
    /// mock embeddings, or disabled retrieval/write safeguards when this is true.
    pub production_mode: bool,
    /// Postgres connection string (sqlx / pgvector), e.g.
    /// `postgres://user:pass@localhost:5432/synapse`.
    pub database_url: String,
    /// Socket address the HTTP server binds to. Default `127.0.0.1:8080`.
    pub bind_addr: String,
    /// Maximum database pool size (`DB_MAX_CONNECTIONS`, default 20).
    pub db_max_connections: u32,
    /// Maximum wait for a database connection (`DB_ACQUIRE_TIMEOUT_SECS`, default 10).
    pub db_acquire_timeout_secs: u64,
    /// Maximum accepted HTTP request body (`MAX_REQUEST_BODY_BYTES`, default 16 MiB).
    pub max_request_body_bytes: usize,
    /// End-to-end request deadline (`REQUEST_TIMEOUT_SECS`, default 180).
    pub request_timeout_secs: u64,
    /// Maximum requests admitted concurrently (`MAX_IN_FLIGHT_REQUESTS`, default 256).
    pub max_in_flight_requests: usize,
    /// Embedding model identifier used by the retrieval pipeline.
    ///
    /// Versioning the embedding model is a first-class design principle:
    /// derived vectors are rebuildable, so the model id is recorded on every
    /// [`crate::domain::Chunk`]. Defaults from [`EmbeddingProvider`].
    pub embedding_model: String,
    /// Selected embedding provider. Inferred from API key env vars unless `EMBEDDING_PROVIDER` is
    /// set explicitly.
    pub embedding_provider: EmbeddingProvider,
    /// API key for the embedding provider (`GEMINI_API_KEY` preferred, or legacy
    /// `OPENAI_API_KEY` for OpenAI-compatible providers).
    ///
    /// When unset, the service uses the deterministic [`crate::retrieval::embed::MockEmbedder`]
    /// (dev/CI, no network) — so the embedder choice is made ONCE at startup and
    /// never mixes real and mock vectors in the same index. Kept as `openai_api_key`
    /// internally for struct compatibility with existing tests/config construction.
    pub openai_api_key: Option<String>,
    /// Base URL of the embeddings API. With `GEMINI_API_KEY`, defaults to Gemini's
    /// native API (`https://generativelanguage.googleapis.com/v1beta`). Otherwise it
    /// defaults to the OpenAI-compatible API (`https://api.openai.com/v1`). Operator-only,
    /// never derived from a request — point it at Gemini / Azure / vLLM / LiteLLM / a
    /// self-hosted endpoint to keep document text in-boundary. No SSRF surface.
    pub embedding_base_url: String,
    /// Max inputs per embeddings request (batching). Default 96.
    pub embedding_max_batch: usize,
    /// Per-request timeout for embeddings calls, in seconds. Default 30.
    pub embedding_timeout_secs: u64,
    /// Max retries on 429 / 5xx / transport errors (exponential backoff). Default 3.
    pub embedding_max_retries: u32,
    /// Optional OpenTelemetry OTLP HTTP/protobuf base endpoint.
    ///
    /// Signal paths are appended by [`crate::telemetry`], which exports traces and metrics. The URL
    /// may use HTTP for a local/sidecar collector or HTTPS for a remote collector; credentials belong
    /// in `OTEL_EXPORTER_OTLP_HEADERS`, never in this URL.
    pub otel_endpoint: Option<String>,
    /// Optional HS256 secret for verifying caller `Authorization: Bearer <JWT>`
    /// tokens (`AUTH_JWT_SECRET`).
    ///
    /// When **set**, real caller authentication is ON: the [`crate::auth::Principal`]
    /// extractor requires a valid, unexpired, correctly-signed JWT and derives the
    /// caller's identity from its VERIFIED claims — the raw `X-Principal-Id` /
    /// `X-Tenant-Id` / `X-Role` headers are ignored, so they can no longer be
    /// spoofed. When **unset** (the default), the service stays in the documented
    /// trusted-upstream-gateway mode and reads identity from those headers verbatim.
    /// A secret, so it is redacted in `Debug`.
    pub auth_jwt_secret: Option<String>,
    /// Optional RS256 **public** key in PEM (`AUTH_JWT_PUBLIC_KEY`). When set, the caller must
    /// present an `Authorization: Bearer <RS256 JWT>` verified against this public key — ASYMMETRIC
    /// auth, so the service holds ONLY the public half and CANNOT mint tokens (unlike the shared HS256
    /// secret). Takes PRECEDENCE over `auth_jwt_secret` when both are set. Validated (parseable) at
    /// load — a set-but-unparseable key is a startup error, never a per-request 401. Public, not
    /// secret (a public key is not sensitive), so it is NOT redacted.
    pub auth_jwt_public_key: Option<String>,
    /// Optional expected JWT `aud` (audience) claim (`AUTH_JWT_AUDIENCE`), applied
    /// whenever ANY verified-JWT mode is active — i.e. when [`Config::auth_jwks_url`]
    /// (JWKS), [`Config::auth_jwt_public_key`] (static RS256), or
    /// [`Config::auth_jwt_secret`] (HS256) is set (the extractor pins `aud` in all).
    ///
    /// When **set**, a token's `aud` must match it (rejecting tokens minted for a
    /// different service). When **unset**, audience validation is DISABLED, so
    /// tokens with or without an `aud` claim are accepted — the interoperable
    /// default (many issuers always emit `aud`, and `jsonwebtoken` would otherwise
    /// reject any `aud`-bearing token when no audience is configured). Not a secret.
    pub auth_jwt_audience: Option<String>,
    /// Optional expected JWT `iss` (issuer) claim (`AUTH_JWT_ISSUER`). Production mode requires it so a key set shared across realms or identity-provider tenants cannot validate a token from the wrong security domain.
    pub auth_jwt_issuer: Option<String>,
    /// Optional JWKS endpoint URL (`AUTH_JWKS_URL`) for ROTATING RS256 public keys. When set, the
    /// caller must present an `Authorization: Bearer <RS256 JWT>` whose `kid` header matches a key
    /// served by this endpoint — asymmetric like [`Config::auth_jwt_public_key`], but the key set is
    /// fetched over HTTPS and refreshed on rotation instead of statically configured, so the service
    /// still holds only PUBLIC keys and CANNOT mint tokens. Takes PRECEDENCE over `auth_jwt_public_key`
    /// and `auth_jwt_secret` when set. Validated at load to be a well-formed `https://` URL (a
    /// non-HTTPS or malformed URL is a startup error); the keys themselves are fetched LAZILY on first
    /// use, so a JWKS outage at boot does NOT block startup. Not a secret (a public JWKS URL/keys
    /// aren't sensitive).
    pub auth_jwks_url: Option<String>,
    /// HTTP timeout (seconds) for fetching the JWKS document (`AUTH_JWKS_TIMEOUT_SECS`, default 10).
    /// Only meaningful when [`Config::auth_jwks_url`] is set.
    pub auth_jwks_timeout_secs: u64,
    /// Minimum interval (seconds) between JWKS refetches (`AUTH_JWKS_MIN_REFETCH_SECS`, default 60).
    /// A token whose `kid` is not cached triggers at most ONE refetch per this window — so a burst of
    /// unknown-`kid` tokens can't hammer the endpoint (an unknown-`kid` DoS), while a genuine key
    /// rotation is still picked up within the window. Only meaningful when [`Config::auth_jwks_url`]
    /// is set.
    pub auth_jwks_min_refetch_secs: u64,
    /// Opt-in stateful token revocation (`AUTH_REVOCATION_ENABLED`, default off). When **true**, a
    /// verified bearer token is additionally checked against the per-`(tenant, principal)`
    /// `revocations` cutoff after it verifies: a token whose `iat` is strictly before the cutoff (or
    /// that carries no `iat` while a cutoff exists) is rejected `401`. Enforced only in the
    /// verified-JWT modes (JWKS / static RS256 / HS256), never trusted-headers mode. When **false**
    /// (the default), no revocation check runs — preserving the pre-existing behavior and adding no
    /// per-request DB lookup. Fail-fast: a present-but-unrecognized value is a hard startup error.
    pub auth_revocation_enabled: bool,
    /// Opt-in resource ABAC: enforce context-ownership (`ABAC_CONTEXT_OWNERSHIP`).
    ///
    /// When **true**, a caller may read/write only their OWN context — the
    /// PolicyGateway requires `caller == subject` for `context.get`/`context.upsert`
    /// (the `resource` becomes authoritative) after the coarse role×action check.
    /// When **false** (the default), context access is governed by role RBAC alone
    /// (any authorized caller may access any principal's context in their tenant),
    /// preserving existing behavior. A future elevated/admin tier will allow
    /// governed cross-principal access.
    pub abac_context_ownership: bool,
    /// Opt-in embedding-model consistency at retrieval (`EMBEDDING_MODEL_CONSISTENCY`, default
    /// off). When **true**, the vector arm only cosine-compares chunks embedded by the SAME model
    /// as the current query (`chunks.embedding_model = embedding_model`), so a model change never
    /// silently compares vectors across incompatible spaces. Off preserves the pre-existing
    /// behavior (any stored vector is compared). Fail-fast on an unrecognized value.
    pub embedding_model_consistency: bool,
    /// Default MMR relevance↔diversity balance `λ` (`RETRIEVAL_MMR_LAMBDA`, default `0.5`, valid range
    /// `[0, 1]` — validated fail-fast at load). Used when a `retrieve` request sets `mmr: true` but
    /// does not supply its own `mmr_lambda`. `1.0` = pure relevance (no diversification); `0.0` =
    /// pure diversity. Inert unless a request opts into MMR.
    pub retrieval_mmr_lambda: f64,
    /// Opt-in per-tenant request rate limiting (`RATE_LIMIT_ENABLED`, default off). When **true**, a
    /// Postgres-backed token bucket per tenant is refilled+consumed on every authenticated request;
    /// a tenant that can't consume a token is rejected `429` with a `Retry-After` header. When
    /// **false** (the default), no bucket lookup runs. Fail-fast on an unrecognized value.
    pub rate_limit_enabled: bool,
    /// Token refill rate per tenant, tokens/second (`RATE_LIMIT_TENANT_RPS`, default `10.0`). The
    /// sustained allowed request rate. Must be `> 0` when rate limiting is enabled (validated at
    /// load). Inert unless [`Config::rate_limit_enabled`].
    pub rate_limit_tenant_rps: f64,
    /// Token bucket capacity per tenant (`RATE_LIMIT_BURST`, default `20.0`) — the maximum burst a
    /// tenant may spend at once, and the balance a new tenant starts with. Must be `>= 1` when rate
    /// limiting is enabled (validated at load). Inert unless [`Config::rate_limit_enabled`].
    pub rate_limit_burst: f64,
    /// Opt-in idempotent document ingest (`INGEST_IDEMPOTENCY_ENABLED`, default off). When **true**,
    /// `documents.ingest` fingerprints the FULL request (content + metadata + owners + ACL) by
    /// `doc_id`: a BYTE-IDENTICAL re-ingest is a no-op REPLAY (skips re-chunking + the expensive
    /// re-embedding, returns `status: "replayed"`), while any CHANGED field — content OR metadata/ACL —
    /// re-ingests as usual and refreshes the fingerprint (so an ACL/metadata update is never silently
    /// dropped). When **false** (the default), every ingest re-embeds + replaces chunks (the
    /// pre-existing behavior). Fail-fast on an unrecognized value.
    pub ingest_idempotency_enabled: bool,
    /// Optional MCP/HTTP tool-connector endpoint (`MCP_ENDPOINT`).
    ///
    /// When **set**, the [`crate::tools::gateway::ToolGateway`] actually EXECUTES an
    /// auto-approved tool call by POSTing an MCP-style JSON-RPC `tools/call` to this
    /// endpoint and recording the real result. When **unset** (the default), execution
    /// is a placeholder (the pre-connector behavior — dev/CI, no network). Operator-only,
    /// never derived from a request (no SSRF surface).
    pub mcp_endpoint: Option<String>,
    /// Optional bearer credential for the outbound MCP connector (`MCP_AUTH_TOKEN`). Secret and redacted.
    pub mcp_auth_token: Option<String>,
    /// Optional file containing the outbound MCP bearer credential (`MCP_AUTH_TOKEN_FILE`).
    ///
    /// The connector reads this file immediately before each logical tool call, so an atomic file
    /// replacement rotates the credential without restarting Synapse. Mutually exclusive with
    /// [`Config::mcp_auth_token`]. In production the path must be absolute and the file must not be
    /// accessible by group or other users.
    pub mcp_auth_token_file: Option<PathBuf>,
    /// Operator-declared connector credential scopes (`MCP_SCOPES`, comma-separated). Registered tools may require a subset.
    pub mcp_scopes: Vec<String>,
    /// Exact outbound MCP host allowlist (`MCP_ALLOWED_HOSTS`, comma-separated). Production requires a matching host whenever MCP_ENDPOINT is set.
    pub mcp_allowed_hosts: Vec<String>,
    /// Per-call timeout for the MCP connector, in seconds (`MCP_TIMEOUT_SECS`). Default 30.
    pub mcp_timeout_secs: u64,
    /// Max retries for the MCP connector on CONNECT-PHASE errors only (exponential backoff;
    /// `MCP_MAX_RETRIES`, default 2). A tool call is a side effect, so a non-2xx or a
    /// post-connect transport fault is NOT retried — only a request that provably never
    /// reached the endpoint is re-sent, so a tool can't be double-executed (see [`crate::mcp`]).
    pub mcp_max_retries: u32,
    /// Opt-in background worker (`WORKER_ENABLED`, default off).
    ///
    /// When **true**, a background task periodically reconciles runs stuck in `running`: a
    /// driver that crashed mid tool-execution is failed safely (its external outcome is
    /// unknown), a run left un-driven is driven to completion, and a step whose retry backoff
    /// has elapsed is retried. Off by default — enable it once the deployment's migration role
    /// can own the SECURITY DEFINER discovery function (migration 0017).
    ///
    /// This ALSO gates step retry/backoff (retries need the worker to make progress): with the
    /// worker off, a retriable step failure is terminal. Treat the flag as immutable-once-set —
    /// disabling it after a retry was scheduled leaves that run `running` until it is re-enabled.
    pub worker_enabled: bool,
    /// Poll interval for the crash-recovery worker, in seconds (`WORKER_POLL_SECS`). Default 30.
    pub worker_poll_secs: u64,
    /// A `running` run is treated as ORPHANED once its `updated_at` is older than this many
    /// seconds (`WORKER_STALE_SECS`, default 300). MUST exceed the max tool-call duration
    /// (`mcp_timeout_secs` + connect timeout) so an actively-driven run is never reconciled
    /// out from under its in-request driver.
    pub worker_stale_secs: i64,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("production_mode", &self.production_mode)
            .field("database_url", &"<redacted>")
            .field("bind_addr", &self.bind_addr)
            .field("db_max_connections", &self.db_max_connections)
            .field("db_acquire_timeout_secs", &self.db_acquire_timeout_secs)
            .field("max_request_body_bytes", &self.max_request_body_bytes)
            .field("request_timeout_secs", &self.request_timeout_secs)
            .field("max_in_flight_requests", &self.max_in_flight_requests)
            .field("embedding_model", &self.embedding_model)
            .field("embedding_provider", &self.embedding_provider.as_str())
            .field(
                "embedding_api_key",
                &self.openai_api_key.as_ref().map(|_| "<redacted>"),
            )
            .field("embedding_base_url", &self.embedding_base_url)
            .field("embedding_max_batch", &self.embedding_max_batch)
            .field("embedding_timeout_secs", &self.embedding_timeout_secs)
            .field("embedding_max_retries", &self.embedding_max_retries)
            .field("otel_endpoint", &self.otel_endpoint)
            .field(
                "auth_jwt_secret",
                &self.auth_jwt_secret.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "auth_jwt_public_key",
                &self.auth_jwt_public_key.as_ref().map(|_| "<set>"),
            )
            .field("auth_jwt_audience", &self.auth_jwt_audience)
            .field("auth_jwt_issuer", &self.auth_jwt_issuer)
            .field("auth_jwks_url", &self.auth_jwks_url)
            .field("auth_jwks_timeout_secs", &self.auth_jwks_timeout_secs)
            .field(
                "auth_jwks_min_refetch_secs",
                &self.auth_jwks_min_refetch_secs,
            )
            .field("auth_revocation_enabled", &self.auth_revocation_enabled)
            .field("abac_context_ownership", &self.abac_context_ownership)
            .field(
                "embedding_model_consistency",
                &self.embedding_model_consistency,
            )
            .field("retrieval_mmr_lambda", &self.retrieval_mmr_lambda)
            .field("rate_limit_enabled", &self.rate_limit_enabled)
            .field("rate_limit_tenant_rps", &self.rate_limit_tenant_rps)
            .field("rate_limit_burst", &self.rate_limit_burst)
            .field(
                "ingest_idempotency_enabled",
                &self.ingest_idempotency_enabled,
            )
            .field("mcp_endpoint", &self.mcp_endpoint)
            .field(
                "mcp_auth_token",
                &self.mcp_auth_token.as_ref().map(|_| "<redacted>"),
            )
            .field("mcp_auth_token_file", &self.mcp_auth_token_file)
            .field("mcp_scopes", &self.mcp_scopes)
            .field("mcp_allowed_hosts", &self.mcp_allowed_hosts)
            .field("mcp_timeout_secs", &self.mcp_timeout_secs)
            .field("mcp_max_retries", &self.mcp_max_retries)
            .field("worker_enabled", &self.worker_enabled)
            .field("worker_poll_secs", &self.worker_poll_secs)
            .field("worker_stale_secs", &self.worker_stale_secs)
            .finish()
    }
}

impl Config {
    /// Load configuration from the process environment.
    ///
    /// `DATABASE_URL` is required; everything else has a sensible default.
    pub fn from_env() -> anyhow::Result<Config> {
        let production_mode = match cleaned_env("SYNAPSE_ENV")
            .unwrap_or_else(|| "development".to_string())
            .to_ascii_lowercase()
            .as_str()
        {
            "development" | "dev" | "test" => false,
            "production" | "prod" => true,
            other => {
                anyhow::bail!("SYNAPSE_ENV must be development, test, or production; got {other:?}")
            }
        };
        let database_url = std::env::var("DATABASE_URL").context(
            "DATABASE_URL must be set (e.g. postgres://user:pass@localhost:5432/synapse)",
        )?;
        let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
        bind_addr.parse::<std::net::SocketAddr>().with_context(|| {
            format!("BIND_ADDR must be an IP socket address (host:port); got {bind_addr:?}")
        })?;

        let db_max_connections = parse_env("DB_MAX_CONNECTIONS", 20_u32)?;
        let db_acquire_timeout_secs = parse_env("DB_ACQUIRE_TIMEOUT_SECS", 10_u64)?;
        let max_request_body_bytes = parse_env("MAX_REQUEST_BODY_BYTES", 16 * 1024 * 1024_usize)?;
        let request_timeout_secs = parse_env("REQUEST_TIMEOUT_SECS", 180_u64)?;
        let max_in_flight_requests = parse_env("MAX_IN_FLIGHT_REQUESTS", 256_usize)?;
        if db_max_connections == 0
            || db_acquire_timeout_secs == 0
            || max_request_body_bytes == 0
            || request_timeout_secs == 0
            || max_in_flight_requests == 0
        {
            anyhow::bail!(
                "DB_MAX_CONNECTIONS, DB_ACQUIRE_TIMEOUT_SECS, MAX_REQUEST_BODY_BYTES, \
                 REQUEST_TIMEOUT_SECS, and MAX_IN_FLIGHT_REQUESTS must all be greater than zero"
            );
        }

        let explicit_embedding_provider = cleaned_env("EMBEDDING_PROVIDER")
            .map(|raw| EmbeddingProvider::from_env_value(&raw))
            .transpose()?;
        let embedding_provider = explicit_embedding_provider.unwrap_or_else(|| {
            if cleaned_env("GEMINI_API_KEY").is_some() {
                EmbeddingProvider::Gemini
            } else if cleaned_env("OPENAI_API_KEY")
                .or_else(|| cleaned_env("EMBEDDING_API_KEY"))
                .is_some()
            {
                EmbeddingProvider::OpenAi
            } else {
                EmbeddingProvider::Mock
            }
        });

        let openai_api_key = match embedding_provider {
            EmbeddingProvider::Gemini => {
                cleaned_env("GEMINI_API_KEY").or_else(|| cleaned_env("EMBEDDING_API_KEY"))
            }
            EmbeddingProvider::OpenAi => {
                cleaned_env("OPENAI_API_KEY").or_else(|| cleaned_env("EMBEDDING_API_KEY"))
            }
            EmbeddingProvider::Mock => None,
        };
        if !matches!(embedding_provider, EmbeddingProvider::Mock) && openai_api_key.is_none() {
            let key = embedding_provider
                .api_key_env_var()
                .unwrap_or("EMBEDDING_API_KEY");
            anyhow::bail!(
                "EMBEDDING_PROVIDER={} requires {} or EMBEDDING_API_KEY",
                embedding_provider.as_str(),
                key
            );
        }

        let embedding_model = cleaned_env("EMBEDDING_MODEL")
            .or_else(|| match embedding_provider {
                EmbeddingProvider::Gemini => cleaned_env("GEMINI_EMBEDDING_MODEL"),
                EmbeddingProvider::OpenAi => cleaned_env("OPENAI_EMBEDDING_MODEL"),
                EmbeddingProvider::Mock => None,
            })
            .unwrap_or_else(|| embedding_provider.default_model().to_string());

        let embedding_base_url = cleaned_env("EMBEDDING_BASE_URL")
            .or_else(|| match embedding_provider {
                EmbeddingProvider::Gemini => cleaned_env("GEMINI_BASE_URL"),
                EmbeddingProvider::OpenAi => cleaned_env("OPENAI_BASE_URL"),
                EmbeddingProvider::Mock => None,
            })
            .unwrap_or_else(|| embedding_provider.default_base_url().to_string());
        let embedding_max_batch = parse_env("EMBEDDING_MAX_BATCH", 96)?;
        let embedding_timeout_secs = parse_env("EMBEDDING_TIMEOUT_SECS", 30)?;
        let embedding_max_retries = parse_env("EMBEDDING_MAX_RETRIES", 3)?;

        // Accept either the standard OTel var or the legacy Synapse-specific alias. This is a
        // generic BASE endpoint; telemetry appends the signal-specific HTTP paths.
        let otel_endpoint =
            cleaned_env("OTEL_EXPORTER_OTLP_ENDPOINT").or_else(|| cleaned_env("OTEL_ENDPOINT"));
        if let Some(endpoint) = otel_endpoint.as_deref() {
            validate_otel_endpoint(endpoint)?;
        }

        // Empty/whitespace-only secret is treated as unset (stay in header-trust
        // mode) — an operator can't accidentally enable auth with a blank secret.
        let auth_jwt_secret = std::env::var("AUTH_JWT_SECRET")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        // RS256 public key (PEM). Validate it parses as an RSA public key at LOAD so a bad key is a
        // startup error (fail-fast, like ABAC), never a per-request 401. Trim surrounding whitespace
        // first (consistent with AUTH_JWT_SECRET / OPENAI_API_KEY, and defensive — a stray trailing
        // newline can't matter), THEN un-escape escaped `\n` (base64 PEM bodies never contain a
        // backslash, so this is safe) so a single-line env value works too.
        let auth_jwt_public_key = match std::env::var("AUTH_JWT_PUBLIC_KEY") {
            Ok(raw) if !raw.trim().is_empty() => {
                let pem = raw.trim().replace("\\n", "\n");
                jsonwebtoken::DecodingKey::from_rsa_pem(pem.as_bytes()).map_err(|e| {
                    anyhow::anyhow!("AUTH_JWT_PUBLIC_KEY is not a valid RSA public key (PEM): {e}")
                })?;
                Some(pem)
            }
            _ => None,
        };
        let auth_jwt_audience = std::env::var("AUTH_JWT_AUDIENCE")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let auth_jwt_issuer = cleaned_env("AUTH_JWT_ISSUER");
        // RS256 JWKS endpoint (rotating public keys). Validate at LOAD that it's a well-formed
        // HTTPS URL — fail-fast like the static public key, so a non-HTTPS/malformed URL is a startup
        // error, never a per-request 401. HTTPS is required so the fetched keys can't be MITM'd (a
        // downgraded key set would let an attacker forge tokens). The keys are fetched LAZILY on first
        // use (a JWKS outage at boot doesn't block startup); only the URL shape is checked here.
        let auth_jwks_url = match std::env::var("AUTH_JWKS_URL") {
            Ok(raw) if !raw.trim().is_empty() => {
                let url = raw.trim().to_string();
                let parsed = reqwest::Url::parse(&url)
                    .map_err(|e| anyhow::anyhow!("AUTH_JWKS_URL is not a valid URL: {e}"))?;
                if parsed.scheme() != "https" {
                    return Err(anyhow::anyhow!(
                        "AUTH_JWKS_URL must be an https:// URL (got scheme {:?})",
                        parsed.scheme()
                    ));
                }
                Some(url)
            }
            _ => None,
        };
        let auth_jwks_timeout_secs = parse_env("AUTH_JWKS_TIMEOUT_SECS", 10)?;
        let auth_jwks_min_refetch_secs = parse_env("AUTH_JWKS_MIN_REFETCH_SECS", 60)?;
        // Opt-in token revocation — a SECURITY toggle, so parse it FAIL-FAST like ABAC (a typo'd
        // value must not silently leave revocation OFF while the operator believes it on).
        let auth_revocation_enabled = match std::env::var("AUTH_REVOCATION_ENABLED") {
            Err(_) => false,
            Ok(raw) => match raw.trim().to_ascii_lowercase().as_str() {
                "" => false,
                "1" | "true" | "yes" | "on" | "enable" | "enabled" => true,
                "0" | "false" | "no" | "off" | "disable" | "disabled" => false,
                other => {
                    return Err(anyhow::anyhow!(
                        "AUTH_REVOCATION_ENABLED must be a boolean \
                         (true/false/1/0/yes/no/on/off); got {other:?}"
                    ))
                }
            },
        };
        // A SECURITY toggle, so parse it FAIL-FAST: unset (or blank) is off (the
        // backward-compatible default), a recognized boolean is honored, and a
        // present-but-unrecognized value is a hard error — an operator who typos
        // `ABAC_CONTEXT_OWNERSHIP=enable` must NOT silently boot with the control
        // OFF while believing it on (the dangerous silent-disable).
        let abac_context_ownership = match std::env::var("ABAC_CONTEXT_OWNERSHIP") {
            Err(_) => false,
            Ok(raw) => match raw.trim().to_ascii_lowercase().as_str() {
                "" => false,
                "1" | "true" | "yes" | "on" | "enable" | "enabled" => true,
                "0" | "false" | "no" | "off" | "disable" | "disabled" => false,
                other => {
                    return Err(anyhow::anyhow!(
                        "ABAC_CONTEXT_OWNERSHIP must be a boolean \
                         (true/false/1/0/yes/no/on/off); got {other:?}"
                    ))
                }
            },
        };

        // Opt-in retrieval correctness toggle — parse FAIL-FAST like ABAC (a typo'd value must
        // not silently disable a correctness control).
        let embedding_model_consistency = match std::env::var("EMBEDDING_MODEL_CONSISTENCY") {
            Err(_) => false,
            Ok(raw) => match raw.trim().to_ascii_lowercase().as_str() {
                "" => false,
                "1" | "true" | "yes" | "on" | "enable" | "enabled" => true,
                "0" | "false" | "no" | "off" | "disable" | "disabled" => false,
                other => {
                    return Err(anyhow::anyhow!(
                        "EMBEDDING_MODEL_CONSISTENCY must be a boolean \
                         (true/false/1/0/yes/no/on/off); got {other:?}"
                    ))
                }
            },
        };

        // Default MMR lambda. Invalid or out-of-range values fail startup.
        let retrieval_mmr_lambda = parse_env("RETRIEVAL_MMR_LAMBDA", 0.5_f64)?;
        if !(0.0..=1.0).contains(&retrieval_mmr_lambda) {
            return Err(anyhow::anyhow!(
                "RETRIEVAL_MMR_LAMBDA must be in [0, 1]; got {retrieval_mmr_lambda}"
            ));
        }

        // Opt-in per-tenant rate limiting — a SECURITY/availability toggle, parsed FAIL-FAST like ABAC.
        let rate_limit_enabled = match std::env::var("RATE_LIMIT_ENABLED") {
            Err(_) => false,
            Ok(raw) => match raw.trim().to_ascii_lowercase().as_str() {
                "" => false,
                "1" | "true" | "yes" | "on" | "enable" | "enabled" => true,
                "0" | "false" | "no" | "off" | "disable" | "disabled" => false,
                other => {
                    return Err(anyhow::anyhow!(
                        "RATE_LIMIT_ENABLED must be a boolean \
                         (true/false/1/0/yes/no/on/off); got {other:?}"
                    ))
                }
            },
        };
        let rate_limit_tenant_rps = parse_env("RATE_LIMIT_TENANT_RPS", 10.0_f64)?;
        let rate_limit_burst = parse_env("RATE_LIMIT_BURST", 20.0_f64)?;
        // Only meaningful when enabled; validate then so a nonsensical rate/burst can never admit
        // every request (rate <= 0 would never refill) or reject every request (burst < 1 could never
        // hold a whole token). A finite check also rejects NaN/inf.
        if rate_limit_enabled {
            if !(rate_limit_tenant_rps.is_finite() && rate_limit_tenant_rps > 0.0) {
                return Err(anyhow::anyhow!(
                    "RATE_LIMIT_TENANT_RPS must be a finite value > 0 when RATE_LIMIT_ENABLED; got {rate_limit_tenant_rps}"
                ));
            }
            if !(rate_limit_burst.is_finite() && rate_limit_burst >= 1.0) {
                return Err(anyhow::anyhow!(
                    "RATE_LIMIT_BURST must be a finite value >= 1 when RATE_LIMIT_ENABLED; got {rate_limit_burst}"
                ));
            }
        }

        // Opt-in idempotent document ingest — parse FAIL-FAST like the other toggles.
        let ingest_idempotency_enabled = match std::env::var("INGEST_IDEMPOTENCY_ENABLED") {
            Err(_) => false,
            Ok(raw) => match raw.trim().to_ascii_lowercase().as_str() {
                "" => false,
                "1" | "true" | "yes" | "on" | "enable" | "enabled" => true,
                "0" | "false" | "no" | "off" | "disable" | "disabled" => false,
                other => {
                    return Err(anyhow::anyhow!(
                        "INGEST_IDEMPOTENCY_ENABLED must be a boolean \
                         (true/false/1/0/yes/no/on/off); got {other:?}"
                    ))
                }
            },
        };

        // Empty/whitespace-only endpoint is treated as unset (stay on the placeholder).
        let mcp_endpoint = std::env::var("MCP_ENDPOINT")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let mcp_auth_token = cleaned_env("MCP_AUTH_TOKEN");
        let mcp_auth_token_file = cleaned_env("MCP_AUTH_TOKEN_FILE").map(PathBuf::from);
        validate_mcp_auth_sources(
            production_mode,
            mcp_endpoint.is_some(),
            mcp_auth_token.as_deref(),
            mcp_auth_token_file.as_deref(),
        )?;
        if let Some(token) = mcp_auth_token.as_deref() {
            validate_mcp_auth_token(token)?;
        }
        if let Some(path) = mcp_auth_token_file.as_deref() {
            validate_mcp_auth_token_file(path, production_mode)?;
        }
        let mcp_scopes = split_env_list("MCP_SCOPES");
        let mcp_allowed_hosts = split_env_list("MCP_ALLOWED_HOSTS")
            .into_iter()
            .map(|host| host.to_ascii_lowercase())
            .collect::<Vec<_>>();
        if let Some(endpoint) = mcp_endpoint.as_deref() {
            let parsed = reqwest::Url::parse(endpoint)
                .map_err(|e| anyhow::anyhow!("MCP_ENDPOINT is not a valid URL: {e}"))?;
            let host = parsed
                .host_str()
                .ok_or_else(|| anyhow::anyhow!("MCP_ENDPOINT must include a host"))?;
            if production_mode && parsed.scheme() != "https" {
                anyhow::bail!("SYNAPSE_ENV=production requires an https:// MCP_ENDPOINT");
            }
            if production_mode && mcp_allowed_hosts.is_empty() {
                anyhow::bail!(
                    "SYNAPSE_ENV=production requires MCP_ALLOWED_HOSTS when MCP_ENDPOINT is set"
                );
            }
            if !mcp_allowed_hosts.is_empty()
                && !mcp_allowed_hosts
                    .iter()
                    .any(|allowed| allowed == &host.to_ascii_lowercase())
            {
                anyhow::bail!("MCP_ENDPOINT host is not present in MCP_ALLOWED_HOSTS");
            }
        }
        let mcp_timeout_secs = parse_env("MCP_TIMEOUT_SECS", 30)?;
        let mcp_max_retries = parse_env("MCP_MAX_RETRIES", 2)?;

        // A security-adjacent toggle (it spawns a cross-tenant background task), so parse it
        // FAIL-FAST like ABAC: unset/blank is off, a recognized boolean is honored, and a
        // present-but-unrecognized value is a hard error.
        let worker_enabled = match std::env::var("WORKER_ENABLED") {
            Err(_) => false,
            Ok(raw) => match raw.trim().to_ascii_lowercase().as_str() {
                "" => false,
                "1" | "true" | "yes" | "on" | "enable" | "enabled" => true,
                "0" | "false" | "no" | "off" | "disable" | "disabled" => false,
                other => {
                    return Err(anyhow::anyhow!(
                        "WORKER_ENABLED must be a boolean (true/false/1/0/yes/no/on/off); got {other:?}"
                    ))
                }
            },
        };
        let worker_poll_secs = parse_env("WORKER_POLL_SECS", 30)?;
        let worker_stale_secs = parse_env("WORKER_STALE_SECS", 300)?;
        // Safety-critical: an out-of-range staleness window lets the crash-recovery worker fail
        // a LIVE run mid tool-execution (its `runs` row is unlocked during the out-of-tx
        // connector call, so the lease's `SKIP LOCKED` can't protect it). Enforce fail-fast.
        validate_worker_stale_secs(
            worker_stale_secs,
            worker_enabled,
            mcp_timeout_secs,
            mcp_max_retries,
        )?;

        validate_production_posture(
            production_mode,
            ProductionPosture {
                verified_jwt: auth_jwks_url.is_some()
                    || auth_jwt_public_key.is_some()
                    || auth_jwt_secret.is_some(),
                jwt_audience: auth_jwt_audience.is_some(),
                jwt_issuer: auth_jwt_issuer.is_some(),
                real_embeddings: !matches!(embedding_provider, EmbeddingProvider::Mock),
                embedding_model_consistency,
                rate_limit_enabled,
                ingest_idempotency_enabled,
                worker_enabled,
            },
        )?;

        Ok(Config {
            production_mode,
            database_url,
            bind_addr,
            db_max_connections,
            db_acquire_timeout_secs,
            max_request_body_bytes,
            request_timeout_secs,
            max_in_flight_requests,
            embedding_model,
            embedding_provider,
            openai_api_key,
            embedding_base_url,
            embedding_max_batch,
            embedding_timeout_secs,
            embedding_max_retries,
            otel_endpoint,
            auth_jwt_secret,
            auth_jwt_public_key,
            auth_jwt_audience,
            auth_jwt_issuer,
            auth_jwks_url,
            auth_jwks_timeout_secs,
            auth_jwks_min_refetch_secs,
            auth_revocation_enabled,
            abac_context_ownership,
            embedding_model_consistency,
            retrieval_mmr_lambda,
            rate_limit_enabled,
            rate_limit_tenant_rps,
            rate_limit_burst,
            ingest_idempotency_enabled,
            mcp_endpoint,
            mcp_auth_token,
            mcp_auth_token_file,
            mcp_scopes,
            mcp_allowed_hosts,
            mcp_timeout_secs,
            mcp_max_retries,
            worker_enabled,
            worker_poll_secs,
            worker_stale_secs,
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct ProductionPosture {
    verified_jwt: bool,
    jwt_audience: bool,
    jwt_issuer: bool,
    real_embeddings: bool,
    embedding_model_consistency: bool,
    rate_limit_enabled: bool,
    ingest_idempotency_enabled: bool,
    worker_enabled: bool,
}

fn validate_production_posture(
    production_mode: bool,
    posture: ProductionPosture,
) -> anyhow::Result<()> {
    if !production_mode {
        return Ok(());
    }
    if !posture.verified_jwt {
        anyhow::bail!(
            "SYNAPSE_ENV=production requires verified JWT auth: set AUTH_JWKS_URL, \
             AUTH_JWT_PUBLIC_KEY, or AUTH_JWT_SECRET"
        );
    }
    if !posture.jwt_audience {
        anyhow::bail!(
            "SYNAPSE_ENV=production requires AUTH_JWT_AUDIENCE to prevent cross-service tokens"
        );
    }
    if !posture.jwt_issuer {
        anyhow::bail!(
            "SYNAPSE_ENV=production requires AUTH_JWT_ISSUER to prevent cross-issuer tokens"
        );
    }
    if !posture.real_embeddings {
        anyhow::bail!(
            "SYNAPSE_ENV=production refuses EMBEDDING_PROVIDER=mock; configure Gemini or OpenAI"
        );
    }
    if !posture.embedding_model_consistency {
        anyhow::bail!("SYNAPSE_ENV=production requires EMBEDDING_MODEL_CONSISTENCY=true");
    }
    if !posture.rate_limit_enabled {
        anyhow::bail!("SYNAPSE_ENV=production requires RATE_LIMIT_ENABLED=true");
    }
    if !posture.ingest_idempotency_enabled {
        anyhow::bail!("SYNAPSE_ENV=production requires INGEST_IDEMPOTENCY_ENABLED=true");
    }
    if !posture.worker_enabled {
        anyhow::bail!("SYNAPSE_ENV=production requires WORKER_ENABLED=true for durable retries");
    }
    Ok(())
}

/// Read and trim an env var, treating missing/blank as unset.
fn cleaned_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn split_env_list(name: &str) -> Vec<String> {
    let mut values = cleaned_env(name)
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    values.sort();
    values.dedup();
    values
}

/// Validate the generic OTLP endpoint. Credentials are deliberately forbidden in the URL because
/// `Config` logs this low-risk value; use `OTEL_EXPORTER_OTLP_HEADERS` for collector authentication.
fn validate_otel_endpoint(raw: &str) -> anyhow::Result<()> {
    let parsed = reqwest::Url::parse(raw)
        .with_context(|| format!("OTEL_EXPORTER_OTLP_ENDPOINT is not a valid URL: {raw:?}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        anyhow::bail!("OTEL_EXPORTER_OTLP_ENDPOINT must use http:// or https://");
    }
    if parsed.host_str().is_none() {
        anyhow::bail!("OTEL_EXPORTER_OTLP_ENDPOINT must include a host");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        anyhow::bail!(
            "OTEL_EXPORTER_OTLP_ENDPOINT must not contain credentials; use OTEL_EXPORTER_OTLP_HEADERS"
        );
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        anyhow::bail!("OTEL_EXPORTER_OTLP_ENDPOINT must not contain a query string or fragment");
    }
    Ok(())
}

/// Enforce an unambiguous connector credential source and production authentication.
fn validate_mcp_auth_sources(
    production: bool,
    endpoint_configured: bool,
    static_token: Option<&str>,
    token_file: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    if static_token.is_some() && token_file.is_some() {
        anyhow::bail!("set only one of MCP_AUTH_TOKEN or MCP_AUTH_TOKEN_FILE");
    }
    if !endpoint_configured && (static_token.is_some() || token_file.is_some()) {
        anyhow::bail!("MCP_AUTH_TOKEN and MCP_AUTH_TOKEN_FILE require MCP_ENDPOINT");
    }
    if production && endpoint_configured && static_token.is_none() && token_file.is_none() {
        anyhow::bail!(
            "SYNAPSE_ENV=production requires MCP_AUTH_TOKEN or MCP_AUTH_TOKEN_FILE when MCP_ENDPOINT is set"
        );
    }
    Ok(())
}

/// Validate a file-backed connector credential without retaining or returning its contents.
///
/// Runtime dispatch reads the file again for every logical call so an atomic replacement takes
/// effect without a restart. This startup check catches a missing, empty, oversized, malformed, or
/// overly permissive credential before Synapse accepts traffic.
fn validate_mcp_auth_token_file(path: &std::path::Path, production: bool) -> anyhow::Result<()> {
    if production && !path.is_absolute() {
        anyhow::bail!("MCP_AUTH_TOKEN_FILE must be an absolute path in production");
    }

    let file = std::fs::File::open(path)
        .with_context(|| format!("could not open MCP_AUTH_TOKEN_FILE at {}", path.display()))?;
    let metadata = file
        .metadata()
        .context("could not inspect MCP_AUTH_TOKEN_FILE")?;
    validate_mcp_auth_token_metadata(&metadata, production)?;

    use std::io::Read as _;
    let mut token = String::new();
    file.take(MCP_AUTH_TOKEN_MAX_BYTES + 1)
        .read_to_string(&mut token)
        .context("could not read MCP_AUTH_TOKEN_FILE as UTF-8 text")?;
    validate_mcp_auth_token(&token)
}

/// Validate metadata from the same open file handle that supplies the credential, avoiding a
/// check/read race during atomic rotation.
fn validate_mcp_auth_token_metadata(
    metadata: &std::fs::Metadata,
    production: bool,
) -> anyhow::Result<()> {
    if !metadata.is_file() {
        anyhow::bail!("MCP_AUTH_TOKEN_FILE must reference a regular file");
    }
    if metadata.len() > MCP_AUTH_TOKEN_MAX_BYTES {
        anyhow::bail!(
            "MCP_AUTH_TOKEN_FILE exceeds the {MCP_AUTH_TOKEN_MAX_BYTES}-byte credential limit"
        );
    }

    #[cfg(unix)]
    if production {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o077 != 0 {
            anyhow::bail!(
                "MCP_AUTH_TOKEN_FILE must not be accessible by group or other users in production"
            );
        }
    }
    #[cfg(not(unix))]
    let _ = production;

    Ok(())
}

/// Validate a bearer token without including it in any error.
fn validate_mcp_auth_token(raw: &str) -> anyhow::Result<()> {
    let token = raw.trim();
    if token.is_empty() {
        anyhow::bail!("MCP connector credential is empty");
    }
    if token.len() > MCP_AUTH_TOKEN_MAX_BYTES as usize {
        anyhow::bail!("MCP connector credential exceeds the {MCP_AUTH_TOKEN_MAX_BYTES}-byte limit");
    }
    reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
        .context("MCP connector credential is not valid in an HTTP Authorization header")?;
    Ok(())
}

/// Generous upper bound that prevents accidentally reading an unrelated large file on every call.
pub(crate) const MCP_AUTH_TOKEN_MAX_BYTES: u64 = 16 * 1024;

/// Parse an env var into `T`, using `default` only when missing or blank.
/// A present invalid value is an operator error and fails startup.
fn parse_env<T>(name: &str, default: T) -> anyhow::Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match std::env::var(name) {
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(e) => Err(anyhow::anyhow!("could not read {name}: {e}")),
        Ok(raw) if raw.trim().is_empty() => Ok(default),
        Ok(raw) => raw
            .trim()
            .parse()
            .map_err(|e| anyhow::anyhow!("{name} has invalid value {raw:?}: {e}")),
    }
}

/// Enforce the crash-recovery worker's staleness invariant fail-fast.
///
/// A run is treated as an orphan once its `updated_at` is `stale_secs` old; if that window does
/// not strictly exceed the longest possible tool call, the worker can lease and FAIL a run whose
/// driver is merely mid out-of-tx connector call (the `runs` row is unlocked during that network
/// I/O, so the lease's `FOR UPDATE SKIP LOCKED` does not skip it), fabricating a failure for a
/// live, in-flight, non-idempotent tool. So a non-positive value is always rejected, and — when
/// the worker is enabled — the window must exceed the worst-case tool-call wall time
/// (`mcp_timeout_secs × (mcp_max_retries + 1)`; each attempt can burn the full request timeout and
/// a connect-phase fault is retried). The duration cross-check is gated on `worker_enabled` because
/// it only matters when the worker actually runs (and the MCP settings are only then in play).
fn validate_worker_stale_secs(
    stale_secs: i64,
    worker_enabled: bool,
    mcp_timeout_secs: u64,
    mcp_max_retries: u32,
) -> anyhow::Result<()> {
    if stale_secs <= 0 {
        anyhow::bail!("WORKER_STALE_SECS must be a positive number of seconds; got {stale_secs}");
    }
    if worker_enabled {
        // The TRUE worst-case connector wall time — request timeouts AND the connect-phase retry
        // backoff sleeps (sourced from the mcp module so the connector's own constants are the
        // single source of truth; the earlier formula omitted the backoff sum and under-bounded
        // it). Plus a margin: the reconciler's staleness anchor (a run's `updated_at` / a tool
        // intent's `created_at`) is stamped BEFORE dispatch, so leave slack for the pre-call DB
        // writes. Too small a window lets the crash-recovery worker fail a LIVE run/tool-call.
        let max_tool_secs = crate::mcp::worst_case_call_secs(mcp_timeout_secs, mcp_max_retries);
        let min_stale = max_tool_secs.saturating_add(WORKER_STALE_MARGIN_SECS);
        if (stale_secs as u64) <= min_stale {
            anyhow::bail!(
                "WORKER_STALE_SECS ({stale_secs}) must exceed the worst-case tool-call duration + \
                 margin (~{min_stale}s: {max_tool_secs}s of request timeouts + retry backoff for \
                 MCP_TIMEOUT_SECS {mcp_timeout_secs} / MCP_MAX_RETRIES {mcp_max_retries}, plus \
                 {WORKER_STALE_MARGIN_SECS}s slack); a too-small window lets the crash-recovery \
                 worker fail a live run/tool-call mid execution"
            );
        }
    }
    Ok(())
}

/// Slack added above the worst-case tool-call duration when validating `WORKER_STALE_SECS`: the
/// reconciler anchors staleness on a timestamp stamped just BEFORE dispatch (a run's `updated_at`
/// / a tool intent's `created_at`), so leave room for the pre-call DB writes.
const WORKER_STALE_MARGIN_SECS: u64 = 10;

#[cfg(test)]
mod tests {
    use super::{
        validate_mcp_auth_sources, validate_mcp_auth_token, validate_otel_endpoint,
        validate_production_posture, validate_worker_stale_secs, ProductionPosture,
    };

    fn hardened_posture() -> ProductionPosture {
        ProductionPosture {
            verified_jwt: true,
            jwt_audience: true,
            jwt_issuer: true,
            real_embeddings: true,
            embedding_model_consistency: true,
            rate_limit_enabled: true,
            ingest_idempotency_enabled: true,
            worker_enabled: true,
        }
    }

    #[test]
    fn production_requires_hardening_controls() {
        assert!(validate_production_posture(true, hardened_posture()).is_ok());

        let mut posture = hardened_posture();
        posture.verified_jwt = false;
        assert!(validate_production_posture(true, posture)
            .unwrap_err()
            .to_string()
            .contains("verified JWT"));

        let mut posture = hardened_posture();
        posture.real_embeddings = false;
        assert!(validate_production_posture(true, posture)
            .unwrap_err()
            .to_string()
            .contains("EMBEDDING_PROVIDER=mock"));

        let mut posture = hardened_posture();
        posture.worker_enabled = false;
        assert!(validate_production_posture(true, posture)
            .unwrap_err()
            .to_string()
            .contains("WORKER_ENABLED=true"));
    }

    #[test]
    fn development_does_not_require_production_posture() {
        let disabled = ProductionPosture {
            verified_jwt: false,
            jwt_audience: false,
            jwt_issuer: false,
            real_embeddings: false,
            embedding_model_consistency: false,
            rate_limit_enabled: false,
            ingest_idempotency_enabled: false,
            worker_enabled: false,
        };
        assert!(validate_production_posture(false, disabled).is_ok());
    }

    #[test]
    fn otel_endpoint_rejects_embedded_credentials_and_non_http_urls() {
        assert!(validate_otel_endpoint("http://127.0.0.1:4318").is_ok());
        assert!(validate_otel_endpoint("https://otel.example.com/prefix").is_ok());
        assert!(validate_otel_endpoint("grpc://otel.example.com").is_err());
        assert!(validate_otel_endpoint("https://user:secret@otel.example.com").is_err());
        assert!(validate_otel_endpoint("https://otel.example.com?token=secret").is_err());
    }

    #[test]
    fn connector_auth_sources_are_unambiguous_and_fail_closed_in_production() {
        let file = std::path::Path::new("/run/secrets/mcp-token");
        assert!(validate_mcp_auth_sources(false, false, None, None).is_ok());
        assert!(validate_mcp_auth_sources(false, true, None, None).is_ok());
        assert!(validate_mcp_auth_sources(true, true, Some("token"), None).is_ok());
        assert!(validate_mcp_auth_sources(true, true, None, Some(file)).is_ok());
        assert!(validate_mcp_auth_sources(true, true, None, None).is_err());
        assert!(validate_mcp_auth_sources(false, true, Some("token"), Some(file)).is_err());
        assert!(validate_mcp_auth_sources(false, false, Some("token"), None).is_err());
    }

    #[test]
    fn connector_token_validation_rejects_empty_and_invalid_headers() {
        assert!(validate_mcp_auth_token("secret-token").is_ok());
        assert!(validate_mcp_auth_token("  ").is_err());
        assert!(validate_mcp_auth_token("token\nheader-injection").is_err());
    }

    #[test]
    fn rejects_non_positive_stale_secs_regardless_of_enablement() {
        // Zero/negative are never sane, whether or not the worker runs.
        for enabled in [false, true] {
            assert!(validate_worker_stale_secs(0, enabled, 30, 2).is_err());
            assert!(validate_worker_stale_secs(-1, enabled, 30, 2).is_err());
        }
    }

    #[test]
    fn enabled_rejects_window_not_exceeding_worst_case_tool_duration() {
        // Worst case = 30×(2+1) request timeouts + ceil(0.2+0.4=0.6)=1s backoff = 91s, + a 10s
        // margin = 101s; a window <= 101 is unsafe when the worker runs.
        assert!(validate_worker_stale_secs(101, true, 30, 2).is_err());
        assert!(validate_worker_stale_secs(50, true, 30, 2).is_err());
        assert!(validate_worker_stale_secs(102, true, 30, 2).is_ok());
    }

    #[test]
    fn worst_case_includes_retry_backoff_sleeps() {
        // Regression: the earlier formula counted only request timeouts (1×(10+1)=11s), so
        // WORKER_STALE_SECS=20 would have (wrongly) passed. The true worst case adds ~31s of
        // capped connect-phase backoff, so 20 must now be REJECTED; a comfortably larger window
        // is accepted.
        assert!(validate_worker_stale_secs(20, true, 1, 10).is_err());
        assert!(validate_worker_stale_secs(120, true, 1, 10).is_ok());
    }

    #[test]
    fn disabled_allows_small_positive_window() {
        // When the worker is off, the tool-duration floor is moot — only positivity is enforced.
        assert!(validate_worker_stale_secs(1, false, 30, 2).is_ok());
        assert!(validate_worker_stale_secs(10, false, 300, 5).is_ok());
    }

    #[test]
    fn default_window_is_valid_under_default_mcp_settings() {
        // The shipped defaults (300s window; 30s timeout, 2 retries → 90s worst case) are safe.
        assert!(validate_worker_stale_secs(300, true, 30, 2).is_ok());
    }
}
