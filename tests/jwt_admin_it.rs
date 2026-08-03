//! Live integration test: the JWT-verified admin path.
//!
//! The elevated `admin` role is honored only from a TRUSTED source — `principals.role`
//! (DB-authoritative) OR a cryptographically VERIFIED signed-JWT `role` claim — and NEVER from an
//! unverified `X-Role` header. This proves the JWT path end-to-end against an admin-gated operation
//! (`teams.create`): a signed `role:admin` token creates a team; a signed non-admin token is 403;
//! and in trusted-headers mode `X-Role: admin` cannot forge admin.
//!
//! **DB-gated:** skipped unless `DATABASE_URL` is set. Run locally:
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=postgres -p 5475:5432 pgvector/pgvector:pg16
//! export DATABASE_URL=postgres://postgres:postgres@localhost:5475/postgres
//! cargo test --test jwt_admin_it -- --nocapture
//! ```

mod common;

use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use common::{app_pool, apply_schema, TestDb};
use http_body_util::BodyExt;
use jsonwebtoken::{encode, EncodingKey, Header};
use serde_json::{json, Value};
use tower::ServiceExt; // oneshot

use synapse::config::Config;
use synapse::state::AppState;

const SECRET: &str = "test-secret-value-for-hs256-signing";

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn mint(claims: &Value) -> String {
    encode(
        &Header::default(),
        claims,
        &EncodingKey::from_secret(SECRET.as_bytes()),
    )
    .unwrap()
}

fn cfg(database_url: &str, jwt_mode: bool) -> Config {
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
        auth_jwt_secret: jwt_mode.then(|| SECRET.to_string()),
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

/// POST `/teams.create` with a Bearer token (JWT mode ignores `X-*`).
async fn create_team_jwt(router: &Router, token: &str, team_id: &str) -> StatusCode {
    let body = json!({ "team_id": team_id });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/teams.create")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    resp.status()
}

#[tokio::test]
async fn verified_jwt_may_assert_admin_but_a_header_may_not() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };
    let t = "tenant_a";

    let test_db = TestDb::create(&base_url, "jwtadmin").await;
    let admin_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&test_db.url)
        .await
        .expect("connect admin");
    apply_schema(&admin_pool, &test_db.role).await;

    // --- JWT mode: a signed role:admin token may create a team; a non-admin token cannot ---
    let jwt_router = synapse::app(AppState::new(
        app_pool(&test_db.url, &test_db.role).await,
        cfg(&test_db.url, true),
    ));

    let admin_tok =
        mint(&json!({ "sub": "boss", "tenant": t, "role": "admin", "exp": unix_now() + 3600 }));
    let s = create_team_jwt(&jwt_router, &admin_tok, "alpha").await;
    assert_eq!(
        s,
        StatusCode::OK,
        "a VERIFIED role:admin JWT creates a team"
    );

    let member_tok =
        mint(&json!({ "sub": "alice", "tenant": t, "role": "member", "exp": unix_now() + 3600 }));
    let s = create_team_jwt(&jwt_router, &member_tok, "beta").await;
    assert_eq!(
        s,
        StatusCode::FORBIDDEN,
        "a signed role:member JWT is NOT admin"
    );

    let norole_tok = mint(&json!({ "sub": "nobody", "tenant": t, "exp": unix_now() + 3600 }));
    let s = create_team_jwt(&jwt_router, &norole_tok, "gamma").await;
    assert_eq!(
        s,
        StatusCode::FORBIDDEN,
        "a signed token with no role claim is NOT admin"
    );
    println!("(jwt) a VERIFIED role:admin claim asserts admin; a signed non-admin / no-role token does not");

    // Confirm the admin token actually created the team (side effect real, not just a 200).
    let (exists,): (bool,) = sqlx::query_as(
        "SELECT EXISTS(SELECT 1 FROM teams WHERE tenant_id = $1 AND team_id = 'alpha' AND owners @> ARRAY['boss'])",
    )
    .bind(t)
    .fetch_one(&admin_pool)
    .await
    .unwrap();
    assert!(exists, "the admin JWT's team was created and owned by boss");

    // --- Trusted-headers mode: X-Role: admin cannot forge admin ---
    let hdr_router = synapse::app(AppState::new(
        app_pool(&test_db.url, &test_db.role).await,
        cfg(&test_db.url, false),
    ));
    let resp = hdr_router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/teams.create")
                .header("content-type", "application/json")
                .header("x-principal-id", "mallory")
                .header("x-tenant-id", t)
                .header("x-role", "admin") // UNVERIFIED — must not assert admin
                .body(Body::from(json!({ "team_id": "delta" }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "an UNVERIFIED X-Role: admin header cannot create a team (admin is not header-assertable)"
    );
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["error"]["code"], "forbidden");
    println!("(header) an unverified X-Role: admin cannot forge admin — admin stays DB- or verified-JWT-only");

    admin_pool.close().await;
    println!("jwt admin: the elevated admin role is honored from principals.role OR a verified signed-JWT claim, never from a plaintext X-Role header.");
}
