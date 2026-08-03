//! DB-free smoke tests for the HTTP surface.
//!
//! These build the real router against a **lazily-connected** pool (from a
//! dummy `DATABASE_URL`) so they exercise routing, extractors, and handler
//! wiring without needing a live Postgres — they pass in CI without a database.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt; // for `oneshot`

use synapse::config::Config;
use synapse::state::AppState;
use synapse::{app, db};

fn test_state() -> AppState {
    let config = Config {
        production_mode: false,
        database_url: "postgres://synapse:synapse@localhost:5432/synapse".to_string(),
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
    };
    let pool = db::init(&config).expect("build lazy pool");
    AppState::new(pool, config)
}

#[tokio::test]
async fn health_returns_200() {
    let router = app(test_state());
    let resp = router
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["status"], "ok");
}

#[tokio::test]
async fn retrieve_blank_query_is_bad_request() {
    // A blank query is rejected (400) after tenant resolution but BEFORE any
    // embed/DB access, so this stays DB-free.
    let router = app(test_state());
    let body = serde_json::json!({ "tenant_id": "t1", "principal_id": "p1", "query": "   " });

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/retrieve")
                .header("content-type", "application/json")
                .header("x-principal-id", "p1")
                .header("x-tenant-id", "t1")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["error"]["code"], "bad_request");
}

#[tokio::test]
async fn retrieve_without_principal_is_unauthorized() {
    let router = app(test_state());
    let body = serde_json::json!({ "tenant_id": "t1", "principal_id": "p1", "query": "hello" });

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/retrieve")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn retrieve_with_mismatched_tenant_is_forbidden() {
    // /retrieve now performs real, tenant-scoped DB work, but it first checks
    // that the request's tenant agrees with the authenticated principal's
    // tenant. That guard runs *before* any DB access, so this stays DB-free:
    // principal tenant `t2` (X-Tenant-Id) vs body tenant `t1` => 403.
    let router = app(test_state());
    let body = serde_json::json!({ "tenant_id": "t1", "principal_id": "p1", "query": "hello" });

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/retrieve")
                .header("content-type", "application/json")
                .header("x-principal-id", "p1")
                .header("x-tenant-id", "t2")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["error"]["code"], "forbidden");
}

#[tokio::test]
async fn retrieve_without_tenant_header_is_unauthorized() {
    // Fail closed: a caller that omits X-Tenant-Id must NOT be able to name any
    // tenant in the body (that was a cross-tenant read bypass). No X-Tenant-Id
    // => 401, before any DB access, so this stays DB-free.
    let router = app(test_state());
    let body =
        serde_json::json!({ "tenant_id": "victim", "principal_id": "attacker", "query": "hello" });

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/retrieve")
                .header("content-type", "application/json")
                .header("x-principal-id", "attacker")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["error"]["code"], "unauthorized");
}

#[tokio::test]
async fn skills_register_without_tenant_is_unauthorized() {
    // skills.register is tenant-scoped and its manifest carries no tenant, so the
    // tenant is the authenticated principal's. Omitting X-Tenant-Id must fail
    // closed (401) BEFORE any DB access — locking the fail-closed tenant rule.
    let router = app(test_state());
    let body = serde_json::json!({ "skill_id": "s", "version": "1.0.0", "name": "n" });

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/skills.register")
                .header("content-type", "application/json")
                .header("x-principal-id", "p1")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["error"]["code"], "unauthorized");
}

#[tokio::test]
async fn tool_execute_without_tenant_is_unauthorized() {
    // tool.execute is tenant-scoped; with no X-Tenant-Id it must fail closed (401)
    // before any DB access.
    let router = app(test_state());
    let body = serde_json::json!({ "tenant_id": "t1", "principal_id": "p1", "tool_id": "x" });

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/tool.execute")
                .header("content-type", "application/json")
                .header("x-principal-id", "p1")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["error"]["code"], "unauthorized");
}

#[tokio::test]
async fn runs_start_without_tenant_is_unauthorized() {
    // runs.start is tenant-scoped; with no X-Tenant-Id it must fail closed (401)
    // before any DB access.
    let router = app(test_state());
    let body = serde_json::json!({ "tenant_id": "t1", "run_type": "summarize" });

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/runs.start")
                .header("content-type", "application/json")
                .header("x-principal-id", "p1")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["error"]["code"], "unauthorized");
}

#[tokio::test]
async fn runs_resume_without_tenant_is_unauthorized() {
    // runs.resume carries no body tenant; with no X-Tenant-Id it must fail closed
    // (401) before any DB access.
    let router = app(test_state());
    let body =
        serde_json::json!({ "run_id": "00000000-0000-0000-0000-000000000000", "token": "x" });

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/runs.resume")
                .header("content-type", "application/json")
                .header("x-principal-id", "p1")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["error"]["code"], "unauthorized");
}

#[tokio::test]
async fn context_upsert_without_tenant_is_unauthorized() {
    // context.upsert is tenant-scoped; with no X-Tenant-Id and no body tenant it
    // must fail closed (401) before any DB access.
    let router = app(test_state());
    let body = serde_json::json!({ "principal_id": "user_1" });

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/context.upsert")
                .header("content-type", "application/json")
                .header("x-principal-id", "admin_1")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["error"]["code"], "unauthorized");
}

#[tokio::test]
async fn audit_events_without_tenant_is_unauthorized() {
    // audit/events is tenant-scoped; with no X-Tenant-Id it must fail closed (401)
    // before any DB access.
    let router = app(test_state());

    let resp = router
        .oneshot(
            Request::builder()
                .uri("/audit/events")
                .header("x-principal-id", "p1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["error"]["code"], "unauthorized");
}
