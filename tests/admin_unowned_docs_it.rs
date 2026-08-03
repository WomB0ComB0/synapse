//! Live integration test: an admin may manage the ACL of an UNOWNED document.
//!
//! `documents.grant`/`revoke` are owner-scoped. An owner-less document that carries a grant
//! previously had NO manager via this path (a non-owner is 404'd, and there is no owner) — it could
//! only be fixed by re-ingesting with owners. This lets an **admin** grant/revoke on any UNOWNED doc
//! (owners = {}), so an orphaned/owner-less doc is always governable — while an admin still may NOT
//! seize an OWNED doc's ACL (ownership is respected; admin power here is scoped to unowned docs).
//!
//! **DB-gated:** skipped unless `DATABASE_URL` is set. Run locally:
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=postgres -p 5476:5432 pgvector/pgvector:pg16
//! export DATABASE_URL=postgres://postgres:postgres@localhost:5476/postgres
//! cargo test --test admin_unowned_docs_it -- --nocapture
//! ```

mod common;

use axum::http::StatusCode;
use axum::Router;
use common::{post_json, provision_role, setup_with_db};
use serde_json::json;

async fn ingest(
    router: &Router,
    tenant: &str,
    caller: &str,
    doc_id: &str,
    owners: serde_json::Value,
) {
    let (s, r) = post_json(
        router,
        "/documents.ingest",
        tenant,
        caller,
        json!({ "doc_id": doc_id, "tenant_id": tenant, "owners": owners, "content": "marker content" }),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "ingest {doc_id}: {r}");
}

async fn grant(
    router: &Router,
    tenant: &str,
    caller: &str,
    doc_id: &str,
    group: &str,
) -> StatusCode {
    post_json(
        router,
        "/documents.grant",
        tenant,
        caller,
        json!({ "doc_id": doc_id, "grantee_type": "group", "grantee_id": group }),
    )
    .await
    .0
}

async fn revoke(
    router: &Router,
    tenant: &str,
    caller: &str,
    doc_id: &str,
    group: &str,
) -> StatusCode {
    post_json(
        router,
        "/documents.revoke",
        tenant,
        caller,
        json!({ "doc_id": doc_id, "grantee_type": "group", "grantee_id": group }),
    )
    .await
    .0
}

#[tokio::test]
async fn admin_manages_unowned_documents() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };
    let t = "tenant_a";
    let (router, admin) = setup_with_db(&base_url, "adminunowned").await;
    provision_role(&admin, t, "boss", "admin").await;

    // An UNOWNED doc. Truly-public at first (no owners, no grants), so a Member may add the FIRST
    // grant — after which it is owner-less BUT restricted (unowned-but-granted).
    ingest(&router, t, "ingester", "orphan", json!([])).await;
    assert_eq!(
        grant(&router, t, "mallory", "orphan", "squad").await,
        StatusCode::OK,
        "the first grant on a truly-public doc is allowed for any member"
    );

    // --- (a) now unowned-but-granted: a non-admin Member can no longer manage it (404, no oracle) ---
    assert_eq!(
        grant(&router, t, "mallory", "orphan", "eng").await,
        StatusCode::NOT_FOUND,
        "a non-admin cannot manage an unowned-but-granted doc (self-grant fence holds)"
    );
    println!(
        "(a) a non-admin is 404'd on an unowned-but-granted doc (the self-grant fence still holds)"
    );

    // --- (b) an ADMIN may manage the same unowned doc (the new capability) ---
    assert_eq!(
        grant(&router, t, "boss", "orphan", "eng").await,
        StatusCode::OK,
        "an admin CAN grant on an unowned (owner-less) doc"
    );
    assert_eq!(
        revoke(&router, t, "boss", "orphan", "squad").await,
        StatusCode::OK,
        "an admin CAN revoke on an unowned doc"
    );
    println!(
        "(b) an admin grants + revokes on an unowned (owner-less) doc — it is always governable"
    );

    // --- (c) an admin may NOT seize an OWNED doc's ACL (admin power is scoped to unowned) ---
    ingest(&router, t, "ingester", "owned", json!(["carol"])).await;
    assert_eq!(
        grant(&router, t, "boss", "owned", "eng").await,
        StatusCode::NOT_FOUND,
        "an admin may NOT manage an OWNED doc it does not own (same 404, no oracle)"
    );
    // ...but the owner still can (regression).
    assert_eq!(
        grant(&router, t, "carol", "owned", "eng").await,
        StatusCode::OK,
        "the owner still manages their own doc"
    );
    println!("(c) an admin is 404'd on an OWNED doc it doesn't own; the owner still manages it");

    admin.close().await;
    println!("admin-manages-unowned-docs: an admin governs any owner-less doc (incl. unowned-but-granted); an owned doc stays owner-managed; non-admins keep the self-grant fence + 404 no-oracle.");
}
