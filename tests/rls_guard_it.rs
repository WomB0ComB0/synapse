//! Integration test for the RLS-enforcement guard (`db::assert_rls_enforcing`
//! + fail-closed `/ready`).
//!
//! **DB-gated:** skipped unless `DATABASE_URL` is set (CI stays database-free).
//! A privileged DB role (SUPERUSER or BYPASSRLS) silently voids every RLS
//! policy — even `FORCE ROW LEVEL SECURITY` — so the guard must refuse to run
//! as one. Run locally:
//!
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=postgres -p 5459:5432 pgvector/pgvector:pg16
//! export DATABASE_URL=postgres://postgres:postgres@localhost:5459/postgres
//! cargo test --test rls_guard_it -- --nocapture
//! ```

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{app_pool, apply_schema, router_for, TestDb};
use http_body_util::BodyExt;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tower::ServiceExt;

use synapse::db::{self, tenant_tx, RlsCheckError};

#[tokio::test]
async fn rls_guard_rejects_privileged_roles_and_ready_fails_closed() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };

    // Fresh DB + the normal non-superuser app role, plus a BYPASSRLS role.
    let test_db = TestDb::create(&base_url, "rlsguard").await;
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&test_db.url)
        .await
        .expect("connect admin pool");
    apply_schema(&admin, &test_db.role).await;
    let bypass_role = "synapse_bypass_rlsguard";
    sqlx::raw_sql(&format!(
        "DROP ROLE IF EXISTS {bypass_role};
         CREATE ROLE {bypass_role} NOLOGIN BYPASSRLS;
         GRANT USAGE ON SCHEMA public TO {bypass_role};
         GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO {bypass_role};"
    ))
    .execute(&admin)
    .await
    .expect("create bypassrls role");
    // (admin stays open — case (e) uses it to make the app role own a table.)

    // (a) the normal non-superuser app role enforces RLS -> Ok; /ready -> 200.
    let ok_pool = app_pool(&test_db.url, &test_db.role).await;
    db::assert_rls_enforcing(&ok_pool)
        .await
        .expect("non-privileged app role must pass the guard");
    let ready = router_for(ok_pool, &test_db.url)
        .oneshot(
            Request::builder()
                .uri("/ready")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        ready.status(),
        StatusCode::OK,
        "non-privileged role -> /ready 200"
    );
    println!("(a) non-superuser app role -> guard Ok, /ready 200");

    // (b) the base superuser (`postgres`, no SET ROLE) is caught via rolsuper.
    let super_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&test_db.url)
        .await
        .expect("connect superuser pool");
    match db::assert_rls_enforcing(&super_pool).await {
        Err(RlsCheckError::Privileged(_)) => {}
        other => panic!("superuser must be rejected as Privileged, got {other:?}"),
    }
    super_pool.close().await;
    println!("(b) superuser role -> guard rejects as Privileged");

    // (c) a BYPASSRLS role is caught via rolbypassrls; /ready fails closed (503).
    let bypass_pool = app_pool(&test_db.url, bypass_role).await;
    match db::assert_rls_enforcing(&bypass_pool).await {
        Err(RlsCheckError::Privileged(role)) => assert_eq!(role, bypass_role),
        other => panic!("BYPASSRLS role must be rejected as Privileged, got {other:?}"),
    }
    let ready = router_for(bypass_pool, &test_db.url)
        .oneshot(
            Request::builder()
                .uri("/ready")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        ready.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "BYPASSRLS role -> /ready fails closed (503)"
    );
    let bytes = ready.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["status"], "unavailable");
    println!("(c) BYPASSRLS role -> guard rejects as Privileged, /ready 503");

    // (d) tenant_tx gate (the true data chokepoint): a BYPASSRLS role is refused
    //     BEFORE any tenant query runs, while the non-privileged role opens a tx
    //     normally — so enforcement holds on every request, not only via /ready.
    //     (bypass is checked first: the good role latches the process-wide flag.)
    let bypass_pool2 = app_pool(&test_db.url, bypass_role).await;
    assert!(
        tenant_tx(&bypass_pool2, "tenant_a").await.is_err(),
        "tenant_tx must refuse a BYPASSRLS role before any tenant query"
    );
    let good_pool2 = app_pool(&test_db.url, &test_db.role).await;
    tenant_tx(&good_pool2, "tenant_a")
        .await
        .expect("tenant_tx must open under the non-privileged role");
    println!("(d) tenant_tx gate: BYPASSRLS refused before any tenant query; good role opens a tx");

    // (e) owner bypass: make the good app role OWN a table with RLS enabled but
    //     NOT forced (owner is exempt from un-FORCEd RLS) -> the guard rejects it.
    sqlx::raw_sql(&format!(
        "CREATE TABLE app_owned (tenant_id text NOT NULL, x int);
         ALTER TABLE app_owned OWNER TO {role};
         ALTER TABLE app_owned ENABLE ROW LEVEL SECURITY;",
        role = test_db.role
    ))
    .execute(&admin)
    .await
    .expect("create app-owned unforced-RLS table");
    let owner_pool: PgPool = app_pool(&test_db.url, &test_db.role).await;
    match db::assert_rls_enforcing(&owner_pool).await {
        Err(RlsCheckError::OwnerBypass { role, tables }) => {
            assert_eq!(role, test_db.role);
            assert!(
                tables.contains(&"app_owned".to_string()),
                "must name the offending table: {tables:?}"
            );
        }
        other => panic!("owning an unforced-RLS table must be OwnerBypass, got {other:?}"),
    }
    println!("(e) owner of an RLS-enabled-but-unforced table -> guard rejects as OwnerBypass");

    // (f) INHERITED owner bypass: a role that is a member (INHERIT) of a group
    //     that OWNS an unforced-RLS table inherits the owner's RLS bypass, so the
    //     guard must match via pg_has_role(USAGE), not direct ownership equality.
    let group_role = "synapse_grp_rlsguard";
    let member_role = "synapse_member_rlsguard";
    sqlx::raw_sql(&format!(
        "DROP ROLE IF EXISTS {member};
         DROP ROLE IF EXISTS {group};
         CREATE ROLE {group} NOLOGIN;
         CREATE TABLE grp_owned (tenant_id text NOT NULL, x int);
         ALTER TABLE grp_owned OWNER TO {group};
         ALTER TABLE grp_owned ENABLE ROW LEVEL SECURITY;
         CREATE ROLE {member} NOLOGIN INHERIT;
         GRANT {group} TO {member};",
        group = group_role,
        member = member_role
    ))
    .execute(&admin)
    .await
    .expect("create group + inheriting member roles");
    let member_pool = app_pool(&test_db.url, member_role).await;
    match db::assert_rls_enforcing(&member_pool).await {
        Err(RlsCheckError::OwnerBypass { tables, .. }) => assert!(
            tables.contains(&"grp_owned".to_string()),
            "inherited owner must flag the group-owned table: {tables:?}"
        ),
        other => panic!("inherited owner bypass must be OwnerBypass, got {other:?}"),
    }
    admin.close().await;
    println!("(f) member of a group owning an unforced-RLS table -> OwnerBypass (inherited)");

    println!("RLS guard: superuser/bypassrls/(inherited-)owner-bypass roles refused; non-privileged role serves; tenant_tx gate + /ready both fail closed.");
}
