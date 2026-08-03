//! Integration test for the versioned skill registry (`/skills.register` + `/skills.get`).
//!
//! **DB-gated:** skipped unless `DATABASE_URL` is set (CI stays database-free).
//! Each test provisions its own throwaway database via [`common::TestDb`], so the
//! gated suite runs in parallel. To run locally against pgvector:
//!
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=postgres -p 5459:5432 pgvector/pgvector:pg16
//! export DATABASE_URL=postgres://postgres:postgres@localhost:5459/postgres
//! cargo test --test skills_it -- --nocapture
//! ```

mod common;

use axum::http::StatusCode;
use common::{app_pool, apply_schema, post_json, router_for, TestDb};
use serde_json::json;
use sqlx::postgres::PgPoolOptions;

/// Provision a throwaway DB (+ role), apply all migrations, return the app router
/// running as the non-superuser role (so RLS is enforced).
async fn setup(base_url: &str, slug: &str) -> axum::Router {
    let test_db = TestDb::create(base_url, slug).await;
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&test_db.url)
        .await
        .expect("connect admin pool");
    apply_schema(&admin, &test_db.role).await;
    let pool = app_pool(&test_db.url, &test_db.role).await;
    router_for(pool, &test_db.url)
}

#[tokio::test]
async fn skill_registry_versioning_immutability_and_isolation() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };
    let router = setup(&base_url, "skills").await;

    const SID: &str = "skill.approval.draft_response";
    let manifest = |v: &str| {
        json!({
            "skill_id": SID,
            "version": v,
            "name": "Draft response",
            "summary": format!("v{v}"),
            "owners": ["team-eng"],
            "triggers": ["approval_requested"],
            "input_schema": {"type": "object"},
            "output_schema": {"type": "object"},
            "required_tools": ["gmail.send"],
            "policy_tags": ["human_approval_required"],
            "examples": [{"input": {}, "output": {}}]
        })
    };

    // (a) register 1.0.0 -> becomes latest; fetch latest returns it.
    let (s, r) = post_json(
        &router,
        "/skills.register",
        "tenant_a",
        "user_1",
        manifest("1.0.0"),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "register 1.0.0: {r}");
    assert_eq!(r["version"], "1.0.0");
    assert_eq!(r["status"], "registered (latest)");

    let (s, r) = post_json(
        &router,
        "/skills.get",
        "tenant_a",
        "user_1",
        json!({"skill_id": SID}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r["version"], "1.0.0");
    assert_eq!(r["is_latest"], true);
    assert_eq!(r["name"], "Draft response"); // flattened manifest
    assert_eq!(r["policy_tags"][0], "human_approval_required");
    assert!(r["id"].is_string(), "surrogate id present: {r}");
    println!("(a) 1.0.0 registered + is the latest");

    // (b) register 2.0.0 -> becomes latest; the old version is still fetchable but not latest.
    let (_s, r) = post_json(
        &router,
        "/skills.register",
        "tenant_a",
        "user_1",
        manifest("2.0.0"),
    )
    .await;
    assert_eq!(r["status"], "registered (latest)");
    let (_s, r) = post_json(
        &router,
        "/skills.get",
        "tenant_a",
        "user_1",
        json!({"skill_id": SID}),
    )
    .await;
    assert_eq!(r["version"], "2.0.0", "latest is now 2.0.0");
    let (s, r) = post_json(
        &router,
        "/skills.get",
        "tenant_a",
        "user_1",
        json!({"skill_id": SID, "version": "1.0.0"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r["version"], "1.0.0");
    assert_eq!(r["is_latest"], false, "1.0.0 demoted when 2.0.0 arrived");
    println!("(b) 2.0.0 is latest; 1.0.0 retained + demoted");

    // (c) a LOWER version does NOT become latest (latest = highest semver, not last-write).
    let (_s, r) = post_json(
        &router,
        "/skills.register",
        "tenant_a",
        "user_1",
        manifest("1.5.0"),
    )
    .await;
    assert_eq!(
        r["status"], "registered",
        "1.5.0 < 2.0.0 must not become latest"
    );
    let (_s, r) = post_json(
        &router,
        "/skills.get",
        "tenant_a",
        "user_1",
        json!({"skill_id": SID}),
    )
    .await;
    assert_eq!(r["version"], "2.0.0", "latest unchanged by a lower version");
    println!("(c) latest = highest semver (1.5.0 did not displace 2.0.0)");

    // (d) immutability: re-registering an existing version -> 409 Conflict.
    let (s, r) = post_json(
        &router,
        "/skills.register",
        "tenant_a",
        "user_1",
        manifest("1.0.0"),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "re-register must conflict: {r}");
    assert_eq!(r["error"]["code"], "conflict");
    println!("(d) re-registering 1.0.0 -> 409 (immutable)");

    // (e) non-semver version -> 400 Bad Request.
    let (s, r) = post_json(
        &router,
        "/skills.register",
        "tenant_a",
        "user_1",
        manifest("not-semver"),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(r["error"]["code"], "bad_request");
    println!("(e) invalid semver -> 400");

    // (e2) build-metadata versions carry no registry precedence -> rejected 400.
    let (s, r) = post_json(
        &router,
        "/skills.register",
        "tenant_a",
        "user_1",
        manifest("1.0.0+build9"),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "build metadata must be rejected: {r}"
    );
    assert_eq!(r["error"]["code"], "bad_request");
    println!("(e2) build-metadata version -> 400");

    // (f) tenant isolation: tenant_b cannot see tenant_a's skill (RLS + tenant predicate).
    let (s, r) = post_json(
        &router,
        "/skills.get",
        "tenant_b",
        "user_2",
        json!({"skill_id": SID}),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::NOT_FOUND,
        "tenant_b must NOT see tenant_a's skill: {r}"
    );

    // tenant_b registers its OWN skill with the SAME skill_id + version — no collision.
    let (s, _r) = post_json(
        &router,
        "/skills.register",
        "tenant_b",
        "user_2",
        manifest("1.0.0"),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "same skill_id/version in another tenant must not collide"
    );
    let (_s, r) = post_json(
        &router,
        "/skills.get",
        "tenant_b",
        "user_2",
        json!({"skill_id": SID}),
    )
    .await;
    assert_eq!(
        r["version"], "1.0.0",
        "tenant_b's latest is its own 1.0.0, isolated from tenant_a's 2.0.0"
    );
    println!("(f) per-tenant isolation proven (same skill_id, no collision, no cross-tenant read)");

    // (g) a missing skill -> 404.
    let (s, _r) = post_json(
        &router,
        "/skills.get",
        "tenant_a",
        "user_1",
        json!({"skill_id": "skill.nonexistent"}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    println!("skill registry: versioning (semver-latest), immutability (409), validation (400), and tenant isolation all proven.");
}
