//! Integration test for the policy-guarded tool gateway (`POST /tool.execute`).
//!
//! **DB-gated:** skipped unless `DATABASE_URL` is set (CI stays database-free).
//! Each test provisions its own throwaway database via [`common`]. Run locally:
//!
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=postgres -p 5459:5432 pgvector/pgvector:pg16
//! export DATABASE_URL=postgres://postgres:postgres@localhost:5459/postgres
//! cargo test --test tool_it -- --nocapture
//! ```

mod common;

use axum::http::StatusCode;
use common::{get_json, post_json, setup};
use serde_json::json;

#[tokio::test]
async fn tool_gateway_approval_policy_and_audit() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };
    let router = setup(&base_url, "tools").await;

    // (a) auto tool (no server policy, default approval) -> executed.
    let (s, r) = post_json(&router, "/tool.execute", "tenant_a", "user_1",
        json!({"tenant_id": "tenant_a", "principal_id": "user_1", "tool_id": "calendar.read", "arguments": {"day": "today"}})).await;
    assert_eq!(s, StatusCode::OK, "auto tool: {r}");
    assert_eq!(r["status"], "executed");
    assert_eq!(r["requires_approval"], false);
    println!("(a) auto tool -> executed");

    // (b) client declares approval_mode=required -> pending, not executed.
    let (s, r) = post_json(
        &router,
        "/tool.execute",
        "tenant_a",
        "user_1",
        json!({"tenant_id": "tenant_a", "principal_id": "user_1", "tool_id": "calendar.write",
               "policy": {"approval_mode": "required", "reason": "writes to the calendar"}}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r["status"], "pending");
    assert_eq!(r["requires_approval"], true);
    println!("(b) client approval_mode=required -> pending");

    // (c) SERVER-side policy: a skill requiring "gmail.send" tagged
    //     human_approval_required forces approval even when the client asks for auto.
    let (s, _) = post_json(
        &router,
        "/skills.register",
        "tenant_a",
        "user_1",
        json!({"skill_id": "skill.email", "version": "1.0.0", "name": "Email",
               "required_tools": ["gmail.send"], "policy_tags": ["human_approval_required"]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, r) = post_json(
        &router,
        "/tool.execute",
        "tenant_a",
        "user_1",
        json!({"tenant_id": "tenant_a", "principal_id": "user_1", "tool_id": "gmail.send",
               "arguments": {"to": "x@y.z"}, "policy": {"approval_mode": "none"}}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        r["status"], "pending",
        "server policy must force approval despite client 'none': {r}"
    );
    assert_eq!(r["requires_approval"], true);
    println!("(c) server-side skill policy forces approval (client can't downgrade)");

    // (d) each governed call was audited (tenant-scoped), with the human-in-the-loop outcome.
    let (_s, r) = get_json(
        &router,
        "/audit/events?action=tool.execute",
        "tenant_a",
        "user_1",
    )
    .await;
    let ev = r["events"].as_array().unwrap();
    assert_eq!(ev.len(), 3, "3 tool.execute audit events for tenant_a: {r}");
    let outcomes: Vec<&str> = ev.iter().map(|e| e["outcome"].as_str().unwrap()).collect();
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| **o == "require_approval")
            .count(),
        2
    );
    assert_eq!(outcomes.iter().filter(|o| **o == "success").count(), 1);
    println!("(d) audit trail: 3 tool.execute events (1 success, 2 require_approval)");

    // (e) tenant isolation: tenant_b sees none of tenant_a's tool events, and its OWN
    //     gmail.send is auto-executed (the skill policy lives only in tenant_a).
    let (_s, r) = get_json(
        &router,
        "/audit/events?action=tool.execute",
        "tenant_b",
        "user_2",
    )
    .await;
    assert_eq!(
        r["events"].as_array().unwrap().len(),
        0,
        "tenant_b must not see tenant_a's tool events"
    );
    let (_s, r) = post_json(&router, "/tool.execute", "tenant_b", "user_2",
        json!({"tenant_id": "tenant_b", "principal_id": "user_2", "tool_id": "gmail.send", "arguments": {}})).await;
    assert_eq!(
        r["status"], "executed",
        "tenant_b gmail.send is auto (no cross-tenant skill policy): {r}"
    );
    println!("(e) tenant isolation: policy + audit both tenant-scoped");

    // (f) body-tenant mismatch -> 403; empty tool_id -> 400.
    let (s, _) = post_json(
        &router,
        "/tool.execute",
        "tenant_a",
        "user_1",
        json!({"tenant_id": "tenant_b", "principal_id": "user_1", "tool_id": "x"}),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "body tenant mismatch -> 403");
    let (s, _) = post_json(
        &router,
        "/tool.execute",
        "tenant_a",
        "user_1",
        json!({"tenant_id": "tenant_a", "principal_id": "user_1", "tool_id": "  "}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "empty tool_id -> 400");
    println!("(f) tenant-mismatch -> 403, empty tool_id -> 400");

    // (g) normalization: a skill registered with a whitespace/uppercase tag and a
    //     whitespace-padded tool still gates the tool — the approval gate can't be
    //     fooled OPEN by casing/spaces (required_tools trimmed, tags lowercased).
    let (s, _) = post_json(&router, "/skills.register", "tenant_a", "user_1",
        json!({"skill_id": "skill.slack", "version": "1.0.0", "name": "Slack",
               "required_tools": ["  slack.post  "], "policy_tags": ["  Human_Approval_Required  "]})).await;
    assert_eq!(s, StatusCode::OK);
    let (_s, r) = post_json(
        &router,
        "/tool.execute",
        "tenant_a",
        "user_1",
        json!({"tenant_id": "tenant_a", "principal_id": "user_1", "tool_id": "slack.post"}),
    )
    .await;
    assert_eq!(
        r["status"], "pending",
        "normalized tag/tool must still gate approval: {r}"
    );
    assert_eq!(r["requires_approval"], true);
    println!("(g) normalization: whitespace/case tag + tool still gates approval");

    // (h) policy EVOLUTION: publishing a newer skill version that drops the approval
    //     tag downgrades the tool to auto — only the LATEST version's policy applies,
    //     so an immutable old (v1, gated) version can't lock the requirement in forever.
    let (s, _) = post_json(
        &router,
        "/skills.register",
        "tenant_a",
        "user_1",
        json!({"skill_id": "skill.email", "version": "2.0.0", "name": "Email",
               "required_tools": ["gmail.send"], "policy_tags": []}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (_s, r) = post_json(&router, "/tool.execute", "tenant_a", "user_1",
        json!({"tenant_id": "tenant_a", "principal_id": "user_1", "tool_id": "gmail.send", "arguments": {}})).await;
    assert_eq!(
        r["status"], "executed",
        "latest skill version dropped the tag -> gmail.send now auto: {r}"
    );
    println!("(h) policy evolution: a newer version dropping the tag downgrades the tool to auto");

    println!("tool gateway: approval enforcement (client + server-side skill policy), executed vs pending, audit trail, tenant isolation all proven.");
}
