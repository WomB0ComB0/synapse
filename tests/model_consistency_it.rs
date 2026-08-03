//! Live integration test: opt-in embedding-model consistency at retrieval.
//!
//! With `EMBEDDING_MODEL_CONSISTENCY` on, the VECTOR arm only cosine-compares chunks embedded by
//! the SAME model as the query, so a model change never silently compares across embedding
//! spaces. The LEXICAL arm is model-agnostic and stays unfiltered. Off = the pre-existing
//! behavior (any stored vector is compared).
//!
//! **DB-gated:** skipped unless `DATABASE_URL` is set. Run locally:
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=postgres -p 5470:5432 pgvector/pgvector:pg16
//! export DATABASE_URL=postgres://postgres:postgres@localhost:5470/postgres
//! cargo test --test model_consistency_it -- --nocapture
//! ```

mod common;

use axum::http::StatusCode;
use axum::Router;
use common::{app_pool, apply_schema, post_json, TestDb};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;

use synapse::config::Config;
use synapse::state::AppState;

fn cfg(database_url: &str, model: &str, consistency: bool) -> Config {
    Config {
        production_mode: false,
        database_url: database_url.to_string(),
        bind_addr: "0.0.0.0:8080".to_string(),
        db_max_connections: 20,
        db_acquire_timeout_secs: 10,
        max_request_body_bytes: 16 * 1024 * 1024,
        request_timeout_secs: 180,
        max_in_flight_requests: 256,
        embedding_model: model.to_string(),
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
        embedding_model_consistency: consistency,
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

async fn router(database_url: &str, role: &str, model: &str, consistency: bool) -> Router {
    let pool = app_pool(database_url, role).await;
    synapse::app(AppState::new(pool, cfg(database_url, model, consistency)))
}

async fn ingest(router: &Router, tenant: &str, doc_id: &str, content: &str) {
    let body = json!({
        "doc_id": doc_id,
        "tenant_id": tenant,
        "principal_id": "user_1",
        "team_scope": ["eng"],
        "source_uri": format!("uri://{doc_id}"),
        "content": content,
    });
    let (status, val) = post_json(router, "/documents.ingest", tenant, "user_1", body).await;
    assert_eq!(status, StatusCode::OK, "ingest {doc_id} failed: {val}");
}

fn n_results(resp: &Value) -> usize {
    resp["results"].as_array().map(|a| a.len()).unwrap_or(0)
}

fn vector_query(tenant: &str, q: &str) -> Value {
    json!({
        "tenant_id": tenant, "principal_id": "user_1", "query": q,
        "retrieval": { "mode": "vector", "top_k": 5 }
    })
}

#[tokio::test]
async fn vector_arm_filters_by_embedding_model_when_consistency_on() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };
    let t = "tenant_a";

    let test_db = TestDb::create(&base_url, "modelconsistency").await;
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&test_db.url)
        .await
        .expect("connect admin");
    apply_schema(&admin, &test_db.role).await;

    // Ingest with the DEFAULT model — the chunk rows are stamped embedding_model='text-embedding-3-small'.
    let ingest_router = router(&test_db.url, &test_db.role, "text-embedding-3-small", false).await;
    ingest(
        &ingest_router,
        t,
        "doc-1",
        "the quarterly database outage runbook and incident review",
    )
    .await;

    // Retrieve routers over the SAME db: flag off / on with the same model / on with a different model.
    let same_off = router(&test_db.url, &test_db.role, "text-embedding-3-small", false).await;
    let same_on = router(&test_db.url, &test_db.role, "text-embedding-3-small", true).await;
    let other_on = router(&test_db.url, &test_db.role, "text-embedding-3-large", true).await;

    // (a) flag OFF → the vector arm compares regardless of model (returns the chunk).
    let (s, r) = post_json(
        &same_off,
        "/retrieve",
        t,
        "user_1",
        vector_query(t, "database outage"),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "retrieve: {r}");
    assert!(
        n_results(&r) >= 1,
        "flag off: the vector arm returns results: {r}"
    );

    // (b) flag ON + SAME model → returns (same-model comparison).
    let (_, r) = post_json(
        &same_on,
        "/retrieve",
        t,
        "user_1",
        vector_query(t, "database outage"),
    )
    .await;
    assert!(n_results(&r) >= 1, "same model returns: {r}");

    // (c) flag ON + DIFFERENT model → the vector arm is EMPTY (cross-model chunks filtered out).
    let (_, r) = post_json(
        &other_on,
        "/retrieve",
        t,
        "user_1",
        vector_query(t, "database outage"),
    )
    .await;
    assert_eq!(
        n_results(&r),
        0,
        "a different-model query filters out the chunks: {r}"
    );
    println!("(a-c) the vector arm compares only same-model chunks when consistency is on (off = compare all)");

    // (d) flag ON + DIFFERENT model + LEXICAL mode → still returns (the lexical arm is model-agnostic).
    let lexical = json!({
        "tenant_id": t, "principal_id": "user_1", "query": "database outage runbook",
        "retrieval": { "mode": "lexical", "top_k": 5 }
    });
    let (_, r) = post_json(&other_on, "/retrieve", t, "user_1", lexical).await;
    assert!(
        n_results(&r) >= 1,
        "the lexical arm is NOT filtered by model: {r}"
    );
    println!("(d) the lexical arm stays model-agnostic (a stale-model chunk via FTS is not a distance-validity bug)");

    admin.close().await;
    println!("model consistency: the vector arm compares only same-model embeddings when EMBEDDING_MODEL_CONSISTENCY is on; the lexical arm is unaffected; off preserves the pre-existing behavior.");
}
