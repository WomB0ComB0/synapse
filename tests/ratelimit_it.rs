//! Live integration test: per-tenant token-bucket rate limiting.
//!
//! With `RATE_LIMIT_ENABLED=true` and a small burst, a tenant is allowed up to `burst` rapid requests
//! and then throttled `429` (with a `Retry-After` header); a DIFFERENT tenant is unaffected (its own
//! bucket); and refilling (simulated deterministically by backdating the bucket's `updated_at`, so no
//! sleeping) re-allows the throttled tenant. Trusted-headers mode (no JWT needed); the probe is a
//! blank `/retrieve` query, which is a 400 AFTER the extractor's rate check passes and a 429 when it
//! doesn't.
//!
//! **DB-gated:** skipped unless `DATABASE_URL` is set. Run locally:
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=postgres -p 5482:5432 pgvector/pgvector:pg16
//! export DATABASE_URL=postgres://postgres:postgres@localhost:5482/postgres
//! cargo test --test ratelimit_it -- --nocapture
//! ```

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use common::{app_pool, apply_schema, TestDb};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt; // oneshot

use synapse::config::Config;
use synapse::state::AppState;

/// Trusted-headers router with rate limiting ON: a slow refill (0.1/s) and small burst (3) so a few
/// rapid requests exhaust the bucket and the refill within the test window is negligible.
fn router_for(pool: sqlx::PgPool, database_url: &str) -> Router {
    let config = Config {
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
        rate_limit_enabled: true,
        rate_limit_tenant_rps: 0.1,
        rate_limit_burst: 3.0,
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
    synapse::app(AppState::new(pool, config))
}

/// A blank-query `/retrieve` probe (trusted-headers). Returns the status, the `Retry-After` header (if
/// any), and the JSON body.
async fn probe(router: &Router, tenant: &str) -> (StatusCode, Option<String>, Value) {
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/retrieve")
                .header("content-type", "application/json")
                .header("x-principal-id", "u")
                .header("x-tenant-id", tenant)
                .body(Body::from(
                    json!({ "tenant_id": tenant, "principal_id": "u", "query": "" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let retry = resp
        .headers()
        .get("retry-after")
        .map(|v| v.to_str().unwrap().to_string());
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, retry, body)
}

#[tokio::test]
async fn rate_limit_throttles_a_tenant_isolates_others_and_refills() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };

    let test_db = TestDb::create(&base_url, "ratelimit").await;
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&test_db.url)
        .await
        .expect("connect admin");
    apply_schema(&admin, &test_db.role).await;
    let router = router_for(app_pool(&test_db.url, &test_db.role).await, &test_db.url);

    // (1) tenant_a: a full bucket (burst 3) allows exactly 3 rapid requests (400 = auth + rate passed).
    for i in 1..=3 {
        let (status, _, _) = probe(&router, "tenant_a").await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "request {i} within burst should pass (blank query -> 400)"
        );
    }

    // (2) the 4th is throttled: 429 + a Retry-After header + the stable error code.
    let (status, retry, body) = probe(&router, "tenant_a").await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "over-burst request is 429"
    );
    let retry = retry.expect("429 carries a Retry-After header");
    assert!(
        retry.parse::<u64>().unwrap() >= 1,
        "Retry-After is a positive second count, got {retry:?}"
    );
    assert_eq!(body["error"]["code"], "too_many_requests");

    // (3) per-tenant isolation: tenant_b has its OWN full bucket and is unaffected by tenant_a's throttle.
    let (status, _, _) = probe(&router, "tenant_b").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a different tenant is not throttled by tenant_a"
    );

    // (4) refill: backdate tenant_a's bucket so tokens accrue (0.1/s over 1000s -> capped at burst),
    // then it authenticates again — deterministic, no sleeping.
    sqlx::query("UPDATE rate_limits SET updated_at = now() - interval '1000 seconds' WHERE tenant_id = 'tenant_a'")
        .execute(&admin)
        .await
        .unwrap();
    let (status, _, _) = probe(&router, "tenant_a").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "after refill, tenant_a authenticates again"
    );

    admin.close().await;
    println!("rate limit: burst allows N then 429 + Retry-After; a different tenant is isolated; a refill re-allows.");
}
