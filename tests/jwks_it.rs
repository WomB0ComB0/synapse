//! Live-router (DB-free) test: RS256 verification against a rotating JWKS endpoint.
//!
//! With `AUTH_JWKS_URL` set (and no static key/secret), the service fetches RS256 PUBLIC keys from a
//! JWKS endpoint and selects the verifying key by the token's `kid`. Keys are cached lazily and
//! refetched (throttled) when an unknown `kid` arrives, so a rotation is picked up automatically. The
//! service holds only public keys, so it CANNOT mint tokens; the algorithm is pinned to RS256.
//!
//! A throwaway local axum server plays the JWKS endpoint: it serves a MUTABLE key set and counts
//! fetches, so the tests can prove (1) a signed token verifies, (2) unknown-`kid`/confusion/malformed
//! /missing/expired/no-`kid` tokens are 401, (3) a rotated-in key is picked up lazily, and (4)
//! unknown-`kid` refetches are throttled. The RSA keypairs below are THROWAWAY (never production).
//! No DB is needed: assertions ride on the auth outcome + the pre-DB blank-query / tenant guards.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::{Json, Router};
use http_body_util::BodyExt;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde_json::{json, Value};
use tower::ServiceExt;

use synapse::config::Config;
use synapse::state::AppState;
use synapse::{app, db};

const KID1: &str = "key-1";
const KID2: &str = "key-2";

// Throwaway keypair #1 (kid = "key-1"): private PEM for minting + the JWK modulus `n` (base64url).
const PRIV1: &str = "-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQDIzGCVvwXHwo3k
JG5oCSNj4Qu6SOOCZs/T2AKJ7BFPmr1mAulpg7CyCawiG3ps2/BiLqnIQuw8lE9x
DLbP3/KT13NYdOUnHzIVRm/DApzpEEppVCAhgwWp068WOHlYjDO5irRpLFJK8v5P
VfEIj3+nCmmCYRSBErvYygOebXK+dsIMae40S+aO4xJrDulgfINSNV2sx4EcXoz2
B5T4GpqdcORIP1MvkeGkmuELKf9Jw7el4AIi0M0peh3eybtTGaxDEbbxrwWJ6YI7
48h2e2bsL2Coy3Xgt4RW4t2yas+e4KlX9x05jFT+GEK/oPXBudIpJJnOUcL+5LRb
Jc93SsjtAgMBAAECggEABTiLKlmVJOSCG/R2im3yZZ5sV6Odhr51mOR87Gke6hrz
4bshpoSuC3ME7r4YKMxvK55a+8IBsnGIvz+9YRpJjF6FuT8Q1juRacwzC7b9rXGm
/aYaT8TAWPIQE1vUi+DZV3GrzzA/04MN6bIqWjag8w7qP2GWzuRVzgUyouPln94a
0QMiJO4mtZa7aLi1bA9LqDR+rix0hrraBxxzITbSEP2iHoigVySL105bkkDVVWfc
qJ6HiScDEjV2WUzK982w4OWBD6ED9hkkhOSR0ma2IrNC3FEcuD0MR0xmLnfi6Cpm
1b+6s/pVgGL4jmu0H0Wm3/A+0fzXs9iVXx8xp3W0YQKBgQDrn+W/sFVgaqHX/x+M
WjdLWmRzKbMSQXuLELWERGmzquAOK+p8W1aOm15lLhfNCd1iuaKDIMfmbGD0gaVS
eDi6zVWjtAb648qIOYXvKc9BCyrzQwxc7P7rpBMeH4yzBUYj4+3UblaIQ0P7jovE
e+Mb2mY3tZDLNf6uBP13A4f7XwKBgQDaKYTozAgBX4f9NTtcTuBNrUbeF6RAtIUl
qEDm/LZrLrRs69gDVUznfXzz9aYW3vgMvONU/XIINTS4ksE24zFd11RraJyvcHi5
Zykoxy8Sb9S3+i63ZaJIuLgW4UernZ0TQJI7BCTATWMo74qYL/A64E2X2IEDszjF
errbqL1rMwKBgQCMxgxq2Tw5DZxCQz+jCCdvEsNe9rPxHURlkocQThtk55tTfDNt
Ntjg/LyJ8N7xdopZOJV6iHRGG8xVaLvQKNmj6ZfX5XAiJ0RS3SNC/5S+xKBVlGJn
hoTLXky5u5nBP05nlP774yw53w5X1hN1QZsvge1+LTEj58+QQpT4rRhqOwKBgQCR
IJ+e8eO9bhyb7+Z+QKZsZgHHyrhkpvIQG/6Y6rI7WQWDk9zOUtdnA461B8wmWMtw
RdOA/Vz3YtWgl1fbOIXlpFIvZZceClb1F1BFJUQGIsjCXrbnH8A2WlN0PQcdfis4
3HKqudXs6040tC1hkjpgIEjd45Pnrzjr/foCGB1yCwKBgFuEG9wN1ym7atRJi8EL
IAE4Id59c2d65T1uTSR5teO8N2vjkALQi2v2D+IDMU8HCd857atMBSjML9Wvjl6E
wprBMjG1IiQVdkAV2vMRH7cXfNcXfwBqh+ynb4T7Nxm2ONnRruGXr2FmTBxVZuDo
wF37O+bF/H5hSx1WiIC2eKQb
-----END PRIVATE KEY-----";
const N1: &str = "yMxglb8Fx8KN5CRuaAkjY-ELukjjgmbP09gCiewRT5q9ZgLpaYOwsgmsIht6bNvwYi6pyELsPJRPcQy2z9_yk9dzWHTlJx8yFUZvwwKc6RBKaVQgIYMFqdOvFjh5WIwzuYq0aSxSSvL-T1XxCI9_pwppgmEUgRK72MoDnm1yvnbCDGnuNEvmjuMSaw7pYHyDUjVdrMeBHF6M9geU-BqanXDkSD9TL5HhpJrhCyn_ScO3peACItDNKXod3sm7UxmsQxG28a8FiemCO-PIdntm7C9gqMt14LeEVuLdsmrPnuCpV_cdOYxU_hhCv6D1wbnSKSSZzlHC_uS0WyXPd0rI7Q";

// Throwaway keypair #2 (kid = "key-2"): used for the rotation + unknown-`kid` cases.
const PRIV2: &str = "-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQCtpE4oeGUicQx7
a08S07OOTqiODusIOA9A1o/dwRNgX7l762T5UlbrqL0W/Zn4H3yd4m1GGUIGJMuU
b+z/XNcSBeMiVXUG4zO2Tb9nk87F4Ljqcd1D+wfauI9Xcx696Mf7fltmsBmL+2xt
wH+/1dS9aa9F6QKPQXjQeA+H4RlMBiQFGwy7dOt5WlNfIvXldCIH5brtvvZvYFCE
BC9X8diiBIMj6k3UMzuxIO47TivpU2FEn026TCGbMGYL4jTB30w16QhcDZZ+GMCf
mzljZzbwo42FZ7AfNMHzGRc1s2IZPwYmfzZV0kzdhpcn42etV0Mpsm3dsC7Wczzm
wTF3mfFNAgMBAAECggEAB6q7qSDLwmFAd/CaILT7qbrPL9qSLFFuJvw/HowuyVbg
rz6Ko+srtSMRs+kSejivSjj1smsqFaOPrTTiv4TymbQ5CuQmeZCtncAoEknRKItb
TteK0zx+l8mfY3EAHspSji6BmNkJf7tKidWk+ywhxROpOz2zF1Uz1IYzyIzUjtBo
Re/TftbRojeOtbFzdZ9JSOhDVeDV7qe59QOPFpENdgGdu85gRetAaR8tAWduhJvn
iI9is5mF75jnu9kktj4Jonco0XYAob+nZsreGCYGAwFRpIJDWSnc/V6Wpu0jJ4/B
Isl0JCSS22ml1lCM0xnRSellTJnmC46qRPau96GGoQKBgQDqgdjoXowTuMBtHT0S
tVeCuPSEzAQxre0R32IJX6t8rtU8HrRSqDM8+n/xlbdZwoQgwzTO6mMYRvtR3Ha9
0wz/3e1XpHQLwiCIGfkgiChFcnCNbE2x3ZYH4dT4qc6Pw+uPS6e7WeZBHnl2Lwgv
iPZ0Cx0fTre5akid91rAKC3tcQKBgQC9jmWEftFtfX12pEKftPXR0Lq/KkTv4nhZ
JQIJ5Yrkl6zjwdm19rjqyK1zG3p3BqG7PYqoKdX8cjiNin2l9rnkVN6fA08UOg43
5gxkg0xXRd5wOpYQiA6pJkLw17G3HuHKuk3ZIc/FgP2SWyeAeBU5Dq/YqEpQZw1Y
KsHu/JcDnQKBgQDQtjAu37cb9lqMwnEQrYTtO1+ksU8qR/mu5nmCjjs6BQCTOWCU
EE9J/kjQ4scEhDLEVfgyEDmR6drTyLuFxsjTENmkHyGJNYVunG81nPj6lhfGRpX/
r49QBJZfmgHVwjFsn5DxFdnwKwc/QCyw4d02+o04x/6MbyOiM/v4+cmmgQKBgCrp
NWoNG3Ph2Kkm/j4RRSS+T8g+1WRIrF3h1thOsmaVP3o/w/1BYRMlYr6QFeUkBzDP
+bef4OVJJixEkbUkaWibHdp5cUlu6xEUbvHCF2IaWwSk/pu3cToxgy3qZjzCLPMr
wbvJv7NCRCUBpaubg5JrFLvDPS9+ZLL02vozDCyxAoGBAIxHaqWnPiqL7Di+ALNW
3xTdAT/rhTO3zbmdkSs+melBbBwXqvgDUhU7SbbzDROOs/JI/pXWVQXROmXgW+7d
xN7dEdji7hZfVxalxqJOcdqoMFTcyR+KZjG8ZgdP8qFe/4GEz6NsQMBsqQW83jqo
7ETulniclCWkFITZR+Q3+mY4
-----END PRIVATE KEY-----";
const N2: &str = "raROKHhlInEMe2tPEtOzjk6ojg7rCDgPQNaP3cETYF-5e-tk-VJW66i9Fv2Z-B98neJtRhlCBiTLlG_s_1zXEgXjIlV1BuMztk2_Z5POxeC46nHdQ_sH2riPV3MevejH-35bZrAZi_tsbcB_v9XUvWmvRekCj0F40HgPh-EZTAYkBRsMu3TreVpTXyL15XQiB-W67b72b2BQhAQvV_HYogSDI-pN1DM7sSDuO04r6VNhRJ9NukwhmzBmC-I0wd9MNekIXA2WfhjAn5s5Y2c28KONhWewHzTB8xkXNbNiGT8GJn82VdJM3YaXJ-NnrVdDKbJt3bAu1nM85sExd5nxTQ";

/// One RSA JWK (`use:sig`, `alg:RS256`) for the given `kid` + modulus.
fn jwk(kid: &str, n: &str) -> Value {
    json!({ "kty": "RSA", "use": "sig", "alg": "RS256", "kid": kid, "n": n, "e": "AQAB" })
}

/// Shared state for the fake JWKS endpoint: a MUTABLE key set + a fetch counter.
#[derive(Clone)]
struct JwksState {
    served: Arc<Mutex<Value>>,
    fetches: Arc<AtomicUsize>,
}

async fn serve_jwks(State(s): State<JwksState>) -> Json<Value> {
    s.fetches.fetch_add(1, Ordering::SeqCst);
    let body = s.served.lock().unwrap().clone();
    Json(body)
}

/// Spawn a throwaway JWKS server on a loopback port; returns its URL + the shared state so a test can
/// rotate the served keys and read the fetch count.
async fn spawn_jwks(initial_keys: Vec<Value>) -> (String, JwksState) {
    let state = JwksState {
        served: Arc::new(Mutex::new(json!({ "keys": initial_keys }))),
        fetches: Arc::new(AtomicUsize::new(0)),
    };
    let router = Router::new()
        .route("/jwks", get(serve_jwks))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://{addr}/jwks"), state)
}

/// Synapse router in JWKS mode: `auth_jwks_url` set, NO static key/secret (so the service can't mint
/// tokens). `min_refetch_secs` is tunable so a test can force or throttle refetches.
fn jwks_router(jwks_url: String, min_refetch_secs: u64) -> Router {
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
        auth_jwks_url: Some(jwks_url),
        auth_jwks_timeout_secs: 5,
        auth_jwks_min_refetch_secs: min_refetch_secs,
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

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Mint an RS256 token signed by `priv_pem` with the given `kid` in the header.
fn mint_rs256(priv_pem: &str, kid: &str, claims: &Value) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(kid.to_string());
    encode(
        &header,
        claims,
        &EncodingKey::from_rsa_pem(priv_pem.as_bytes()).unwrap(),
    )
    .unwrap()
}

/// Mint an RS256 token with NO `kid` header (must be rejected — `kid` is required in JWKS mode).
fn mint_no_kid(priv_pem: &str, claims: &Value) -> String {
    encode(
        &Header::new(Algorithm::RS256),
        claims,
        &EncodingKey::from_rsa_pem(priv_pem.as_bytes()).unwrap(),
    )
    .unwrap()
}

/// An HS256 token carrying a valid `kid` but signed with a symmetric secret — the confusion attempt.
/// Must be rejected because the JWKS verifier pins RS256.
fn mint_hs256_confusion(kid: &str, claims: &Value) -> String {
    let mut header = Header::new(Algorithm::HS256);
    header.kid = Some(kid.to_string());
    encode(&header, claims, &EncodingKey::from_secret(N1.as_bytes())).unwrap()
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
    let _ = resp.into_body().collect().await;
    status
}

fn query(tenant: &str, q: &str) -> Value {
    json!({ "tenant_id": tenant, "principal_id": "u", "query": q })
}

#[tokio::test]
async fn jwks_verifies_a_signed_token_and_rejects_bad_ones() {
    // Serve only key-1; use a normal refetch window.
    let (url, _state) = spawn_jwks(vec![jwk(KID1, N1)]).await;
    let router = jwks_router(url, 60);

    // (a) a VALID RS256 token whose kid=key-1 is served -> auth passes (blank query -> 400).
    let tok = mint_rs256(
        PRIV1,
        KID1,
        &json!({ "sub": "u", "tenant": "t1", "exp": now() + 3600 }),
    );
    let s = post(&router, Some(&tok), query("t1", "")).await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "valid JWKS token authenticates (blank query -> 400)"
    );

    // (b) an UNKNOWN kid (key-2, not in the served set) -> 401.
    let unknown = mint_rs256(
        PRIV2,
        KID2,
        &json!({ "sub": "u", "tenant": "t1", "exp": now() + 3600 }),
    );
    let s = post(&router, Some(&unknown), query("t1", "hi")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "an unknown-kid token is 401");

    // (c) the RS256->HS256 CONFUSION attempt (HS256 with a valid kid) -> 401 (algorithm pinned).
    let forged = mint_hs256_confusion(
        KID1,
        &json!({ "sub": "x", "tenant": "t1", "exp": now() + 3600 }),
    );
    let s = post(&router, Some(&forged), query("t1", "hi")).await;
    assert_eq!(
        s,
        StatusCode::UNAUTHORIZED,
        "an HS256-confusion token is 401"
    );

    // (d) a malformed / non-JWT bearer -> 401.
    let s = post(&router, Some("not.a.jwt"), query("t1", "hi")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "a malformed token is 401");

    // (e) no bearer at all -> 401 (X-* headers ignored in verified-JWT mode).
    let s = post(&router, None, query("t1", "hi")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "no bearer is 401");

    // (f) an EXPIRED token with a valid kid -> 401 (exp enforced; well past the 60s leeway).
    let expired = mint_rs256(
        PRIV1,
        KID1,
        &json!({ "sub": "u", "tenant": "t1", "exp": now() - 3600 }),
    );
    let s = post(&router, Some(&expired), query("t1", "hi")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "an expired token is 401");

    // (g) a valid signature but NO kid header -> 401 (kid is required to select a JWKS key).
    let no_kid = mint_no_kid(
        PRIV1,
        &json!({ "sub": "u", "tenant": "t1", "exp": now() + 3600 }),
    );
    let s = post(&router, Some(&no_kid), query("t1", "hi")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "a token with no kid is 401");
}

#[tokio::test]
async fn jwks_picks_up_a_rotated_key_lazily() {
    // min_refetch = 0 so every unknown-kid request may refetch. Serve only key-1 initially.
    let (url, state) = spawn_jwks(vec![jwk(KID1, N1)]).await;
    let router = jwks_router(url, 0);

    let tok2 = mint_rs256(
        PRIV2,
        KID2,
        &json!({ "sub": "u", "tenant": "t1", "exp": now() + 3600 }),
    );

    // key-2 isn't served yet -> unknown kid -> 401 (a refetch happens but still can't find it).
    let s = post(&router, Some(&tok2), query("t1", "")).await;
    assert_eq!(
        s,
        StatusCode::UNAUTHORIZED,
        "before rotation, key-2 is unknown -> 401"
    );

    // Rotate: the endpoint now serves BOTH keys.
    *state.served.lock().unwrap() = json!({ "keys": [jwk(KID1, N1), jwk(KID2, N2)] });

    // The next key-2 request refetches (min_refetch=0), picks up the rotated-in key, and authenticates.
    let s = post(&router, Some(&tok2), query("t1", "")).await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "after rotation, key-2 verifies (blank query -> 400)"
    );
}

#[tokio::test]
async fn jwks_throttles_unknown_kid_refetches() {
    // Huge refetch window: after the first fetch, an unknown kid must NOT trigger another fetch.
    let (url, state) = spawn_jwks(vec![jwk(KID1, N1)]).await;
    let router = jwks_router(url, 3600);

    // First request (key-1) triggers the initial fetch and authenticates.
    let tok1 = mint_rs256(
        PRIV1,
        KID1,
        &json!({ "sub": "u", "tenant": "t1", "exp": now() + 3600 }),
    );
    let s = post(&router, Some(&tok1), query("t1", "")).await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "key-1 authenticates (blank query -> 400)"
    );
    assert_eq!(
        state.fetches.load(Ordering::SeqCst),
        1,
        "exactly one fetch so far"
    );

    // An unknown kid within the throttle window is rejected WITHOUT a second fetch.
    let tok2 = mint_rs256(
        PRIV2,
        KID2,
        &json!({ "sub": "u", "tenant": "t1", "exp": now() + 3600 }),
    );
    let s = post(&router, Some(&tok2), query("t1", "hi")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "unknown kid -> 401");
    assert_eq!(
        state.fetches.load(Ordering::SeqCst),
        1,
        "the unknown-kid request is throttled (no second fetch)"
    );
}
