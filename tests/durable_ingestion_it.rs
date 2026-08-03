//! DB-gated integration test for durable document embedding recovery.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use common::{app_pool, apply_schema, post_json, TestDb};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;

use synapse::config::{Config, EmbeddingProvider};
use synapse::ingestion;
use synapse::retrieval::embed::{EmbedderImpl, OpenAiEmbedder};
use synapse::retrieval::EMBEDDING_DIM;
use synapse::state::AppState;

fn config(database_url: &str, embedding_base_url: String) -> Config {
    Config {
        production_mode: false,
        database_url: database_url.to_string(),
        bind_addr: "0.0.0.0:8080".to_string(),
        db_max_connections: 20,
        db_acquire_timeout_secs: 10,
        max_request_body_bytes: 16 * 1024 * 1024,
        request_timeout_secs: 180,
        max_in_flight_requests: 256,
        embedding_model: "test-embedding-model".to_string(),
        embedding_provider: EmbeddingProvider::OpenAi,
        openai_api_key: Some("test-key".to_string()),
        embedding_base_url,
        embedding_max_batch: 96,
        embedding_timeout_secs: 2,
        embedding_max_retries: 0,
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
        embedding_model_consistency: true,
        retrieval_mmr_lambda: 0.5,
        rate_limit_enabled: false,
        rate_limit_tenant_rps: 10.0,
        rate_limit_burst: 20.0,
        ingest_idempotency_enabled: true,
        mcp_endpoint: None,
        mcp_auth_token: None,
        mcp_auth_token_file: None,
        mcp_scopes: Vec::new(),
        mcp_allowed_hosts: Vec::new(),
        mcp_timeout_secs: 30,
        mcp_max_retries: 2,
        worker_enabled: true,
        worker_poll_secs: 30,
        worker_stale_secs: 300,
    }
}

async fn embeddings(
    State(failing): State<Arc<AtomicBool>>,
    Json(request): Json<Value>,
) -> (StatusCode, Json<Value>) {
    if failing.load(Ordering::SeqCst) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "simulated outage"})),
        );
    }
    let count = request["input"].as_array().map_or(0, Vec::len);
    let data: Vec<Value> = (0..count)
        .map(|index| {
            json!({
                "index": index,
                "embedding": vec![0.01_f32; EMBEDDING_DIM]
            })
        })
        .collect();
    (StatusCode::OK, Json(json!({"data": data})))
}

#[tokio::test]
async fn provider_outage_persists_lexical_data_then_worker_recovers_vectors() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset; skipping DB-gated integration test");
        return;
    };

    let failing = Arc::new(AtomicBool::new(true));
    let provider = Router::new()
        .route("/embeddings", post(embeddings))
        .with_state(failing.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind provider");
    let address = listener.local_addr().expect("provider address");
    let provider_task = tokio::spawn(async move {
        axum::serve(listener, provider)
            .await
            .expect("serve provider");
    });

    let test_db = TestDb::create(&base_url, "durable_ingestion").await;
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&test_db.url)
        .await
        .expect("connect admin");
    apply_schema(&admin, &test_db.role).await;

    let provider_url = format!("http://{address}");
    let config = config(&test_db.url, provider_url.clone());
    let router = synapse::app(AppState::new(
        app_pool(&test_db.url, &test_db.role).await,
        config,
    ));
    let (status, response) = post_json(
        &router,
        "/documents.ingest",
        "tenant_a",
        "agent",
        json!({
            "doc_id": "outage-doc",
            "tenant_id": "tenant_a",
            "principal_id": "agent",
            "source_uri": "test://outage-doc",
            "content": "database recovery runbook keeps lexical search available"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["status"], "queued");

    let state: (String, String, i64, i64) = sqlx::query_as(
        "SELECT ingestion_status, content, \
                (SELECT count(*) FROM chunks c WHERE c.tenant_id = d.tenant_id \
                 AND c.doc_id = d.doc_id), \
                (SELECT count(*) FROM chunks c WHERE c.tenant_id = d.tenant_id \
                 AND c.doc_id = d.doc_id AND c.embedding IS NOT NULL) \
         FROM documents d WHERE tenant_id = 'tenant_a' AND doc_id = 'outage-doc'",
    )
    .fetch_one(&admin)
    .await
    .expect("durable document state");
    assert_eq!(state.0, "retry");
    assert_eq!(
        state.1,
        "database recovery runbook keeps lexical search available"
    );
    assert!(state.2 > 0);
    assert_eq!(state.3, 0);

    let (status, lexical) = post_json(
        &router,
        "/retrieve",
        "tenant_a",
        "agent",
        json!({
            "tenant_id": "tenant_a",
            "principal_id": "agent",
            "query": "database recovery runbook",
            "retrieval": {"mode": "lexical", "top_k": 5}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{lexical}");
    assert_eq!(lexical["results"][0]["doc_id"], "outage-doc");

    failing.store(false, Ordering::SeqCst);
    sqlx::query(
        "UPDATE documents SET next_embedding_attempt_at = now() \
         WHERE tenant_id = 'tenant_a' AND doc_id = 'outage-doc'",
    )
    .execute(&admin)
    .await
    .expect("make retry due");
    let worker_pool = app_pool(&test_db.url, &test_db.role).await;
    let embedder = EmbedderImpl::OpenAi(OpenAiEmbedder::new(
        provider_url,
        "test-key".to_string(),
        "test-embedding-model".to_string(),
        EMBEDDING_DIM,
        96,
        0,
        2,
    ));
    let processed =
        ingestion::reconcile_embedding_jobs(&worker_pool, &embedder, "test-embedding-model", 300)
            .await
            .expect("reconcile embedding jobs");
    assert_eq!(processed, 1);

    let recovered: (String, i64) = sqlx::query_as(
        "SELECT ingestion_status, \
                (SELECT count(*) FROM chunks c WHERE c.tenant_id = d.tenant_id \
                 AND c.doc_id = d.doc_id AND c.embedding IS NOT NULL) \
         FROM documents d WHERE tenant_id = 'tenant_a' AND doc_id = 'outage-doc'",
    )
    .fetch_one(&admin)
    .await
    .expect("recovered document state");
    assert_eq!(recovered.0, "ready");
    assert_eq!(recovered.1, state.2);

    provider_task.abort();
}
