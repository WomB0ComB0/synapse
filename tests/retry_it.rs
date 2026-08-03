//! Live integration test: a step that fails RETRIABLY is rescheduled with a backoff and driven
//! to completion (or terminal failure) by the background worker (executor PR5-remainder).
//!
//! The flaky step is an idempotent `Transform` (no external side effect), so retrying is safe —
//! it exercises retry/backoff without the non-idempotent-tool hazard (tool-step retry is a
//! deliberate follow-up, pending run-level idempotency keys).
//!
//! **DB-gated:** skipped unless `DATABASE_URL` is set. Run locally:
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=postgres -p 5466:5432 pgvector/pgvector:pg16
//! export DATABASE_URL=postgres://postgres:postgres@localhost:5466/postgres
//! cargo test --test retry_it -- --nocapture
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

/// A Config with the background worker ENABLED so the in-request driver reschedules a retriable
/// failure (rather than failing it terminally) and the worker can drive the due retry.
fn cfg_worker_on(database_url: &str) -> Config {
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
        worker_enabled: true,
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

/// `(status, attempts)` of the run's single step.
async fn step_state(admin: &PgPool, run_id: &str) -> (String, i32) {
    sqlx::query_as("SELECT status, attempts FROM run_steps WHERE run_id = $1::uuid")
        .bind(run_id)
        .fetch_one(admin)
        .await
        .unwrap()
}

/// Force any scheduled retry on the run to be DUE now (simulate the backoff elapsing).
async fn make_retry_due(admin: &PgPool, run_id: &str) {
    sqlx::query(
        "UPDATE run_steps SET next_attempt_at = now() - interval '1 second' \
         WHERE run_id = $1::uuid AND status = 'pending' AND next_attempt_at IS NOT NULL",
    )
    .bind(run_id)
    .execute(admin)
    .await
    .unwrap();
}

#[tokio::test]
async fn failed_step_retries_with_backoff_then_worker_drives_it() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };
    let t = "tenant_a";

    let test_db = TestDb::create(&base_url, "stepretry").await;
    let admin = PgPoolOptions::new()
        .max_connections(4)
        .connect(&test_db.url)
        .await
        .expect("connect admin");
    apply_schema(&admin, &test_db.role).await;
    // One RLS-enforcing app pool drives the HTTP router; a second backs the worker (both point
    // at the same throwaway DB — the worker reaches every tenant via the SECURITY DEFINER fn).
    let router = synapse::app(AppState::new(
        app_pool(&test_db.url, &test_db.role).await,
        cfg_worker_on(&test_db.url),
    ));
    let worker_pool = app_pool(&test_db.url, &test_db.role).await;
    let connector = ConnectorImpl::Disabled; // flaky transforms need no connector

    // --- (a) a flaky step fails its first attempt → a retry is scheduled, run stays running ---
    let (s, r) = post_json(
        &router,
        "/runs.start",
        t,
        "agent",
        json!({"tenant_id": t, "run_type": "retry.recovers"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "start retry.recovers: {r}");
    assert_eq!(
        r["status"], "running",
        "the first attempt failed retriably → run stays running (retry pending), not failed"
    );
    let run_id = r["run_id"].as_str().unwrap().to_string();
    let (step_status, attempts) = step_state(&admin, &run_id).await;
    assert_eq!(step_status, "pending", "the step is rescheduled pending");
    assert_eq!(attempts, 1, "one attempt has been made");
    let (has_future_retry,): (bool,) =
        sqlx::query_as("SELECT next_attempt_at > now() FROM run_steps WHERE run_id = $1::uuid")
            .bind(&run_id)
            .fetch_one(&admin)
            .await
            .unwrap();
    assert!(
        has_future_retry,
        "the retry is scheduled in the future (backoff)"
    );
    // The retry-scheduled event is recorded on the run history.
    let (retry_events,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM run_events WHERE run_id = $1::uuid AND event_type = 'step_retry_scheduled'",
    )
    .bind(&run_id)
    .fetch_one(&admin)
    .await
    .unwrap();
    assert_eq!(retry_events, 1, "a step_retry_scheduled event was appended");

    // --- (b) the backoff hasn't elapsed → the worker leaves the run alone ---
    let n = worker::reconcile_runs(&worker_pool, &connector, 300)
        .await
        .unwrap();
    assert_eq!(n, 0, "a not-yet-due retry is NOT driven");
    assert_eq!(
        run_status(&admin, &run_id).await,
        "running",
        "still awaiting its backoff"
    );

    // --- (c) backoff elapsed → the worker drives the due retry → attempt 2 succeeds ---
    make_retry_due(&admin, &run_id).await;
    let n = worker::reconcile_runs(&worker_pool, &connector, 300)
        .await
        .unwrap();
    assert_eq!(n, 1, "the due retry was driven");
    assert_eq!(
        run_status(&admin, &run_id).await,
        "completed",
        "the retry succeeded → run completed"
    );
    let (step_status, attempts) = step_state(&admin, &run_id).await;
    assert_eq!(step_status, "completed");
    assert_eq!(attempts, 2, "it succeeded on the second attempt");
    println!(
        "(a-c) a flaky step reschedules with a backoff; the worker drives the due retry to success"
    );

    // --- (d) a step that never recovers exhausts max_attempts → terminal failure ---
    let (s, r) = post_json(
        &router,
        "/runs.start",
        t,
        "agent",
        json!({"tenant_id": t, "run_type": "retry.exhausts"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "start retry.exhausts: {r}");
    assert_eq!(
        r["status"], "running",
        "first attempt failed retriably → running with a retry"
    );
    let doomed = r["run_id"].as_str().unwrap().to_string();
    make_retry_due(&admin, &doomed).await;
    let n = worker::reconcile_runs(&worker_pool, &connector, 300)
        .await
        .unwrap();
    assert_eq!(n, 1, "the due retry was driven");
    assert_eq!(
        run_status(&admin, &doomed).await,
        "failed",
        "the retry exhausted max_attempts → terminal failure"
    );
    let (step_status, attempts) = step_state(&admin, &doomed).await;
    assert_eq!(step_status, "failed");
    assert_eq!(
        attempts, 2,
        "both attempts were consumed (max_attempts = 2)"
    );
    println!("(d) a step that keeps failing exhausts max_attempts and terminal-fails the run");

    admin.close().await;
    worker_pool.close().await;
    println!("step retry/backoff: a retriable failure reschedules with a backoff; the worker drives due retries to success or terminal exhaustion; a not-yet-due retry is left alone.");
}
