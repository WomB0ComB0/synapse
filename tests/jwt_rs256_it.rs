//! Live-router (DB-free) test: RS256 asymmetric JWT verification.
//!
//! With `AUTH_JWT_PUBLIC_KEY` set (and no secret), the service verifies `Authorization: Bearer <RS256
//! JWT>` against a PUBLIC key only — so it CANNOT mint tokens. A valid RS256 token authenticates; an
//! HS256 token forged with the public-key bytes as an HMAC secret (the classic RS256->HS256 confusion
//! attack) is rejected because the algorithm is pinned to RS256.
//!
//! The RSA keypair below is a THROWAWAY generated for this test only (never a production key).
//! No DB is needed: the assertions ride on the auth outcome + the pre-DB blank-query / tenant guards.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};
use tower::ServiceExt;

use synapse::config::Config;
use synapse::state::AppState;
use synapse::{app, db};

const PUB_PEM: &str = "-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAwqy/ZwhJleO0GSuQBDmP
NjXPqGqoWaLtiOk8T5PXCQoDebVmjh/yhnIENnXI82UO9IllrW86yJjaPHOlEFAo
AOP54t+LrIlf43BZi3pNcGY7nZENIUmVZSB0CPJTs3+xvso34V+L9TIl/dktfNnn
mfWu2xw7QdwYWSxjjH572RM0+gdQ/G9OHDA5Bwwqu+pLmzUsq8O5MyCftoEiDIJ9
qni2AE25QhhzLXcNaGkVK2rsAoO8xE79ARDHutEZCPaT3YYnf8PUTCWn4JnirJIO
fGXOACiHj/ylXlfsFapTax/tHzsUqQniW4A9F8mIujHy/kMTlWESRSutTMdhXhJa
dQIDAQAB
-----END PUBLIC KEY-----";

const PRIV_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQDCrL9nCEmV47QZ
K5AEOY82Nc+oaqhZou2I6TxPk9cJCgN5tWaOH/KGcgQ2dcjzZQ70iWWtbzrImNo8
c6UQUCgA4/ni34usiV/jcFmLek1wZjudkQ0hSZVlIHQI8lOzf7G+yjfhX4v1MiX9
2S182eeZ9a7bHDtB3BhZLGOMfnvZEzT6B1D8b04cMDkHDCq76kubNSyrw7kzIJ+2
gSIMgn2qeLYATblCGHMtdw1oaRUrauwCg7zETv0BEMe60RkI9pPdhid/w9RMJafg
meKskg58Zc4AKIeP/KVeV+wVqlNrH+0fOxSpCeJbgD0XyYi6MfL+QxOVYRJFK61M
x2FeElp1AgMBAAECggEAXbAtXTSL1WsEXaitYpsg5QH4siDCbIEQt/cnY1TPBDah
fY1jkbqmSTXN+TeuQhS8ocsN9+2z6J5HSRiOs88fsW4F8L2MxrhGQXrsXUe6xQEu
Z6JLI136W/TGYxfcWGJ39E31nq0Q+ivsRMKkNZXY9Ctcv25SxltaDHBkaFTm3Yyc
CyI6/p86+qbMoljssc1IrD7BVmWgVDh4wkJfZM5y/BC9M5Z2aIvmgsaBJKa911MV
HjEEoko2y3hqfo2KexAJeDhbg/4WmGFfftjjWS9+HOwstBwCqPSW6BV6FzQJ3+H6
OlxdXvFxmMvywLc9TtgEot6oP7qENnv96hLS+i6V8QKBgQD+2iIfhW4RMtfYYzS2
XSwhgK6emxKvbXEPl67p++hl/mHgxW4Qng7ba9xjz4Xnz3Z7cU7593Mg9f0rP1E+
sid02+jJHC0MvwHsgwNQif1oMg6qN/0MZGkD/cud/pjBjALChGAAEiHjxectJv4d
tG1x9raN8S9chqnCbrilkASXHwKBgQDDjTmGQtASAfkX8A+QmBAfcS8TqAb85oDN
jsX/g7bDJBaguokWGBZT0G7l5Q4kk5AU3vJPpXxuW2fYrx8HXZL9C0ZkfWtiClTa
45M9zV1t7MC1U+7ogqBC1CBeOHWn6sVLXG8IjMlGzfrjW8GYwE9/i4glbFQ492uD
HphqmPU/6wKBgQCjZ+/7MA2b33LAXxO8Xk9eh+ju71VywAR/T+2qP4gKZaoSeeSR
qRazoBwmrzgXo1E/4y4VXpEmMDONGEMapRZhemNvF67W/l3YbUShzmh596apg86v
tG4VThTRkB4X85MNb90yDm5GYm1Q6TCEkVyfduYkauHIPNv6PA4OsiIPVwKBgBtp
ma3Dge10T1nWsiff2Sq/MA0+WbRsD5RBNmpKKX2TeoSPgZYSTFb1egZKJMBl2yXB
1w/pL9c8gwMyEVRz/p3wTa7akgoNTrXcfxCD0FwPezgwCuaXISYdHGh4261tULju
vTXinniJeWkTvMDP/JTxl2U/mVLfBDg+Orl+tap/AoGBAI1n2bw/4q47caQaADU4
N9nfARvooZ2sq3hX5IlVq1zNEoTGQv8pubkYeJrcnim7o5aZg27L0bckguF7fNkW
BohSIvkq4NMm67ciilPb0BA7IPCblRJvXOuWtLRf3UNytVBKzjPUx8c6YgK2TM4P
QXk3wGld4X8f+KdDkMc3Bppm
-----END PRIVATE KEY-----";

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Router in RS256 mode: the public key is set, NO secret (so the service can't mint tokens).
fn rs256_router() -> Router {
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
        auth_jwt_secret: None, // no shared secret -> the service holds ONLY the public key
        auth_jwt_public_key: Some(PUB_PEM.to_string()),
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
    app(AppState::new(pool, config))
}

fn mint_rs256(claims: &Value) -> String {
    encode(
        &Header::new(Algorithm::RS256),
        claims,
        &EncodingKey::from_rsa_pem(PRIV_PEM.as_bytes()).unwrap(),
    )
    .unwrap()
}

/// An HS256 token forged with the PUBLIC key bytes as the HMAC secret — the RS256->HS256 confusion
/// attack. It must be rejected because the verifier pins RS256.
fn mint_hs256_confusion(claims: &Value) -> String {
    encode(
        &Header::new(Algorithm::HS256),
        claims,
        &EncodingKey::from_secret(PUB_PEM.as_bytes()),
    )
    .unwrap()
}

async fn post(router: &Router, token: Option<&str>, body: Value) -> StatusCode {
    let mut b = Request::builder()
        .method("POST")
        .uri("/retrieve")
        .header("content-type", "application/json");
    if let Some(tok) = token {
        b = b.header("authorization", format!("Bearer {tok}"));
    }
    let resp = router
        .clone()
        .oneshot(b.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    // Drain the body (keeps things tidy; value unused).
    let _ = resp.into_body().collect().await;
    status
}

#[tokio::test]
async fn rs256_verifies_a_signed_token_and_rejects_hs256_confusion() {
    let router = rs256_router();
    // A blank query is rejected 400 AFTER auth + tenant resolution but BEFORE any DB access, so these
    // stay DB-free — a 400 means auth PASSED, a 401 means it was rejected.
    let query = |t: &str, q: &str| json!({ "tenant_id": t, "principal_id": "u", "query": q });

    // (a) a VALID RS256 token (tenant t1) authenticates -> the blank query is a 400 (auth passed).
    let tok = mint_rs256(&json!({ "sub": "u", "tenant": "t1", "exp": unix_now() + 3600 }));
    let s = post(&router, Some(&tok), query("t1", "")).await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "a valid RS256 token authenticates (blank query -> 400)"
    );

    // (b) the RS256->HS256 CONFUSION attack (HS256 signed with the public-key bytes) -> 401.
    let forged = mint_hs256_confusion(
        &json!({ "sub": "attacker", "tenant": "t1", "exp": unix_now() + 3600 }),
    );
    let s = post(&router, Some(&forged), query("t1", "hi")).await;
    assert_eq!(
        s,
        StatusCode::UNAUTHORIZED,
        "an HS256-confusion token is rejected (algorithm pinned to RS256)"
    );

    // (c) a malformed / non-JWT bearer -> 401.
    let s = post(&router, Some("not.a.jwt"), query("t1", "hi")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "a malformed token -> 401");

    // (d) no bearer at all -> 401 (X-* headers are ignored in verified-JWT mode).
    let s = post(&router, None, query("t1", "hi")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "no bearer -> 401");

    // (e) an EXPIRED RS256 token -> 401 (exp still enforced under RS256). Well past jsonwebtoken's
    // default 60s leeway so it is decisively expired.
    let expired = mint_rs256(&json!({ "sub": "u", "tenant": "t1", "exp": unix_now() - 3600 }));
    let s = post(&router, Some(&expired), query("t1", "hi")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "an expired RS256 token -> 401");

    println!("RS256: a signed token authenticates; the RS256->HS256 confusion, malformed, missing, and expired tokens are all 401 — and the service holds only the public key (no secret), so it cannot mint tokens.");
}
