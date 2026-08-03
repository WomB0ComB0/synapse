//! Integration tests for real caller authentication (verified JWT mode).
//!
//! When `Config.auth_jwt_secret` is set, the `Principal` extractor requires a valid
//! `Authorization: Bearer <HS256 JWT>` and derives identity from the SIGNED claims;
//! the `X-*` identity headers are ignored (no spoofing). Most of this is DB-free —
//! auth is rejected (or the tenant is resolved) before any DB access — so those
//! cases always run; one end-to-end case is `DATABASE_URL`-gated.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};
use tower::ServiceExt;

use jsonwebtoken::{encode, EncodingKey, Header};
use synapse::config::Config;
use synapse::state::AppState;

const SECRET: &str = "test-signing-secret-please-change";
const OTHER_SECRET: &str = "a-totally-different-secret";

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Mint an HS256 token from an arbitrary claims value (so tests can omit `exp` etc.).
fn mint(secret: &str, claims: &Value) -> String {
    encode(
        &Header::default(),
        claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .unwrap()
}

/// A valid token: subject, tenant, and an `exp` one hour in the future.
fn valid_token(sub: &str, tenant: &str) -> String {
    mint(
        SECRET,
        &json!({ "sub": sub, "tenant": tenant, "exp": unix_now() + 3600 }),
    )
}

fn cfg(database_url: &str, auth: Option<&str>, audience: Option<&str>) -> Config {
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
        auth_jwt_secret: auth.map(str::to_string),
        auth_jwt_public_key: None,
        auth_jwt_audience: audience.map(str::to_string),
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

/// A DB-free router with JWT auth ON (lazy pool — never connected by these cases).
/// `audience` is the optional expected `aud`.
fn auth_router_with(audience: Option<&str>, issuer: Option<&str>) -> axum::Router {
    let mut config = cfg("postgres://u:p@localhost:5432/nodb", Some(SECRET), audience);
    config.auth_jwt_issuer = issuer.map(str::to_string);
    let pool = synapse::db::init(&config).expect("build lazy pool");
    synapse::app(AppState::new(pool, config))
}

fn auth_router() -> axum::Router {
    auth_router_with(None, None)
}

/// POST `body` to `uri` with optional `Authorization` and `X-*` headers.
async fn post(
    router: &axum::Router,
    uri: &str,
    bearer: Option<&str>,
    extra: &[(&str, &str)],
    body: Value,
) -> (StatusCode, Value) {
    let mut b = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(tok) = bearer {
        b = b.header("authorization", format!("Bearer {tok}"));
    }
    for (k, v) in extra {
        b = b.header(*k, *v);
    }
    let resp = router
        .clone()
        .oneshot(b.body(Body::from(body.to_string())).unwrap())
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

#[tokio::test]
async fn jwt_mode_rejects_and_derives_identity_db_free() {
    let router = auth_router();
    let retrieve = |t: &str| json!({ "tenant_id": t, "principal_id": "u", "query": "hello" });

    // (1) No Authorization header -> 401 (the X-* headers alone are NOT accepted).
    let (s, _) = post(
        &router,
        "/retrieve",
        None,
        &[("x-principal-id", "u"), ("x-tenant-id", "t1")],
        retrieve("t1"),
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "no bearer -> 401");

    // (2) Non-Bearer scheme -> 401.
    let (s, _) = post(
        &router,
        "/retrieve",
        None,
        &[("authorization", "Basic dXNlcjpwYXNz")],
        retrieve("t1"),
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "non-bearer scheme -> 401");

    // (3) Garbage token -> 401.
    let (s, _) = post(&router, "/retrieve", Some("not.a.jwt"), &[], retrieve("t1")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "malformed token -> 401");

    // (4) Correctly-formed token signed with the WRONG secret -> 401 (signature).
    let forged = mint(
        OTHER_SECRET,
        &json!({ "sub": "u", "tenant": "t1", "exp": unix_now() + 3600 }),
    );
    let (s, _) = post(&router, "/retrieve", Some(&forged), &[], retrieve("t1")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "wrong-signature token -> 401");

    // (5) Expired token -> 401. Expire an hour ago, well beyond jsonwebtoken's
    //     default 60s exp leeway (a 10s-ago token would still be within it).
    let expired = mint(
        SECRET,
        &json!({ "sub": "u", "tenant": "t1", "exp": unix_now() - 3600 }),
    );
    let (s, _) = post(&router, "/retrieve", Some(&expired), &[], retrieve("t1")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "expired token -> 401");

    // (6) Token with NO exp claim -> 401 (exp is required, fail-closed).
    let no_exp = mint(SECRET, &json!({ "sub": "u", "tenant": "t1" }));
    let (s, _) = post(&router, "/retrieve", Some(&no_exp), &[], retrieve("t1")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "missing exp -> 401");

    // (7) Valid token + a blank query -> 400 (token accepted; identity flows into
    //     the handler; the blank-query 400 is reached before any DB access).
    let (s, _) = post(
        &router,
        "/retrieve",
        Some(&valid_token("u", "t1")),
        &[],
        json!({ "tenant_id": "t1", "principal_id": "u", "query": "   " }),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "valid token + blank query -> 400"
    );

    // (8) The tenant comes from the TOKEN: token tenant=t1, body tenant=t2 -> 403.
    let (s, r) = post(
        &router,
        "/retrieve",
        Some(&valid_token("u", "t1")),
        &[],
        retrieve("t2"),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::FORBIDDEN,
        "token tenant vs body tenant -> 403: {r}"
    );

    // (9) Anti-spoof: a spoofed X-Tenant-Id is IGNORED. Token tenant=t1, header
    //     X-Tenant-Id=t2, body tenant=t1, blank query. If the header were honored,
    //     body t1 vs header t2 would 403; a 400 proves the TOKEN's t1 won.
    let (s, _) = post(
        &router,
        "/retrieve",
        Some(&valid_token("u", "t1")),
        &[("x-tenant-id", "t2")],
        json!({ "tenant_id": "t1", "principal_id": "u", "query": "   " }),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "spoofed X-Tenant-Id ignored (token tenant authoritative) -> 400"
    );

    // (10) A valid token with NO tenant claim can't act tenant-less: fail-closed 401.
    let no_tenant = mint(SECRET, &json!({ "sub": "u", "exp": unix_now() + 3600 }));
    let (s, _) = post(&router, "/retrieve", Some(&no_tenant), &[], retrieve("t1")).await;
    assert_eq!(
        s,
        StatusCode::UNAUTHORIZED,
        "token without a tenant claim -> 401 (fail-closed)"
    );

    println!("JWT auth (DB-free): missing/non-bearer/garbage/wrong-sig/expired/no-exp -> 401; valid token flows identity; tenant is token-authoritative (X-Tenant-Id spoof ignored); no-tenant token -> 401.");
}

#[tokio::test]
async fn jwt_audience_and_optional_claims_db_free() {
    let blank = |t: &str| json!({ "tenant_id": t, "principal_id": "u", "query": "   " });

    // --- Audience UNSET: a token that CARRIES an `aud` claim is still accepted ---
    // (the default jsonwebtoken Validation would wrongly reject any aud-bearing
    // token; we disable aud validation when no audience is configured).
    let router = auth_router(); // no expected audience
    let with_aud = mint(
        SECRET,
        &json!({ "sub": "u", "tenant": "t1", "aud": "some-service", "exp": unix_now() + 3600 }),
    );
    let (s, _) = post(&router, "/retrieve", Some(&with_aud), &[], blank("t1")).await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "audience unset: an aud-bearing token is accepted (reaches blank-query 400)"
    );

    // --- Audience SET: aud must match ---
    let router = auth_router_with(Some("synapse-api"), None);
    // Matching aud -> accepted.
    let good = mint(
        SECRET,
        &json!({ "sub": "u", "tenant": "t1", "aud": "synapse-api", "exp": unix_now() + 3600 }),
    );
    let (s, _) = post(&router, "/retrieve", Some(&good), &[], blank("t1")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "matching aud accepted -> 400");
    // Wrong aud -> 401.
    let wrong = mint(
        SECRET,
        &json!({ "sub": "u", "tenant": "t1", "aud": "other-api", "exp": unix_now() + 3600 }),
    );
    let (s, _) = post(&router, "/retrieve", Some(&wrong), &[], blank("t1")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "wrong aud -> 401");
    // Missing aud when one is required -> 401.
    let (s, _) = post(
        &router,
        "/retrieve",
        Some(&valid_token("u", "t1")),
        &[],
        blank("t1"),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::UNAUTHORIZED,
        "missing aud when required -> 401"
    );

    // --- Issuer SET: iss must be present and match ---
    let router = auth_router_with(None, Some("https://issuer.example"));
    let good_issuer = mint(
        SECRET,
        &json!({ "sub": "u", "tenant": "t1", "iss": "https://issuer.example", "exp": unix_now() + 3600 }),
    );
    let (s, _) = post(&router, "/retrieve", Some(&good_issuer), &[], blank("t1")).await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "matching issuer accepted -> 400"
    );
    let wrong_issuer = mint(
        SECRET,
        &json!({ "sub": "u", "tenant": "t1", "iss": "https://other.example", "exp": unix_now() + 3600 }),
    );
    let (s, _) = post(&router, "/retrieve", Some(&wrong_issuer), &[], blank("t1")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "wrong issuer -> 401");
    let (s, _) = post(
        &router,
        "/retrieve",
        Some(&valid_token("u", "t1")),
        &[],
        blank("t1"),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::UNAUTHORIZED,
        "missing issuer when required -> 401"
    );

    // --- Optional `teams` claim: null / absent must NOT 401 an otherwise-valid token ---
    let router = auth_router();
    let teams_null = mint(
        SECRET,
        &json!({ "sub": "u", "tenant": "t1", "teams": Value::Null, "exp": unix_now() + 3600 }),
    );
    let (s, _) = post(&router, "/retrieve", Some(&teams_null), &[], blank("t1")).await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "teams: null is tolerated (-> 400)"
    );
    let teams_arr = mint(
        SECRET,
        &json!({ "sub": "u", "tenant": "t1", "teams": ["eng", "ops"], "exp": unix_now() + 3600 }),
    );
    let (s, _) = post(&router, "/retrieve", Some(&teams_arr), &[], blank("t1")).await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "teams: array is accepted (-> 400)"
    );

    println!("JWT auth (DB-free): aud disabled-when-unset (interop) / pinned-when-set (match->ok, wrong/missing->401); teams null|absent|array all tolerated.");
}

#[tokio::test]
async fn jwt_mode_authorizes_a_real_request_end_to_end() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated end-to-end JWT test");
        return;
    };
    // Provision a throwaway DB (schema + tenant_a), then build a router with JWT ON.
    let test_db = common::TestDb::create(&base_url, "authjwt").await;
    let admin = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&test_db.url)
        .await
        .expect("connect admin");
    common::apply_schema(&admin, &test_db.role).await;
    admin.close().await;
    let pool = common::app_pool(&test_db.url, &test_db.role).await;
    let router = synapse::app(AppState::new(pool, cfg(&test_db.url, Some(SECRET), None)));

    // A valid token for tenant_a drives a real documents.ingest -> 200 (identity
    // from the signed claim reaches the DB layer under RLS).
    let (s, r) = post(
        &router,
        "/documents.ingest",
        Some(&valid_token("agent_1", "tenant_a")),
        &[],
        json!({ "doc_id": "d1", "tenant_id": "tenant_a", "content": "hello world" }),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "valid token drives documents.ingest -> 200: {r}"
    );

    // Same endpoint, but a garbage bearer is still rejected end-to-end.
    let (s, _) = post(
        &router,
        "/documents.ingest",
        Some("garbage"),
        &[],
        json!({ "doc_id": "d2", "tenant_id": "tenant_a", "content": "x" }),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::UNAUTHORIZED,
        "garbage token -> 401 end-to-end"
    );

    println!(
        "JWT auth (live): a signed token drives a real documents.ingest -> 200; garbage -> 401."
    );
}
