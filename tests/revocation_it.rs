//! Live integration test: stateful per-subject token revocation (revoke-all).
//!
//! With `AUTH_REVOCATION_ENABLED=true`, a verified bearer token is checked against a per-`(tenant,
//! principal)` `not_before` cutoff after it verifies: a token whose `iat` is strictly before the
//! cutoff is rejected `401`, while a token issued after it still passes. This proves the full
//! vertical against the admin write API (`/admin/revocations`) + the extractor gate:
//!   - a non-revoked token authenticates (blank `/retrieve` query -> 400, i.e. it got past auth);
//!   - an admin revokes the principal; the old token is then 401;
//!   - with revocation DISABLED the SAME cutoff row is ignored (opt-in toggle);
//!   - a freshly-issued token (iat after the cutoff) authenticates again;
//!   - a non-admin cannot revoke (403); and clearing the cutoff re-allows the old token.
//!
//! **DB-gated:** skipped unless `DATABASE_URL` is set. Run locally:
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=postgres -p 5480:5432 pgvector/pgvector:pg16
//! export DATABASE_URL=postgres://postgres:postgres@localhost:5480/postgres
//! cargo test --test revocation_it -- --nocapture
//! ```

mod common;

use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use common::{app_pool, apply_schema, TestDb};
use jsonwebtoken::{encode, EncodingKey, Header};
use serde_json::{json, Value};
use tower::ServiceExt; // oneshot

use synapse::config::Config;
use synapse::state::AppState;

const SECRET: &str = "test-secret-value-for-hs256-signing";

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn mint(claims: &Value) -> String {
    encode(
        &Header::default(),
        claims,
        &EncodingKey::from_secret(SECRET.as_bytes()),
    )
    .unwrap()
}

/// HS256 verified mode; `revocation` toggles `AUTH_REVOCATION_ENABLED`.
fn cfg(database_url: &str, revocation: bool) -> Config {
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
        auth_jwt_secret: Some(SECRET.to_string()),
        auth_jwt_public_key: None,
        auth_jwt_audience: None,
        auth_jwt_issuer: None,
        auth_jwks_url: None,
        auth_jwks_timeout_secs: 10,
        auth_jwks_min_refetch_secs: 60,
        auth_revocation_enabled: revocation,
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

/// Auth probe: a blank `/retrieve` query is a 400 AFTER the extractor (incl. the revocation gate)
/// passes, so 400 == "authenticated (not revoked)", 401 == "rejected (revoked / bad auth)".
async fn probe(router: &Router, token: &str, tenant: &str) -> StatusCode {
    let body = json!({ "tenant_id": tenant, "principal_id": "alice", "query": "" });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/retrieve")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    resp.status()
}

/// `POST`/`DELETE` `/admin/revocations` with a bearer token.
async fn admin_revocation(
    router: &Router,
    method: &str,
    token: &str,
    principal_id: &str,
) -> StatusCode {
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri("/admin/revocations")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(
                    json!({ "principal_id": principal_id }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    resp.status()
}

#[tokio::test]
async fn revocation_rejects_pre_cutoff_tokens_and_honors_the_toggle() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };
    let t = "tenant_a";

    let test_db = TestDb::create(&base_url, "revocation").await;
    let admin_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&test_db.url)
        .await
        .expect("connect admin");
    apply_schema(&admin_pool, &test_db.role).await;

    // Two routers on the SAME DB: revocation ENABLED vs DISABLED (to prove the opt-in toggle).
    let enabled = synapse::app(AppState::new(
        app_pool(&test_db.url, &test_db.role).await,
        cfg(&test_db.url, true),
    ));
    let disabled = synapse::app(AppState::new(
        app_pool(&test_db.url, &test_db.role).await,
        cfg(&test_db.url, false),
    ));

    // alice's token, issued 60s ago; boss is a verified admin.
    let alice_old = mint(
        &json!({ "sub": "alice", "tenant": t, "iat": now_secs() - 60, "exp": now_secs() + 3600 }),
    );
    let boss = mint(
        &json!({ "sub": "boss", "tenant": t, "role": "admin", "iat": now_secs(), "exp": now_secs() + 3600 }),
    );

    // (1) Before any revocation, alice's token authenticates.
    assert_eq!(
        probe(&enabled, &alice_old, t).await,
        StatusCode::BAD_REQUEST,
        "no cutoff -> alice authenticates"
    );

    // (2) Admin revokes alice (cutoff = now, which is AFTER alice_old's iat).
    assert_eq!(
        admin_revocation(&enabled, "POST", &boss, "alice").await,
        StatusCode::OK,
        "an admin revokes alice"
    );

    // (3) alice's OLD token (iat < cutoff) is now rejected.
    assert_eq!(
        probe(&enabled, &alice_old, t).await,
        StatusCode::UNAUTHORIZED,
        "pre-cutoff token is revoked -> 401"
    );

    // (4) The SAME cutoff row is IGNORED when revocation is disabled (opt-in toggle).
    assert_eq!(
        probe(&disabled, &alice_old, t).await,
        StatusCode::BAD_REQUEST,
        "with AUTH_REVOCATION_ENABLED off, the cutoff is not enforced"
    );

    // (5) A freshly-issued token (iat AFTER the cutoff) authenticates again — re-login works.
    let alice_new = mint(
        &json!({ "sub": "alice", "tenant": t, "iat": now_secs() + 5, "exp": now_secs() + 3600 }),
    );
    assert_eq!(
        probe(&enabled, &alice_new, t).await,
        StatusCode::BAD_REQUEST,
        "post-cutoff token is accepted"
    );

    // (6) A non-admin cannot revoke (alice_new is a no-role Viewer) -> 403.
    assert_eq!(
        admin_revocation(&enabled, "POST", &alice_new, "boss").await,
        StatusCode::FORBIDDEN,
        "a non-admin cannot revoke"
    );

    // (7) Clearing the cutoff re-allows even the old token.
    assert_eq!(
        admin_revocation(&enabled, "DELETE", &boss, "alice").await,
        StatusCode::OK,
        "an admin clears alice's revocation"
    );
    assert_eq!(
        probe(&enabled, &alice_old, t).await,
        StatusCode::BAD_REQUEST,
        "after clear, the old token authenticates again"
    );

    // Side effect is real: the cutoff row is gone.
    let (exists,): (bool,) = sqlx::query_as(
        "SELECT EXISTS(SELECT 1 FROM revocations WHERE tenant_id = $1 AND principal_id = 'alice')",
    )
    .bind(t)
    .fetch_one(&admin_pool)
    .await
    .unwrap();
    assert!(!exists, "the revocation row was cleared");

    admin_pool.close().await;
    println!("revocation: a pre-cutoff token is 401 when enabled, ignored when disabled; a post-cutoff token passes; non-admin revoke is 403; clear re-allows.");
}

#[tokio::test]
async fn revocation_is_fail_closed_at_the_cutoff_second() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };
    let t = "tenant_a";
    let test_db = TestDb::create(&base_url, "revocationboundary").await;
    let admin_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&test_db.url)
        .await
        .expect("connect admin");
    apply_schema(&admin_pool, &test_db.role).await;
    let enabled = synapse::app(AppState::new(
        app_pool(&test_db.url, &test_db.role).await,
        cfg(&test_db.url, true),
    ));

    // Pin a cutoff EXACTLY at a token's iat second (not_before = to_timestamp(iat), sub-second 0), so
    // not_before.timestamp() == iat. A token issued in that second must be REJECTED — this passes
    // under the fail-closed `iat > cutoff` and would WRONGLY pass under `iat >= cutoff`.
    let iat = now_secs() - 100;
    sqlx::query(
        "INSERT INTO revocations (tenant_id, principal_id, not_before) \
         VALUES ($1, 'alice', to_timestamp($2::double precision))",
    )
    .bind(t)
    .bind(iat)
    .execute(&admin_pool)
    .await
    .unwrap();

    let same_second =
        mint(&json!({ "sub": "alice", "tenant": t, "iat": iat, "exp": now_secs() + 3600 }));
    assert_eq!(
        probe(&enabled, &same_second, t).await,
        StatusCode::UNAUTHORIZED,
        "a token issued in the cutoff second is rejected (fail-closed `>`, not `>=`)"
    );

    // One second later is strictly after the cutoff -> admitted.
    let next_second =
        mint(&json!({ "sub": "alice", "tenant": t, "iat": iat + 1, "exp": now_secs() + 3600 }));
    assert_eq!(
        probe(&enabled, &next_second, t).await,
        StatusCode::BAD_REQUEST,
        "a token issued one second after the cutoff is admitted"
    );

    admin_pool.close().await;
    println!("revocation boundary: the cutoff second is fail-closed (iat == floor(not_before) -> 401); iat+1 -> pass.");
}
