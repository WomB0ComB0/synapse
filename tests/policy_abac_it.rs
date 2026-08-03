//! Integration test for PolicyGateway PR3 — resource ABAC (context ownership).
//!
//! When `abac_context_ownership` is enabled, a caller may read/write only their OWN
//! context: the PolicyGateway requires `caller == subject` for context.get/upsert
//! (the `resource` — the subject principal_id — becomes authoritative), after the
//! coarse role×action RBAC — EXCEPT an `Admin`, who may perform governed cross-principal
//! access (the elevated-tier override). When disabled (the default, exercised by every
//! other suite), context access is governed by role RBAC alone.
//!
//! **DB-gated:** skipped unless `DATABASE_URL` is set. Run locally:
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=postgres -p 5459:5432 pgvector/pgvector:pg16
//! export DATABASE_URL=postgres://postgres:postgres@localhost:5459/postgres
//! cargo test --test policy_abac_it -- --nocapture
//! ```

mod common;

use axum::http::StatusCode;
use common::{post_json_with_role, TestDb};
use serde_json::json;
use sqlx::postgres::PgPoolOptions;

use synapse::config::Config;
use synapse::state::AppState;

fn cfg(database_url: &str, abac: bool) -> Config {
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
        abac_context_ownership: abac,
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

/// Provision a throwaway DB (schema + tenant_a) and return (router, admin pool),
/// with context-ownership ABAC set to `abac`.
async fn setup_abac(base_url: &str, slug: &str, abac: bool) -> (axum::Router, sqlx::PgPool) {
    let test_db = TestDb::create(base_url, slug).await;
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&test_db.url)
        .await
        .expect("connect admin");
    common::apply_schema(&admin, &test_db.role).await;
    let pool = common::app_pool(&test_db.url, &test_db.role).await;
    let router = synapse::app(AppState::new(pool, cfg(&test_db.url, abac)));
    (router, admin)
}

#[tokio::test]
async fn context_ownership_abac() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };
    let (router, admin) = setup_abac(&base_url, "policyabac", true).await;
    let t = "tenant_a";
    // The elevated `admin` role is DB-authoritative only (not header-assertable), so
    // provision `boss` with principals.role='admin' for the cross-principal override (e).
    common::provision_role(&admin, t, "boss", "admin").await;

    // (a) SELF: a caller may upsert + read their OWN context (caller == subject).
    let (s, r) = post_json_with_role(
        &router,
        "/context.upsert",
        t,
        "alice", // caller (X-Principal-Id)
        None,
        json!({"tenant_id": t, "principal_id": "alice"}), // subject == caller
    )
    .await;
    assert_eq!(s, StatusCode::OK, "self upsert allowed: {r}");
    let (s, _) = post_json_with_role(
        &router,
        "/context.get",
        t,
        "alice",
        None,
        json!({"principal_id": "alice"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "self get allowed");
    println!("(a) self context.upsert + context.get allowed (caller == subject)");

    // (b) OTHER: a caller may NOT write another principal's context (caller != subject).
    let (s, r) = post_json_with_role(
        &router,
        "/context.upsert",
        t,
        "alice",
        None,
        json!({"tenant_id": t, "principal_id": "bob"}), // subject != caller
    )
    .await;
    assert_eq!(
        s,
        StatusCode::FORBIDDEN,
        "cross-principal upsert denied: {r}"
    );
    assert_eq!(r["error"]["code"], "forbidden");
    // ...nor read it.
    let (s, r) = post_json_with_role(
        &router,
        "/context.get",
        t,
        "alice",
        None,
        json!({"principal_id": "bob"}),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "cross-principal get denied: {r}");
    println!("(b) cross-principal context.upsert + context.get denied (403)");

    // (e) ADMIN OVERRIDE: a DB-authoritative admin (boss, provisioned above — NOT via a
    //     header) may access ANOTHER principal's context even with ABAC on. The absent
    //     X-Role proves the Admin role comes from principals.role, not the caller.
    let (s, r) = post_json_with_role(
        &router,
        "/context.upsert",
        t,
        "boss",
        None,
        json!({"tenant_id": t, "principal_id": "bob"}), // subject != caller, but caller is a DB admin
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "admin cross-principal upsert allowed: {r}"
    );
    let (s, _) = post_json_with_role(
        &router,
        "/context.get",
        t,
        "boss",
        None,
        json!({"principal_id": "bob"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "admin cross-principal get allowed");
    println!("(e) a DB-authoritative admin may access another principal's context (cross-principal override)");

    // The override is audited exactly once for the cross-principal READ (the get) — NOT on
    // the write (the upsert handler already success-audits it), so no double-count.
    let (override_rows,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM audit_events \
         WHERE tenant_id = $1 AND principal_id = 'boss' AND resource = 'bob' \
           AND outcome = 'success' AND metadata->>'override' = 'admin'",
    )
    .bind(t)
    .fetch_one(&admin)
    .await
    .unwrap();
    assert_eq!(
        override_rows, 1,
        "admin override audited once (the get, not the upsert): got {override_rows}"
    );

    // (f) FORGE-RESISTANCE: a header-asserted admin (mallory sends X-Role: admin but has
    //     NO DB role) is NOT an admin — the override does not fire and she is denied.
    let (s, r) = post_json_with_role(
        &router,
        "/context.get",
        t,
        "mallory",
        Some("admin"),
        json!({"principal_id": "bob"}),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::FORBIDDEN,
        "header-asserted X-Role:admin denied cross-principal: {r}"
    );
    assert_eq!(r["error"]["code"], "forbidden");
    println!("(f) a header-asserted X-Role:admin does NOT unlock the override (admin is DB-authoritative only)");

    // (c) the ABAC denial is audited as `deny` with metadata.abac = context_ownership.
    let (deny_rows,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM audit_events \
         WHERE tenant_id = $1 AND principal_id = 'alice' AND resource = 'bob' \
           AND outcome = 'deny' AND metadata->>'abac' = 'context_ownership'",
    )
    .bind(t)
    .fetch_one(&admin)
    .await
    .unwrap();
    assert_eq!(
        deny_rows, 2,
        "both cross-principal denials audited (upsert + get): got {deny_rows}"
    );
    println!("(c) ABAC denials audited as deny (metadata.abac=context_ownership)");

    admin.close().await;

    // (d) FLAG OFF (the default): the same cross-principal access is NOT blocked by
    //     ABAC — governed by role RBAC alone (backward-compatible behavior).
    let (router_off, admin_off) = setup_abac(&base_url, "policyabacoff", false).await;
    let (s, _) = post_json_with_role(
        &router_off,
        "/context.upsert",
        t,
        "alice",
        None,
        json!({"tenant_id": t, "principal_id": "bob"}),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "ABAC off: cross-principal upsert allowed (role RBAC only)"
    );
    let (s, _) = post_json_with_role(
        &router_off,
        "/context.get",
        t,
        "alice",
        None,
        json!({"principal_id": "bob"}),
    )
    .await;
    assert_ne!(
        s,
        StatusCode::FORBIDDEN,
        "ABAC off: cross-principal get not blocked by ownership"
    );
    admin_off.close().await;
    println!(
        "(d) ABAC off (default): cross-principal access governed by role RBAC alone (unchanged)"
    );

    println!("PolicyGateway PR3: context is self-owned when ABAC_CONTEXT_OWNERSHIP is set (caller==subject); an admin may cross-access (override); off by default.");
}
