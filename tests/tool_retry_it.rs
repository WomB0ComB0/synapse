//! Live integration test: tool-STEP retry under the two-sided MCP idempotency-key contract.
//!
//! A retriable tool step (`tool.auto.retries`, `max_attempts = 3`) whose connector call FAILS is
//! rescheduled with a backoff and re-driven by the worker — re-sending the SAME deterministic
//! `Idempotency-Key`, so a compliant MCP server can dedup a re-sent (possibly-executed) call. The
//! mock endpoint fails the first call then succeeds; the run completes on the retry, and BOTH calls
//! carry the identical idempotency key (the service side of the contract).
//!
//! **DB-gated:** skipped unless `DATABASE_URL` is set. Run locally:
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=postgres -p 5477:5432 pgvector/pgvector:pg16
//! export DATABASE_URL=postgres://postgres:postgres@localhost:5477/postgres
//! cargo test --test tool_retry_it -- --nocapture
//! ```

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use common::{app_pool, apply_schema, post_json, seed_tool_definition, TestDb};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

use synapse::config::Config;
use synapse::mcp::{default_connector, ConnectorImpl};
use synapse::orchestration::worker;
use synapse::state::AppState;

/// Mock MCP endpoint: fail the first `fail_until` calls (JSON-RPC error), then succeed. Records the
/// `Idempotency-Key` header of every call.
#[derive(Clone)]
struct MockState {
    calls: Arc<AtomicUsize>,
    fail_until: usize,
    keys: Arc<Mutex<Vec<String>>>,
}

async fn tools_call(
    State(st): State<MockState>,
    headers: HeaderMap,
    Json(_body): Json<Value>,
) -> Response {
    let n = st.calls.fetch_add(1, Ordering::SeqCst); // 0-based index of THIS call
    let key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    st.keys.lock().unwrap().push(key);
    if n < st.fail_until {
        return Json(json!({
            "jsonrpc": "2.0", "id": 1, "error": { "code": -32000, "message": "transient" }
        }))
        .into_response();
    }
    Json(json!({
        "jsonrpc": "2.0", "id": 1,
        "result": { "content": [{ "type": "text", "text": "ok" }], "isError": false }
    }))
    .into_response()
}

async fn spawn_mock(fail_until: usize) -> (String, Arc<AtomicUsize>, Arc<Mutex<Vec<String>>>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let keys = Arc::new(Mutex::new(Vec::new()));
    let st = MockState {
        calls: calls.clone(),
        fail_until,
        keys: keys.clone(),
    };
    let app = Router::new().route("/", post(tools_call)).with_state(st);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), calls, keys)
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
        worker_enabled: true, // the retry decision requires a worker to drive it
        worker_poll_secs: 30,
        worker_stale_secs: 300,
    }
}

async fn run_status(admin: &PgPool, run_id: &str) -> String {
    let (s,): (String,) = sqlx::query_as("SELECT status FROM runs WHERE run_id = $1::uuid")
        .bind(run_id)
        .fetch_one(admin)
        .await
        .unwrap();
    s
}

#[tokio::test]
async fn tool_step_retries_and_resends_the_same_idempotency_key() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };
    let t = "tenant_a";
    let (mcp_url, calls, keys) = spawn_mock(1).await; // fail call #1, succeed on the retry

    let test_db = TestDb::create(&base_url, "toolretry").await;
    let admin = PgPoolOptions::new()
        .max_connections(4)
        .connect(&test_db.url)
        .await
        .expect("connect admin");
    apply_schema(&admin, &test_db.role).await;
    seed_tool_definition(&admin, t, "flaky-echo").await;
    let router = synapse::app(AppState::new(
        app_pool(&test_db.url, &test_db.role).await,
        cfg(&test_db.url, Some(mcp_url.clone())),
    ));
    let worker_pool = app_pool(&test_db.url, &test_db.role).await;
    let connector: ConnectorImpl = default_connector(&cfg(&test_db.url, Some(mcp_url)));

    // --- (a) start the retriable tool run: the FIRST connector call fails -> the step is
    //         rescheduled (run stays running, not failed) ---
    let (s, r) = post_json(
        &router,
        "/runs.start",
        t,
        "agent",
        json!({ "tenant_id": t, "run_type": "tool.auto.retries" }),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "start tool.auto.retries: {r}");
    assert_eq!(
        r["status"], "running",
        "a failed retriable tool step reschedules (does not fail the run)"
    );
    let run_id = r["run_id"].as_str().unwrap().to_string();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the tool was called once (and failed)"
    );
    let (step_status, attempts): (String, i32) = sqlx::query_as(
        "SELECT status, attempts FROM run_steps WHERE run_id = $1::uuid AND kind = 'tool'",
    )
    .bind(&run_id)
    .fetch_one(&admin)
    .await
    .unwrap();
    assert_eq!(
        step_status, "pending",
        "the failed tool step is rescheduled to pending"
    );
    assert_eq!(attempts, 1, "one attempt recorded after the first failure");
    println!("(a) a failed retriable tool step reschedules (run stays running); attempts=1");

    // --- (b) the backoff elapses -> the worker re-drives -> the SECOND call succeeds -> completed ---
    sqlx::query(
        "UPDATE run_steps SET next_attempt_at = now() - interval '1 second' \
         WHERE run_id = $1::uuid AND kind = 'tool' AND status = 'pending'",
    )
    .bind(&run_id)
    .execute(&admin)
    .await
    .unwrap();
    let n = worker::reconcile_runs(&worker_pool, &connector, 300)
        .await
        .unwrap();
    assert_eq!(n, 1, "the worker drove the due tool retry");
    assert_eq!(
        run_status(&admin, &run_id).await,
        "completed",
        "the retry succeeded -> run completed"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the tool was called again on the retry"
    );
    println!(
        "(b) the worker re-drives the due tool retry; the second call succeeds -> run completed"
    );

    // --- (c) BOTH calls carried the SAME deterministic Idempotency-Key (the two-sided contract) ---
    let captured = keys.lock().unwrap().clone();
    assert_eq!(captured.len(), 2, "two calls captured");
    assert!(
        !captured[0].is_empty(),
        "the first call carried an Idempotency-Key: {captured:?}"
    );
    assert_eq!(
        captured[0], captured[1],
        "the retry re-sent the SAME idempotency key: {captured:?}"
    );
    // The key is the deterministic run_id:step_index (step 0).
    assert_eq!(
        captured[0],
        format!("{run_id}:0000"),
        "the key is deterministic (run_id:step_index)"
    );
    println!(
        "(c) both the initial call and the retry sent the identical deterministic Idempotency-Key"
    );

    admin.close().await;
    worker_pool.close().await;
    println!("tool retry: a retriable tool step reschedules on failure and the worker re-drives it, re-sending the same deterministic Idempotency-Key so a compliant MCP server can dedup.");
}
