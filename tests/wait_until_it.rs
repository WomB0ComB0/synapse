//! Live integration test: the ABSOLUTE-time `wait.until` timer step.
//!
//! Unlike the relative `wait` (`wait_secs`, see `timer_it.rs`), an absolute timer arms
//! `run_steps.next_attempt_at` to a FIXED wall-clock instant supplied as the run input's
//! `wait_until` (RFC3339 UTC). The run defers on it; the leased background worker fires it once the
//! instant passes. A `wait_until` already in the past (or a disabled worker) completes inline so a
//! deferral never strands the run. A missing/invalid `wait_until` is a 400.
//!
//! **DB-gated:** skipped unless `DATABASE_URL` is set. Run locally:
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=postgres -p 5473:5432 pgvector/pgvector:pg16
//! export DATABASE_URL=postgres://postgres:postgres@localhost:5473/postgres
//! cargo test --test wait_until_it -- --nocapture
//! ```

mod common;

use axum::http::StatusCode;
use axum::Router;
use chrono::{Duration, Utc};
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

async fn start_wait_until(
    router: &Router,
    tenant: &str,
    wait_until: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    post_json(
        router,
        "/runs.start",
        tenant,
        "agent",
        json!({"tenant_id": tenant, "run_type": "wait.until", "input": {"wait_until": wait_until}}),
    )
    .await
}

/// DB-free: documents WHY `runs.start` trims `input.wait_until` — chrono's RFC3339 parser is strict
/// and does NOT skip surrounding whitespace, so an untrimmed padded stamp would 400. Empties/blanks
/// are likewise rejected (they become the "required" 400 after the trim+filter).
#[test]
fn chrono_rfc3339_is_strict_about_whitespace() {
    assert!(chrono::DateTime::parse_from_rfc3339("2026-07-10T09:00:00Z").is_ok());
    assert!(
        chrono::DateTime::parse_from_rfc3339("  2026-07-10T09:00:00Z  ").is_err(),
        "chrono does not trim surrounding whitespace — hence runs.start trims first"
    );
    assert!(chrono::DateTime::parse_from_rfc3339("").is_err());
    assert!(chrono::DateTime::parse_from_rfc3339("   ").is_err());
}

#[tokio::test]
async fn absolute_timer_defers_until_the_deadline_then_the_worker_fires_it() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };
    let t = "tenant_a";

    let test_db = TestDb::create(&base_url, "waituntil").await;
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

    // --- (a) FUTURE deadline: the run defers; the wait step arms to the ABSOLUTE instant ---
    let deadline = Utc::now() + Duration::hours(1);
    let (s, r) = start_wait_until(&router, t, json!(deadline.to_rfc3339())).await;
    assert_eq!(s, StatusCode::OK, "start wait.until: {r}");
    assert_eq!(
        r["status"], "running",
        "a future deadline defers the run (does not complete inline)"
    );
    let run_id = r["run_id"].as_str().unwrap().to_string();
    assert_eq!(
        wait_step_status(&admin, &run_id).await,
        "pending",
        "the wait step is pending"
    );
    // The armed wake time is the SUPPLIED absolute instant (not now()+offset): within a few seconds.
    let (skew_secs,): (f64,) = sqlx::query_as(
        "SELECT abs(extract(epoch FROM next_attempt_at - $2::timestamptz))::float8 \
         FROM run_steps WHERE run_id = $1::uuid AND kind = 'wait'",
    )
    .bind(&run_id)
    .bind(deadline)
    .fetch_one(&admin)
    .await
    .unwrap();
    assert!(
        skew_secs < 5.0,
        "next_attempt_at equals the supplied wait_until (skew {skew_secs}s)"
    );
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
    assert_eq!(n, 0, "a not-yet-due deadline is NOT fired");
    assert_eq!(run_status(&admin, &run_id).await, "running");

    // --- (c) the deadline passes → the worker fires it → `after` runs → completed ---
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
    assert_eq!(n, 1, "the due deadline was driven");
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
    println!("(a-c) an absolute timer arms to the supplied instant, defers, and the worker fires it once the deadline passes");

    // --- (d) PAST deadline: completes inline even with the worker enabled (nothing to wait for) ---
    let (s, r) = start_wait_until(
        &router,
        t,
        json!((Utc::now() - Duration::hours(1)).to_rfc3339()),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "start wait.until (past): {r}");
    assert_eq!(
        r["status"], "completed",
        "a wait_until already in the past completes inline"
    );
    println!("(d) a past deadline completes inline (no deferral)");

    // --- (e) worker OFF + future deadline → skipped inline so the run never strands ---
    let router_off = synapse::app(AppState::new(
        app_pool(&test_db.url, &test_db.role).await,
        cfg(&test_db.url, false), // worker DISABLED
    ));
    let (s, r) = start_wait_until(
        &router_off,
        t,
        json!((Utc::now() + Duration::hours(1)).to_rfc3339()),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "start wait.until (worker off): {r}");
    assert_eq!(
        r["status"], "completed",
        "with no worker, the absolute wait is skipped (never strands)"
    );
    println!("(e) with the worker disabled, the absolute wait is skipped so the run never strands");

    // --- (f) missing / invalid wait_until → 400 (fails before persisting a run) ---
    let (s, r) = post_json(
        &router,
        "/runs.start",
        t,
        "agent",
        json!({"tenant_id": t, "run_type": "wait.until"}), // no input.wait_until
    )
    .await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "missing wait_until is a 400: {r}"
    );
    let (s, r) = start_wait_until(&router, t, json!("not-a-timestamp")).await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "an invalid wait_until is a 400: {r}"
    );
    let (s, r) = start_wait_until(&router, t, json!("   ")).await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "a whitespace-only wait_until is a 400 (required): {r}"
    );
    println!("(f) a missing / malformed / whitespace-only input.wait_until is rejected with 400 before any run is persisted");

    // --- (g) a whitespace-PADDED but valid timestamp is normalized (trimmed) and accepted ---
    let padded = format!("  {}  ", (Utc::now() + Duration::hours(1)).to_rfc3339());
    let (s, r) = start_wait_until(&router, t, json!(padded)).await;
    assert_eq!(
        s,
        StatusCode::OK,
        "a padded-but-valid wait_until is accepted: {r}"
    );
    assert_eq!(
        r["status"], "running",
        "a padded future deadline still defers the run"
    );
    println!("(g) a whitespace-padded but valid wait_until is trimmed and accepted");

    admin.close().await;
    worker_pool.close().await;
    println!("wait.until: an absolute-time timer arms next_attempt_at to the supplied wall-clock instant; the leased worker fires it once it passes; a past deadline / disabled worker completes inline; a bad wait_until is a 400.");
}
