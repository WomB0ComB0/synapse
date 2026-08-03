//! Live integration test: `runs.start` idempotency key (run/tool idempotency-keys pillar).
//!
//! A repeat `runs.start` carrying the same `Idempotency-Key` REPLAYS the original run instead of
//! starting a second one (closing the at-least-once gap); the key is tenant-scoped; a keyless
//! request keeps today's at-least-once behavior.
//!
//! **DB-gated:** skipped unless `DATABASE_URL` is set. Run locally:
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=postgres -p 5467:5432 pgvector/pgvector:pg16
//! export DATABASE_URL=postgres://postgres:postgres@localhost:5467/postgres
//! cargo test --test idempotency_it -- --nocapture
//! ```

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use common::setup_with_db;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;

/// POST /runs.start as (`tenant`, `principal`) with an optional `Idempotency-Key`.
async fn start_with_key(
    router: &Router,
    tenant: &str,
    principal: &str,
    key: Option<&str>,
    run_type: &str,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/runs.start")
        .header("content-type", "application/json")
        .header("x-principal-id", principal)
        .header("x-tenant-id", tenant);
    if let Some(k) = key {
        builder = builder.header("idempotency-key", k);
    }
    let body = json!({ "tenant_id": tenant, "run_type": run_type });
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

/// Count `runs` rows carrying `key` (via the superuser admin pool, across tenants).
async fn runs_with_key(admin: &PgPool, key: &str) -> i64 {
    let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM runs WHERE idempotency_key = $1")
        .bind(key)
        .fetch_one(admin)
        .await
        .unwrap();
    n
}

#[tokio::test]
async fn runs_start_idempotency_key_replays_the_original_run() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };
    let (router, admin) = setup_with_db(&base_url, "idempotency").await;

    // --- (a) same key twice → the SAME run, no second run created ---
    let (s1, r1) = start_with_key(
        &router,
        "tenant_a",
        "agent",
        Some("k-alpha"),
        "pipeline.three",
    )
    .await;
    assert_eq!(s1, StatusCode::OK, "first start: {r1}");
    assert_eq!(
        r1["status"], "completed",
        "pipeline.three completes synchronously"
    );
    let (s2, r2) = start_with_key(
        &router,
        "tenant_a",
        "agent",
        Some("k-alpha"),
        "pipeline.three",
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "replay start: {r2}");
    assert_eq!(
        r2["run_id"], r1["run_id"],
        "a repeat with the same key replays the ORIGINAL run_id"
    );
    assert_eq!(
        r2["status"], "completed",
        "replay returns the original run's status"
    );
    assert_eq!(
        runs_with_key(&admin, "k-alpha").await,
        1,
        "exactly one run was created for the key"
    );

    // --- (b) a DIFFERENT key → a different run ---
    let (_, r3) = start_with_key(
        &router,
        "tenant_a",
        "agent",
        Some("k-beta"),
        "pipeline.three",
    )
    .await;
    assert_ne!(
        r3["run_id"], r1["run_id"],
        "a distinct key starts a distinct run"
    );

    // --- (c) keyless requests keep at-least-once (two distinct runs) ---
    let (_, r4) = start_with_key(&router, "tenant_a", "agent", None, "pipeline.three").await;
    let (_, r5) = start_with_key(&router, "tenant_a", "agent", None, "pipeline.three").await;
    assert_ne!(
        r4["run_id"], r5["run_id"],
        "keyless starts always create a new run (back-compat)"
    );

    // --- (d) the key is TENANT-scoped: the same string in another tenant is a distinct run ---
    let (_, r6) = start_with_key(
        &router,
        "tenant_b",
        "agent",
        Some("k-alpha"),
        "pipeline.three",
    )
    .await;
    assert_ne!(
        r6["run_id"], r1["run_id"],
        "tenant_b's 'k-alpha' does not collide with tenant_a's"
    );
    assert_eq!(
        runs_with_key(&admin, "k-alpha").await,
        2,
        "one 'k-alpha' run per tenant"
    );
    println!("(a-d) same key replays the original run; distinct keys/tenants/keyless each create a new run");

    // --- (e) replaying a WAITING run returns the same run_id AND its resume token ---
    let (s7, r7) =
        start_with_key(&router, "tenant_a", "agent", Some("k-gate"), "review.gated").await;
    assert_eq!(s7, StatusCode::OK, "gated start: {r7}");
    assert_eq!(
        r7["status"], "waiting",
        "review.gated suspends for approval"
    );
    let token = r7["resume_token"]
        .as_str()
        .expect("a resume token")
        .to_string();
    let (s8, r8) =
        start_with_key(&router, "tenant_a", "agent", Some("k-gate"), "review.gated").await;
    assert_eq!(s8, StatusCode::OK, "gated replay: {r8}");
    assert_eq!(
        r8["run_id"], r7["run_id"],
        "the gated run replays the same run_id"
    );
    assert_eq!(r8["status"], "waiting");
    assert_eq!(
        r8["resume_token"].as_str().unwrap(),
        token,
        "the replay re-derives the SAME single-use resume token from the open checkpoint"
    );
    assert_eq!(
        runs_with_key(&admin, "k-gate").await,
        1,
        "the gated run was not duplicated"
    );
    println!(
        "(e) replaying a waiting run returns the same run_id + its resume token (still resumable)"
    );

    // --- (f) an over-long key is rejected 400 (never silently truncated into the unique index) ---
    let long_key = "x".repeat(300);
    let (s9, _r9) = start_with_key(
        &router,
        "tenant_a",
        "agent",
        Some(&long_key),
        "pipeline.three",
    )
    .await;
    assert_eq!(
        s9,
        StatusCode::BAD_REQUEST,
        "an over-long Idempotency-Key is a 400"
    );
    println!("(f) an over-long Idempotency-Key is rejected 400");

    // --- (g) a non-ASCII UTF-8 key is accepted (not wrongly 400'd) and still dedupes ---
    let utf8_key = "café-clé-🔑";
    let (s10, r10) = start_with_key(
        &router,
        "tenant_a",
        "agent",
        Some(utf8_key),
        "pipeline.three",
    )
    .await;
    assert_eq!(s10, StatusCode::OK, "a valid UTF-8 key is accepted: {r10}");
    let (s11, r11) = start_with_key(
        &router,
        "tenant_a",
        "agent",
        Some(utf8_key),
        "pipeline.three",
    )
    .await;
    assert_eq!(s11, StatusCode::OK, "utf-8 replay: {r11}");
    assert_eq!(
        r11["run_id"], r10["run_id"],
        "a UTF-8 key dedupes like any other"
    );
    println!("(g) a non-ASCII UTF-8 Idempotency-Key is accepted and dedupes");

    admin.close().await;
    println!("runs.start idempotency: a keyed repeat replays the original run (tenant-scoped); keyless stays at-least-once; a waiting replay stays resumable.");
}
