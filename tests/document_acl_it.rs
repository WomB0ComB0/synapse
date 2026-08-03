//! Integration test for document ACL in retrieval.
//!
//! /retrieve returns a chunk only if the caller may READ its document. A document
//! is readable when it is UNRESTRICTED (no owners, no document_acl grants — the
//! tenant-public default), OR the AUTHENTICATED caller is an owner, OR a document_acl
//! grant matches: a `user` grant = their principal_id, a `group` grant = a team they
//! belong to PER `team_members` (the authoritative source — NOT the self-asserted
//! X-Team-Ids header or request scope). Existing docs (empty owners/acl) stay public.
//!
//! **DB-gated:** skipped unless `DATABASE_URL` is set. Run locally:
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=postgres -p 5459:5432 pgvector/pgvector:pg16
//! export DATABASE_URL=postgres://postgres:postgres@localhost:5459/postgres
//! cargo test --test document_acl_it -- --nocapture
//! ```

mod common;

use std::collections::HashSet;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use common::{post_json, setup, swap_db};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;

/// POST /retrieve as `caller` (optionally sending X-Team-Ids `teams` and/or a
/// request-body `scope`); return the SET of doc_ids in the results.
async fn retrieve_doc_ids(
    router: &Router,
    caller: &str,
    teams: &[&str],
    scope: Value,
    query: &str,
) -> HashSet<String> {
    let mut b = Request::builder()
        .method("POST")
        .uri("/retrieve")
        .header("content-type", "application/json")
        .header("x-principal-id", caller)
        .header("x-tenant-id", "tenant_a");
    if !teams.is_empty() {
        b = b.header("x-team-ids", teams.join(","));
    }
    let mut body = json!({"tenant_id": "tenant_a", "principal_id": caller, "query": query});
    if !scope.is_null() {
        body["scope"] = scope;
    }
    let resp = router
        .clone()
        .oneshot(b.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "retrieve as {caller} -> 200");
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
async fn retrieval_enforces_document_acl() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };
    let router = setup(&base_url, "docacl").await;
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&swap_db(&base_url, "synapse_it_docacl"))
        .await
        .expect("connect admin pool");
    let q = "quarterly budget report";

    // Four docs in tenant_a, all matching the same query, differing only by access.
    let docs = [
        ("pub", json!({})),
        ("u_alice", json!({ "acl": { "users": ["alice"] } })),
        ("g_eng", json!({ "acl": { "groups": ["eng"] } })),
        ("own_carol", json!({ "owners": ["carol"] })),
    ];
    for (doc_id, access) in &docs {
        let mut body = json!({
            "doc_id": doc_id,
            "tenant_id": "tenant_a",
            "content": format!("quarterly budget report — marker {doc_id}"),
        });
        for (k, v) in access.as_object().unwrap() {
            body[k] = v.clone();
        }
        let (s, r) = post_json(&router, "/documents.ingest", "tenant_a", "ingester", body).await;
        assert_eq!(s, StatusCode::OK, "ingest {doc_id}: {r}");
    }

    // erin is a MEMBER of team `eng` per team_members (the authoritative source).
    sqlx::query("INSERT INTO principals (tenant_id, principal_id) VALUES ('tenant_a','erin') ON CONFLICT DO NOTHING")
        .execute(&admin).await.unwrap();
    sqlx::query("INSERT INTO teams (tenant_id, team_id, name) VALUES ('tenant_a','eng','Engineering') ON CONFLICT DO NOTHING")
        .execute(&admin).await.unwrap();
    sqlx::query("INSERT INTO team_members (tenant_id, team_id, principal_id) VALUES ('tenant_a','eng','erin') ON CONFLICT DO NOTHING")
        .execute(&admin).await.unwrap();

    // (a) alice: public + her own user-granted doc; NOT the eng-group or carol docs.
    let alice = retrieve_doc_ids(&router, "alice", &[], Value::Null, q).await;
    assert!(
        alice.contains("pub") && alice.contains("u_alice"),
        "alice: {alice:?}"
    );
    assert!(
        !alice.contains("g_eng") && !alice.contains("own_carol"),
        "alice: {alice:?}"
    );
    println!("(a) alice sees public + her user grant only");

    // (b) bob (no grants): ONLY the public doc.
    let bob = retrieve_doc_ids(&router, "bob", &[], Value::Null, q).await;
    assert_eq!(bob, HashSet::from(["pub".to_string()]), "bob: {bob:?}");
    println!("(b) bob (no grants) sees only the public doc");

    // (c) erin is in team `eng` per team_members -> sees public + the group-granted
    //     doc, WITHOUT sending any X-Team-Ids header (membership is DB-sourced).
    let erin = retrieve_doc_ids(&router, "erin", &[], Value::Null, q).await;
    assert!(
        erin.contains("pub") && erin.contains("g_eng"),
        "erin: {erin:?}"
    );
    assert!(
        !erin.contains("u_alice") && !erin.contains("own_carol"),
        "erin: {erin:?}"
    );
    println!("(c) erin (team_members ∈ eng) sees public + the group grant — no header needed");

    // (c2) SECURITY: a caller who is NOT in team_members but SPOOFS X-Team-Ids: eng
    //      does NOT gain the group-granted doc — grants come from team_members, never
    //      the self-asserted header.
    let spoof_hdr = retrieve_doc_ids(&router, "mallory", &["eng"], Value::Null, q).await;
    assert!(
        !spoof_hdr.contains("g_eng"),
        "spoofed X-Team-Ids must NOT grant the group doc: {spoof_hdr:?}"
    );
    // ...and neither does a spoofed request-body scope (which is only a fence).
    let spoof_scope =
        retrieve_doc_ids(&router, "mallory", &[], json!({ "team_ids": ["eng"] }), q).await;
    assert!(
        !spoof_scope.contains("g_eng"),
        "spoofed scope: {spoof_scope:?}"
    );
    println!("(c2) a spoofed X-Team-Ids / scope does NOT grant the group doc (membership is DB-authoritative)");

    // (d) carol: public + her owned doc (owner always reads).
    let carol = retrieve_doc_ids(&router, "carol", &[], Value::Null, q).await;
    assert!(
        carol.contains("pub") && carol.contains("own_carol"),
        "carol: {carol:?}"
    );
    assert!(
        !carol.contains("u_alice") && !carol.contains("g_eng"),
        "carol: {carol:?}"
    );
    println!("(d) owner carol sees public + her owned doc only");

    // (e) re-ingest safety: a CONTENT-ONLY re-ingest of a restricted doc (omitting
    //     acl/owners) must PRESERVE its grants, not silently republish it.
    let (s, _) = post_json(
        &router,
        "/documents.ingest",
        "tenant_a",
        "ingester",
        json!({"doc_id": "u_alice", "tenant_id": "tenant_a", "content": "quarterly budget report — marker u_alice v2"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "re-ingest u_alice content-only");
    let bob2 = retrieve_doc_ids(&router, "bob", &[], Value::Null, q).await;
    assert!(
        !bob2.contains("u_alice"),
        "re-ingest must not republish to bob: {bob2:?}"
    );
    let alice2 = retrieve_doc_ids(&router, "alice", &[], Value::Null, q).await;
    assert!(
        alice2.contains("u_alice"),
        "alice keeps her grant after re-ingest: {alice2:?}"
    );
    println!("(e) a content-only re-ingest preserves grants (no silent republish)");

    admin.close().await;
    println!("document ACL: retrieval returns only readable docs (public/owner/user/group); group membership is DB-authoritative (team_members); re-ingest is preserve-on-empty.");
}
