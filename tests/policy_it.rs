//! Integration test for the PolicyGateway (coarse RBAC, PR1).
//!
//! Proves the role×action matrix end-to-end through the real router:
//!   - a `Viewer` (`X-Role: viewer`) is denied every write action with a 403
//!     `forbidden`, and each denial lands an audited `deny` row carrying
//!     `metadata.code = "forbidden"` under the caller's tenant;
//!   - a role-less caller (the `Member` baseline) still performs those writes;
//!   - a `Viewer` may perform every read action (never a 403).
//!
//! **DB-gated:** skipped unless `DATABASE_URL` is set. Run locally:
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=postgres -p 5459:5432 pgvector/pgvector:pg16
//! export DATABASE_URL=postgres://postgres:postgres@localhost:5459/postgres
//! cargo test --test policy_it -- --nocapture
//! ```

mod common;

use axum::http::StatusCode;
use common::{get_json_with_role, post_json_with_role, setup};
use serde_json::json;

#[tokio::test]
async fn viewer_is_denied_writes_and_allowed_reads() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };
    let router = setup(&base_url, "policy").await;
    let t = "tenant_a";

    // (a) Every write action is denied for a Viewer with a 403 `forbidden`.
    //     enforce() runs right after tenant resolution, so the deny is reached
    //     before any service/DB work (documents never touches the embedder here).
    let writes: [(&str, serde_json::Value); 6] = [
        (
            "/documents.ingest",
            json!({"doc_id": "d1", "tenant_id": t, "content": "hello world"}),
        ),
        (
            "/context.upsert",
            json!({"tenant_id": t, "principal_id": "u"}),
        ),
        (
            "/tool.execute",
            json!({"tenant_id": t, "principal_id": "u", "tool_id": "search"}),
        ),
        ("/runs.start", json!({"tenant_id": t, "run_type": "r"})),
        (
            "/skills.register",
            json!({"skill_id": "s", "version": "1.0.0", "name": "n"}),
        ),
        ("/runs.resume", json!({"run_id": "nope", "token": "nope"})),
    ];
    for (uri, body) in &writes {
        let (s, r) = post_json_with_role(&router, uri, t, "u", Some("viewer"), body.clone()).await;
        assert_eq!(
            s,
            StatusCode::FORBIDDEN,
            "{uri} as viewer -> 403, got {s}: {r}"
        );
        assert_eq!(r["error"]["code"], "forbidden", "{uri}: {r}");
    }
    println!("(a) viewer denied all 6 write actions with 403 forbidden");

    // (b) Each denial is audited as `deny` with metadata.code = "forbidden".
    //     `audit.events` is a read, so a Viewer may fetch its own deny trail.
    let (s, r) = get_json_with_role(&router, "/audit/events", t, "u", Some("viewer")).await;
    assert_eq!(s, StatusCode::OK, "viewer reads audit.events: {r}");
    let denies: Vec<&str> = r["events"]
        .as_array()
        .expect("events array")
        .iter()
        .filter(|e| e["outcome"] == "deny" && e["metadata"]["code"] == "forbidden")
        .map(|e| e["action"].as_str().unwrap())
        .collect();
    for action in [
        "documents.ingest",
        "context.upsert",
        "tool.execute",
        "runs.start",
        "skills.register",
        "runs.resume",
    ] {
        assert!(
            denies.contains(&action),
            "expected an audited deny for {action}, saw {denies:?}"
        );
    }
    println!("(b) every denied write left an audited deny row (metadata.code=forbidden)");

    // (c) The Member baseline (no X-Role) still performs the same writes. Spot-check
    //     the two that complete synchronously against a seeded tenant.
    let (s, r) = post_json_with_role(
        &router,
        "/documents.ingest",
        t,
        "u",
        None,
        json!({"doc_id": "d1", "tenant_id": t, "content": "hello world"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "member documents.ingest -> 200: {r}");
    let (s, r) = post_json_with_role(
        &router,
        "/runs.start",
        t,
        "u",
        None,
        json!({"tenant_id": t, "run_type": "ok"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "member runs.start -> 200: {r}");
    println!("(c) member baseline (no role) still performs writes (200)");

    // (d) A Viewer may perform every read action — never a 403. A missing row is a
    //     404 (not a 401/403): policy allowed the read, the resource just isn't there.
    let (s, _) = post_json_with_role(
        &router,
        "/retrieve",
        t,
        "u",
        Some("viewer"),
        json!({"tenant_id": t, "principal_id": "u", "query": "hello"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "viewer retrieve -> 200 (read allowed)");

    let (s, _) = post_json_with_role(
        &router,
        "/context.get",
        t,
        "u",
        Some("viewer"),
        json!({"principal_id": "absent"}),
    )
    .await;
    assert_ne!(s, StatusCode::FORBIDDEN, "viewer context.get must not 403");

    let (s, _) = post_json_with_role(
        &router,
        "/skills.get",
        t,
        "u",
        Some("viewer"),
        json!({"skill_id": "absent"}),
    )
    .await;
    assert_ne!(s, StatusCode::FORBIDDEN, "viewer skills.get must not 403");

    let (s, _) = get_json_with_role(&router, "/audit/events", t, "u", Some("viewer")).await;
    assert_eq!(
        s,
        StatusCode::OK,
        "viewer audit.events -> 200 (read allowed)"
    );
    println!(
        "(d) viewer allowed all read actions (retrieve 200, context/skills.get not-403, audit 200)"
    );

    // (e) An asserted-but-unrecognized role fails CLOSED to Viewer (least
    //     privilege) rather than silently promoting to the Member baseline: an
    //     unknown role is denied writes exactly like a Viewer.
    let (s, r) = post_json_with_role(
        &router,
        "/documents.ingest",
        t,
        "u",
        Some("auditor"),
        json!({"doc_id": "d2", "tenant_id": t, "content": "x"}),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::FORBIDDEN,
        "unknown role 'auditor' must fail closed (denied write), got {s}: {r}"
    );
    assert_eq!(r["error"]["code"], "forbidden", "{r}");
    println!("(e) unknown role (auditor) fails closed to read-only -> write 403");

    println!("PolicyGateway PR1: viewer writes -> 403 + audited deny; member writes -> 200; viewer reads -> allowed.");
}
