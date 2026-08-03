//! Live integration test: an AUTO tool step inside a run actually EXECUTES against the MCP
//! connector (executor PR4 drive-loop). A mock MCP endpoint stands in for the tool; the run
//! is driven to completion with the real result, and a connector failure fails the run.
//!
//! **DB-gated:** skipped unless `DATABASE_URL` is set. Run locally:
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=postgres -p 5463:5432 pgvector/pgvector:pg16
//! export DATABASE_URL=postgres://postgres:postgres@localhost:5463/postgres
//! cargo test --test run_tool_exec_it -- --nocapture
//! ```

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use common::{app_pool, apply_schema, post_json, seed_tool_definition, TestDb};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

use synapse::config::Config;
use synapse::state::AppState;

#[derive(Clone)]
struct MockState {
    calls: Arc<AtomicUsize>,
    last_body: Arc<Mutex<Option<Value>>>,
    error: bool,
}

async fn tools_call(State(st): State<MockState>, Json(body): Json<Value>) -> Response {
    st.calls.fetch_add(1, Ordering::SeqCst);
    *st.last_body.lock().unwrap() = Some(body.clone());
    if st.error {
        return Json(
            json!({"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"tool blew up"}}),
        )
        .into_response();
    }
    let args = body["params"]["arguments"].clone();
    Json(json!({"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"ok"}],"echo":args,"isError":false}}))
        .into_response()
}

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
async fn run_executes_auto_tool_step_via_connector() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };
    let t = "tenant_a";

    // --- (a) a `tool.auto` run drives its auto tool step through the connector for real ---
    let (mcp_url, calls, last_body) = spawn_mock(false).await;
    let (router, admin) = setup_mcp(&base_url, "runtoolexec", Some(mcp_url), &["echo"]).await;
    let (s, r) = post_json(
        &router,
        "/runs.start",
        t,
        "agent",
        json!({"tenant_id": t, "run_type": "tool.auto"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "start tool.auto: {r}");
    assert_eq!(
        r["status"], "completed",
        "run drove to completion via the connector"
    );
    let run_id = r["run_id"].as_str().unwrap().to_string();
    // The connector was actually called with the step's tool_id + arguments.
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the MCP endpoint was called once"
    );
    let body = last_body.lock().unwrap().clone().unwrap();
    assert_eq!(body["params"]["name"], "echo");
    assert_eq!(body["params"]["arguments"]["msg"], "hello");
    // The tool_executions row for the run carries the REAL result (not a placeholder).
    let (te_status, te_result): (String, Value) =
        sqlx::query_as("SELECT status, result FROM tool_executions WHERE run_id = $1::uuid")
            .bind(&run_id)
            .fetch_one(&admin)
            .await
            .unwrap();
    assert_eq!(te_status, "executed");
    assert_eq!(
        te_result["echo"]["msg"], "hello",
        "the real connector result was recorded"
    );
    // The run step completed with the tool output.
    let (step_status,): (String,) =
        sqlx::query_as("SELECT status FROM run_steps WHERE run_id = $1::uuid AND kind = 'tool'")
            .bind(&run_id)
            .fetch_one(&admin)
            .await
            .unwrap();
    assert_eq!(step_status, "completed");
    admin.close().await;
    println!("(a) an auto tool step inside a run executes against the connector + records the real result");

    // --- (b) a connector/tool failure fails the run ---
    let (mcp_url_err, _c, _b) = spawn_mock(true).await;
    let (router_err, admin_err) =
        setup_mcp(&base_url, "runtoolexecerr", Some(mcp_url_err), &["echo"]).await;
    let (s, r) = post_json(
        &router_err,
        "/runs.start",
        t,
        "agent",
        json!({"tenant_id": t, "run_type": "tool.auto"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "start (err) returns the run: {r}");
    assert_eq!(r["status"], "failed", "a connector failure fails the run");
    let run_id_err = r["run_id"].as_str().unwrap().to_string();
    let (te_status,): (String,) =
        sqlx::query_as("SELECT status FROM tool_executions WHERE run_id = $1::uuid")
            .bind(&run_id_err)
            .fetch_one(&admin_err)
            .await
            .unwrap();
    assert_eq!(
        te_status, "failed",
        "the failed in-run execution is recorded"
    );
    admin_err.close().await;
    println!("(b) a connector/tool failure fails the run + records a 'failed' tool_executions row");

    // --- (c) an APPROVAL-GATED tool executes for real on RESUME (not a placeholder) ---
    let (mcp_url_g, calls_g, last_body_g) = spawn_mock(false).await;
    let (router_g, admin_g) =
        setup_mcp(&base_url, "runtoolgated", Some(mcp_url_g), &["deploy.prod"]).await;
    // start tool.gated -> `prepare` completes, the gated `deploy` tool suspends `waiting`.
    let (s, r) = post_json(
        &router_g,
        "/runs.start",
        t,
        "agent",
        json!({"tenant_id": t, "run_type": "tool.gated"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "start tool.gated: {r}");
    assert_eq!(
        r["status"], "waiting",
        "the gated tool suspends for approval"
    );
    let run_id_g = r["run_id"].as_str().unwrap().to_string();
    let token = r["resume_token"].as_str().unwrap().to_string();
    assert_eq!(
        calls_g.load(Ordering::SeqCst),
        0,
        "the connector is NOT called before approval"
    );
    // resume (approve) -> the gated tool now EXECUTES for real via the connector.
    let (s, r) = post_json(
        &router_g,
        "/runs.resume",
        t,
        "agent",
        json!({"run_id": run_id_g, "token": token, "resume_input": {"approved_by": "boss"}}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "resume: {r}");
    assert_eq!(
        r["status"], "completed",
        "run completes after the approved tool executes"
    );
    assert_eq!(
        calls_g.load(Ordering::SeqCst),
        1,
        "the connector was called on approval"
    );
    let body_g = last_body_g.lock().unwrap().clone().unwrap();
    assert_eq!(body_g["params"]["name"], "deploy.prod");
    // The gated tool's tool_executions row is `executed` with the REAL result (has `echo`,
    // not the placeholder `note`).
    let (te_status, te_result): (String, Value) =
        sqlx::query_as("SELECT status, result FROM tool_executions WHERE run_id = $1::uuid")
            .bind(&run_id_g)
            .fetch_one(&admin_g)
            .await
            .unwrap();
    assert_eq!(te_status, "executed");
    assert!(
        te_result.get("echo").is_some() && te_result.get("note").is_none(),
        "the gated tool recorded the REAL connector result, not the placeholder: {te_result}"
    );
    admin_g.close().await;
    println!("(c) an approval-gated tool executes for real on resume (via the connector), not a placeholder");

    println!("executor PR4/gated: AUTO + APPROVAL-GATED tool steps both execute for real inside a run (connector outside the run tx); failures fail the run.");
}
