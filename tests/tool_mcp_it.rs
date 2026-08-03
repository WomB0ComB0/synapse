//! Live integration test for the real MCP tool connector.
//!
//! A tiny loopback axum server stands in for an MCP JSON-RPC endpoint. With
//! `MCP_ENDPOINT` configured, an auto-approved `/tool.execute` actually POSTs a
//! `tools/call` to it and records the real result; an approval-gated call stays
//! `pending` WITHOUT calling the connector; a tool/connector fault is a 502 and a
//! `failed` row. The external call runs OUTSIDE the DB transaction.
//!
//! **DB-gated:** skipped unless `DATABASE_URL` is set. Run locally:
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=postgres -p 5462:5432 pgvector/pgvector:pg16
//! export DATABASE_URL=postgres://postgres:postgres@localhost:5462/postgres
//! cargo test --test tool_mcp_it -- --nocapture
//! ```

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use common::{app_pool, apply_schema, post_json, seed_tool_definition, TestDb};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

use synapse::config::Config;
use synapse::state::AppState;

/// Mock MCP JSON-RPC endpoint state.
#[derive(Clone)]
struct MockState {
    calls: Arc<AtomicUsize>,
    last_body: Arc<Mutex<Option<Value>>>,
    /// When true, reply with a JSON-RPC `error` (a tool failure).
    error: bool,
}

async fn tools_call(
    State(st): State<MockState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    st.calls.fetch_add(1, Ordering::SeqCst);
    *st.last_body.lock().unwrap() = Some(body.clone());
    assert_eq!(
        headers
            .get(header::ACCEPT)
            .and_then(|value| value.to_str().ok()),
        Some("application/json, text/event-stream")
    );
    assert_eq!(
        headers
            .get("mcp-method")
            .and_then(|value| value.to_str().ok()),
        Some("tools/call")
    );
    assert_eq!(
        headers
            .get("mcp-name")
            .and_then(|value| value.to_str().ok()),
        body["params"]["name"].as_str()
    );
    assert!(headers.contains_key("idempotency-key"));

    let payload = if st.error {
        json!({
            "jsonrpc": "2.0", "id": 1,
            "error": { "code": -32000, "message": "tool blew up" }
        })
    } else {
        // Echo the arguments back inside a well-formed tools/call result.
        let args = body["params"]["arguments"].clone();
        json!({
            "jsonrpc": "2.0", "id": 1,
            "result": { "content": [{ "type": "text", "text": "ok" }], "echo": args, "isError": false }
        })
    };
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        format!("event: message\ndata: {payload}\n\n"),
    )
        .into_response()
}

/// Spawn the mock on an ephemeral loopback port; return (url, call-counter, last-body).
async fn spawn_mock(error: bool) -> (String, Arc<AtomicUsize>, Arc<Mutex<Option<Value>>>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let last_body = Arc::new(Mutex::new(None));
    let st = MockState {
        calls: calls.clone(),
        last_body: last_body.clone(),
        error,
    };
    let app = Router::new().route("/", post(tools_call)).with_state(st);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), calls, last_body)
}

fn cfg(database_url: &str, mcp_endpoint: Option<String>) -> Config {
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
        abac_context_ownership: false,
        embedding_model_consistency: false,
        retrieval_mmr_lambda: 0.5,
        rate_limit_enabled: false,
        rate_limit_tenant_rps: 10.0,
        rate_limit_burst: 20.0,
        ingest_idempotency_enabled: false,
        mcp_endpoint,
        mcp_auth_token: None,
        mcp_auth_token_file: None,
        mcp_scopes: Vec::new(),
        mcp_allowed_hosts: Vec::new(),
        mcp_timeout_secs: 5,
        mcp_max_retries: 1,
        worker_enabled: false,
        worker_poll_secs: 30,
        worker_stale_secs: 300,
    }
}

/// Provision a throwaway DB + a router whose tool connector targets `mcp_endpoint`.
async fn setup_mcp(
    base_url: &str,
    slug: &str,
    mcp_endpoint: Option<String>,
    tool_ids: &[&str],
) -> (Router, PgPool) {
    let test_db = TestDb::create(base_url, slug).await;
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&test_db.url)
        .await
        .expect("connect admin");
    apply_schema(&admin, &test_db.role).await;
    for tool_id in tool_ids {
        seed_tool_definition(&admin, "tenant_a", tool_id).await;
    }
    let pool = app_pool(&test_db.url, &test_db.role).await;
    let router = synapse::app(AppState::new(pool, cfg(&test_db.url, mcp_endpoint)));
    (router, admin)
}

#[tokio::test]
async fn mcp_connector_executes_tools() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };
    let t = "tenant_a";

    // --- (a) an auto-approved tool call actually hits the MCP endpoint + returns its result ---
    let (mcp_url, calls, last_body) = spawn_mock(false).await;
    let (router, admin) = setup_mcp(&base_url, "toolmcp", Some(mcp_url), &["echo", "gated"]).await;
    let (s, r) = post_json(
        &router,
        "/tool.execute",
        t,
        "agent",
        json!({"tenant_id": t, "principal_id": "agent", "tool_id": "echo", "arguments": {"x": 1}}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "auto tool executed: {r}");
    assert_eq!(r["status"], "executed");
    assert_eq!(r["requires_approval"], false);
    assert_eq!(
        r["output"]["echo"]["x"], 1,
        "connector returned the tool's real result"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the MCP endpoint was called once"
    );
    // The mock received an MCP tools/call for this tool.
    let body = last_body.lock().unwrap().clone().unwrap();
    assert_eq!(body["method"], "tools/call");
    assert_eq!(body["params"]["name"], "echo");
    // The executed row carries the real result (queried as the RLS-bypassing superuser).
    let (status, result): (String, Value) = sqlx::query_as(
        "SELECT status, result FROM tool_executions WHERE tenant_id = $1 AND tool_id = 'echo'",
    )
    .bind(t)
    .fetch_one(&admin)
    .await
    .unwrap();
    assert_eq!(status, "executed");
    assert_eq!(result["echo"]["x"], 1);
    println!("(a) an auto tool call executes via the MCP connector + records the real result");

    // --- (b) an approval-gated tool -> pending, connector NOT called ---
    let (s, r) = post_json(
        &router,
        "/tool.execute",
        t,
        "agent",
        json!({"tenant_id": t, "principal_id": "agent", "tool_id": "gated", "arguments": {},
               "policy": {"approval_mode": "required"}}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r["status"], "pending");
    assert_eq!(r["requires_approval"], true);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "an approval-gated call did NOT invoke the connector"
    );
    admin.close().await;
    println!("(b) an approval-gated call is pending and does NOT call the connector");

    // --- (c) a tool/connector error -> 502 + a `failed` row ---
    let (mcp_url_err, _c, _b) = spawn_mock(true).await;
    let (router_err, admin_err) =
        setup_mcp(&base_url, "toolmcperr", Some(mcp_url_err), &["boom"]).await;
    let (s, _r) = post_json(
        &router_err,
        "/tool.execute",
        t,
        "agent",
        json!({"tenant_id": t, "principal_id": "agent", "tool_id": "boom", "arguments": {}}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_GATEWAY, "connector/tool error -> 502");
    let (status,): (String,) = sqlx::query_as(
        "SELECT status FROM tool_executions WHERE tenant_id = $1 AND tool_id = 'boom'",
    )
    .bind(t)
    .fetch_one(&admin_err)
    .await
    .unwrap();
    assert_eq!(status, "failed", "the failed execution is recorded durably");
    admin_err.close().await;
    println!("(c) a connector/tool error -> 502 and a 'failed' tool_executions row");

    println!("MCP connector: auto calls execute against the endpoint (real result recorded); approval-gated stays pending; failures -> 502 + failed row.");
}
