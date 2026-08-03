//! Integration test for the team + grant management API (end-to-end through retrieval).
//!
//! Proves the write paths that activate document ACLs: /documents.grant + /revoke
//! change what a caller retrieves, and /teams.add_member + /remove_member gate a
//! `group` grant (retrieval resolves it via team_members). RBAC-gated (Viewer denied)
//! and ownership-scoped (only a doc's owner may manage its ACL; a non-owner gets 404,
//! the same as a missing doc — no existence oracle), idempotent.
//!
//! **DB-gated:** skipped unless `DATABASE_URL` is set. Run locally:
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=postgres -p 5459:5432 pgvector/pgvector:pg16
//! export DATABASE_URL=postgres://postgres:postgres@localhost:5459/postgres
//! cargo test --test team_grant_it -- --nocapture
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
async fn team_and_grant_management_api() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };
    let (router, admin) = setup_with_db(&base_url, "teamgrant").await;
    // Team creation requires a DB-authoritative admin (the `admin` role is not
    // header-assertable), so provision the `admin` caller with principals.role='admin'.
    common::provision_role(&admin, "tenant_a", "admin", "admin").await;
    let q = "quarterly budget report";

    // Two RESTRICTED docs (owners set, so NOT tenant-public); bob/erin see neither.
    for doc_id in ["budget", "eng_doc"] {
        let (s, _) = post_json(
            &router,
            "/documents.ingest",
            "tenant_a",
            "ingester",
            json!({"doc_id": doc_id, "tenant_id": "tenant_a", "owners": ["owner_x"],
                   "content": format!("quarterly budget report — marker {doc_id}")}),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "ingest {doc_id}");
    }
    assert!(
        retrieve_doc_ids(&router, "bob", q).await.is_empty(),
        "bob sees neither restricted doc initially"
    );

    // --- user grant: owner_x (the doc's owner) grants budget -> bob, revoke, retrieve ---
    // Management is ownership-scoped, so the grant is issued AS the owner (owner_x).
    let (s, r) = post_json(
        &router,
        "/documents.grant",
        "tenant_a",
        "owner_x",
        json!({"doc_id": "budget", "grantee_type": "user", "grantee_id": "bob"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "grant: {r}");
    assert_eq!(r["status"], "granted");
    assert_eq!(r["permission"], "read");
    assert!(
        retrieve_doc_ids(&router, "bob", q).await.contains("budget"),
        "bob sees budget after the grant"
    );
    // idempotent re-grant.
    let (s, _) = post_json(
        &router,
        "/documents.grant",
        "tenant_a",
        "owner_x",
        json!({"doc_id": "budget", "grantee_type": "user", "grantee_id": "bob"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "re-grant is idempotent");
    // revoke (also as the owner).
    let (s, r) = post_json(
        &router,
        "/documents.revoke",
        "tenant_a",
        "owner_x",
        json!({"doc_id": "budget", "grantee_type": "user", "grantee_id": "bob"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "revoke: {r}");
    assert_eq!(r["status"], "revoked");
    assert!(
        !retrieve_doc_ids(&router, "bob", q).await.contains("budget"),
        "bob loses budget after revoke"
    );
    println!("(a) documents.grant/revoke change retrieval for a user grant");

    // --- group grant + team membership: grant eng_doc -> group eng ---
    let (s, _) = post_json(
        &router,
        "/documents.grant",
        "tenant_a",
        "owner_x",
        json!({"doc_id": "eng_doc", "grantee_type": "group", "grantee_id": "eng"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    // erin isn't in team eng yet -> can't see it.
    assert!(
        !retrieve_doc_ids(&router, "erin", q)
            .await
            .contains("eng_doc"),
        "erin not in eng yet"
    );
    // add erin to eng. Creating the team requires the admin role (team authority).
    let (s, r) = post_json_with_role(
        &router,
        "/teams.add_member",
        "tenant_a",
        "admin",
        Some("admin"),
        json!({"team_id": "eng", "principal_id": "erin", "role": "member"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "add_member: {r}");
    assert_eq!(r["status"], "added");
    assert!(
        retrieve_doc_ids(&router, "erin", q)
            .await
            .contains("eng_doc"),
        "erin sees eng_doc after joining eng"
    );
    // remove erin -> loses access (as the team's admin/owner).
    let (s, r) = post_json_with_role(
        &router,
        "/teams.remove_member",
        "tenant_a",
        "admin",
        Some("admin"),
        json!({"team_id": "eng", "principal_id": "erin"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "remove_member: {r}");
    assert!(
        !retrieve_doc_ids(&router, "erin", q)
            .await
            .contains("eng_doc"),
        "erin loses eng_doc after leaving eng"
    );
    println!("(b) teams.add_member/remove_member gate a group grant end-to-end");

    // --- RBAC: a Viewer is denied every management op ---
    for (uri, body) in [
        (
            "/documents.grant",
            json!({"doc_id": "budget", "grantee_type": "user", "grantee_id": "x"}),
        ),
        (
            "/documents.revoke",
            json!({"doc_id": "budget", "grantee_type": "user", "grantee_id": "x"}),
        ),
        (
            "/teams.add_member",
            json!({"team_id": "eng", "principal_id": "x"}),
        ),
        (
            "/teams.remove_member",
            json!({"team_id": "eng", "principal_id": "x"}),
        ),
    ] {
        let (s, r) = post_json_with_role(&router, uri, "tenant_a", "v", Some("viewer"), body).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "viewer {uri} -> 403: {r}");
        assert_eq!(r["error"]["code"], "forbidden");
    }
    println!("(c) a Viewer is denied grant/revoke/add_member/remove_member (403)");

    // --- validation: grant to a missing doc -> 404; bad grantee_type -> 400 ---
    let (s, _) = post_json(
        &router,
        "/documents.grant",
        "tenant_a",
        "admin",
        json!({"doc_id": "ghost", "grantee_type": "user", "grantee_id": "bob"}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "grant on a missing doc -> 404");
    let (s, _) = post_json(
        &router,
        "/documents.grant",
        "tenant_a",
        "admin",
        json!({"doc_id": "budget", "grantee_type": "nonsense", "grantee_id": "bob"}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "bad grantee_type -> 400");
    println!("(d) grant on a missing doc -> 404; invalid grantee_type -> 400");

    // --- ownership scoping: a NON-OWNER Member cannot self-grant (bypass closed) ---
    // mallory (no X-Role => the default Member, and NOT an owner of `budget`) tries to
    // grant herself read. The ownership gate returns the SAME 404 as a missing doc (so
    // she can't even probe existence), and she still cannot retrieve the doc — the
    // self-grant bypass of the read ACL is closed.
    let (s, r) = post_json(
        &router,
        "/documents.grant",
        "tenant_a",
        "mallory",
        json!({"doc_id": "budget", "grantee_type": "user", "grantee_id": "mallory"}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "non-owner self-grant -> 404: {r}");
    assert!(
        !retrieve_doc_ids(&router, "mallory", q)
            .await
            .contains("budget"),
        "mallory still cannot read budget (self-grant bypass is closed)"
    );
    println!("(e) a non-owner Member cannot self-grant on an owned doc (404; bypass closed)");

    // --- (f) owner-LESS but ACL-restricted doc: a non-owner still can't self-grant ---
    // A doc with a group grant but NO owners is restricted (readable only by the group)
    // yet has no owner. It must NOT be freely self-granted — treat it like any restricted
    // doc (a non-owner gets 404, not a self-grant + read).
    let (s, _) = post_json(
        &router,
        "/documents.ingest",
        "tenant_a",
        "ingester",
        json!({"doc_id": "grouponly", "tenant_id": "tenant_a", "acl": {"groups": ["special"]},
               "content": "quarterly budget report — marker grouponly"}),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "ingest grouponly (owner-less, group-restricted)"
    );
    let (s, _) = post_json(
        &router,
        "/documents.grant",
        "tenant_a",
        "mallory",
        json!({"doc_id": "grouponly", "grantee_type": "user", "grantee_id": "mallory"}),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::NOT_FOUND,
        "non-owner self-grant on an owner-less restricted doc -> 404"
    );
    assert!(
        !retrieve_doc_ids(&router, "mallory", q)
            .await
            .contains("grouponly"),
        "mallory cannot read grouponly (owner-less-restricted self-grant closed)"
    );
    println!(
        "(f) an owner-less ACL-restricted doc still can't be self-granted by a non-owner (404)"
    );

    println!("team + grant API: grant/revoke + add_member/remove_member drive document-ACL visibility; ownership-scoped; RBAC-gated; validated.");
}
