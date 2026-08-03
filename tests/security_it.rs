//! Integration test for security hardening items 2 + 4:
//!   - an unknown-tenant FK violation (SQLSTATE 23503) maps to 400, not 500,
//!     across every write path;
//!   - failed governed actions are audited (deny/error), and a failure for an
//!     unprovisioned tenant leaves NO audit row (best-effort FK drop).
//!
//! **DB-gated:** skipped unless `DATABASE_URL` is set. Run locally:
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=postgres -p 5459:5432 pgvector/pgvector:pg16
//! export DATABASE_URL=postgres://postgres:postgres@localhost:5459/postgres
//! cargo test --test security_it -- --nocapture
//! ```

mod common;

use axum::http::StatusCode;
use common::{get_json, post_json, setup, swap_db};
use serde_json::json;
use sqlx::postgres::PgPoolOptions;

#[tokio::test]
async fn unknown_tenant_is_400_and_failures_are_audited() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };
    let router = setup(&base_url, "security").await;

    // (a) Item 2: every write path maps the unknown-tenant FK (23503) to 400
    //     bad_request (not a 500). "ghost" is NOT a seeded tenant.
    let ghost = "ghost";
    let cases: [(&str, serde_json::Value); 5] = [
        (
            "/context.upsert",
            json!({"tenant_id": ghost, "principal_id": "u"}),
        ),
        (
            "/tool.execute",
            json!({"tenant_id": ghost, "principal_id": "u", "tool_id": "t"}),
        ),
        ("/runs.start", json!({"tenant_id": ghost, "run_type": "r"})),
        (
            // Document is #[serde(flatten)] into the request, so its fields are top-level.
            "/documents.ingest",
            json!({"doc_id": "d", "tenant_id": ghost, "content": "hi"}),
        ),
        // skills carries no tenant in the body; the tenant is the authenticated one.
        (
            "/skills.register",
            json!({"skill_id": "s", "version": "1.0.0", "name": "n"}),
        ),
    ];
    for (uri, body) in &cases {
        let (s, r) = post_json(&router, uri, ghost, "u", body.clone()).await;
        assert_eq!(
            s,
            StatusCode::BAD_REQUEST,
            "{uri} unknown tenant -> 400, got {s}: {r}"
        );
        assert_eq!(r["error"]["code"], "bad_request", "{uri}: {r}");
    }
    println!("(a) unknown tenant -> 400 bad_request across all 5 write paths");

    // (b) a seeded tenant still succeeds (proves the mapping didn't break the happy path).
    let (s, r) = post_json(
        &router,
        "/runs.start",
        "tenant_a",
        "u",
        json!({"tenant_id": "tenant_a", "run_type": "ok"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "seeded tenant runs.start: {r}");
    assert_eq!(r["status"], "completed");
    println!("(b) seeded tenant still works (runs.start -> 200 completed)");

    // (c) Item 4: a failed governed action in a SEEDED tenant is audited as `error`.
    //     Registering the same immutable (skill_id, version) twice -> 409 Conflict.
    let reg = json!({"skill_id": "skill.sec", "version": "1.0.0", "name": "Sec"});
    let (s, _) = post_json(&router, "/skills.register", "tenant_a", "u", reg.clone()).await;
    assert_eq!(s, StatusCode::OK, "first register");
    let (s, _) = post_json(&router, "/skills.register", "tenant_a", "u", reg.clone()).await;
    assert_eq!(
        s,
        StatusCode::CONFLICT,
        "re-register immutable version -> 409"
    );
    let (_s, r) = get_json(
        &router,
        "/audit/events?action=skills.register",
        "tenant_a",
        "u",
    )
    .await;
    let outcomes: Vec<&str> = r["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["outcome"].as_str().unwrap())
        .collect();
    assert!(
        outcomes.contains(&"success"),
        "first register audited success: {outcomes:?}"
    );
    assert!(
        outcomes.contains(&"error"),
        "failed re-register audited as error: {outcomes:?}"
    );
    println!("(c) failed governed action audited as `error` (success + error both present)");

    // (d) a failure for the unprovisioned tenant leaves NO audit row: the audit
    //     write itself hits the tenant FK and is dropped best-effort (not fatal).
    //     Verify via a superuser pool (bypasses RLS) that `ghost` has zero events.
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&swap_db(&base_url, "synapse_it_security"))
        .await
        .expect("connect admin pool");
    let (ghost_events,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM audit_events WHERE tenant_id = $1")
            .bind(ghost)
            .fetch_one(&admin)
            .await
            .expect("count ghost audit events");
    admin.close().await;
    assert_eq!(
        ghost_events, 0,
        "unknown-tenant failure must not leak an audit row"
    );
    println!("(d) unknown-tenant failure left no audit row (best-effort FK drop)");

    println!("security hardening 2+4: unknown-tenant -> 400 everywhere; failures audited (deny/error); no cross-tenant audit leak.");
}
