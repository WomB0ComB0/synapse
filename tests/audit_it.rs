//! Integration test for the audit log (`GET /audit/events` + recording via governed actions).
//!
//! **DB-gated:** skipped unless `DATABASE_URL` is set (CI stays database-free).
//! Each test provisions its own throwaway database via [`common`]. Run locally:
//!
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=postgres -p 5459:5432 pgvector/pgvector:pg16
//! export DATABASE_URL=postgres://postgres:postgres@localhost:5459/postgres
//! cargo test --test audit_it -- --nocapture
//! ```

mod common;

use axum::http::StatusCode;
use common::{get_json, post_json, setup};
use serde_json::json;

#[tokio::test]
async fn audit_records_governed_actions_and_queries_tenant_scoped() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };
    let router = setup(&base_url, "audit").await;

    // Two governed actions as tenant_a (each records an audit event), one as tenant_b.
    let (s, _) = post_json(
        &router,
        "/skills.register",
        "tenant_a",
        "user_1",
        json!({"skill_id": "skill.demo", "version": "1.0.0", "name": "Demo"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = post_json(
        &router,
        "/context.upsert",
        "tenant_a",
        "user_1",
        json!({"principal_id": "user_9", "tenant_id": "tenant_a", "role": "eng",
               "data_classification": {"contains_pii": false, "special_category": false}}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = post_json(
        &router,
        "/skills.register",
        "tenant_b",
        "user_2",
        json!({"skill_id": "skill.b", "version": "1.0.0", "name": "B"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // (a) tenant_a's audit: exactly its 2 events, newest-first, no tenant_b leak.
    let (s, r) = get_json(&router, "/audit/events", "tenant_a", "user_1").await;
    assert_eq!(s, StatusCode::OK);
    let events = r["events"].as_array().unwrap();
    assert_eq!(events.len(), 2, "tenant_a has 2 audit events: {r}");
    assert!(
        events[0]["ts"].as_str().unwrap() >= events[1]["ts"].as_str().unwrap(),
        "newest-first ordering"
    );
    let actions: Vec<&str> = events
        .iter()
        .map(|e| e["action"].as_str().unwrap())
        .collect();
    assert!(
        actions.contains(&"context.upsert") && actions.contains(&"skills.register"),
        "both actions: {actions:?}"
    );
    assert!(events.iter().all(|e| e["outcome"] == "success"));
    assert!(
        !events.iter().any(|e| e["resource"] == "skill.b"),
        "tenant_b event leaked into tenant_a"
    );
    println!("(a) tenant-scoped audit: 2 events, newest-first, no cross-tenant leak");

    // (b) filter by action.
    let (_s, r) = get_json(
        &router,
        "/audit/events?action=skills.register",
        "tenant_a",
        "user_1",
    )
    .await;
    let ev = r["events"].as_array().unwrap();
    assert_eq!(ev.len(), 1, "action filter: {r}");
    assert_eq!(ev[0]["action"], "skills.register");
    assert_eq!(ev[0]["resource"], "skill.demo");
    println!("(b) action filter works");

    // (c) tenant_b sees only its own event.
    let (_s, r) = get_json(&router, "/audit/events", "tenant_b", "user_2").await;
    let ev = r["events"].as_array().unwrap();
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0]["resource"], "skill.b");
    println!("(c) tenant_b isolated: sees only its own event");

    // (d) keyset pagination: limit=1 -> 1 event + a next_cursor; follow it for page 2.
    let (_s, r1) = get_json(&router, "/audit/events?limit=1", "tenant_a", "user_1").await;
    assert_eq!(r1["events"].as_array().unwrap().len(), 1);
    let cursor = r1["next_cursor"].as_str().expect("next_cursor on page 1");
    let (_s, r2) = get_json(
        &router,
        &format!("/audit/events?limit=1&cursor={cursor}"),
        "tenant_a",
        "user_1",
    )
    .await;
    assert_eq!(r2["events"].as_array().unwrap().len(), 1);
    assert_ne!(
        r1["events"][0]["event_id"], r2["events"][0]["event_id"],
        "page 2 is a different event"
    );
    assert!(
        r2["next_cursor"].is_null(),
        "no more pages after the second"
    );
    println!("(d) keyset pagination via next_cursor works");

    // (e) an invalid cursor -> 400.
    let (s, _r) = get_json(
        &router,
        "/audit/events?cursor=not-a-uuid",
        "tenant_a",
        "user_1",
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "invalid cursor -> 400");
    println!("(e) invalid cursor -> 400");

    println!("audit log: records governed actions, tenant-scoped query, action filter, keyset pagination, cursor validation all proven.");
}
