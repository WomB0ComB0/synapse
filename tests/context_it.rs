//! Integration test for the Context Service (`/context.upsert` + `/context.get`).
//!
//! **DB-gated:** skipped unless `DATABASE_URL` is set (CI stays database-free).
//! Each test provisions its own throwaway database via [`common`], so the gated
//! suite runs in parallel. To run locally against pgvector:
//!
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=postgres -p 5459:5432 pgvector/pgvector:pg16
//! export DATABASE_URL=postgres://postgres:postgres@localhost:5459/postgres
//! cargo test --test context_it -- --nocapture
//! ```

mod common;

use axum::http::StatusCode;
use common::{post_json, setup};
use serde_json::json;

#[tokio::test]
async fn context_upsert_get_isolation_and_cross_tenant_conflict() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };
    let router = setup(&base_url, "context").await;

    // (a) upsert context for a NEW principal (auto-provisioned into `principals`).
    let (s, r) = post_json(
        &router,
        "/context.upsert",
        "tenant_a",
        "admin_1",
        json!({
            "principal_id": "user_1",
            "tenant_id": "tenant_a",
            "team_ids": ["eng", "oncall"],
            "role": "engineer",
            "location": "us-east",
            "approval_limit_usd": 2500.50,
            "preferred_tools": ["gmail.send"],
            "active_projects": ["proj-x"],
            "policy_overrides": ["allow_pii_read"],
            "data_classification": {"contains_pii": true, "special_category": false}
        }),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "upsert: {r}");
    assert_eq!(r["principal_id"], "user_1");
    assert_eq!(r["status"], "upserted");
    println!("(a) upsert provisioned the principal + stored context");

    // (b) get it back — every field roundtrips, incl the numeric limit + PII flag.
    let (s, r) = post_json(
        &router,
        "/context.get",
        "tenant_a",
        "admin_1",
        json!({"principal_id": "user_1"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r["role"], "engineer");
    assert_eq!(r["team_ids"][1], "oncall");
    assert_eq!(
        r["approval_limit_usd"], 2500.50,
        "numeric approval_limit roundtrips as f64"
    );
    assert_eq!(r["policy_overrides"][0], "allow_pii_read");
    assert_eq!(r["data_classification"]["contains_pii"], true);
    assert!(r["updated_at"].is_string());
    println!("(b) get roundtrips numeric + PII classification");

    // (c) a second upsert replaces the document in place (PUT-like): role changes,
    //     and fields omitted this time reset to their defaults.
    let (s, _r) = post_json(
        &router,
        "/context.upsert",
        "tenant_a",
        "admin_1",
        json!({
            "principal_id": "user_1", "tenant_id": "tenant_a",
            "role": "manager", "approval_limit_usd": 10000.0,
            "data_classification": {"contains_pii": false, "special_category": false}
        }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (_s, r) = post_json(
        &router,
        "/context.get",
        "tenant_a",
        "admin_1",
        json!({"principal_id": "user_1"}),
    )
    .await;
    assert_eq!(r["role"], "manager", "update in place");
    assert_eq!(r["approval_limit_usd"], 10000.0);
    assert_eq!(r["data_classification"]["contains_pii"], false);
    assert_eq!(
        r["team_ids"].as_array().unwrap().len(),
        0,
        "omitted fields reset to default on upsert"
    );
    println!("(c) upsert replaces in place; omitted fields reset");

    // (d) tenant isolation: tenant_b cannot read tenant_a's context.
    let (s, _r) = post_json(
        &router,
        "/context.get",
        "tenant_b",
        "admin_2",
        json!({"principal_id": "user_1"}),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::NOT_FOUND,
        "tenant_b must NOT see tenant_a's context"
    );

    // (e) tenant-scoped identity: tenant_b may use the SAME principal_id "user_1"
    //     independently — no collision, no cross-read, and tenant_a is untouched.
    let (s, r) = post_json(
        &router,
        "/context.upsert",
        "tenant_b",
        "admin_2",
        json!({
            "principal_id": "user_1", "tenant_id": "tenant_b", "role": "analyst",
            "data_classification": {"contains_pii": false, "special_category": false}
        }),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "same principal_id in another tenant is independent: {r}"
    );
    let (_s, rb) = post_json(
        &router,
        "/context.get",
        "tenant_b",
        "admin_2",
        json!({"principal_id": "user_1"}),
    )
    .await;
    assert_eq!(
        rb["role"], "analyst",
        "tenant_b sees its OWN user_1 context"
    );
    let (_s, ra) = post_json(
        &router,
        "/context.get",
        "tenant_a",
        "admin_1",
        json!({"principal_id": "user_1"}),
    )
    .await;
    assert_eq!(
        ra["role"], "manager",
        "tenant_a's user_1 is untouched + isolated"
    );
    println!("(e) same principal_id across tenants is isolated (no collision, no cross-read)");

    // (f) body tenant != authenticated tenant -> 403 (fail closed).
    let (s, _r) = post_json(
        &router,
        "/context.upsert",
        "tenant_a",
        "admin_1",
        json!({
            "principal_id": "user_2", "tenant_id": "tenant_b", "role": "x",
            "data_classification": {"contains_pii": false, "special_category": false}
        }),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "body tenant mismatch -> 403");
    println!("(f) body-tenant mismatch -> 403");

    // (g) get a principal with no context -> 404.
    let (s, _r) = post_json(
        &router,
        "/context.get",
        "tenant_a",
        "admin_1",
        json!({"principal_id": "nobody"}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // (h) an out-of-range approval limit is a clean 400 (not a 500 numeric overflow).
    let (s, r) = post_json(
        &router,
        "/context.upsert",
        "tenant_a",
        "admin_1",
        json!({
            "principal_id": "user_3", "tenant_id": "tenant_a", "approval_limit_usd": 1e13,
            "data_classification": {"contains_pii": false, "special_category": false}
        }),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "out-of-range approval limit -> 400: {r}"
    );
    assert_eq!(r["error"]["code"], "bad_request");
    println!("(h) out-of-range approval_limit -> 400");

    println!("context service: upsert/get roundtrip (numeric + PII), update-in-place, tenant-scoped identity isolation, tenant-mismatch 403, limit-range 400 all proven.");
}
