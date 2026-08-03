//! Integration test for team-membership authority (admin-authorized team management).
//!
//! Proves the #24 residual AND team-namespace squatting are closed: only an `admin` may
//! CREATE a team, and only a team's owner OR an admin may manage its membership. A
//! non-admin Member can therefore neither pre-create a `team_id` (squatting) nor self-join
//! an existing team it doesn't own — so a document-ACL `group` grant confers access only
//! to the members an admin/owner actually put in the team.
//!
//! **DB-gated:** skipped unless `DATABASE_URL` is set. Run locally:
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=postgres -p 5460:5432 pgvector/pgvector:pg16
//! export DATABASE_URL=postgres://postgres:postgres@localhost:5460/postgres
//! cargo test --test team_authority_it -- --nocapture
//! ```

mod common;

use std::collections::HashSet;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use common::{post_json, post_json_with_role, setup_with_db};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

async fn retrieve_doc_ids(router: &Router, caller: &str, query: &str) -> HashSet<String> {
    let body = json!({"tenant_id": "tenant_a", "principal_id": caller, "query": query});
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/retrieve")
                .header("content-type", "application/json")
                .header("x-principal-id", caller)
                .header("x-tenant-id", "tenant_a")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "retrieve as {caller}");
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let val: Value = serde_json::from_slice(&bytes).unwrap();
    val["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["doc_id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn team_membership_authority() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };
    let (router, admin) = setup_with_db(&base_url, "teamauth").await;
    // Team creation/management authority is a DB-authoritative admin (not header-
    // assertable), so provision the admin callers with principals.role='admin'.
    common::provision_role(&admin, "tenant_a", "boss", "admin").await;
    common::provision_role(&admin, "tenant_a", "boss2", "admin").await;
    let q = "architecture spec";

    // A restricted doc owned by owner_x, granted (by its owner) to the `squad` group.
    let (s, _) = post_json(
        &router,
        "/documents.ingest",
        "tenant_a",
        "ingester",
        json!({"doc_id": "spec", "tenant_id": "tenant_a", "owners": ["owner_x"],
               "content": "architecture spec — marker spec"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "ingest spec");
    let (s, _) = post_json(
        &router,
        "/documents.grant",
        "tenant_a",
        "owner_x",
        json!({"doc_id": "spec", "grantee_type": "group", "grantee_id": "squad"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "owner_x grants group squad");

    // --- (a) a non-admin Member cannot CREATE a team (squatting closed) ---
    // mallory (default Member) tries to pre-create `squad` -> 403; she gains nothing.
    let (s, r) = post_json(
        &router,
        "/teams.add_member",
        "tenant_a",
        "mallory",
        json!({"team_id": "squad", "principal_id": "mallory"}),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "non-admin create -> 403: {r}");
    assert_eq!(r["error"]["code"], "forbidden");
    assert!(
        !retrieve_doc_ids(&router, "mallory", q)
            .await
            .contains("spec"),
        "mallory cannot read spec (could not squat squad)"
    );
    println!("(a) a non-admin Member cannot create/squat a team (403)");

    // --- (b) an admin creates + owns the team ---
    let (s, r) = post_json_with_role(
        &router,
        "/teams.add_member",
        "tenant_a",
        "boss",
        Some("admin"),
        json!({"team_id": "squad", "principal_id": "alice", "role": "member"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "admin creates squad: {r}");
    assert_eq!(r["status"], "added");
    println!("(b) an admin creates + owns the team");

    // --- (c) a non-owner non-admin cannot manage the existing team ---
    let (s, r) = post_json(
        &router,
        "/teams.add_member",
        "tenant_a",
        "mallory",
        json!({"team_id": "squad", "principal_id": "mallory"}),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "non-owner self-join -> 403: {r}");
    assert_eq!(r["error"]["code"], "forbidden");
    assert!(
        !retrieve_doc_ids(&router, "mallory", q)
            .await
            .contains("spec"),
        "mallory still cannot read spec (self-join blocked)"
    );
    println!("(c) a non-owner non-admin cannot self-join an existing team (403)");

    // --- (d) the owner-admin manages membership; it gates the group grant end-to-end ---
    let (s, _) = post_json_with_role(
        &router,
        "/teams.add_member",
        "tenant_a",
        "boss",
        Some("admin"),
        json!({"team_id": "squad", "principal_id": "mallory", "role": "member"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "owner-admin adds mallory");
    assert!(
        retrieve_doc_ids(&router, "mallory", q)
            .await
            .contains("spec"),
        "mallory reads spec once the owner-admin adds her to squad"
    );
    let (s, _) = post_json_with_role(
        &router,
        "/teams.remove_member",
        "tenant_a",
        "boss",
        Some("admin"),
        json!({"team_id": "squad", "principal_id": "mallory"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "owner-admin removes mallory");
    assert!(
        !retrieve_doc_ids(&router, "mallory", q)
            .await
            .contains("spec"),
        "mallory loses spec after the owner-admin removes her"
    );
    println!("(d) the owner-admin may add/remove; membership gates the group grant");

    // --- (e) a DIFFERENT admin (not the owner) may also manage the team ---
    let (s, r) = post_json_with_role(
        &router,
        "/teams.add_member",
        "tenant_a",
        "boss2",
        Some("admin"),
        json!({"team_id": "squad", "principal_id": "dave"}),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "a non-owner admin may manage any team: {r}"
    );
    println!("(e) any admin may manage any team, owner or not");

    // --- (f) a Viewer is denied by RBAC before authority is even considered ---
    let (s, r) = post_json_with_role(
        &router,
        "/teams.add_member",
        "tenant_a",
        "v",
        Some("viewer"),
        json!({"team_id": "squad", "principal_id": "v"}),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "viewer add -> 403: {r}");
    assert_eq!(r["error"]["code"], "forbidden");

    // --- (g) managing a team that doesn't exist -> 404 (even for an admin) ---
    let (s, _) = post_json_with_role(
        &router,
        "/teams.remove_member",
        "tenant_a",
        "boss",
        Some("admin"),
        json!({"team_id": "ghost", "principal_id": "alice"}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "remove on a missing team -> 404");
    println!("(f) Viewer denied by RBAC; (g) manage-missing-team -> 404");

    println!(
        "team authority: admin-only creation + owner/admin management; group grants can't be self-joined or squatted."
    );
}
