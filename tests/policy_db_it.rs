//! Integration test for PolicyGateway PR2 — DB-authoritative roles + role_permissions.
//!
//! Proves, end-to-end through the real router (with an admin/superuser pool used
//! ONLY to seed roles + grants, bypassing RLS the way an operator would):
//!   (a) `principals.role` is authoritative OVER the `X-Role` header — a DB `viewer`
//!       is denied writes even when the request asserts `X-Role: member`;
//!   (b) `role_permissions`, when populated for a role, REPLACES the in-code default
//!       matrix for that role — a tenant can restrict `member` below the default;
//!   (c) that override is PER-TENANT — the same identity in a tenant with no rows
//!       keeps the default;
//!   (d) `role_permissions` can also BROADEN a role, and stays authoritative — a
//!       granted action is allowed while a default action NOT in the allowlist is
//!       denied (replace, not merge).
//!
//! **DB-gated:** skipped unless `DATABASE_URL` is set. Run locally:
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=postgres -p 5459:5432 pgvector/pgvector:pg16
//! export DATABASE_URL=postgres://postgres:postgres@localhost:5459/postgres
//! cargo test --test policy_db_it -- --nocapture
//! ```

mod common;

use axum::http::StatusCode;
use common::{post_json_with_role, setup, swap_db};
use serde_json::json;
use sqlx::postgres::PgPoolOptions;

#[tokio::test]
async fn db_authoritative_roles_and_role_permissions() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };
    let router = setup(&base_url, "policydb").await;
    // Superuser pool (bypasses RLS) to seed roles + grants, exactly as an operator
    // would provision them out-of-band. tenant_a/tenant_b are seeded by apply_schema.
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&swap_db(&base_url, "synapse_it_policydb"))
        .await
        .expect("connect admin pool");

    // Give db_viewer an explicit DB role of `viewer` in tenant_a.
    sqlx::query(
        "INSERT INTO principals (tenant_id, principal_id, role) VALUES ($1, $2, $3)
         ON CONFLICT (tenant_id, principal_id) DO UPDATE SET role = EXCLUDED.role",
    )
    .bind("tenant_a")
    .bind("db_viewer")
    .bind("viewer")
    .execute(&admin)
    .await
    .expect("seed db_viewer role");

    // (a) principals.role WINS over the X-Role header: db_viewer is a DB `viewer`,
    //     so a write is denied even though the request claims `X-Role: member`.
    let ingest = json!({"doc_id": "d1", "tenant_id": "tenant_a", "content": "x"});
    let (s, r) = post_json_with_role(
        &router,
        "/documents.ingest",
        "tenant_a",
        "db_viewer",
        Some("member"),
        ingest.clone(),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::FORBIDDEN,
        "DB role viewer must beat header member: {r}"
    );
    // ...and with no header at all, still a DB viewer -> denied.
    let (s, _) = post_json_with_role(
        &router,
        "/documents.ingest",
        "tenant_a",
        "db_viewer",
        None,
        ingest,
    )
    .await;
    assert_eq!(
        s,
        StatusCode::FORBIDDEN,
        "DB viewer denied write (no header)"
    );
    // A read is still allowed under the default viewer matrix (no role_permissions yet).
    let (s, _) = post_json_with_role(
        &router,
        "/context.get",
        "tenant_a",
        "db_viewer",
        None,
        json!({"principal_id": "whoever"}),
    )
    .await;
    assert_ne!(
        s,
        StatusCode::FORBIDDEN,
        "DB viewer read allowed by default"
    );
    println!("(a) principals.role is authoritative over X-Role (DB viewer beats header member)");

    // (b) role_permissions REPLACES the default for a role: restrict `member` in
    //     tenant_a to retrieve-only. A plain member (unprovisioned -> Member baseline)
    //     can now no longer ingest, but may still retrieve.
    sqlx::query("INSERT INTO role_permissions (tenant_id, role, action) VALUES ('tenant_a','member','retrieve')")
        .execute(&admin)
        .await
        .expect("seed member->retrieve");
    let (s, r) = post_json_with_role(
        &router,
        "/documents.ingest",
        "tenant_a",
        "plain_member",
        None,
        json!({"doc_id": "d2", "tenant_id": "tenant_a", "content": "x"}),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::FORBIDDEN,
        "member restricted to retrieve-only cannot ingest: {r}"
    );
    let (s, r) = post_json_with_role(
        &router,
        "/retrieve",
        "tenant_a",
        "plain_member",
        None,
        json!({"tenant_id": "tenant_a", "principal_id": "plain_member", "query": "hello"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "member retrieve still granted: {r}");
    println!("(b) role_permissions replaces the default: member restricted to retrieve-only");

    // (c) the override is PER-TENANT: the same identity in tenant_b (no rows) keeps
    //     the default Member -> full access.
    let (s, r) = post_json_with_role(
        &router,
        "/documents.ingest",
        "tenant_b",
        "plain_member",
        None,
        json!({"doc_id": "d3", "tenant_id": "tenant_b", "content": "x"}),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "tenant_b member unaffected by tenant_a's override: {r}"
    );
    println!("(c) the override is per-tenant (tenant_b member keeps default full access)");

    // (d) role_permissions can BROADEN too and stays authoritative (replace, not
    //     merge): grant the viewer role tool.execute in tenant_a. db_viewer may now
    //     tool.execute, but a default read NOT in the allowlist is denied.
    sqlx::query("INSERT INTO role_permissions (tenant_id, role, action) VALUES ('tenant_a','viewer','tool.execute')")
        .execute(&admin)
        .await
        .expect("seed viewer->tool.execute");
    let (s, _) = post_json_with_role(
        &router,
        "/tool.execute",
        "tenant_a",
        "db_viewer",
        None,
        json!({"tenant_id": "tenant_a", "principal_id": "db_viewer", "tool_id": "search"}),
    )
    .await;
    assert_ne!(
        s,
        StatusCode::FORBIDDEN,
        "viewer granted tool.execute via role_permissions"
    );
    let (s, _) = post_json_with_role(
        &router,
        "/context.get",
        "tenant_a",
        "db_viewer",
        None,
        json!({"principal_id": "whoever"}),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::FORBIDDEN,
        "replace semantics: viewer allowlist {{tool.execute}} excludes the default read context.get"
    );
    println!("(d) role_permissions broadens too and is authoritative (grant tool.execute; default read now denied)");

    // (e) A principal whose principals.role holds a NON-RBAC DOMAIN value (the
    //     schema's own example, "manager") is NOT an RBAC assignment: it must be
    //     inert (fall back to header/default), never silently downgraded to Viewer.
    //     This guards the PR1->PR2 upgrade path for principals provisioned with a
    //     domain role.
    sqlx::query(
        "INSERT INTO principals (tenant_id, principal_id, role) VALUES ($1, $2, $3)
         ON CONFLICT (tenant_id, principal_id) DO UPDATE SET role = EXCLUDED.role",
    )
    .bind("tenant_b") // tenant_b has no role_permissions rows -> pure default matrix
    .bind("domain_mgr")
    .bind("manager")
    .execute(&admin)
    .await
    .expect("seed domain role");
    // No X-Role header: domain role is inert -> Member baseline -> write allowed.
    let (s, r) = post_json_with_role(
        &router,
        "/documents.ingest",
        "tenant_b",
        "domain_mgr",
        None,
        json!({"doc_id": "d4", "tenant_id": "tenant_b", "content": "x"}),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "a domain role ('manager') must NOT be downgraded to Viewer: {r}"
    );
    // And the X-Role header still governs a domain-role principal (fallback path):
    // X-Role: viewer -> Viewer -> write denied.
    let (s, _) = post_json_with_role(
        &router,
        "/documents.ingest",
        "tenant_b",
        "domain_mgr",
        Some("viewer"),
        json!({"doc_id": "d5", "tenant_id": "tenant_b", "content": "x"}),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::FORBIDDEN,
        "domain-role principal still honors the X-Role fallback header"
    );
    println!(
        "(e) a non-RBAC domain role ('manager') is inert (Member baseline), not a silent downgrade"
    );

    admin.close().await;
    println!("PolicyGateway PR2: principals.role authoritative when it names an RBAC role (else header/default); role_permissions per-tenant replace of the default matrix.");
}
