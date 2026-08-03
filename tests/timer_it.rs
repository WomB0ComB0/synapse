//! Live integration test: the `wait` timer step (scheduled/delayed steps).
//!
//! A `wait` step defers the run until its wake time; the background worker completes it once the
//! delay elapses (reusing the retry `next_attempt_at` machinery). With no worker, the delay is
//! skipped and the run completes synchronously (a deferral never strands a run).
//!
//! **DB-gated:** skipped unless `DATABASE_URL` is set. Run locally:
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=postgres -p 5469:5432 pgvector/pgvector:pg16
//! export DATABASE_URL=postgres://postgres:postgres@localhost:5469/postgres
//! cargo test --test timer_it -- --nocapture
//! ```

mod common;

use axum::http::StatusCode;
use common::{app_pool, apply_schema, post_json, TestDb};
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

use synapse::config::Config;
use synapse::mcp::ConnectorImpl;
use synapse::orchestration::worker;
use synapse::state::AppState;

fn cfg(database_url: &str, worker_enabled: bool) -> Config {
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
        mcp_endpoint: None,
        mcp_auth_token: None,
        mcp_auth_token_file: None,
        mcp_scopes: Vec::new(),
        mcp_allowed_hosts: Vec::new(),
        mcp_timeout_secs: 5,
        mcp_max_retries: 1,
        worker_enabled,
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

async fn wait_step_status(admin: &PgPool, run_id: &str) -> String {
    let (s,): (String,) =
        sqlx::query_as("SELECT status FROM run_steps WHERE run_id = $1::uuid AND kind = 'wait'")
            .bind(run_id)
            .fetch_one(admin)
            .await
            .unwrap();
    s
}

#[tokio::test]
async fn wait_step_defers_until_the_worker_fires_the_timer() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };
    let t = "tenant_a";

    let test_db = TestDb::create(&base_url, "timer").await;
    let admin = PgPoolOptions::new()
        .max_connections(4)
        .connect(&test_db.url)
        .await
        .expect("connect admin");
    apply_schema(&admin, &test_db.role).await;
    let router = synapse::app(AppState::new(
        app_pool(&test_db.url, &test_db.role).await,
        cfg(&test_db.url, true), // worker ENABLED
    ));
    let worker_pool = app_pool(&test_db.url, &test_db.role).await;
    let connector = ConnectorImpl::Disabled;

    // --- (a) the run defers on the timer: `before` completes, `pause` arms, run stays running ---
    let (s, r) = post_json(
        &router,
        "/runs.start",
        t,
        "agent",
        json!({"tenant_id": t, "run_type": "wait.then"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "start wait.then: {r}");
    assert_eq!(
        r["status"], "running",
        "the run defers on the timer (does not complete inline)"
    );
    let run_id = r["run_id"].as_str().unwrap().to_string();
    assert_eq!(
        wait_step_status(&admin, &run_id).await,
        "pending",
        "the wait step is pending"
    );
    let (armed_future,): (bool,) = sqlx::query_as(
        "SELECT next_attempt_at > now() FROM run_steps WHERE run_id = $1::uuid AND kind = 'wait'",
    )
    .bind(&run_id)
    .fetch_one(&admin)
    .await
    .unwrap();
    assert!(armed_future, "the timer is armed in the future");
    let (scheduled_events,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM run_events WHERE run_id = $1::uuid AND event_type = 'step_scheduled'",
    )
    .bind(&run_id)
    .fetch_one(&admin)
    .await
    .unwrap();
    assert_eq!(
        scheduled_events, 1,
        "a step_scheduled event was appended when the timer armed"
    );

    // --- (b) not due → the worker leaves the run alone ---
    let n = worker::reconcile_runs(&worker_pool, &connector, 300)
        .await
        .unwrap();
    assert_eq!(n, 0, "a not-yet-due timer is NOT fired");
    assert_eq!(run_status(&admin, &run_id).await, "running");

    // --- (c) the wake time elapses → the worker fires the timer → `after` runs → completed ---
    sqlx::query(
        "UPDATE run_steps SET next_attempt_at = now() - interval '1 second' \
         WHERE run_id = $1::uuid AND kind = 'wait' AND status = 'pending'",
    )
    .bind(&run_id)
    .execute(&admin)
    .await
    .unwrap();
    let n = worker::reconcile_runs(&worker_pool, &connector, 300)
        .await
        .unwrap();
    assert_eq!(n, 1, "the due timer was driven");
    assert_eq!(
        run_status(&admin, &run_id).await,
        "completed",
        "the timer fired → run completed"
    );
    assert_eq!(
        wait_step_status(&admin, &run_id).await,
        "completed",
        "the wait step completed"
    );
    println!(
        "(a-c) a wait step defers the run; the worker fires the due timer and the run completes"
    );

    // --- (d) with NO worker, the delay is skipped and the run completes synchronously ---
    let router_off = synapse::app(AppState::new(
        app_pool(&test_db.url, &test_db.role).await,
        cfg(&test_db.url, false), // worker DISABLED
    ));
    let (s, r) = post_json(
        &router_off,
        "/runs.start",
        t,
        "agent",
        json!({"tenant_id": t, "run_type": "wait.then"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "start wait.then (worker off): {r}");
    assert_eq!(
        r["status"], "completed",
        "with no worker to fire the timer, the wait is skipped and the run completes"
    );
    println!("(d) with the worker disabled, a wait step is skipped so the run never strands");

    admin.close().await;
    worker_pool.close().await;
    println!("wait/timer: a wait step defers the run until the worker fires it once the delay elapses; a not-due timer is left alone; with no worker the delay is skipped.");
}
