//! Live integration test: the generic idempotency-key registry (0024) — request-fingerprint gate.
//!
//! Reusing an `Idempotency-Key` with the SAME request body replays (the per-table #32/#33 replay);
//! reusing it with a DIFFERENT body is a `409 Conflict` (a key is bound to its first request). The
//! gate is per (tenant, scope, key), so `runs.start` and `tool.execute` keep separate namespaces.
//!
//! **DB-gated:** skipped unless `DATABASE_URL` is set. Run locally:
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=postgres -p 5478:5432 pgvector/pgvector:pg16
//! export DATABASE_URL=postgres://postgres:postgres@localhost:5478/postgres
//! cargo test --test idempotency_fingerprint_it -- --nocapture
//! ```

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use common::setup_with_db;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

/// POST `uri` as (`tenant`, `principal`) with an `Idempotency-Key` + an arbitrary body.
async fn post_key(
    router: &Router,
    uri: &str,
    tenant: &str,
    principal: &str,
    key: &str,
    body: Value,
) -> (StatusCode, Value) {
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .header("x-principal-id", principal)
                .header("x-tenant-id", tenant)
                .header("idempotency-key", key)
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
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

#[tokio::test]
async fn idempotency_key_reuse_with_a_different_body_is_a_conflict() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };
    let t = "tenant_a";
    let (router, admin) = setup_with_db(&base_url, "idemfp").await;

    // --- runs.start ---
    let body_a = json!({ "tenant_id": t, "run_type": "pipeline.three" });
    let (s1, r1) = post_key(&router, "/runs.start", t, "agent", "K1", body_a.clone()).await;
    assert_eq!(s1, StatusCode::OK, "first keyed start: {r1}");

    // same key + SAME body -> replays the ORIGINAL run
    let (s2, r2) = post_key(&router, "/runs.start", t, "agent", "K1", body_a).await;
    assert_eq!(s2, StatusCode::OK, "same key + same body replays: {r2}");
    assert_eq!(
        r2["run_id"], r1["run_id"],
        "the replay returns the original run_id"
    );

    // same key + DIFFERENT body -> 409 (no new run)
    let (s3, r3) = post_key(
        &router,
        "/runs.start",
        t,
        "agent",
        "K1",
        json!({ "tenant_id": t, "run_type": "review.gated" }),
    )
    .await;
    assert_eq!(
        s3,
        StatusCode::CONFLICT,
        "same key + different body is a 409: {r3}"
    );
    assert_eq!(r3["error"]["code"], "conflict");
    let (runs_for_k1,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM runs WHERE tenant_id = $1 AND idempotency_key = 'K1'")
            .bind(t)
            .fetch_one(&admin)
            .await
            .unwrap();
    assert_eq!(
        runs_for_k1, 1,
        "the conflicting request created NO second run"
    );
    println!("(runs.start) same key+body replays; same key+different body -> 409, no new run");

    // --- tool.execute (separate scope: the same key text does NOT collide with runs.start) ---
    let tool_a = json!({ "tenant_id": t, "principal_id": "agent", "tool_id": "echo", "arguments": { "msg": "a" } });
    let (s4, r4) = post_key(&router, "/tool.execute", t, "agent", "K1", tool_a.clone()).await;
    assert_eq!(
        s4,
        StatusCode::OK,
        "tool.execute with key K1 (distinct scope from runs.start): {r4}"
    );

    // same key + SAME body -> replays (no conflict)
    let (s5, _r5) = post_key(&router, "/tool.execute", t, "agent", "K1", tool_a).await;
    assert_eq!(
        s5,
        StatusCode::OK,
        "tool.execute same key + same body replays"
    );

    // same key + DIFFERENT arguments -> 409
    let (s6, r6) = post_key(
        &router,
        "/tool.execute",
        t,
        "agent",
        "K1",
        json!({ "tenant_id": t, "principal_id": "agent", "tool_id": "echo", "arguments": { "msg": "b" } }),
    )
    .await;
    assert_eq!(
        s6,
        StatusCode::CONFLICT,
        "tool.execute same key + different args is a 409: {r6}"
    );
    assert_eq!(r6["error"]["code"], "conflict");
    println!("(tool.execute) separate key namespace; same key+body replays; different body -> 409");

    // --- a KEYED call from an authenticated-but-UNPROVISIONED tenant is a 400 (not a 500) — the
    //     fingerprint gate's tenant FK maps to the same BadRequest as the keyless provisioning path ---
    let (s7, r7) = post_key(
        &router,
        "/runs.start",
        "ghost_tenant",
        "agent",
        "GK",
        json!({ "tenant_id": "ghost_tenant", "run_type": "pipeline.three" }),
    )
    .await;
    assert_eq!(
        s7,
        StatusCode::BAD_REQUEST,
        "a keyed call from an unknown tenant is a 400: {r7}"
    );
    println!("(tenant) a keyed request from an unprovisioned tenant is a 400 (fingerprint gate maps the tenant FK, not a 500)");

    admin.close().await;
    println!("idempotency fingerprint: a key is bound to its first request body per (tenant, scope) — a reuse with a different body is a 409, not a silent replay.");
}
