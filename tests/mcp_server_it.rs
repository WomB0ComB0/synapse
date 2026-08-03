//! DB-free protocol tests for Synapse's inbound MCP Streamable HTTP endpoint.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use synapse::config::{Config, EmbeddingProvider};
use synapse::state::AppState;
use tower::ServiceExt;

fn router(max_body: usize) -> axum::Router {
    let config = Config {
        production_mode: false,
        database_url: "postgres://synapse:synapse@localhost:5432/synapse".to_string(),
        bind_addr: "127.0.0.1:8080".to_string(),
        db_max_connections: 2,
        db_acquire_timeout_secs: 1,
        max_request_body_bytes: max_body,
        request_timeout_secs: 5,
        max_in_flight_requests: 8,
        embedding_model: "text-embedding-3-small".to_string(),
        embedding_provider: EmbeddingProvider::Mock,
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
    let pool = synapse::db::init(&config).expect("lazy pool");
    synapse::app(AppState::new(pool, config))
}

async fn post(
    app: &axum::Router,
    body: Value,
    extra_headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("content-type", "application/json")
        .header("x-principal-id", "agent-1")
        .header("x-tenant-id", "tenant-a");
    for (name, value) in extra_headers {
        request = request.header(*name, *value);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    (status, value)
}

#[tokio::test]
async fn initializes_and_lists_deterministic_tools() {
    let app = router(1024 * 1024);
    let (status, response) = post(
        &app,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "test-agent", "version": "1"}
            }
        }),
        &[("mcp-method", "initialize")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(response["result"]["serverInfo"]["name"], "synapse");
    assert_eq!(
        response["result"]["capabilities"]["tools"]["listChanged"],
        false
    );

    let (status, response) = post(
        &app,
        json!({"jsonrpc": "2.0", "id": "tools", "method": "tools/list"}),
        &[("mcp-method", "tools/list")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = response["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(names.len(), 14);
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "tools/list must be deterministic");
    assert!(names.contains(&"synapse_retrieve"));
    assert!(names.contains(&"synapse_ingest_document"));
    assert!(names.contains(&"synapse_reembed_document"));
    assert!(names.contains(&"synapse_execute_tool"));
    assert!(names.contains(&"synapse_register_tool"));
    assert!(names.contains(&"synapse_list_tools"));
    assert!(names.contains(&"synapse_decide_tool"));
    assert!(names.contains(&"synapse_rollback_tool"));
}

#[tokio::test]
async fn tool_errors_use_mcp_result_without_touching_the_database() {
    let app = router(1024 * 1024);
    let (status, response) = post(
        &app,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "synapse_retrieve", "arguments": {"query": "   "}}
        }),
        &[
            ("mcp-method", "tools/call"),
            ("mcp-name", "synapse_retrieve"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response["result"]["isError"], true);
    assert!(response["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("query is required"));
}

#[tokio::test]
async fn rejects_origin_header_mismatches_and_unsupported_versions() {
    let app = router(1024 * 1024);
    let request = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {"name": "synapse_retrieve", "arguments": {"query": "x"}}
    });

    let (status, _) = post(&app, request.clone(), &[("origin", "https://evil.example")]).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, response) = post(
        &app,
        request.clone(),
        &[
            ("mcp-method", "tools/list"),
            ("mcp-name", "synapse_retrieve"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(response["error"]["code"], -32600);

    let (status, response) = post(&app, request, &[("mcp-protocol-version", "2099-01-01")]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(response["error"]["code"], -32600);
}

#[tokio::test]
async fn enforces_global_body_limit_and_post_only_transport() {
    let app = router(128);
    let oversized = json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "ping",
        "padding": "x".repeat(512)
    });
    let (status, _) = post(&app, oversized, &[]).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/mcp")
                .header("x-principal-id", "agent-1")
                .header("x-tenant-id", "tenant-a")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(response.headers()["allow"], "POST");
}
