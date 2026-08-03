//! Live integration test: `/tool.execute` idempotency key + the stuck-intent reconciler (A+).
//!
//! A same-key repeat REPLAYS the original outcome and NEVER re-invokes the non-idempotent
//! connector (executed→result, pending→pending, failed→502, in-flight `approved`→409). The
//! background reconciler fails a crash-stranded ad-hoc `approved` intent so a poison key can't
//! 409 forever.
//!
//! **DB-gated:** skipped unless `DATABASE_URL` is set. Run locally:
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=postgres -p 5468:5432 pgvector/pgvector:pg16
//! export DATABASE_URL=postgres://postgres:postgres@localhost:5468/postgres
//! cargo test --test tool_idempotency_it -- --nocapture
//! ```

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use common::{app_pool, apply_schema, seed_tool_definition, TestDb};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tower::ServiceExt;

use synapse::config::Config;
use synapse::orchestration::worker;
use synapse::state::AppState;

#[derive(Clone)]
struct MockState {
    calls: Arc<AtomicUsize>,
    error: bool,
}

async fn tools_call(State(st): State<MockState>, Json(body): Json<Value>) -> Response {
    st.calls.fetch_add(1, Ordering::SeqCst);
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

async fn spawn_mock(error: bool) -> (String, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let st = MockState {
        calls: calls.clone(),
        error,
    };
    let app = Router::new().route("/", post(tools_call)).with_state(st);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), calls)
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

async fn setup(
    base_url: &str,
    slug: &str,
    mcp: Option<String>,
    tool_ids: &[&str],
) -> (Router, PgPool) {
    let test_db = TestDb::create(base_url, slug).await;
    let admin = PgPoolOptions::new()
        .max_connections(4)
        .connect(&test_db.url)
        .await
        .expect("connect admin");
    apply_schema(&admin, &test_db.role).await;
    for tenant in ["tenant_a", "tenant_b"] {
        for tool_id in tool_ids {
            seed_tool_definition(&admin, tenant, tool_id).await;
        }
    }
    let pool = app_pool(&test_db.url, &test_db.role).await;
    let router = synapse::app(AppState::new(pool, cfg(&test_db.url, mcp)));
    (router, admin)
}

/// POST /tool.execute as (`tenant`, `agent`) with an optional `Idempotency-Key`.
async fn exec_tool(
    router: &Router,
    tenant: &str,
    key: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/tool.execute")
        .header("content-type", "application/json")
        .header("x-principal-id", "agent")
        .header("x-tenant-id", tenant);
    if let Some(k) = key {
        builder = builder.header("idempotency-key", k);
    }
    let resp = router
        .clone()
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
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

fn echo_body(tenant: &str) -> Value {
    json!({"tenant_id": tenant, "principal_id": "agent", "tool_id": "echo", "arguments": {"x": 1}})
}

#[tokio::test]
async fn tool_execute_idempotency_replays_without_re_invoking_the_connector() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };
    let t = "tenant_a";

    // ---- success cases (a-d, f) against one mock+router ----
    let (mcp_url, calls) = spawn_mock(false).await;
    let (router, _admin) = setup(&base_url, "toolidem", Some(mcp_url), &["echo", "deploy"]).await;

    // (a) same key twice → the connector is called ONCE; the 2nd replays the same result.
    let (s1, r1) = exec_tool(&router, t, Some("k-a"), echo_body(t)).await;
    assert_eq!(s1, StatusCode::OK, "first exec: {r1}");
    assert_eq!(r1["status"], "executed");
    let (s2, r2) = exec_tool(&router, t, Some("k-a"), echo_body(t)).await;
    assert_eq!(s2, StatusCode::OK, "replay exec: {r2}");
    assert_eq!(r2["status"], "executed");
    assert_eq!(
        r2["output"], r1["output"],
        "the replay returns the ORIGINAL result"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the connector was invoked exactly once for k-a"
    );

    // (b) a distinct key → the connector is invoked again.
    let (_, _r) = exec_tool(&router, t, Some("k-b"), echo_body(t)).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "a distinct key re-invokes the connector"
    );

    // (c) keyless calls stay at-least-once (each invokes the connector).
    exec_tool(&router, t, None, echo_body(t)).await;
    exec_tool(&router, t, None, echo_body(t)).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        4,
        "keyless calls each invoke the connector"
    );

    // (f) the key is TENANT-scoped: the same string in tenant_b is a distinct execution.
    let (_, _r) = exec_tool(&router, "tenant_b", Some("k-a"), echo_body("tenant_b")).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        5,
        "tenant_b's 'k-a' does not collide with tenant_a's"
    );

    // (d) an approval-gated call with a key → pending, connector NOT called; a repeat replays
    //     the same pending outcome (still no connector call).
    let gated = json!({"tenant_id": t, "principal_id": "agent", "tool_id": "deploy",
                       "arguments": {}, "policy": {"approval_mode": "required"}});
    let (sg1, rg1) = exec_tool(&router, t, Some("k-gate"), gated.clone()).await;
    assert_eq!(sg1, StatusCode::OK, "gated: {rg1}");
    assert_eq!(rg1["status"], "pending");
    assert_eq!(rg1["requires_approval"], true);
    let (sg2, rg2) = exec_tool(&router, t, Some("k-gate"), gated).await;
    assert_eq!(sg2, StatusCode::OK, "gated replay: {rg2}");
    assert_eq!(rg2["status"], "pending");
    assert_eq!(rg2["requires_approval"], true);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        5,
        "an approval-gated call never invokes the connector"
    );
    println!("(a-d,f) keyed repeats replay the original outcome; distinct keys/tenants/keyless each re-invoke; gated replays pending — all without a second connector call");

    // ---- (e) a FAILED tool replays the failure without re-invoking ----
    let (mcp_err, calls_err) = spawn_mock(true).await;
    let (router_err, _admin_err) = setup(&base_url, "toolidemerr", Some(mcp_err), &["boom"]).await;
    let fail_body =
        json!({"tenant_id": t, "principal_id": "agent", "tool_id": "boom", "arguments": {}});
    let (se1, _re1) = exec_tool(&router_err, t, Some("k-fail"), fail_body.clone()).await;
    assert_eq!(se1, StatusCode::BAD_GATEWAY, "a tool fault is a 502");
    let (se2, _re2) = exec_tool(&router_err, t, Some("k-fail"), fail_body).await;
    assert_eq!(se2, StatusCode::BAD_GATEWAY, "the replay re-raises the 502");
    assert_eq!(
        calls_err.load(Ordering::SeqCst),
        1,
        "a failed tool is NOT re-invoked on replay"
    );
    println!("(e) a failed tool replays the 502 without re-invoking the connector");

    // ---- (g) the reconciler fails a crash-stranded ad-hoc 'approved' intent ----
    let test_db = TestDb::create(&base_url, "toolidemrecon").await;
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&test_db.url)
        .await
        .expect("connect admin");
    apply_schema(&admin, &test_db.role).await;
    let pool = app_pool(&test_db.url, &test_db.role).await;

    // A stuck ad-hoc intent (run_id NULL, approved, created an hour ago) — a crash mid-saga.
    let (stuck_id,): (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO tool_executions (tenant_id, tool_id, approval_mode, status, run_id, \
             idempotency_key, created_at, started_at) \
         VALUES ('tenant_a', 'stuck.tool', 'none', 'approved', NULL, 'stuck-key', \
             now() - interval '1 hour', now() - interval '1 hour') RETURNING id",
    )
    .fetch_one(&admin)
    .await
    .unwrap();
    // A FRESH ad-hoc intent (created just now) — a legitimately in-flight call, must be left alone.
    let (fresh_id,): (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO tool_executions (tenant_id, tool_id, approval_mode, status, run_id, \
             idempotency_key) \
         VALUES ('tenant_a', 'fresh.tool', 'none', 'approved', NULL, 'fresh-key') RETURNING id",
    )
    .fetch_one(&admin)
    .await
    .unwrap();

    let failed = worker::reconcile_stuck_tool_intents(&pool, 60)
        .await
        .unwrap();
    assert_eq!(failed, 1, "only the stale stuck intent is reconciled");
    let status = |id: uuid::Uuid, admin: PgPool| async move {
        let (s,): (String,) = sqlx::query_as("SELECT status FROM tool_executions WHERE id = $1")
            .bind(id)
            .fetch_one(&admin)
            .await
            .unwrap();
        s
    };
    assert_eq!(
        status(stuck_id, admin.clone()).await,
        "failed",
        "the stranded intent was failed"
    );
    assert_eq!(
        status(fresh_id, admin.clone()).await,
        "approved",
        "a fresh in-flight intent is untouched"
    );
    // Idempotent: a second pass finds nothing new.
    assert_eq!(
        worker::reconcile_stuck_tool_intents(&pool, 60)
            .await
            .unwrap(),
        0,
        "no stuck intents remain"
    );
    admin.close().await;
    pool.close().await;
    println!("(g) the reconciler fails a crash-stranded ad-hoc approved intent (clearing the poison key) and leaves a fresh in-flight one alone");

    println!("tool.execute idempotency: a keyed repeat replays the outcome (executed/pending/failed) and never re-invokes the connector; the reconciler clears crash-stranded intents.");
}
