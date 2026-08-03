//! Live integration test: MMR (Maximal-Marginal-Relevance) diversity re-ranking end to end.
//!
//! Seeds two NEAR-DUPLICATE chunks (identical text but for the last word → near-identical MockEmbedder
//! vectors) plus one DISTINCT chunk, all in the candidate pool. A `vector`-mode `top_k=2` query ranks
//! the two near-duplicates above the distinct chunk by pure relevance, so:
//!   - `mmr:false` returns BOTH near-duplicates (the distinct chunk is crowded out);
//!   - `mmr:true` (λ=0.2, diversity-weighted) demotes the second near-duplicate below the distinct
//!     chunk, so the distinct chunk appears in the top_2 — proving the diversity re-rank + that
//!     candidate embeddings are actually materialized and used.
//!
//! Uses the deterministic MockEmbedder (openai key unset), whose embedding is the L2-normalized
//! position-aligned byte vector of the text — so two texts differing only in a trailing word are
//! near-identical, and a structurally different text is dissimilar.
//!
//! **DB-gated:** skipped unless `DATABASE_URL` is set. Run locally:
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=postgres -p 5481:5432 pgvector/pgvector:pg16
//! export DATABASE_URL=postgres://postgres:postgres@localhost:5481/postgres
//! cargo test --test mmr_it -- --nocapture
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
    synapse::app(AppState::new(pool, config))
}

async fn post_json(
    router: &Router,
    uri: &str,
    tenant: &str,
    principal: &str,
    body: Value,
) -> (StatusCode, Value) {
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .header("x-principal-id", principal)
                .header("x-tenant-id", tenant)
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
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

async fn ingest(router: &Router, tenant: &str, doc_id: &str, content: &str) {
    let body = json!({
        "doc_id": doc_id,
        "tenant_id": tenant,
        "team_scope": ["eng"],
        "source_uri": format!("uri://{doc_id}"),
        "content": content,
    });
    let (status, val) = post_json(router, "/documents.ingest", tenant, "user_1", body).await;
    assert_eq!(status, StatusCode::OK, "ingest {doc_id} failed: {val}");
    assert!(
        val["chunks_ingested"].as_u64().unwrap() >= 1,
        "expected >=1 chunk for {doc_id}: {val}"
    );
}

fn doc_ids(resp: &Value) -> Vec<String> {
    resp["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["doc_id"].as_str().unwrap().to_string())
        .collect()
}

async fn retrieve(router: &Router, tenant: &str, query: &str, mmr: bool) -> Vec<String> {
    let mut retrieval = json!({ "mode": "vector", "top_k": 2 });
    if mmr {
        retrieval["mmr"] = json!(true);
        // Strong diversity weight so the near-duplicate is clearly demoted below the distinct chunk.
        retrieval["mmr_lambda"] = json!(0.2);
    }
    let (status, resp) = post_json(
        router,
        "/retrieve",
        tenant,
        "user_1",
        json!({ "tenant_id": tenant, "principal_id": "user_1", "query": query, "retrieval": retrieval }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "retrieve failed: {resp}");
    doc_ids(&resp)
}

#[tokio::test]
async fn mmr_diversifies_near_duplicates() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };
    let t = "tenant_a";

    let test_db = TestDb::create(&base_url, "mmr").await;
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&test_db.url)
        .await
        .expect("connect admin");
    apply_schema(&admin, &test_db.role).await;
    let router = router_for(app_pool(&test_db.url, &test_db.role).await, &test_db.url);

    // Two near-duplicates (identical but for the trailing word) + one distinct chunk. NOTE: the
    // MockEmbedder is a position-aligned byte vector, so a much SHORTER text is the reliable way to
    // get a low cosine to the long near-duplicates (same-length English texts share a large positive
    // DC offset and are all fairly similar under this mock — real embeddings have meaningful cosine).
    let base =
        "database failover incident recovery runbook procedure steps alpha alpha alpha alpha";
    ingest(&router, t, "dup_one", &format!("{base} one")).await;
    ingest(&router, t, "dup_two", &format!("{base} two")).await;
    ingest(&router, t, "distinct", "office supplies list").await;

    // Query close to the near-duplicates, so both outrank the distinct chunk by pure relevance.
    let query = format!("{base} zzz");

    // Without MMR: the top-2 are the two near-duplicates; the distinct chunk is crowded out.
    let plain = retrieve(&router, t, &query, false).await;
    assert_eq!(plain.len(), 2, "top_k=2");
    assert!(
        plain.contains(&"dup_one".to_string()) && plain.contains(&"dup_two".to_string()),
        "without MMR the top-2 are both near-duplicates, got {plain:?}"
    );
    assert!(
        !plain.contains(&"distinct".to_string()),
        "without MMR the distinct chunk is crowded out, got {plain:?}"
    );

    // With MMR: the second near-duplicate is demoted for diversity, so the distinct chunk appears.
    let diverse = retrieve(&router, t, &query, true).await;
    assert_eq!(diverse.len(), 2, "top_k=2");
    assert!(
        diverse.contains(&"distinct".to_string()),
        "with MMR the distinct chunk is promoted into the top-2, got {diverse:?}"
    );
    let dups_in_diverse = diverse.iter().filter(|id| id.starts_with("dup_")).count();
    assert_eq!(
        dups_in_diverse, 1,
        "with MMR only ONE of the near-duplicates survives, got {diverse:?}"
    );

    admin.close().await;
    println!("mmr: without MMR top-2 = both near-duplicates; with MMR the second near-duplicate is demoted for the distinct chunk.");
}
