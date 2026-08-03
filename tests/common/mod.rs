//! Shared harness for DATABASE_URL-gated integration tests.
//!
//! Each test provisions its **own throwaway database** (+ its own cluster-global
//! app role) so the gated suite is fully parallel-safe: `pg_extension` is a
//! database-global catalog and every test's `apply_schema` drops + recreates the
//! extensions, so a shared database would race on it. See `retrieve_it.rs` for
//! the original rationale.
#![allow(dead_code)] // each integration-test binary uses a different subset.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tower::ServiceExt;

use synapse::config::Config;
use synapse::state::AppState;

/// A throwaway per-test database + app role.
pub struct TestDb {
    /// URL of the freshly-created per-test database.
    pub url: String,
    /// Non-superuser app role the pool `SET ROLE`s to, so RLS is enforced.
    pub role: String,
}

impl TestDb {
    /// Create `synapse_it_<slug>` (dropping any leftover from a prior run) on the
    /// server named by `base_url`, returning its URL and a per-test app role name.
    pub async fn create(base_url: &str, slug: &str) -> TestDb {
        let db_name = format!("synapse_it_{slug}");
        let role = format!("synapse_app_{slug}");

        // CREATE/DROP DATABASE can't run inside a transaction and must be issued
        // from a different (maintenance) database — use the one in `base_url`.
        let maint = PgPoolOptions::new()
            .max_connections(1)
            .connect(base_url)
            .await
            .expect("connect maintenance db");
        sqlx::query(&format!("DROP DATABASE IF EXISTS {db_name} WITH (FORCE)"))
            .execute(&maint)
            .await
            .expect("drop leftover test db");
        sqlx::query(&format!("CREATE DATABASE {db_name}"))
            .execute(&maint)
            .await
            .expect("create test db");
        maint.close().await;

        TestDb {
            url: swap_db(base_url, &db_name),
            role,
        }
    }
}

/// Replace the database name (last path segment) of a Postgres URL, preserving
/// any `?query` suffix.
pub fn swap_db(base_url: &str, db_name: &str) -> String {
    let (head, query) = match base_url.split_once('?') {
        Some((h, q)) => (h, Some(q)),
        None => (base_url, None),
    };
    let head = match head.rfind('/') {
        Some(i) => format!("{}/{db_name}", &head[..i]),
        None => format!("{head}/{db_name}"),
    };
    match query {
        Some(q) => format!("{head}?{q}"),
        None => head,
    }
}

/// Reset the schema, apply every migration in order, create the given app
/// `role`, and seed two tenants. Runs as the superuser `admin` pool.
pub async fn apply_schema(admin: &PgPool, role: &str) {
    sqlx::raw_sql("DROP SCHEMA IF EXISTS public CASCADE; CREATE SCHEMA public;")
        .execute(admin)
        .await
        .expect("reset public schema");

    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/migrations");
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .expect("read migrations dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "sql").unwrap_or(false))
        .collect();
    files.sort();
    for f in &files {
        let sql = std::fs::read_to_string(f).expect("read migration");
        sqlx::raw_sql(&sql)
            .execute(admin)
            .await
            .unwrap_or_else(|e| panic!("migration {} failed: {e}", f.display()));
    }

    sqlx::raw_sql(&format!(
        "DROP ROLE IF EXISTS {role};
         CREATE ROLE {role} NOLOGIN;
         GRANT {role} TO CURRENT_USER;
         GRANT USAGE ON SCHEMA public TO {role};
         GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO {role};
         GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO {role};
         GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA public TO {role};"
    ))
    .execute(admin)
    .await
    .expect("create app role");

    sqlx::raw_sql(
        "INSERT INTO tenants (tenant_id, name)
         VALUES ('tenant_a','Tenant A'), ('tenant_b','Tenant B')
         ON CONFLICT DO NOTHING;",
    )
    .execute(admin)
    .await
    .expect("seed tenants");
}

/// Seed an enabled tool contract for connector-focused integration tests.
pub async fn seed_tool_definition(admin: &PgPool, tenant: &str, tool_id: &str) {
    sqlx::query(
        r#"INSERT INTO tool_definitions
             (tenant_id, tool_id, input_schema, approval_mode, enabled)
         VALUES ($1, $2, '{"type":"object"}'::jsonb, 'none', true)"#,
    )
    .bind(tenant)
    .bind(tool_id)
    .execute(admin)
    .await
    .expect("seed tool definition");
}

/// Build the app's pool. Every connection `SET ROLE`s to the non-superuser app
/// role so the RLS policies are actually enforced.
pub async fn app_pool(database_url: &str, role: &str) -> PgPool {
    let role = role.to_string();
    PgPoolOptions::new()
        .max_connections(4)
        .after_connect(move |conn, _meta| {
            let role = role.clone();
            Box::pin(async move {
                sqlx::query(&format!("SET ROLE {role}"))
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .connect(database_url)
        .await
        .expect("connect app pool")
}

/// Build a baseline non-production configuration for integration tests.
pub fn config_for(database_url: &str) -> Config {
    Config {
        production_mode: false,
        database_url: database_url.to_string(),
        bind_addr: "0.0.0.0:8080".to_string(),
        db_max_connections: 20,
        db_acquire_timeout_secs: 10,
        max_request_body_bytes: 16 * 1024 * 1024,
        request_timeout_secs: 180,
        max_in_flight_requests: 256,
        embedding_model: "text-embedding-3-small".to_string(),
        embedding_provider: synapse::config::EmbeddingProvider::Mock,
        openai_api_key: None,
        embedding_base_url: "https://api.openai.com/v1".to_string(),
        embedding_max_batch: 96,
        embedding_timeout_secs: 30,
        embedding_max_retries: 3,
        otel_endpoint: None,
        auth_jwt_secret: None,
        auth_jwt_public_key: None,
        auth_jwt_audience: None,
        auth_jwt_issuer: None,
        auth_jwks_url: None,
        auth_jwks_timeout_secs: 10,
        auth_jwks_min_refetch_secs: 60,
        auth_revocation_enabled: false,
        abac_context_ownership: false,
        embedding_model_consistency: false,
        retrieval_mmr_lambda: 0.5,
        rate_limit_enabled: false,
        rate_limit_tenant_rps: 10.0,
        rate_limit_burst: 20.0,
        ingest_idempotency_enabled: false,
        mcp_endpoint: None,
        mcp_auth_token: None,
        mcp_auth_token_file: None,
        mcp_scopes: Vec::new(),
        mcp_allowed_hosts: Vec::new(),
        mcp_timeout_secs: 30,
        mcp_max_retries: 2,
        worker_enabled: false,
        worker_poll_secs: 30,
        worker_stale_secs: 300,
    }
}

/// Build the synapse router over `pool`.
pub fn router_for(pool: PgPool, database_url: &str) -> Router {
    synapse::app(AppState::new(pool, config_for(database_url)))
}

/// POST `body` to `uri` as (`tenant`, `principal`) with an explicit `X-Role`;
/// returns (status, json). `role: None` omits the header (the Member baseline).
pub async fn post_json_with_role(
    router: &Router,
    uri: &str,
    tenant: &str,
    principal: &str,
    role: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-principal-id", principal)
        .header("x-tenant-id", tenant);
    if let Some(role) = role {
        builder = builder.header("x-role", role);
    }
    let resp = router
        .clone()
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let val: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, val)
}

/// POST `body` to `uri` as (`tenant`, `principal`); returns (status, json).
pub async fn post_json(
    router: &Router,
    uri: &str,
    tenant: &str,
    principal: &str,
    body: Value,
) -> (StatusCode, Value) {
    post_json_with_role(router, uri, tenant, principal, None, body).await
}

/// GET `uri` as (`tenant`, `principal`) with an explicit `X-Role`; returns
/// (status, json). `role: None` omits the header (the Member baseline).
pub async fn get_json_with_role(
    router: &Router,
    uri: &str,
    tenant: &str,
    principal: &str,
    role: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("GET")
        .uri(uri)
        .header("x-principal-id", principal)
        .header("x-tenant-id", tenant);
    if let Some(role) = role {
        builder = builder.header("x-role", role);
    }
    let resp = router
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let val: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, val)
}

/// GET `uri` as (`tenant`, `principal`); returns (status, json).
pub async fn get_json(
    router: &Router,
    uri: &str,
    tenant: &str,
    principal: &str,
) -> (StatusCode, Value) {
    get_json_with_role(router, uri, tenant, principal, None).await
}

/// Provision a throwaway DB (+ role), apply all migrations, and return the app
/// router running as the non-superuser role (so RLS is enforced).
pub async fn setup(base_url: &str, slug: &str) -> Router {
    let test_db = TestDb::create(base_url, slug).await;
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&test_db.url)
        .await
        .expect("connect admin pool");
    apply_schema(&admin, &test_db.role).await;
    let pool = app_pool(&test_db.url, &test_db.role).await;
    router_for(pool, &test_db.url)
}

/// Like [`setup`] but also returns the superuser admin pool, so a test can seed rows no
/// API endpoint sets — e.g. provision a principal with a DB-authoritative role (the only
/// way to grant the elevated `admin` role, which is not header-assertable).
pub async fn setup_with_db(base_url: &str, slug: &str) -> (Router, PgPool) {
    let test_db = TestDb::create(base_url, slug).await;
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&test_db.url)
        .await
        .expect("connect admin pool");
    apply_schema(&admin, &test_db.role).await;
    let pool = app_pool(&test_db.url, &test_db.role).await;
    (router_for(pool, &test_db.url), admin)
}

/// Provision (or update) a principal's DB-authoritative role (`principals.role`) via the
/// admin pool — the only source of the elevated `admin` role.
pub async fn provision_role(admin: &PgPool, tenant: &str, principal_id: &str, role: &str) {
    sqlx::query(
        "INSERT INTO principals (tenant_id, principal_id, role) VALUES ($1, $2, $3) \
         ON CONFLICT (tenant_id, principal_id) DO UPDATE SET role = EXCLUDED.role",
    )
    .bind(tenant)
    .bind(principal_id)
    .bind(role)
    .execute(admin)
    .await
    .expect("provision principal role");
}
